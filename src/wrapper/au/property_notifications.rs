//! Main-run-loop delivery of render-thread AU property changes.
//! The timer owns an Arc to notification state, never a pointer to Wrapper.
//! Closing an AU invalidates the timer before dropping the wrapper. A callback
//! keeps its own Arc so reentrant host disposal cannot free its state.
//! Wrapper entry points never dispatch inline: a callback can dispose the AU,
//! so delivery must not leave a borrowed Wrapper in use after host code returns.
use au_sys as au;
use core_foundation::base::TCFType;
use core_foundation::date::CFAbsoluteTimeGetCurrent;
use core_foundation::runloop::{
    kCFRunLoopCommonModes, CFRunLoopTimerContext, CFRunLoopTimerInvalidate, CFRunLoopTimerRef,
};
use core_foundation::runloop::{CFRunLoop, CFRunLoopTimer};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// One registered host property listener. The `proc` is called whenever the
/// matching property changes. The host owns `user_data`; we only forward it.
#[derive(Copy, Clone)]
pub(super) struct Listener {
    pub(super) property_id: au::AudioUnitPropertyID,
    pub(super) proc: au::AudioUnitPropertyListenerProc,
    pub(super) user_data: *mut c_void,
}

// SAFETY: `proc` is a function pointer (Send/Sync trivially), `user_data` is
// host-owned opaque storage we never dereference, only forward.
unsafe impl Send for Listener {}
unsafe impl Sync for Listener {}

// Pending notification bits. Each bit maps to a property whose listeners
// must be called from the main thread.
pub(super) const NOTIFY_LATENCY: u32 = 1 << 0;
pub(super) const NOTIFY_STREAM_FORMAT: u32 = 1 << 1;
pub(super) const NOTIFY_BYPASS_EFFECT: u32 = 1 << 2;
pub(super) const NOTIFY_MAX_FRAMES_PER_SLICE: u32 = 1 << 3;
pub(super) const NOTIFY_TAIL_TIME: u32 = 1 << 4;
pub(super) const NOTIFY_LAST_RENDER_ERROR: u32 = 1 << 5;
pub(super) const NOTIFY_PRESENT_PRESET: u32 = 1 << 6;

// Delivery owns gate1; a control-side listener removal owns gate2. A busy
// removal returns an error without changing the registration. No thread waits
// for host code, which may itself be waiting for the remover/closer to return.
struct DispatchGuard<'a>(&'a AtomicUsize);
impl Drop for DispatchGuard<'_> {
    fn drop(&mut self) {
        self.0.store(0, Ordering::SeqCst);
    }
}

pub(super) struct PropertyNotifications {
    instance: AtomicU64,
    pending: AtomicU32,
    dispatching: AtomicUsize,
    pub(super) listeners: Mutex<Vec<Listener>>,
    timer: Mutex<Option<CFRunLoopTimer>>,
}
// The CF timer is only created/invalidated under `timer`; delivery is on the
// main run loop. Core Foundation permits invalidation from another thread.
unsafe impl Send for PropertyNotifications {}
unsafe impl Sync for PropertyNotifications {}

