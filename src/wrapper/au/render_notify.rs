//! AU render notifications. The control side owns every immutable snapshot;
//! render traversal only borrows one, without locking, allocating, or reclaiming it.
//! Registration is a control operation and may lock/allocate (the SDK does not
//! mark Add/RemoveRenderNotify CA_REALTIME_API). Re-entry cannot deadlock on a
//! render-held registry lock, but does not make registration realtime-safe.

use std::ffi::c_void;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use au_sys as au;

// au-sys 0.1.1 exports the selectors but omits their procedure types.
pub(super) type NotifyProc =
    unsafe extern "C" fn(*mut c_void, Option<au::AURenderCallback>, *mut c_void) -> au::OSStatus;
pub(super) const POST_RENDER_ERROR: au::AudioUnitRenderActionFlags = 1 << 8;

#[derive(Clone, Copy)]
pub(super) struct Callback {
    pub proc: au::AURenderCallback,
    pub user_data: *mut c_void,
}

// The host keeps user_data alive while any render callback may be in flight.
unsafe impl Send for Callback {}
unsafe impl Sync for Callback {}

type List = Vec<Callback>;

pub(super) struct RenderNotifications {
    current: AtomicPtr<Arc<List>>,
    readers: AtomicUsize,
    // Boxes keep snapshot addresses stable when this owning vector grows.
    #[allow(clippy::vec_box)]
    snapshots: Mutex<Vec<Box<Arc<List>>>>,
}

impl RenderNotifications {
    pub fn new() -> Self {
        let mut initial = Box::new(Arc::new(Vec::new()));
        Self {
            current: AtomicPtr::new(&mut *initial),
            readers: AtomicUsize::new(0),
            snapshots: Mutex::new(vec![initial]),
        }
    }

    pub fn update(&self, callback: Callback, add: bool) -> au::OSStatus {
        let Ok(mut snapshots) = self.snapshots.lock() else {
            return au::kAudioUnitErr_CannotDoInCurrentContext;
        };
        let current = self.current.load(Ordering::SeqCst);
        // All writers hold snapshots; its boxes own current and retired lists.
        let mut next = unsafe { (*current).as_ref().clone() };
        let same = |entry: &Callback| {
            entry.proc as usize == callback.proc as usize && entry.user_data == callback.user_data
        };
        if add {
            if !next.iter().any(same) {
                next.push(callback);
            }
        } else {
            next.retain(|entry| !same(entry));
        }
        let mut next = Box::new(Arc::new(next));
        let next_ptr = &mut *next as *mut Arc<List>;
        snapshots.push(next);
        self.current.store(next_ptr, Ordering::SeqCst);
        // Never wait for readers: a callback may remove itself. Registration
        // still takes the control-side mutex above. The brief reader section
        // protects Arc cloning; thereafter the Arc pins only that render's list.
        // Retaining its owning box makes the audio-side Arc drop non-final.
        // SeqCst orders reader entry against publication and this zero check.
        if self.readers.load(Ordering::SeqCst) == 0 {
            snapshots.retain(|list| std::ptr::eq(&**list, next_ptr) || Arc::strong_count(list) > 1);
        }
        au::noErr
    }

    pub fn snapshot(&self) -> Snapshot<'_> {
        self.readers.fetch_add(1, Ordering::SeqCst);
        let current = self.current.load(Ordering::SeqCst);
        // The writer cannot reclaim this box during the clone. Its own Arc
        // remains retained until a subsequent control-side update sees no reader.
        let list = unsafe { (*current).clone() };
        self.readers.fetch_sub(1, Ordering::SeqCst);
        Snapshot {
            list,
            _owner: PhantomData,
        }
    }
}

pub(super) struct Snapshot<'a> {
    list: Arc<List>,
    _owner: PhantomData<&'a RenderNotifications>,
}

impl Snapshot<'_> {
    pub fn callbacks(&self) -> &[Callback] {
        &self.list
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn callback(
        _: *mut c_void,
        _: *mut au::AudioUnitRenderActionFlags,
        _: *const au::AudioTimeStamp,
        _: au::UInt32,
        _: au::UInt32,
        _: *mut au::AudioBufferList,
    ) -> au::OSStatus {
        au::noErr
    }

    #[test]
    fn identity_snapshot_and_control_side_reclamation() {
        let notifications = RenderNotifications::new();
        let a = Callback {
            proc: callback,
            user_data: std::ptr::null_mut(),
        };
        let mut context = 0u8;
        let b = Callback {
            user_data: &mut context as *mut _ as *mut c_void,
            ..a
        };
        assert_eq!(notifications.update(a, true), au::noErr);
        assert_eq!(notifications.update(a, true), au::noErr);
        assert_eq!(notifications.update(b, true), au::noErr);
        let active = notifications.snapshot();
        assert_eq!(active.callbacks().len(), 2);
        assert_eq!(notifications.update(a, false), au::noErr);
        assert_eq!(active.callbacks().len(), 2);
        assert_eq!(notifications.snapshot().callbacks().len(), 1);
        // Even changes made inside every render must not accumulate all old
        // lists. Only current and the in-flight snapshot remain retained.
        for _ in 0..1000 {
            assert_eq!(notifications.update(a, true), au::noErr);
            assert_eq!(notifications.update(a, false), au::noErr);
        }
        assert_eq!(active.callbacks().len(), 2);
        assert_eq!(notifications.snapshots.lock().unwrap().len(), 2);
        drop(active);
        assert_eq!(notifications.update(b, false), au::noErr);
        assert!(notifications.snapshot().callbacks().is_empty());
        assert_eq!(notifications.snapshots.lock().unwrap().len(), 1);
    }

    #[test]
    fn concurrent_registration_keeps_snapshots_alive() {
        let notifications = RenderNotifications::new();
        // Compare the value registered by the caller, not a second coercion
        // of the same function item: optimized builds can duplicate functions
        // across codegen units and give those coercions different addresses.
        let registered = std::hint::black_box(Callback {
            proc: callback,
            user_data: std::ptr::null_mut(),
        });
        assert_eq!(notifications.update(registered, true), au::noErr);
        let retained = notifications.snapshot();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..10000 {
                    assert_eq!(notifications.update(registered, true), au::noErr);
                    assert_eq!(notifications.update(registered, false), au::noErr);
                }
            });
            for _ in 0..10000 {
                let snapshot = notifications.snapshot();
                for cb in snapshot.callbacks() {
                    assert_eq!(cb.proc as usize, registered.proc as usize);
                    assert_eq!(cb.user_data, registered.user_data);
                }
            }
        });
        // This snapshot definitely contained the registered callback and must
        // survive every control-side replacement, regardless of scheduling.
        assert_eq!(retained.callbacks().len(), 1);
        assert_eq!(
            retained.callbacks()[0].proc as usize,
            registered.proc as usize
        );
        assert_eq!(retained.callbacks()[0].user_data, registered.user_data);
    }
}