extern "C" fn retain(info: *const c_void) -> *const c_void {
    unsafe {
        Arc::increment_strong_count(info as *const PropertyNotifications);
    }
    info
}
extern "C" fn release(info: *const c_void) {
    unsafe {
        Arc::decrement_strong_count(info as *const PropertyNotifications);
    }
}
extern "C" fn tick(_: CFRunLoopTimerRef, info: *mut c_void) {
    // Invalidation can release the timer context during a reentrant callback.
    // Retain locally before invoking any host code.
    let raw = info as *const PropertyNotifications;
    unsafe {
        Arc::increment_strong_count(raw);
    }
    let state = unsafe { Arc::from_raw(raw) };
    state.drain();
}
impl PropertyNotifications {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            instance: AtomicU64::new(0),
            pending: AtomicU32::new(0),
            dispatching: AtomicUsize::new(0),
            listeners: Mutex::new(Vec::new()),
            timer: Mutex::new(None),
        })
    }
    pub(super) fn start(self: &Arc<Self>, instance: au::AudioUnit) -> bool {
        let mut slot = match self.timer.lock() {
            Ok(slot) => slot,
            Err(_) => return false,
        };
        if slot.is_some() {
            return self.instance.load(Ordering::SeqCst) == instance as usize as u64;
        }
        self.instance
            .store(instance as usize as u64, Ordering::SeqCst);
        let mut context = CFRunLoopTimerContext {
            version: 0,
            info: Arc::as_ptr(self) as *mut c_void,
            retain: Some(retain),
            release: Some(release),
            copyDescription: None,
        };
        // Polling avoids allocations, locks or scheduler calls from render.
        // This intentionally costs one100Hz timer per open AU instance.
        let timer = CFRunLoopTimer::new(
            unsafe { CFAbsoluteTimeGetCurrent() } + 0.01,
            0.01,
            0,
            0,
            tick,
            &mut context,
        );
        CFRunLoop::get_main().add_timer(&timer, unsafe { kCFRunLoopCommonModes });
        *slot = Some(timer);
        true
    }
    pub(super) fn stop(&self) {
        self.instance.store(0, Ordering::SeqCst);
        self.pending.store(0, Ordering::Release);
        // Close cancels future dispatch, never waits for an in-flight host
        // callback. Its state survives via tick's Arc. Host-owned refcon must
        // remain valid until the host's current callback returns.
        let timer = self
            .timer
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        if let Some(timer) = timer {
            unsafe {
                CFRunLoopTimerInvalidate(timer.as_concrete_TypeRef());
            }
        }
    }
    pub(super) fn remove_listener(
        &self,
        id: au::AudioUnitPropertyID,
        proc: au::AudioUnitPropertyListenerProc,
        user_data: *mut c_void,
    ) -> bool {
        let _guard =
            match self
                .dispatching
                .compare_exchange(0, 2, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => Some(DispatchGuard(&self.dispatching)),
                Err(1) if unsafe { libc::pthread_main_np() } != 0 => None, // reentrant main callback
                Err(_) => return false,
            };
        let mut listeners = match self.listeners.lock() {
            Ok(listeners) => listeners,
            Err(_) => return false,
        };
        listeners.retain(|listener| {
            !(listener.property_id == id
                && listener.proc as usize == proc as usize
                && listener.user_data == user_data)
        });
        true
    }
    #[inline]
    pub(super) fn mark(&self, mask: u32) {
        self.pending.fetch_or(mask, Ordering::Release);
    }
    pub(super) fn drain(&self) {
        // Host callbacks must never run on a render/worker thread.
        if unsafe { libc::pthread_main_np() } == 0 {
            return;
        }
        if self
            .dispatching
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return; // control-side removal: keep pending bits for the next tick
        }
        let _guard = DispatchGuard(&self.dispatching);
        let pending = self.pending.swap(0, Ordering::AcqRel);
        if pending == 0 {
            return;
        }
        let instance = self.instance.load(Ordering::SeqCst) as usize as au::AudioUnit;
        if instance.is_null() {
            return;
        }
        // Snapshot listeners so the lock isn't held across host callbacks.
        let listeners = match self.listeners.lock() {
            Ok(g) => g.clone(),
            Err(_) => {
                self.mark(pending);
                return;
            }
        };
        let fire = |id: au::AudioUnitPropertyID, scope: au::AudioUnitScope| {
            for l in &listeners {
                if l.property_id == id
                    && self.instance.load(Ordering::SeqCst) == instance as usize as u64
                {
                    // A preceding callback can remove another listener and
                    // free its user_data. Recheck registration before use.
                    let still_registered = match self.listeners.lock() {
                        Ok(current) => current.iter().any(|current| {
                            current.property_id == l.property_id
                                && current.proc as usize == l.proc as usize
                                && current.user_data == l.user_data
                        }),
                        Err(_) => {
                            self.mark(pending);
                            return;
                        }
                    };
                    if !still_registered {
                        continue;
                    }
                    // SAFETY: host owns user_data; instance is the same
                    // AudioUnit handle the host registered against.
                    unsafe {
                        (l.proc)(l.user_data, instance, id, scope, 0);
                    }
                }
            }
        };
        if pending & NOTIFY_LATENCY != 0 {
            fire(au::kAudioUnitProperty_Latency, au::kAudioUnitScope_Global);
        }
        if pending & NOTIFY_STREAM_FORMAT != 0 {
            fire(
                au::kAudioUnitProperty_StreamFormat,
                au::kAudioUnitScope_Output,
            );
            fire(
                au::kAudioUnitProperty_StreamFormat,
                au::kAudioUnitScope_Input,
            );
        }
        if pending & NOTIFY_BYPASS_EFFECT != 0 {
            fire(
                au::kAudioUnitProperty_BypassEffect,
                au::kAudioUnitScope_Global,
            );
        }
        if pending & NOTIFY_MAX_FRAMES_PER_SLICE != 0 {
            fire(
                au::kAudioUnitProperty_MaximumFramesPerSlice,
                au::kAudioUnitScope_Global,
            );
        }
        if pending & NOTIFY_TAIL_TIME != 0 {
            fire(au::kAudioUnitProperty_TailTime, au::kAudioUnitScope_Global);
        }
        if pending & NOTIFY_LAST_RENDER_ERROR != 0 {
            fire(
                au::kAudioUnitProperty_LastRenderError,
                au::kAudioUnitScope_Global,
            );
        }
        if pending & NOTIFY_PRESENT_PRESET != 0 {
            fire(
                au::kAudioUnitProperty_PresentPreset,
                au::kAudioUnitScope_Global,
            );
        }
    }
}
