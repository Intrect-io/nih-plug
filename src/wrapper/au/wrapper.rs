//! AU wrapper — full render pipeline (AUv2).
//!
//! Implements the complete AUv2 plugin lifecycle:
//!   - Parameter hosting: `ParameterList`, `ParameterInfo`, `Get/SetParameter`
//!   - Stream format negotiation and `Initialize` / `Uninitialize`
//!   - `Render` with input-callback pull, bypass, and `Plugin::process`
//!   - Property listeners, transport / tempo via `HostCallbackInfo`
//!   - `BypassEffect` property with `ParamFlags::BYPASS` sync
//!
//! AU parameter IDs are simply the index into `param_map()` — stable across
//! one binary build (the nih-plug derive guarantees field declaration order).
//!
//! # Concurrency model
//!
//! AU hosts call us from at least two threads concurrently:
//!   - **main thread** — `SetProperty`, `GetProperty`, `SetParameter` (UI),
//!     `Initialize` / `Uninitialize`
//!   - **audio thread** — `Render`, `SetParameter` (sample-accurate automation)
//!
//! The wrapper exposes only `&Self` from `from_ptr` so we can never construct
//! two `&mut Self` simultaneously. Mutable state is split into three buckets:
//!
//!   1. **host-setup atomics** — `sample_rate`, `n_channels`,
//!      `max_frames_per_slice`, `latency_seconds`, `initialized`. Written by
//!      main thread before `Initialize`, read by the audio thread thereafter.
//!      `AtomicU32`/`AtomicU64` (latency/sr packed as f64 bits).
//!
//!   2. **audio-thread-owned scratch** — `input_scratch`. Inside `UnsafeCell`,
//!      only ever touched from `render()`. AU guarantees render is not
//!      re-entered, so a `&mut` borrow inside that scope is sound.
//!
//!   3. **main↔audio shared state** — existing audio-input wiring is protected
//!      by a mutex and snapshotted into a local `Copy`. MIDI output callbacks
//!      use an atomic pointer plus reader-count reclamation so their render path
//!      never blocks while replaced callback records are reclaimed promptly.

use std::any::Any;
use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::marker::PhantomData;
use std::mem;
use std::num::NonZeroU32;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use au_sys as au;
use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::data::CFData;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;

use crate::buffer::Buffer;
use crate::context::process::Transport;
use crate::editor::Editor;
use crate::params::internals::ParamPtr;
use crate::params::{ParamFlags, Params};
use crate::plugin::au::AuPlugin;
use crate::prelude::{
    AudioIOLayout, AuxiliaryBuffers, BufferConfig, MidiConfig, ProcessMode, ProcessStatus,
};
use crate::wrapper::state::{self, PluginState};

use super::context::{
    AUParameter, AUParameterListenerNotify, AuGuiContextInner, AuInitContext, AuProcessContext,
    ContextSink,
};
use super::factory::fourcc;
use super::midi;

/// Payload of `kAudioUnitProperty_MakeConnection`.
///
/// AU v2 gives a host two ways to feed an effect's input bus: install a render
/// callback (`kAudioUnitProperty_SetRenderCallback`), or wire a source unit
/// directly with this property. Logic Pro uses the latter; Ableton Live and
/// most other hosts use the former. Supporting only callbacks therefore looks
/// correct everywhere except Logic, where the input bus is never connected and
/// the plug-in renders silence while reporting no error at all.
///
/// `au_sys` 0.1.1 defines the property ID but not this struct, so the
/// AudioToolbox layout is mirrored here.
#[repr(C)]
#[derive(Clone, Copy)]
struct AudioUnitConnection {
    source_audio_unit: au::AudioUnit,
    source_output_number: au::UInt32,
    dest_input_number: au::UInt32,
}

// The pointer field is why this needs a manual `Send`. The host owns the
// source unit and must disconnect before disposing it, so the pointer stays
// valid for as long as we hold the connection; calling `AudioUnitRender` on it
// from the audio thread is precisely the contract AU defines for a connection.
unsafe impl Send for AudioUnitConnection {}

#[link(name = "AudioToolbox", kind = "framework")]
extern "C" {
    /// Pull audio from a connected source unit. Not bound by `au_sys`, which
    /// models the plug-in side — normally only ever called *into*. A connected
    /// input is the one case where the wrapper has to call back out.
    fn AudioUnitRender(
        in_unit: au::AudioUnit,
        io_action_flags: *mut au::AudioUnitRenderActionFlags,
        in_time_stamp: *const au::AudioTimeStamp,
        in_output_bus_number: au::UInt32,
        in_number_frames: au::UInt32,
        io_data: *mut au::AudioBufferList,
    ) -> au::OSStatus;
}

/// Where `render` pulls main input from for this block.
#[derive(Clone, Copy)]
enum InputSource {
    Callback(au::AURenderCallbackStruct),
    Connection(AudioUnitConnection),
}

/// Type-erased slot that holds (or clears) the active GUI editor handle.
///
/// The CocoaUI C-ABI callbacks (`nih_plug_au_cocoaui_spawn` /
/// `nih_plug_au_cocoaui_close_view`) cannot be generic over `P`, so we give
/// them a raw `*const GuiHandleSlot` instead of `*const Wrapper<P>`.  The
/// slot is `#[repr(C)]` and contains only a `Mutex`, so its address is stable
/// and its layout is independent of `P`.
#[repr(C)]
pub struct GuiHandleSlot(Mutex<Option<Box<dyn Any + Send>>>);

impl GuiHandleSlot {
    fn new() -> Self {
        Self(Mutex::new(None))
    }
    fn set(&self, handle: Box<dyn Any + Send>) {
        if let Ok(mut g) = self.0.lock() {
            *g = Some(handle);
        }
    }
    fn clear(&self) {
        if let Ok(mut g) = self.0.lock() {
            *g = None;
        }
    }
}

/// One AU plugin instance. Owned by Apple's component manager via the
/// `AudioComponentPlugInInterface` pointer returned from the factory.
///
/// The first field MUST be the vtable, since Apple's component manager
/// dispatches through `instance->vtable->Lookup(selector)`.
#[repr(C)]
pub struct Wrapper<P: AuPlugin> {
    /// Apple-required vtable. MUST be the first field.
    vtable: au::AudioComponentPlugInInterface,

    /// Host's `AudioUnit` opaque handle, set in `Open` (main thread, once).
    /// Stored as raw ptr behind atomic so we can read from any thread.
    instance: AtomicU64,

    /// Sample rate set via `kAudioUnitProperty_StreamFormat`. f64 bits.
    sample_rate_bits: AtomicU64,

    /// Maximum frames per `AudioUnitRender` slice.
    max_frames_per_slice: AtomicU32,

    /// Channel count set via `StreamFormat`.
    n_channels: AtomicU32,

    /// Latency reported via `kAudioUnitProperty_Latency`. f64 bits.
    latency_seconds_bits: AtomicU64,

    /// Tail duration reported by the most recent `Plugin::process()` call.
    /// Stored as f64 bits so the render thread can update it lock-free.
    tail_seconds_bits: AtomicU64,

    /// OSStatus returned by the most recent failed render, or `noErr` after a
    /// successful render.
    last_render_error: AtomicI32,

    /// Whether `Plugin::initialize()` has run successfully since the last
    /// `Uninitialize` / first construction. Render is a no-op when false.
    initialized: AtomicBool,

    /// Host-controlled bypass (`kAudioUnitProperty_BypassEffect`). When set,
    /// `render()` skips `Plugin::process()` and just passes input → output.
    /// If the plugin declares a `ParamFlags::BYPASS` parameter we keep it in
    /// sync so plugins that observe bypass state through their own param see
    /// the toggle too.
    bypass: AtomicBool,

    /// Index of the BYPASS-flagged param in `params_by_id`, if any.
    bypass_param_idx: Option<usize>,

    /// Property listeners registered by the host. Notifications fire on the
    /// main thread; audio-thread changes (latency, etc.) only set bits in
    /// `pending_notifications` and the next main-thread entry drains them.
    listeners: Mutex<Vec<Listener>>,

    /// Bitset of property changes pending main-thread notification.
    /// See `NOTIFY_*` constants.
    pending_notifications: AtomicU32,

    /// The plugin instance. Kept for the entire lifetime of the wrapper so
    /// the `ParamPtr` raw pointers in `params_by_id` remain valid.
    ///
    /// Wrapped in `UnsafeCell` because `Plugin::initialize` / `process` /
    /// `reset` / `deactivate` take `&mut self`, but we only ever expose `&Self`
    /// from `from_ptr`. AU's threading model guarantees these methods are not
    /// concurrent with each other (Initialize/Uninitialize/Reset run on main,
    /// process runs on audio thread, and the host serialises lifecycle vs.
    /// render via `Initialize`/`Uninitialize` boundaries — render can only
    /// happen between them).
    plugin: UnsafeCell<Box<P>>,

    /// Parameter handles in declaration order. The AU parameter ID is the
    /// index into this vec. Built once in `new()`, then read-only.
    params_by_id: Vec<ParamEntry>,

    /// Host callbacks for transport, tempo, and time signature.
    host_callbacks: Mutex<Option<au::HostCallbackInfo>>,

    /// Strong reference back to the `Params` object referenced by every
    /// `ParamPtr` in `params_by_id`.
    _params_arc: Arc<dyn Params>,

    /// Scratch sink shared between Init/Process contexts. Lets the plugin
    /// report a latency change back via `set_latency_samples()`.
    sink: Arc<ContextSink>,

    /// Input render callback registered by the host via
    /// `kAudioUnitProperty_SetRenderCallback` on element 0 (main input).
    /// Invoked at the top of every `render()` call to pull main input audio.
    ///
    /// `AURenderCallbackStruct` is `Copy`. Audio thread snapshots out under
    /// the mutex at the start of render and releases immediately.
    input_callback: Mutex<Option<au::AURenderCallbackStruct>>,

    /// Source unit wired into the main input by the host via
    /// `kAudioUnitProperty_MakeConnection` (element 0). Mutually exclusive
    /// with `input_callback`: whichever the host installed last wins, and
    /// clearing one clears the other so a stale wiring can never be pulled
    /// after the host meant to replace it.
    ///
    /// Swapping the two is not atomic — they are separate mutexes, so a render
    /// landing between the two updates uses the previous wiring for that one
    /// block. That is harmless: the host is mid-reconnect, and both wirings
    /// produce valid audio; the next block picks up the new one.
    input_connection: Mutex<Option<AudioUnitConnection>>,

    /// Bounded queue populated by the MusicDevice selector calls and drained
    /// once at the start of each render block.
    midi_input: midi::MidiInputState<P>,

    /// Host callback installed through `kAudioUnitProperty_MIDIOutputCallback`.
    /// Loads from render are lock-free; the control-thread writer waits for the
    /// brief pointer-copy section before reclaiming a retired record.
    midi_output_callback: midi::MidiOutputCallbackSlot,

    /// Per-aux-input-port render callbacks (elements 1, 2, … of Input scope).
    /// Length equals `P::AUDIO_IO_LAYOUTS` max `aux_input_ports.len()`.
    /// Each `Mutex` is independent so the audio thread can snapshot without
    /// blocking on a single lock.
    aux_input_callbacks: Vec<Mutex<Option<au::AURenderCallbackStruct>>>,

    /// Audio-thread-owned render state: input scratch, BufferList scratch,
    /// and the persistent `Buffer<'static>` whose channel slot vector is
    /// pre-sized in `Initialize`.
    ///
    /// `UnsafeCell` because only `render()` touches it after `Initialize`,
    /// and AU does not re-enter render.
    render_state: UnsafeCell<RenderState<P>>,

    /// The plugin's `Editor` instance, if the plugin provides one.
    /// Created once in `new()` and never replaced.
    editor: Option<Arc<Mutex<Box<dyn Editor>>>>,

    /// The active editor window handle, returned by `Editor::spawn()`.
    /// Present while the GUI window is open, None otherwise.
    /// Exposed as a `GuiHandleSlot` so the C-ABI CocoaUI callbacks can
    /// update it without being generic over P.
    editor_handle: GuiHandleSlot,

    /// Shared context forwarded to the spawned editor so it can set
    /// parameter values and query plugin state.
    gui_context_inner: Option<Arc<AuGuiContextInner>>,
}

/// All audio-thread mutable state. Reused across render calls and grown
/// only inside `Initialize` (main thread, before render is allowed). The
/// render hot path only writes existing slots — no allocation.
struct RenderState<P: AuPlugin> {
    /// Per-channel scratch for input pulled via the host's render callback.
    /// One inner vec per channel, each pre-sized to `max_frames_per_slice`.
    input_scratch: Vec<Vec<f32>>,

    /// Backing storage for the synthesised `AudioBufferList` we hand to
    /// the host's input callback. Sized once in `Initialize` to fit the
    /// header + N `AudioBuffer` entries with correct alignment.
    /// Stored as `Vec<u64>` to guarantee 8-byte alignment, which matches
    /// `AudioBuffer`'s layout (one `u32` + one `u32` + one `*mut c_void`).
    bl_storage: Vec<u64>,

    /// Persistent `Buffer` whose `output_slices` vector has its capacity
    /// pre-grown to the channel count. Each render call rewrites the
    /// existing slots in place — no `Vec::push`/`with_capacity`.
    ///
    /// The `'static` lifetime parameter is a placeholder; the slices we
    /// stash inside live only as long as the surrounding `render()` call,
    /// and we always clear them before returning.
    buffer: Buffer<'static>,

    /// Per-aux-port scratch storage: `aux_input_scratch[port][channel][frame]`.
    /// Sized in `provision`; the render hot path only writes existing slots.
    aux_input_scratch: Vec<Vec<Vec<f32>>>,

    /// Per-aux-port `AudioBufferList` backing storage (same alignment trick as
    /// `bl_storage`). One `Vec<u64>` per aux input port.
    aux_bl_storages: Vec<Vec<u64>>,

    /// Per-aux-port `Buffer<'static>` whose slot vectors are pre-grown in
    /// `provision`. Slices are cleared after each `render()` call.
    aux_buffers: Vec<Buffer<'static>>,

    /// Preallocated MIDI input/output queues and MIDIPacketList storage.
    midi: midi::MidiRenderState<P>,
}

impl<P: AuPlugin> RenderState<P> {
    fn new() -> Self {
        Self {
            input_scratch: Vec::new(),
            bl_storage: Vec::new(),
            buffer: Buffer::default(),
            aux_input_scratch: Vec::new(),
            aux_bl_storages: Vec::new(),
            aux_buffers: Vec::new(),
            midi: midi::MidiRenderState::new(),
        }
    }

    /// Provision storage for `n_channels` main channels, `aux_ports` aux
    /// input ports (each with its own channel count), and `max_frames` frames.
    /// Called from `Initialize` only.
    fn provision(&mut self, n_channels: usize, max_frames: usize, aux_ports: &[NonZeroU32]) {
        self.input_scratch.clear();
        self.input_scratch.reserve_exact(n_channels);
        for _ in 0..n_channels {
            self.input_scratch.push(vec![0.0_f32; max_frames]);
        }

        // Layout the BufferList: header + N AudioBuffers, with the N-array
        // starting at the natural offset of `mBuffers` (8-byte aligned).
        let bl_bytes = bl_byte_size(n_channels);
        // Round up to u64 count.
        let words = bl_bytes.div_ceil(mem::size_of::<u64>());
        self.bl_storage.clear();
        self.bl_storage.resize(words, 0);

        // Pre-size the channel slot vector so render only rewrites slots.
        // SAFETY: empty slices carry no provenance; rewriting them in
        // render with valid host pointers is the same pattern used by
        // BufferManager::for_audio_io_layout.
        unsafe {
            self.buffer.set_slices(0, |slices| {
                slices.clear();
                slices.reserve_exact(n_channels);
                for _ in 0..n_channels {
                    slices.push(&mut []);
                }
            });
        }

        // Aux input ports: scratch storage + BufferList storage + Buffer slots.
        self.aux_input_scratch.clear();
        self.aux_bl_storages.clear();
        self.aux_buffers.clear();
        for &port_ch in aux_ports {
            let n_ch = port_ch.get() as usize;
            // Per-channel scratch frames.
            self.aux_input_scratch
                .push(vec![vec![0.0_f32; max_frames]; n_ch]);
            // BufferList backing storage.
            let words = bl_byte_size(n_ch).div_ceil(mem::size_of::<u64>());
            self.aux_bl_storages.push(vec![0u64; words]);
            // Pre-sized Buffer.
            let mut buf = Buffer::default();
            unsafe {
                buf.set_slices(0, |slices| {
                    slices.clear();
                    slices.reserve_exact(n_ch);
                    for _ in 0..n_ch {
                        slices.push(&mut []);
                    }
                });
            }
            self.aux_buffers.push(buf);
        }
    }
}

/// Compute the exact byte size of an `AudioBufferList` with `n` buffers,
/// honoring the offset of `mBuffers` (which the C compiler inserts padding
/// for so the array is correctly aligned).
#[inline]
fn bl_byte_size(n: usize) -> usize {
    // The platform's `AudioBufferList` declares `mBuffers: [AudioBuffer; 1]`.
    // Its `offset_of(mBuffers)` is the correct start of the array on this
    // ABI — typically 8 on x86_64/arm64 (4 bytes for `mNumberBuffers` + 4
    // bytes of padding before the 8-byte-aligned `AudioBuffer`).
    let header_offset = mem::offset_of!(au::AudioBufferList, mBuffers);
    header_offset + n * mem::size_of::<au::AudioBuffer>()
}

struct ParamEntry {
    /// Stable string ID — currently unused but kept for future state save/load.
    #[allow(dead_code)]
    id_str: String,
    ptr: ParamPtr,
}

/// One registered host property listener. The `proc` is called whenever the
/// matching property changes. The host owns `user_data`; we only forward it.
#[derive(Copy, Clone)]
struct Listener {
    property_id: au::AudioUnitPropertyID,
    proc: au::AudioUnitPropertyListenerProc,
    user_data: *mut c_void,
}

// SAFETY: `proc` is a function pointer (Send/Sync trivially), `user_data` is
// host-owned opaque storage we never dereference, only forward.
unsafe impl Send for Listener {}
unsafe impl Sync for Listener {}

// Bits in `pending_notifications`. Each bit maps to a property whose listeners
// must be called from the main thread.
const NOTIFY_LATENCY: u32 = 1 << 0;
const NOTIFY_STREAM_FORMAT: u32 = 1 << 1;
const NOTIFY_BYPASS_EFFECT: u32 = 1 << 2;
const NOTIFY_MAX_FRAMES_PER_SLICE: u32 = 1 << 3;
const NOTIFY_TAIL_TIME: u32 = 1 << 4;
const NOTIFY_LAST_RENDER_ERROR: u32 = 1 << 5;

/// `Send` + `Sync` justification:
///
/// - All publicly mutable fields are atomic or behind a `Mutex`.
/// - `plugin` and `input_scratch` are `UnsafeCell`-wrapped, accessed only
///   through `&mut` borrows constructed during a single audio-thread render
///   call (`input_scratch`) or during main-thread lifecycle calls that AU
///   serialises against render (`plugin`). The host contract forbids
///   `Initialize` / `Uninitialize` / `Reset` concurrent with `Render`.
/// - `ParamPtr` in `params_by_id` is built once and read-only thereafter;
///   per nih-plug contract, `set_normalized_value` and the value getters
///   are themselves thread-safe.
/// - The `vtable` and `_params_arc` are immutable after construction.
unsafe impl<P: AuPlugin> Send for Wrapper<P> {}
unsafe impl<P: AuPlugin> Sync for Wrapper<P> {}

#[inline]
fn pack_f64(v: f64) -> u64 {
    v.to_bits()
}
#[inline]
fn unpack_f64(b: u64) -> f64 {
    f64::from_bits(b)
}

impl<P: AuPlugin> Wrapper<P> {
    pub fn new() -> *mut au::AudioComponentPlugInInterface {
        let mut plugin = Box::new(P::default());
        let params_arc = plugin.params();

        let params_by_id: Vec<ParamEntry> = params_arc
            .param_map()
            .into_iter()
            .map(|(id_str, ptr, _group)| ParamEntry { id_str, ptr })
            .collect();

        // Find the BYPASS-flagged param, if the plugin declares one.
        let bypass_param_idx = params_by_id.iter().position(|e| {
            // SAFETY: ParamPtr accessors are documented thread-safe.
            unsafe { e.ptr.flags() }.contains(ParamFlags::BYPASS)
        });

        // Build (ParamPtr → AU param ID) map for GuiContext.
        let params_by_ptr: Vec<(ParamPtr, au::AudioUnitParameterID)> = params_by_id
            .iter()
            .enumerate()
            .map(|(idx, e)| (e.ptr, idx as au::AudioUnitParameterID))
            .collect();

        // Call editor() before moving `plugin` into the UnsafeCell.
        // `AsyncExecutor` stubs — GUI background tasks not yet wired.
        let async_executor = crate::context::gui::AsyncExecutor::<P> {
            execute_background: Arc::new(|_| {}),
            execute_gui: Arc::new(|_| {}),
        };
        let editor = plugin
            .editor(async_executor)
            .map(|e| Arc::new(Mutex::new(e)));

        // `gui_context_inner` is created here with instance = null; the
        // real AudioUnit handle is filled in by `open()`. Because
        // `AuGuiContextInner` is behind an `Arc` the editor closure
        // captures a clone, and we update the pointer inside `open()`.
        // However, `instance` is just an opaque pointer forwarded to the
        // host — for the open() case where the GUI hasn't spawned yet this
        // is fine: the GUI only opens after the component is open.
        let gui_context_inner = editor.as_ref().map(|_| {
            Arc::new(AuGuiContextInner {
                instance_bits: AtomicU64::new(0),
                params_arc: params_arc.clone(),
                params_by_ptr,
            })
        });

        let boxed = Box::new(Wrapper::<P> {
            vtable: au::AudioComponentPlugInInterface {
                Open: Self::open,
                Close: Self::close,
                Lookup: Self::lookup,
                reserved: ptr::null_mut(),
            },
            instance: AtomicU64::new(0),
            sample_rate_bits: AtomicU64::new(pack_f64(44_100.0)),
            max_frames_per_slice: AtomicU32::new(1024),
            n_channels: AtomicU32::new(2),
            latency_seconds_bits: AtomicU64::new(pack_f64(0.0)),
            tail_seconds_bits: AtomicU64::new(pack_f64(0.0)),
            last_render_error: AtomicI32::new(au::noErr),
            initialized: AtomicBool::new(false),
            bypass: AtomicBool::new(false),
            bypass_param_idx,
            plugin: UnsafeCell::new(plugin),
            params_by_id,
            host_callbacks: Mutex::new(None),
            _params_arc: params_arc,
            sink: ContextSink::new(),
            input_callback: Mutex::new(None),
            input_connection: Mutex::new(None),
            midi_input: midi::MidiInputState::new(),
            midi_output_callback: midi::MidiOutputCallbackSlot::new(),
            aux_input_callbacks: {
                let n_aux = P::AUDIO_IO_LAYOUTS
                    .iter()
                    .map(|l| l.aux_input_ports.len())
                    .max()
                    .unwrap_or(0);
                (0..n_aux).map(|_| Mutex::new(None)).collect()
            },
            listeners: Mutex::new(Vec::new()),
            pending_notifications: AtomicU32::new(0),
            render_state: UnsafeCell::new(RenderState::new()),
            editor,
            editor_handle: GuiHandleSlot::new(),
            gui_context_inner,
        });
        Box::into_raw(boxed) as *mut au::AudioComponentPlugInInterface
    }

    /// Returns a shared reference to the wrapper. We never hand out `&mut Self`
    /// because AU's main + audio threads can both be inside the wrapper at
    /// the same time and `&mut Self` aliasing would be UB.
    #[inline]
    unsafe fn from_ptr<'a>(ptr: *mut c_void) -> &'a Self {
        unsafe { &*(ptr as *const Self) }
    }

    // ─────────────────────────────────────────────────────────────────────
    // Field accessors (atomic)
    // ─────────────────────────────────────────────────────────────────────

    #[inline]
    fn sample_rate(&self) -> f64 {
        unpack_f64(self.sample_rate_bits.load(Ordering::Acquire))
    }
    #[inline]
    fn set_sample_rate(&self, sr: f64) {
        self.sample_rate_bits.store(pack_f64(sr), Ordering::Release);
    }
    #[inline]
    fn latency_seconds(&self) -> f64 {
        unpack_f64(self.latency_seconds_bits.load(Ordering::Acquire))
    }
    #[inline]
    fn set_latency_seconds(&self, l: f64) {
        self.latency_seconds_bits
            .store(pack_f64(l), Ordering::Release);
    }
    #[inline]
    fn tail_seconds(&self) -> f64 {
        unpack_f64(self.tail_seconds_bits.load(Ordering::Acquire))
    }
    #[inline]
    fn set_tail_seconds(&self, seconds: f64) {
        let previous = self
            .tail_seconds_bits
            .swap(pack_f64(seconds), Ordering::AcqRel);
        if previous != pack_f64(seconds) {
            self.mark_pending(NOTIFY_TAIL_TIME);
        }
    }
    #[inline]
    fn set_last_render_error(&self, status: au::OSStatus) {
        let previous = self.last_render_error.swap(status, Ordering::AcqRel);
        if status != au::noErr && previous != status {
            self.mark_pending(NOTIFY_LAST_RENDER_ERROR);
        }
    }
    #[inline]
    fn n_channels(&self) -> u32 {
        self.n_channels.load(Ordering::Acquire)
    }
    #[inline]
    fn max_frames_per_slice(&self) -> u32 {
        self.max_frames_per_slice.load(Ordering::Acquire)
    }
    #[inline]
    fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    /// Mark `mask` of property bits as needing notification. Cheap atomic OR;
    /// callable from any thread.
    #[inline]
    fn mark_pending(&self, mask: u32) {
        self.pending_notifications.fetch_or(mask, Ordering::Release);
    }

    /// Drain any pending notifications and call registered listeners.
    /// **Must be called from the main thread only** — host listener procs
    /// generally are not audio-safe. Called from main-thread entry points
    /// (`get_property`, `set_property`, `initialize`, etc.).
    fn drain_notifications(&self) {
        let pending = self.pending_notifications.swap(0, Ordering::AcqRel);
        if pending == 0 {
            return;
        }
        let instance = self.instance.load(Ordering::Acquire) as usize as au::AudioUnit;
        if instance.is_null() {
            return;
        }
        // Snapshot listeners so the lock isn't held across host callbacks.
        let listeners = match self.listeners.lock() {
            Ok(g) => g.clone(),
            Err(_) => return,
        };
        let fire = |id: au::AudioUnitPropertyID, scope: au::AudioUnitScope| {
            for l in &listeners {
                if l.property_id == id {
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
    }

    /// SAFETY: caller must guarantee no other reference (mut or shared) to
    /// `*self.plugin.get()` is alive. AU's contract gives this for the
    /// `Initialize` / `Uninitialize` / `Reset` / `Render` paths because the
    /// host serialises lifecycle calls vs. render and never re-enters render.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    unsafe fn plugin_mut(&self) -> &mut P {
        unsafe { &mut **self.plugin.get() }
    }

    /// SAFETY: same as `plugin_mut` — only call from `render()` or from
    /// `initialize()` (host serialises against render). Returns the
    /// `RenderState` for in-place mutation by the audio thread.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    unsafe fn render_state_mut(&self) -> &mut RenderState<P> {
        unsafe { &mut *self.render_state.get() }
    }

    // ─────────────────────────────────────────────────────────────────────
    // Component manager vtable
    // ─────────────────────────────────────────────────────────────────────

    unsafe extern "C" fn open(self_ptr: *mut c_void, instance: au::AudioUnit) -> au::OSStatus {
        let this = unsafe { Self::from_ptr(self_ptr) };
        let bits = instance as usize as u64;
        this.instance.store(bits, Ordering::Release);
        // Update gui_context_inner so any later GUI spawn sees the real handle.
        if let Some(inner) = &this.gui_context_inner {
            inner.instance_bits.store(bits, Ordering::Release);
        }
        au::noErr
    }

    unsafe extern "C" fn close(self_ptr: *mut c_void) -> au::OSStatus {
        // Drop editor handle before the wrapper is destroyed.
        let this = unsafe { Self::from_ptr(self_ptr) };
        if this.initialized.swap(false, Ordering::AcqRel) {
            // Hosts are allowed to dispose an initialized AudioUnit without a
            // preceding Uninitialize call. Keep Plugin's lifecycle balanced.
            unsafe { this.plugin_mut() }.deactivate();
        }
        this.midi_input.clear();
        let instance = this.instance.swap(0, Ordering::AcqRel) as usize as *mut c_void;
        cocoaui::close_audio_unit_view(instance);
        this.editor_handle.clear();
        unsafe {
            let _ = Box::from_raw(self_ptr as *mut Self);
        }
        au::noErr
    }

    unsafe extern "C" fn lookup(selector: au::SInt16) -> Option<au::AudioComponentMethod> {
        let method: au::AudioComponentMethod = match selector {
            au::kAudioUnitInitializeSelect => unsafe {
                std::mem::transmute::<au::AudioUnitInitializeProc, _>(Self::initialize)
            },
            au::kAudioUnitUninitializeSelect => unsafe {
                std::mem::transmute::<au::AudioUnitUninitializeProc, _>(Self::uninitialize)
            },
            au::kAudioUnitGetPropertyInfoSelect => unsafe {
                std::mem::transmute::<au::AudioUnitGetPropertyInfoProc, _>(Self::get_property_info)
            },
            au::kAudioUnitGetPropertySelect => unsafe {
                std::mem::transmute::<au::AudioUnitGetPropertyProc, _>(Self::get_property)
            },
            au::kAudioUnitSetPropertySelect => unsafe {
                std::mem::transmute::<au::AudioUnitSetPropertyProc, _>(Self::set_property)
            },
            au::kAudioUnitGetParameterSelect => unsafe {
                std::mem::transmute::<au::AudioUnitGetParameterProc, _>(Self::get_parameter)
            },
            au::kAudioUnitSetParameterSelect => unsafe {
                std::mem::transmute::<au::AudioUnitSetParameterProc, _>(Self::set_parameter)
            },
            au::kAudioUnitResetSelect => unsafe {
                std::mem::transmute::<au::AudioUnitResetProc, _>(Self::reset)
            },
            au::kAudioUnitRenderSelect => unsafe {
                std::mem::transmute::<au::AudioUnitRenderProc, _>(Self::render)
            },
            au::kAudioUnitAddPropertyListenerSelect => unsafe {
                std::mem::transmute::<au::AudioUnitAddPropertyListenerProc, _>(
                    Self::add_property_listener,
                )
            },
            au::kAudioUnitRemovePropertyListenerWithUserDataSelect => unsafe {
                std::mem::transmute::<au::AudioUnitRemovePropertyListenerWithUserDataProc, _>(
                    Self::remove_property_listener_with_user_data,
                )
            },
            midi::MUSIC_DEVICE_MIDI_EVENT_SELECT if P::MIDI_INPUT >= MidiConfig::Basic => unsafe {
                std::mem::transmute::<midi::MusicDeviceMidiEventProc, _>(
                    Self::music_device_midi_event,
                )
            },
            midi::MUSIC_DEVICE_SYS_EX_SELECT if P::MIDI_INPUT >= MidiConfig::Basic => unsafe {
                std::mem::transmute::<midi::MusicDeviceSysExProc, _>(Self::music_device_sys_ex)
            },
            midi::MUSIC_DEVICE_START_NOTE_SELECT if P::MIDI_INPUT >= MidiConfig::Basic => unsafe {
                std::mem::transmute::<midi::MusicDeviceStartNoteProc, _>(
                    Self::music_device_start_note,
                )
            },
            midi::MUSIC_DEVICE_STOP_NOTE_SELECT if P::MIDI_INPUT >= MidiConfig::Basic => unsafe {
                std::mem::transmute::<midi::MusicDeviceStopNoteProc, _>(
                    Self::music_device_stop_note,
                )
            },
            _ => return None,
        };
        Some(method)
    }

    /// Called by the host once the audio configuration has been fully
    /// queried — sample rate, stream format, max frames per slice are all
    /// already set. Build the nih-plug `BufferConfig` / `AudioIOLayout`
    /// from those and forward to `Plugin::initialize`.
    unsafe extern "C" fn initialize(self_ptr: *mut c_void) -> au::OSStatus {
        let this = unsafe { Self::from_ptr(self_ptr) };

        let n_ch = this.n_channels().max(1);
        let chans = NonZeroU32::new(n_ch).expect("n_ch is clamped to at least one");
        let selected_layout = match layout_for_output::<P>(chans) {
            Some(layout) => *layout,
            None => return au::kAudioUnitErr_FormatNotSupported,
        };
        let max_frames = this.max_frames_per_slice();
        let sr = this.sample_rate();
        let buffer_config = BufferConfig {
            sample_rate: sr as f32,
            min_buffer_size: None,
            max_buffer_size: max_frames,
            process_mode: ProcessMode::Realtime,
        };

        let mut ctx = AuInitContext::<P> {
            sink: this.sink.clone(),
            _marker: PhantomData,
        };
        // SAFETY: AU forbids Initialize concurrent with Render.
        let plugin = unsafe { this.plugin_mut() };
        let ok = plugin.initialize(&selected_layout, &buffer_config, &mut ctx);
        if !ok {
            return au::kAudioUnitErr_FailedInitialization;
        }
        plugin.reset();

        // Initialize parameter smoothers with the current sample rate.
        // This mirrors what the VST3/CLAP wrappers do in their initialize paths.
        for entry in &this.params_by_id {
            unsafe { entry.ptr.update_smoother(sr as f32, true) };
        }

        // Provision the audio-thread render state so the hot path is
        // allocation-free. SAFETY: render is serialised vs. Initialize.
        let render_state = unsafe { this.render_state_mut() };
        render_state.provision(
            n_ch as usize,
            max_frames as usize,
            selected_layout.aux_input_ports,
        );
        render_state.midi.clear();
        this.midi_input.clear();
        this.set_tail_seconds(0.0);
        this.set_last_render_error(au::noErr);

        let latency = this.sink.latency_samples.load(Ordering::Relaxed);
        if latency > 0 && sr > 0.0 {
            let new_l = latency as f64 / sr;
            let prev_l = this.latency_seconds();
            this.set_latency_seconds(new_l);
            if (prev_l - new_l).abs() > f64::EPSILON {
                this.mark_pending(NOTIFY_LATENCY);
            }
        }

        this.initialized.store(true, Ordering::Release);
        this.drain_notifications();
        au::noErr
    }

    unsafe extern "C" fn uninitialize(self_ptr: *mut c_void) -> au::OSStatus {
        let this = unsafe { Self::from_ptr(self_ptr) };
        if this
            .initialized
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            // SAFETY: AU forbids Uninitialize concurrent with Render.
            unsafe { this.plugin_mut() }.deactivate();
        }
        this.midi_input.clear();
        unsafe { this.render_state_mut() }.midi.clear();
        au::noErr
    }

    unsafe extern "C" fn reset(
        self_ptr: *mut c_void,
        _scope: au::AudioUnitScope,
        _element: au::AudioUnitElement,
    ) -> au::OSStatus {
        let this = unsafe { Self::from_ptr(self_ptr) };
        // SAFETY: AU calls Reset on the main thread, serialised against render.
        unsafe { this.plugin_mut() }.reset();
        this.midi_input.clear();
        unsafe { this.render_state_mut() }.midi.clear();
        this.set_tail_seconds(0.0);
        au::noErr
    }

    unsafe extern "C" fn music_device_midi_event(
        self_ptr: *mut c_void,
        status: au::UInt32,
        data_1: au::UInt32,
        data_2: au::UInt32,
        offset: au::UInt32,
    ) -> au::OSStatus {
        unsafe { Self::from_ptr(self_ptr) }
            .midi_input
            .push_midi_event(status, data_1, data_2, offset)
    }

    unsafe extern "C" fn music_device_sys_ex(
        self_ptr: *mut c_void,
        data: *const u8,
        length: au::UInt32,
    ) -> au::OSStatus {
        unsafe { Self::from_ptr(self_ptr) }
            .midi_input
            .push_sysex(data, length)
    }

    unsafe extern "C" fn music_device_start_note(
        self_ptr: *mut c_void,
        _instrument: au::UInt32,
        group: au::UInt32,
        out_note_id: *mut au::UInt32,
        offset: au::UInt32,
        params: *const midi::MusicDeviceNoteParams,
    ) -> au::OSStatus {
        unsafe { Self::from_ptr(self_ptr) }.midi_input.start_note(
            group,
            out_note_id,
            offset,
            params,
        )
    }

    unsafe extern "C" fn music_device_stop_note(
        self_ptr: *mut c_void,
        group: au::UInt32,
        note_id: au::UInt32,
        offset: au::UInt32,
    ) -> au::OSStatus {
        unsafe { Self::from_ptr(self_ptr) }
            .midi_input
            .stop_note(group, note_id, offset)
    }

    fn get_class_info(&self) -> *mut c_void {
        let state = unsafe {
            state::serialize_object::<P>(
                self._params_arc.clone(),
                self.params_by_id.iter().map(|e| (&e.id_str, e.ptr)),
            )
        };
        let json = serde_json::to_string(&state).unwrap_or_default();
        let data = CFData::from_buffer(json.as_bytes());

        let dict = CFDictionary::from_CFType_pairs(&[
            (
                CFString::from_static_string("version").as_CFType(),
                CFNumber::from(0i32).as_CFType(),
            ),
            (
                CFString::from_static_string("type").as_CFType(),
                CFNumber::from(fourcc(P::AU_TYPE) as i32).as_CFType(),
            ),
            (
                CFString::from_static_string("subtype").as_CFType(),
                CFNumber::from(fourcc(P::AU_SUBTYPE) as i32).as_CFType(),
            ),
            (
                CFString::from_static_string("manufacturer").as_CFType(),
                CFNumber::from(fourcc(P::AU_MANUFACTURER) as i32).as_CFType(),
            ),
            (
                CFString::from_static_string("data").as_CFType(),
                data.as_CFType(),
            ),
        ]);

        let ptr = dict.as_concrete_TypeRef();
        std::mem::forget(dict);
        ptr as *mut c_void
    }

    fn set_class_info(&self, dict_ptr: *const c_void) -> au::OSStatus {
        if dict_ptr.is_null() {
            return au::kAudioUnitErr_InvalidPropertyValue;
        }

        // The host hands this over as an untyped pointer. Reinterpreting some other CoreFoundation
        // object as a `CFDictionary` would pass a wrong-typed object to the CF APIs below, which
        // crashes the host process instead of failing the property set.
        let value = unsafe { CFType::wrap_under_get_rule(dict_ptr as _) };
        if !value.instance_of::<CFDictionary<CFString, CFType>>() {
            return au::kAudioUnitErr_InvalidPropertyValue;
        }
        let dict = unsafe { CFDictionary::<CFString, CFType>::wrap_under_get_rule(dict_ptr as _) };

        let data_key = CFString::from_static_string("data");
        let data = match dict.find(&data_key) {
            // Same for the payload: the AU spec says this is a `CFData`, but nothing enforces it
            Some(d) if d.instance_of::<CFData>() => unsafe {
                CFData::wrap_under_get_rule(d.as_CFTypeRef() as _)
            },
            _ => return au::kAudioUnitErr_InvalidPropertyValue,
        };

        let mut state: PluginState = match serde_json::from_slice(data.bytes()) {
            Ok(s) => s,
            Err(_) => return au::kAudioUnitErr_InvalidPropertyValue,
        };

        let sr = self.sample_rate();
        let buffer_config = if sr > 0.0 {
            Some(BufferConfig {
                sample_rate: sr as f32,
                min_buffer_size: None,
                max_buffer_size: self.max_frames_per_slice(),
                process_mode: ProcessMode::Realtime,
            })
        } else {
            None
        };

        unsafe {
            state::deserialize_object::<P>(
                &mut state,
                self._params_arc.clone(),
                |id| {
                    self.params_by_id
                        .iter()
                        .find(|e| e.id_str == id)
                        .map(|e| e.ptr)
                },
                buffer_config.as_ref(),
            );
        }

        au::noErr
    }

    unsafe extern "C" fn get_property_info(
        self_ptr: *mut c_void,
        id: au::AudioUnitPropertyID,
        scope: au::AudioUnitScope,
        element: au::AudioUnitElement,
        out_data_size: *mut au::UInt32,
        out_writable: *mut au::Boolean,
    ) -> au::OSStatus {
        let this = unsafe { Self::from_ptr(self_ptr) };

        let respond = |size: au::UInt32, writable: bool| -> au::OSStatus {
            unsafe {
                if !out_data_size.is_null() {
                    *out_data_size = size;
                }
                if !out_writable.is_null() {
                    *out_writable = if writable { 1 } else { 0 };
                }
            }
            au::noErr
        };

        match id {
            au::kAudioUnitProperty_SampleRate
                if scope == au::kAudioUnitScope_Input || scope == au::kAudioUnitScope_Output =>
            {
                respond(std::mem::size_of::<au::Float64>() as u32, true)
            }
            au::kAudioUnitProperty_StreamFormat
                if bus_channel_count::<P>(this.n_channels(), scope, element).is_some() =>
            {
                respond(
                    std::mem::size_of::<au::AudioStreamBasicDescription>() as u32,
                    true,
                )
            }
            au::kAudioUnitProperty_ElementCount => {
                respond(std::mem::size_of::<au::UInt32>() as u32, false)
            }
            au::kAudioUnitProperty_Latency if scope == au::kAudioUnitScope_Global => {
                respond(std::mem::size_of::<au::Float64>() as u32, false)
            }
            au::kAudioUnitProperty_TailTime if scope == au::kAudioUnitScope_Global => {
                respond(std::mem::size_of::<au::Float64>() as u32, false)
            }
            au::kAudioUnitProperty_MaximumFramesPerSlice if scope == au::kAudioUnitScope_Global => {
                respond(std::mem::size_of::<au::UInt32>() as u32, true)
            }
            au::kAudioUnitProperty_ParameterList if scope == au::kAudioUnitScope_Global => {
                let n_params = this.params_by_id.len() as u32;
                respond(
                    n_params * std::mem::size_of::<au::AudioUnitParameterID>() as u32,
                    false,
                )
            }
            au::kAudioUnitProperty_ParameterInfo if scope == au::kAudioUnitScope_Global => respond(
                std::mem::size_of::<au::AudioUnitParameterInfo>() as u32,
                false,
            ),
            au::kAudioUnitProperty_SupportedNumChannels if scope == au::kAudioUnitScope_Global => {
                respond(
                    (P::AUDIO_IO_LAYOUTS.len() * std::mem::size_of::<au::AUChannelInfo>()) as u32,
                    false,
                )
            }
            au::kAudioUnitProperty_MakeConnection
                if scope == au::kAudioUnitScope_Input
                    && element == 0
                    && current_layout::<P>(this.n_channels())
                        .is_some_and(|layout| layout.main_input_channels.is_some()) =>
            {
                respond(std::mem::size_of::<AudioUnitConnection>() as u32, true)
            }
            au::kAudioUnitProperty_BypassEffect if scope == au::kAudioUnitScope_Global => {
                respond(std::mem::size_of::<au::UInt32>() as u32, true)
            }
            au::kAudioUnitProperty_LastRenderError if scope == au::kAudioUnitScope_Global => {
                respond(std::mem::size_of::<au::OSStatus>() as u32, false)
            }
            au::kAudioUnitProperty_SetRenderCallback
                if scope == au::kAudioUnitScope_Input
                    && bus_channel_count::<P>(this.n_channels(), scope, element).is_some() =>
            {
                respond(
                    std::mem::size_of::<au::AURenderCallbackStruct>() as u32,
                    true,
                )
            }
            au::kAudioUnitProperty_InPlaceProcessing => {
                respond(std::mem::size_of::<au::UInt32>() as u32, true)
            }
            au::kAudioUnitProperty_ClassInfo if scope == au::kAudioUnitScope_Global => {
                respond(std::mem::size_of::<*mut c_void>() as u32, true)
            }
            au::kAudioUnitProperty_HostCallbacks if scope == au::kAudioUnitScope_Global => {
                respond(std::mem::size_of::<au::HostCallbackInfo>() as u32, true)
            }
            au::kAudioUnitProperty_MIDIOutputCallbackInfo
                if scope == au::kAudioUnitScope_Global && P::MIDI_OUTPUT >= MidiConfig::Basic =>
            {
                respond(std::mem::size_of::<*mut c_void>() as u32, false)
            }
            au::kAudioUnitProperty_MIDIOutputCallback
                if scope == au::kAudioUnitScope_Global && P::MIDI_OUTPUT >= MidiConfig::Basic =>
            {
                respond(
                    std::mem::size_of::<midi::AuMidiOutputCallbackStruct>() as u32,
                    true,
                )
            }
            midi::MUSIC_DEVICE_PROPERTY_SUPPORTS_START_STOP_NOTE
                if scope == au::kAudioUnitScope_Global && P::MIDI_INPUT >= MidiConfig::Basic =>
            {
                respond(std::mem::size_of::<au::UInt32>() as u32, false)
            }
            au::kAudioUnitProperty_CocoaUI if scope == au::kAudioUnitScope_Global => {
                if this.editor.is_some() {
                    respond(std::mem::size_of::<au::AUCocoaViewInfo>() as u32, false)
                } else {
                    au::kAudioUnitErr_InvalidProperty
                }
            }
            // We don't expose AU-native factory presets (plugins manage their own
            // preset browser), so this is read-only: report a "custom, not a
            // factory preset" state rather than leaving it unimplemented. Hosts
            // that support a custom Cocoa UI (`kAudioUnitProperty_CocoaUI` above)
            // query this during validation to confirm the UI can reflect preset
            // state; leaving it unanswered fails that check (AUD-731).
            au::kAudioUnitProperty_PresentPreset if scope == au::kAudioUnitScope_Global => {
                respond(std::mem::size_of::<au::AUPreset>() as u32, false)
            }
            _ => au::kAudioUnitErr_InvalidProperty,
        }
    }

    unsafe extern "C" fn get_property(
        self_ptr: *mut c_void,
        id: au::AudioUnitPropertyID,
        scope: au::AudioUnitScope,
        element: au::AudioUnitElement,
        out_data: *mut c_void,
        io_data_size: *mut au::UInt32,
    ) -> au::OSStatus {
        let this = unsafe { Self::from_ptr(self_ptr) };
        // Main-thread entry: drain any audio-thread-marked notifications.
        this.drain_notifications();

        if out_data.is_null() || io_data_size.is_null() {
            return au::kAudioUnitErr_InvalidParameter;
        }

        macro_rules! require_output {
            ($ty:ty) => {
                if (unsafe { *io_data_size } as usize) < std::mem::size_of::<$ty>() {
                    return au::kAudioUnitErr_InvalidPropertyValue;
                }
            };
        }

        match id {
            au::kAudioUnitProperty_SampleRate => {
                require_output!(au::Float64);
                unsafe {
                    *(out_data as *mut au::Float64) = this.sample_rate();
                    *io_data_size = std::mem::size_of::<au::Float64>() as u32;
                }
                au::noErr
            }
            au::kAudioUnitProperty_ElementCount => {
                require_output!(au::UInt32);
                // Input scope: optional main input followed by aux inputs.
                // Output scope: always 1 (aux outputs not yet implemented).
                let n_ch = this.n_channels().max(1);
                let chans = NonZeroU32::new(n_ch);
                let layout = chans.and_then(layout_for_output::<P>);
                let count: au::UInt32 = match scope {
                    au::kAudioUnitScope_Input => layout
                        .map(|layout| {
                            u32::from(layout.main_input_channels.is_some())
                                + layout.aux_input_ports.len() as u32
                        })
                        .unwrap_or(0),
                    au::kAudioUnitScope_Output => 1,
                    au::kAudioUnitScope_Global => 1,
                    _ => 0,
                };
                unsafe {
                    *(out_data as *mut au::UInt32) = count;
                    *io_data_size = std::mem::size_of::<au::UInt32>() as u32;
                }
                au::noErr
            }
            au::kAudioUnitProperty_Latency if scope == au::kAudioUnitScope_Global => {
                require_output!(au::Float64);
                unsafe {
                    *(out_data as *mut au::Float64) = this.latency_seconds();
                    *io_data_size = std::mem::size_of::<au::Float64>() as u32;
                }
                au::noErr
            }
            au::kAudioUnitProperty_TailTime if scope == au::kAudioUnitScope_Global => {
                require_output!(au::Float64);
                unsafe {
                    *(out_data as *mut au::Float64) = this.tail_seconds();
                    *io_data_size = std::mem::size_of::<au::Float64>() as u32;
                }
                au::noErr
            }
            au::kAudioUnitProperty_MaximumFramesPerSlice => {
                require_output!(au::UInt32);
                unsafe {
                    *(out_data as *mut au::UInt32) = this.max_frames_per_slice();
                    *io_data_size = std::mem::size_of::<au::UInt32>() as u32;
                }
                au::noErr
            }
            au::kAudioUnitProperty_StreamFormat => {
                require_output!(au::AudioStreamBasicDescription);
                let channels = match bus_channel_count::<P>(this.n_channels(), scope, element) {
                    Some(channels) => channels,
                    None => return au::kAudioUnitErr_InvalidElement,
                };
                let asbd = au::AudioStreamBasicDescription {
                    mSampleRate: this.sample_rate(),
                    mFormatID: au::kAudioFormatLinearPCM,
                    mFormatFlags: au::kAudioFormatFlagIsFloat
                        | au::kAudioFormatFlagIsPacked
                        | au::kAudioFormatFlagIsNonInterleaved,
                    mBytesPerPacket: 4,
                    mFramesPerPacket: 1,
                    mBytesPerFrame: 4,
                    mChannelsPerFrame: channels,
                    mBitsPerChannel: 32,
                    mReserved: 0,
                };
                unsafe {
                    *(out_data as *mut au::AudioStreamBasicDescription) = asbd;
                    *io_data_size = std::mem::size_of::<au::AudioStreamBasicDescription>() as u32;
                }
                au::noErr
            }
            au::kAudioUnitProperty_ParameterList if scope == au::kAudioUnitScope_Global => {
                let n = this.params_by_id.len();
                let needed = (n * std::mem::size_of::<au::AudioUnitParameterID>()) as u32;
                if unsafe { *io_data_size } < needed {
                    return au::kAudioUnitErr_InvalidPropertyValue;
                }
                let dst = out_data as *mut au::AudioUnitParameterID;
                for i in 0..n {
                    unsafe {
                        *dst.add(i) = i as au::AudioUnitParameterID;
                    }
                }
                unsafe {
                    *io_data_size = needed;
                }
                au::noErr
            }
            au::kAudioUnitProperty_ParameterInfo if scope == au::kAudioUnitScope_Global => {
                let idx = element as usize;
                let entry = match this.params_by_id.get(idx) {
                    Some(e) => e,
                    None => return au::kAudioUnitErr_InvalidParameter,
                };
                if (unsafe { *io_data_size } as usize)
                    < std::mem::size_of::<au::AudioUnitParameterInfo>()
                {
                    return au::kAudioUnitErr_InvalidPropertyValue;
                }
                let info = build_parameter_info(entry);
                unsafe {
                    *(out_data as *mut au::AudioUnitParameterInfo) = info;
                    *io_data_size = std::mem::size_of::<au::AudioUnitParameterInfo>() as u32;
                }
                au::noErr
            }
            au::kAudioUnitProperty_SupportedNumChannels if scope == au::kAudioUnitScope_Global => {
                let needed = P::AUDIO_IO_LAYOUTS.len() * std::mem::size_of::<au::AUChannelInfo>();
                if (unsafe { *io_data_size } as usize) < needed {
                    return au::kAudioUnitErr_InvalidPropertyValue;
                }
                let dst = out_data as *mut au::AUChannelInfo;
                for (idx, layout) in P::AUDIO_IO_LAYOUTS.iter().enumerate() {
                    unsafe {
                        *dst.add(idx) = au::AUChannelInfo {
                            inChannels: layout
                                .main_input_channels
                                .map(|n| n.get() as i16)
                                .unwrap_or(0),
                            outChannels: layout
                                .main_output_channels
                                .map(|n| n.get() as i16)
                                .unwrap_or(0),
                        };
                    }
                }
                unsafe { *io_data_size = needed as u32 };
                au::noErr
            }
            au::kAudioUnitProperty_BypassEffect if scope == au::kAudioUnitScope_Global => {
                require_output!(au::UInt32);
                let on = if this.bypass.load(Ordering::Acquire) {
                    1
                } else {
                    0
                };
                unsafe {
                    *(out_data as *mut au::UInt32) = on;
                    *io_data_size = std::mem::size_of::<au::UInt32>() as u32;
                }
                au::noErr
            }
            au::kAudioUnitProperty_LastRenderError if scope == au::kAudioUnitScope_Global => {
                require_output!(au::OSStatus);
                unsafe {
                    *(out_data as *mut au::OSStatus) =
                        this.last_render_error.load(Ordering::Acquire);
                    *io_data_size = std::mem::size_of::<au::OSStatus>() as u32;
                }
                au::noErr
            }
            au::kAudioUnitProperty_InPlaceProcessing => {
                require_output!(au::UInt32);
                unsafe {
                    *(out_data as *mut au::UInt32) = 1;
                    *io_data_size = std::mem::size_of::<au::UInt32>() as u32;
                }
                au::noErr
            }
            au::kAudioUnitProperty_ClassInfo if scope == au::kAudioUnitScope_Global => {
                require_output!(*mut c_void);
                let dict = this.get_class_info();
                unsafe {
                    *(out_data as *mut *mut c_void) = dict;
                    *io_data_size = std::mem::size_of::<*mut c_void>() as u32;
                }
                au::noErr
            }
            au::kAudioUnitProperty_CocoaUI if scope == au::kAudioUnitScope_Global => {
                if this.editor.is_none() {
                    return au::kAudioUnitErr_InvalidProperty;
                }
                if (unsafe { *io_data_size } as usize) < std::mem::size_of::<au::AUCocoaViewInfo>()
                {
                    return au::kAudioUnitErr_InvalidPropertyValue;
                }
                // Register (or look up) the per-type ObjC view factory class and
                // return an AUCocoaViewInfo pointing at this dylib bundle + class name.
                match cocoaui::cocoaui_view_info::<P>(this) {
                    Some(info) => {
                        unsafe {
                            *(out_data as *mut au::AUCocoaViewInfo) = info;
                            *io_data_size = std::mem::size_of::<au::AUCocoaViewInfo>() as u32;
                        }
                        au::noErr
                    }
                    None => au::kAudioUnitErr_InvalidProperty,
                }
            }
            au::kAudioUnitProperty_PresentPreset if scope == au::kAudioUnitScope_Global => {
                if (unsafe { *io_data_size } as usize) < std::mem::size_of::<au::AUPreset>() {
                    return au::kAudioUnitErr_InvalidPropertyValue;
                }
                // -1 is the standard AU convention for "current state doesn't
                // correspond to a stored factory preset" (we don't implement
                // kAudioUnitProperty_FactoryPresets). The name is caller-owned
                // per AU's Create Rule, same as the CocoaUI/ClassInfo replies
                // above.
                let preset_name = unsafe { au::cf_string_create("") };
                if preset_name.is_null() {
                    return au::kAudioUnitErr_InvalidProperty;
                }
                unsafe {
                    *(out_data as *mut au::AUPreset) = au::AUPreset {
                        presetNumber: -1,
                        presetName: preset_name,
                    };
                    *io_data_size = std::mem::size_of::<au::AUPreset>() as u32;
                }
                au::noErr
            }
            au::kAudioUnitProperty_MIDIOutputCallbackInfo
                if scope == au::kAudioUnitScope_Global && P::MIDI_OUTPUT >= MidiConfig::Basic =>
            {
                require_output!(*mut c_void);
                let names: CFArray<CFString> =
                    CFArray::from_CFTypes(&[CFString::from_static_string("MIDI Out")]);
                let names_ref = names.as_concrete_TypeRef();
                std::mem::forget(names);
                unsafe {
                    *(out_data as *mut *mut c_void) = names_ref as *mut c_void;
                    *io_data_size = std::mem::size_of::<*mut c_void>() as u32;
                }
                au::noErr
            }
            midi::MUSIC_DEVICE_PROPERTY_SUPPORTS_START_STOP_NOTE
                if scope == au::kAudioUnitScope_Global && P::MIDI_INPUT >= MidiConfig::Basic =>
            {
                require_output!(au::UInt32);
                unsafe {
                    *(out_data as *mut au::UInt32) = 1;
                    *io_data_size = std::mem::size_of::<au::UInt32>() as u32;
                }
                au::noErr
            }
            _ => au::kAudioUnitErr_InvalidProperty,
        }
    }

    unsafe extern "C" fn set_property(
        self_ptr: *mut c_void,
        id: au::AudioUnitPropertyID,
        scope: au::AudioUnitScope,
        element: au::AudioUnitElement,
        in_data: *const c_void,
        in_data_size: au::UInt32,
    ) -> au::OSStatus {
        let this = unsafe { Self::from_ptr(self_ptr) };
        let status = Self::set_property_impl(this, id, scope, element, in_data, in_data_size);
        // Drain after the change so listeners see the new value.
        this.drain_notifications();
        status
    }

    fn set_property_impl(
        this: &Self,
        id: au::AudioUnitPropertyID,
        scope: au::AudioUnitScope,
        element: au::AudioUnitElement,
        in_data: *const c_void,
        in_data_size: au::UInt32,
    ) -> au::OSStatus {
        /// Validate a property payload before it is dereferenced. This is called across the C ABI,
        /// so a host can pass a null pointer alongside a nominally valid size.
        macro_rules! payload {
            ($ty:ty) => {
                if in_data.is_null() || (in_data_size as usize) < std::mem::size_of::<$ty>() {
                    return au::kAudioUnitErr_InvalidPropertyValue;
                }
            };
        }

        match id {
            au::kAudioUnitProperty_SampleRate => {
                payload!(au::Float64);
                if this.is_initialized() {
                    return au::kAudioUnitErr_Initialized;
                }
                let sr = unsafe { *(in_data as *const au::Float64) };
                if !(sr > 0.0 && sr.is_finite()) {
                    return au::kAudioUnitErr_InvalidPropertyValue;
                }
                this.set_sample_rate(sr);
                au::noErr
            }
            au::kAudioUnitProperty_StreamFormat => {
                payload!(au::AudioStreamBasicDescription);
                // AU spec: StreamFormat may not change while initialized.
                if this.is_initialized() {
                    return au::kAudioUnitErr_Initialized;
                }
                let asbd = unsafe { &*(in_data as *const au::AudioStreamBasicDescription) };

                // Reject malformed channel count.
                if asbd.mChannelsPerFrame == 0 {
                    return au::kAudioUnitErr_InvalidPropertyValue;
                }
                if !(asbd.mSampleRate > 0.0 && asbd.mSampleRate.is_finite()) {
                    return au::kAudioUnitErr_InvalidPropertyValue;
                }
                // Reject non-PCM / non-float / interleaved — we only ever
                // advertise non-interleaved 32-bit float in get_property.
                if asbd.mFormatID != au::kAudioFormatLinearPCM
                    || (asbd.mFormatFlags & au::kAudioFormatFlagIsFloat) == 0
                    || (asbd.mFormatFlags & au::kAudioFormatFlagIsNonInterleaved) == 0
                {
                    return au::kAudioUnitErr_FormatNotSupported;
                }
                if asbd.mBitsPerChannel != 32 {
                    return au::kAudioUnitErr_FormatNotSupported;
                }
                let req_ch = asbd.mChannelsPerFrame;
                match scope {
                    au::kAudioUnitScope_Output if element == 0 => {
                        let req = NonZeroU32::new(req_ch).expect("channel count checked above");
                        if layout_for_output::<P>(req).is_none() {
                            return au::kAudioUnitErr_FormatNotSupported;
                        }
                        this.n_channels.store(req_ch, Ordering::Release);
                    }
                    au::kAudioUnitScope_Input => {
                        let expected = match bus_channel_count::<P>(
                            this.n_channels(),
                            au::kAudioUnitScope_Input,
                            element,
                        ) {
                            Some(expected) => expected,
                            None => return au::kAudioUnitErr_InvalidElement,
                        };
                        if expected != req_ch {
                            return au::kAudioUnitErr_FormatNotSupported;
                        }
                    }
                    au::kAudioUnitScope_Output => return au::kAudioUnitErr_InvalidElement,
                    _ => return au::kAudioUnitErr_InvalidScope,
                }

                this.set_sample_rate(asbd.mSampleRate);
                this.mark_pending(NOTIFY_STREAM_FORMAT);
                au::noErr
            }
            au::kAudioUnitProperty_MaximumFramesPerSlice => {
                payload!(au::UInt32);
                if this.is_initialized() {
                    return au::kAudioUnitErr_Initialized;
                }
                let v = unsafe { *(in_data as *const au::UInt32) };
                if v == 0 {
                    return au::kAudioUnitErr_InvalidPropertyValue;
                }
                let prev = this.max_frames_per_slice.swap(v, Ordering::AcqRel);
                if prev != v {
                    this.mark_pending(NOTIFY_MAX_FRAMES_PER_SLICE);
                }
                au::noErr
            }
            au::kAudioUnitProperty_BypassEffect => {
                payload!(au::UInt32);
                let v = unsafe { *(in_data as *const au::UInt32) };
                let on = v != 0;
                let prev = this.bypass.swap(on, Ordering::AcqRel);
                if prev != on {
                    this.mark_pending(NOTIFY_BYPASS_EFFECT);
                }
                // Mirror into the plugin's BYPASS-flagged param if present,
                // so plugins can observe the toggle through their own params.
                if let Some(idx) = this.bypass_param_idx {
                    if let Some(entry) = this.params_by_id.get(idx) {
                        // Boolean params use 0.0 / 1.0 normalised. set_normalized_value
                        // is documented thread-safe.
                        let n = if on { 1.0 } else { 0.0 };
                        unsafe {
                            let _ = entry.ptr.set_normalized_value(n);
                        }
                    }
                }
                au::noErr
            }
            au::kAudioUnitProperty_MakeConnection if scope == au::kAudioUnitScope_Input => {
                payload!(AudioUnitConnection);
                // Only the main input is connectable; aux inputs keep using
                // callbacks. Saying so explicitly beats accepting a wiring we
                // would then never pull from.
                if element != 0 {
                    return au::kAudioUnitErr_InvalidElement;
                }
                let has_main_input = current_layout::<P>(this.n_channels())
                    .map(|layout| layout.main_input_channels.is_some())
                    .unwrap_or(false);
                if !has_main_input {
                    return au::kAudioUnitErr_InvalidElement;
                }
                let conn = unsafe { *(in_data as *const AudioUnitConnection) };
                // `conn.dest_input_number` is deliberately not consulted: AU's
                // convention is that the property's element *is* the
                // destination input, and that is what every caller sets. A
                // host disagreeing with itself would be a host bug, and
                // trusting the element keeps this consistent with how
                // SetRenderCallback is dispatched right below.
                //
                // A null source unit means "disconnect".
                let new_conn = if conn.source_audio_unit.is_null() {
                    None
                } else {
                    Some(conn)
                };
                if let Ok(mut guard) = this.input_connection.lock() {
                    *guard = new_conn;
                }
                // A connection replaces any callback the host installed
                // earlier — and a disconnect must not silently fall back to
                // one either.
                if let Ok(mut guard) = this.input_callback.lock() {
                    *guard = None;
                }
                au::noErr
            }
            au::kAudioUnitProperty_SetRenderCallback if scope == au::kAudioUnitScope_Input => {
                payload!(au::AURenderCallbackStruct);
                let cb = unsafe { *(in_data as *const au::AURenderCallbackStruct) };
                let new_cb = if cb.inputProc.is_some() {
                    Some(cb)
                } else {
                    None
                };
                let layout = match current_layout::<P>(this.n_channels()) {
                    Some(layout) => layout,
                    None => return au::kAudioUnitErr_FormatNotSupported,
                };
                let aux_base = u32::from(layout.main_input_channels.is_some());
                if aux_base == 1 && element == 0 {
                    if let Ok(mut guard) = this.input_callback.lock() {
                        *guard = new_cb;
                    }
                    // Symmetric with MakeConnection above: installing a
                    // callback supersedes a connected source.
                    if let Ok(mut guard) = this.input_connection.lock() {
                        *guard = None;
                    }
                } else {
                    if element < aux_base {
                        return au::kAudioUnitErr_InvalidElement;
                    }
                    let aux_idx = (element - aux_base) as usize;
                    if aux_idx >= layout.aux_input_ports.len() {
                        return au::kAudioUnitErr_InvalidElement;
                    }
                    if let Some(slot) = this.aux_input_callbacks.get(aux_idx) {
                        if let Ok(mut guard) = slot.lock() {
                            *guard = new_cb;
                        }
                    } else {
                        return au::kAudioUnitErr_InvalidElement;
                    }
                }
                au::noErr
            }
            au::kAudioUnitProperty_InPlaceProcessing => au::noErr,
            au::kAudioUnitProperty_ClassInfo if scope == au::kAudioUnitScope_Global => {
                payload!(*mut c_void);
                let dict = unsafe { *(in_data as *const *mut c_void) };
                this.set_class_info(dict)
            }
            au::kAudioUnitProperty_HostCallbacks if scope == au::kAudioUnitScope_Global => {
                payload!(au::HostCallbackInfo);
                let cb = unsafe { ptr::read(in_data as *const au::HostCallbackInfo) };
                if let Ok(mut guard) = this.host_callbacks.lock() {
                    *guard = Some(cb);
                }
                au::noErr
            }
            au::kAudioUnitProperty_MIDIOutputCallback
                if scope == au::kAudioUnitScope_Global && P::MIDI_OUTPUT >= MidiConfig::Basic =>
            {
                payload!(midi::AuMidiOutputCallbackStruct);
                let callback =
                    unsafe { ptr::read(in_data as *const midi::AuMidiOutputCallbackStruct) };
                if this
                    .midi_output_callback
                    .store(callback.callback.map(|_| callback))
                    .is_ok()
                {
                    au::noErr
                } else {
                    au::kAudioUnitErr_CannotDoInCurrentContext
                }
            }
            _ => au::kAudioUnitErr_InvalidProperty,
        }
    }

    /// Read the current parameter value, projected from the plugin's
    /// normalised `[0, 1]` representation back to the plain (display) value.
    unsafe extern "C" fn get_parameter(
        self_ptr: *mut c_void,
        id: au::AudioUnitParameterID,
        _scope: au::AudioUnitScope,
        _element: au::AudioUnitElement,
        out_value: *mut au::AudioUnitParameterValue,
    ) -> au::OSStatus {
        let this = unsafe { Self::from_ptr(self_ptr) };
        let entry = match this.params_by_id.get(id as usize) {
            Some(e) => e,
            None => return au::kAudioUnitErr_InvalidParameter,
        };
        if out_value.is_null() {
            return au::kAudioUnitErr_InvalidParameter;
        }
        let normalized = unsafe { entry.ptr.unmodulated_normalized_value() };
        let plain = unsafe { entry.ptr.preview_plain(normalized) };
        unsafe {
            *out_value = plain;
        }
        au::noErr
    }

    /// Set a parameter from the host. AU sends the plain value; convert back
    /// to the [0, 1] normalised range nih-plug expects.
    ///
    /// # Audio-thread safety
    ///
    /// AU may invoke this from the audio thread for sample-accurate
    /// automation. The call chain is:
    ///   `ParamPtr::set_normalized_value` →
    ///   `<concrete Param>::set_normalized_value` (Float/Int/Bool/Enum) →
    ///   `set_plain_value` → `AtomicF32::swap` + a few `AtomicF32::store`s.
    /// All atomic, no allocation, no locking — verified by code inspection
    /// of `src/params/{float,integer,boolean,enums}.rs`. The only escape
    /// hatch is the user-supplied `value_changed: Arc<dyn Fn(f32)>` callback,
    /// which the plugin author owns and must keep audio-safe; nih-plug's
    /// default is `None`.
    ///
    /// # `buffer_offset_in_frames`
    ///
    /// AU's sample-accurate automation passes a non-zero offset when the
    /// parameter change should land mid-buffer. nih-plug's `Plugin::process`
    /// API operates on a single contiguous buffer per call and has no
    /// per-sample parameter dispatch, so we cannot honour `offset > 0`
    /// precisely without splitting the buffer (which we don't do today).
    /// Current behaviour: apply the change immediately. The next render
    /// call will see it. Worst-case granularity error is `max_frames_per_slice`
    /// samples — typically ≤1024 frames at 48 kHz → ≤21 ms. Tracked as a
    /// known limitation.
    unsafe extern "C" fn set_parameter(
        self_ptr: *mut c_void,
        id: au::AudioUnitParameterID,
        _scope: au::AudioUnitScope,
        _element: au::AudioUnitElement,
        value: au::AudioUnitParameterValue,
        _buffer_offset_in_frames: au::UInt32,
    ) -> au::OSStatus {
        let this = unsafe { Self::from_ptr(self_ptr) };
        let entry = match this.params_by_id.get(id as usize) {
            Some(e) => e,
            None => return au::kAudioUnitErr_InvalidParameter,
        };
        let normalized = unsafe { entry.ptr.preview_normalized(value) };
        unsafe {
            let _ = entry.ptr.set_normalized_value(normalized);
        }
        // Mirror BYPASS-flagged param into our atomic if this update was
        // routed through the bypass parameter (some hosts expose bypass
        // both as a property and as a parameter).
        if let Some(bypass_idx) = this.bypass_param_idx {
            if id as usize == bypass_idx {
                let on = normalized >= 0.5;
                let prev = this.bypass.swap(on, Ordering::AcqRel);
                if prev != on {
                    this.mark_pending(NOTIFY_BYPASS_EFFECT);
                }
            }
        }
        // Update the smoother target so sample-accurate automation and GUI
        // interactions are reflected in the audio thread.
        let sr = this.sample_rate();
        if sr > 0.0 {
            unsafe { entry.ptr.update_smoother(sr as f32, false) };
        }
        // Notify all registered AU parameter listeners (e.g. GUI parameter
        // listeners, Logic's automation recording).
        let instance = this.instance.load(Ordering::Acquire) as usize as au::AudioUnit;
        let au_param = AUParameter {
            mAudioUnit: instance,
            mParameterID: id,
            mScope: au::kAudioUnitScope_Global,
            mElement: 0,
        };
        unsafe { AUParameterListenerNotify(std::ptr::null_mut(), std::ptr::null_mut(), &au_param) };
        au::noErr
    }

    /// Render hot path. Convert the host's `AudioBufferList` into the
    /// nih-plug `Buffer` shape and dispatch to `Plugin::process`.
    ///
    /// Concurrency: runs on the audio thread. The wrapper is borrowed as
    /// `&Self`; mutable access to the plugin and to the render scratch goes
    /// through `UnsafeCell` borrows that are unique by AU's host contract
    /// (no render re-entry, no concurrent Initialize/Uninitialize/Reset).
    ///
    /// Allocation: this function performs no heap allocations on the hot
    /// path. All scratch storage is provisioned in `Initialize`.
    unsafe extern "C" fn render(
        self_ptr: *mut c_void,
        _io_action_flags: *mut au::AudioUnitRenderActionFlags,
        in_time_stamp: *const au::AudioTimeStamp,
        _in_output_bus_number: au::UInt32,
        in_number_frames: au::UInt32,
        io_data: *mut au::AudioBufferList,
    ) -> au::OSStatus {
        let this = unsafe { Self::from_ptr(self_ptr) };

        if io_data.is_null() {
            this.set_last_render_error(au::kAudioUnitErr_InvalidParameter);
            return au::kAudioUnitErr_InvalidParameter;
        }

        if !this.is_initialized() {
            unsafe { zero_buffer_list(io_data, in_number_frames) };
            return au::noErr;
        }

        let n_frames = in_number_frames as usize;
        if in_number_frames > this.max_frames_per_slice() {
            unsafe { zero_buffer_list(io_data, in_number_frames) };
            this.set_last_render_error(au::kAudioUnitErr_TooManyFramesToProcess);
            return au::kAudioUnitErr_TooManyFramesToProcess;
        }

        let layout = match current_layout::<P>(this.n_channels()) {
            Some(layout) => layout,
            None => {
                unsafe { zero_buffer_list(io_data, in_number_frames) };
                this.set_last_render_error(au::kAudioUnitErr_FormatNotSupported);
                return au::kAudioUnitErr_FormatNotSupported;
            }
        };
        let has_main_input = layout.main_input_channels.is_some();
        let aux_element_base = u32::from(has_main_input);

        // Snapshot the input callback under the mutex (cheap struct copy).
        // Released immediately so main-thread updates don't block render
        // for long. Lock-free swap is tracked in AUD-337.
        // The two wirings are kept mutually exclusive when the host sets them,
        // so at most one of these is ever populated; the callback is preferred
        // if both somehow are.
        let input_source = has_main_input
            .then(|| {
                this.input_callback
                    .lock()
                    .ok()
                    .and_then(|g| *g)
                    .map(InputSource::Callback)
                    .or_else(|| {
                        this.input_connection
                            .lock()
                            .ok()
                            .and_then(|g| *g)
                            .map(InputSource::Connection)
                    })
            })
            .flatten();

        // SAFETY: render is not re-entered; we are the sole owner of
        // RenderState for the duration of this call.
        let rs = unsafe { this.render_state_mut() };

        // ── 1) Pull main input from whichever wiring the host installed. ──
        let mut pulled_input = false;
        if let Some(input_source) = input_source {
            let n_ch = rs.input_scratch.len();
            // Verify our pre-allocated BufferList scratch is big enough.
            // Should always hold since `provision` sized it for n_ch and
            // n_ch is fixed between Initialize calls.
            if n_ch > 0
                && rs.input_scratch[0].len() >= n_frames
                && rs.bl_storage.len() * mem::size_of::<u64>() >= bl_byte_size(n_ch)
            {
                let bl_ptr = rs.bl_storage.as_mut_ptr() as *mut au::AudioBufferList;
                unsafe {
                    (*bl_ptr).mNumberBuffers = n_ch as au::UInt32;
                }
                // The N-tuple of AudioBuffer entries lives at the natural
                // C `mBuffers` offset, which the compiler computes
                // accounting for any padding after `mNumberBuffers`.
                let header_offset = mem::offset_of!(au::AudioBufferList, mBuffers);
                let buffers_ptr = unsafe {
                    (rs.bl_storage.as_mut_ptr() as *mut u8).add(header_offset)
                        as *mut au::AudioBuffer
                };
                for ch in 0..n_ch {
                    let scratch_ptr = rs.input_scratch[ch].as_mut_ptr();
                    unsafe {
                        *buffers_ptr.add(ch) = au::AudioBuffer {
                            mNumberChannels: 1,
                            mDataByteSize: (n_frames * mem::size_of::<f32>()) as au::UInt32,
                            mData: scratch_ptr as *mut c_void,
                        };
                    }
                }

                let mut flags: au::AudioUnitRenderActionFlags = 0;
                let status = match input_source {
                    InputSource::Callback(cb) => {
                        // Forward this block's real timestamp, exactly as
                        // AUBase's PullInput does. The zeroed timestamp this
                        // used to pass (mFlags = 0, no valid fields) is a spec
                        // violation a host is entitled to answer with silence
                        // and noErr — Logic Pro does exactly that, which
                        // rendered every nih-plug AU silent there while
                        // Ableton, whose callback ignores the timestamp,
                        // worked fine (AUD-831).
                        let proc = cb.inputProc.unwrap();
                        unsafe {
                            proc(
                                cb.inputProcRefCon,
                                &mut flags,
                                in_time_stamp,
                                0,
                                in_number_frames,
                                bl_ptr,
                            )
                        }
                    }
                    // A connected source is a real AU, so it gets this block's
                    // actual timestamp — anything reading `mSampleTime`
                    // (generators, meters, tempo-synced sources) would
                    // misbehave on a zeroed one.
                    InputSource::Connection(conn) => unsafe {
                        AudioUnitRender(
                            conn.source_audio_unit,
                            &mut flags,
                            in_time_stamp,
                            conn.source_output_number,
                            in_number_frames,
                            bl_ptr,
                        )
                    },
                };
                if status != au::noErr {
                    unsafe { zero_buffer_list(io_data, in_number_frames) };
                    this.set_last_render_error(status);
                    return status;
                } else {
                    pulled_input = true;
                    // The callback is allowed to *replace* the mData pointers
                    // with its own buffers instead of filling the ones we
                    // provided — a zero-copy pull AUBase explicitly supports
                    // by reading input back through the BufferList after the
                    // call. Logic Pro does exactly this; reading our scratch
                    // directly therefore saw only the zeros we put there,
                    // which kept every nih-plug AU silent in Logic while
                    // hosts that copy into the caller's buffers (Ableton,
                    // the QC harnesses) worked (AUD-831). Copy back whenever
                    // the pointer moved so everything downstream can keep
                    // reading scratch.
                    for ch in 0..n_ch {
                        let b = unsafe { &*buffers_ptr.add(ch) };
                        let src = b.mData as *const f32;
                        let dst = rs.input_scratch[ch].as_mut_ptr();
                        if !src.is_null() && src != dst as *const f32 {
                            let frames =
                                n_frames.min(b.mDataByteSize as usize / mem::size_of::<f32>());
                            unsafe { ptr::copy_nonoverlapping(src, dst, frames) };
                        }
                    }
                }
            }
        }

        // ── 2) Wire host io_data slices into the persistent Buffer. ──────
        let bl = unsafe { &mut *io_data };
        let n_buffers = bl.mNumberBuffers as usize;
        let buffers_ptr = bl.mBuffers.as_mut_ptr();

        // If we pulled via callback, copy scratch → io_data so process()
        // sees the input audio.
        if pulled_input {
            for i in 0..n_buffers.min(rs.input_scratch.len()) {
                let buf = unsafe { &mut *buffers_ptr.add(i) };
                if buf.mData.is_null() || !buffer_fits_frames(buf, n_frames) {
                    continue;
                }
                let dst =
                    unsafe { std::slice::from_raw_parts_mut(buf.mData as *mut f32, n_frames) };
                let src = &rs.input_scratch[i][..n_frames];
                dst.copy_from_slice(src);
            }
        }

        // ── 2b) Null-mData outputs (INT-2566). Some hosts (notably Logic Pro)
        // hand us `io_data` buffers with `mData == NULL`, expecting the AU to
        // supply the backing storage itself. Point each such buffer at our
        // persistent per-channel input scratch — which already holds the
        // pulled input — so `process()` reads it and overwrites in place, and
        // the host reads the output back through `mData`. Without this the slot
        // wired below stays empty (`&mut []`) and the host receives silence.
        // Non-null (host-allocated) buffers are left untouched.
        for i in 0..n_buffers.min(rs.input_scratch.len()) {
            let buf = unsafe { &mut *buffers_ptr.add(i) };
            if buf.mData.is_null() {
                let scratch = &mut rs.input_scratch[i];
                if scratch.len() < n_frames {
                    // Host asked for more frames than `provision()` sized the
                    // scratch for (maxFramesPerSlice violation). Leave the
                    // buffer null — the slot below degrades to `&mut []`
                    // (silence) instead of an out-of-bounds host read.
                    continue;
                }
                if !pulled_input {
                    // No upstream input was pulled → present silence rather
                    // than stale scratch left over from a previous render.
                    scratch[..n_frames].fill(0.0);
                }
                buf.mData = scratch.as_mut_ptr() as *mut c_void;
                buf.mDataByteSize = (n_frames * mem::size_of::<f32>()) as au::UInt32;
                if buf.mNumberChannels == 0 {
                    buf.mNumberChannels = 1;
                }
            }
        }

        // Rewrite the persistent Buffer's slot vector. The slot count was
        // pre-grown in `provision()`, so this is purely in-place writes.
        // SAFETY: each slice points to host-owned memory that remains valid
        // for the duration of this render() call. The slices are cleared
        // back to `&mut []` at the end of this function so the `'static`
        // lifetime in the slot type can never outlive the host data.
        unsafe {
            rs.buffer.set_slices(n_frames, |slots| {
                let n = slots.len().min(n_buffers);
                for i in 0..n {
                    let buf = &mut *buffers_ptr.add(i);
                    // A host that reports fewer valid bytes than `in_number_frames` would make the
                    // slice below read and write past the end of its own buffer. Leave that
                    // channel's slot empty so the plugin does not touch it at all; whatever the
                    // host left in its own buffer is its own business.
                    if buf.mData.is_null() || !buffer_fits_frames(buf, n_frames) {
                        slots[i] = &mut [];
                        continue;
                    }
                    let raw = std::slice::from_raw_parts_mut(buf.mData as *mut f32, n_frames);
                    // The slot type carries `'static` because `RenderState`
                    // is itself field-stored; the slice we put here lives
                    // only until we clear it below. This mirrors the
                    // pattern in `BufferManager::create_buffers`.
                    slots[i] = mem::transmute::<&mut [f32], &'static mut [f32]>(raw);
                }
                // Defensively null any extra slots.
                for slot in &mut slots[n..] {
                    *slot = &mut [];
                }
            });
        }

        let sr = this.sample_rate();
        let mut transport = Transport::new(sr as f32);
        if let Ok(guard) = this.host_callbacks.lock() {
            if let Some(cb) = guard.as_ref() {
                if let Some(beat_and_tempo) = cb.beatAndTempoProc {
                    let mut beat = 0.0;
                    let mut tempo = 0.0;
                    if unsafe { beat_and_tempo(cb.hostUserData, &mut beat, &mut tempo) }
                        == au::noErr
                    {
                        transport.pos_beats = Some(beat);
                        transport.tempo = Some(tempo);
                    }
                }
                if let Some(musical_time) = cb.musicalTimeLocationProc {
                    let mut delta = 0u32;
                    let mut num = 0.0f32;
                    let mut den = 0u32;
                    let mut bar_start = 0.0f64;
                    if unsafe {
                        musical_time(
                            cb.hostUserData,
                            &mut delta,
                            &mut num,
                            &mut den,
                            &mut bar_start,
                        )
                    } == au::noErr
                    {
                        transport.time_sig_numerator = Some(num as i32);
                        transport.time_sig_denominator = Some(den as i32);
                        transport.bar_start_pos_beats = Some(bar_start);
                    }
                }
                if let Some(state_proc) = cb.transportStateProc {
                    let mut playing = 0u8;
                    let mut changed = 0u8;
                    let mut sample_pos = 0.0f64;
                    let mut cycling = 0u8;
                    let mut cycle_start = 0.0f64;
                    let mut cycle_end = 0.0f64;
                    if unsafe {
                        state_proc(
                            cb.hostUserData,
                            &mut playing,
                            &mut changed,
                            &mut sample_pos,
                            &mut cycling,
                            &mut cycle_start,
                            &mut cycle_end,
                        )
                    } == au::noErr
                    {
                        transport.playing = playing != 0;
                        transport.pos_samples = Some(sample_pos as i64);
                        if cycling != 0 {
                            transport.loop_range_samples =
                                Some((cycle_start as i64, cycle_end as i64));
                        }
                    }
                }
            }
        }

        // Fallback: if HostCallback didn't provide pos_samples, read it from
        // the timestamp that the host passes every render call. AU hosts
        // always set mSampleTime when kAudioTimeStampSampleTimeValid is set.
        if transport.pos_samples.is_none() && !in_time_stamp.is_null() {
            let ts = unsafe { &*in_time_stamp };
            if ts.mFlags & au::kAudioTimeStampSampleTimeValid != 0 {
                transport.pos_samples = Some(ts.mSampleTime as i64);
                // Without transportStateProc we can't know playing state, so
                // assume the engine is running if a valid timestamp is present.
                transport.playing = true;
            }
        }

        // ── 3) Pull aux inputs and wire them into AuxiliaryBuffers. ──────
        let n_aux = rs.aux_buffers.len();
        let mut upstream_error = au::noErr;
        for aux_idx in 0..n_aux {
            let cb_snapshot = this
                .aux_input_callbacks
                .get(aux_idx)
                .and_then(|m| m.lock().ok().and_then(|g| *g));

            let n_ch = rs.aux_input_scratch[aux_idx].len();
            let bl_storage_ok =
                rs.aux_bl_storages[aux_idx].len() * mem::size_of::<u64>() >= bl_byte_size(n_ch);

            if let Some(cb) = cb_snapshot {
                if n_ch > 0 && bl_storage_ok {
                    // Fill aux scratch via the host's callback for element (aux_idx + 1).
                    let bl_ptr =
                        rs.aux_bl_storages[aux_idx].as_mut_ptr() as *mut au::AudioBufferList;
                    unsafe {
                        (*bl_ptr).mNumberBuffers = n_ch as au::UInt32;
                    }
                    let header_offset = mem::offset_of!(au::AudioBufferList, mBuffers);
                    let buffers_ptr = unsafe {
                        (rs.aux_bl_storages[aux_idx].as_mut_ptr() as *mut u8).add(header_offset)
                            as *mut au::AudioBuffer
                    };
                    for ch in 0..n_ch {
                        let scratch_ptr = rs.aux_input_scratch[aux_idx][ch].as_mut_ptr();
                        unsafe {
                            *buffers_ptr.add(ch) = au::AudioBuffer {
                                mNumberChannels: 1,
                                mDataByteSize: (n_frames * mem::size_of::<f32>()) as au::UInt32,
                                mData: scratch_ptr as *mut c_void,
                            };
                        }
                    }
                    // Real timestamp, same as the main input: a zeroed one is
                    // a spec violation Logic answers with silence (AUD-831).
                    let mut flags: au::AudioUnitRenderActionFlags = 0;
                    // `SetRenderCallback` stores `None` when `inputProc` is
                    // absent, but keep the render boundary fail-closed if a
                    // malformed callback ever reaches this snapshot.
                    let Some(proc) = cb.inputProc else {
                        continue;
                    };
                    let status = unsafe {
                        proc(
                            cb.inputProcRefCon,
                            &mut flags,
                            in_time_stamp,
                            aux_element_base + aux_idx as au::UInt32,
                            in_number_frames,
                            bl_ptr,
                        )
                    };
                    // Same zero-copy contract as the main input: the callback
                    // may have swapped mData to its own buffers (AUD-831).
                    if status == au::noErr {
                        for ch in 0..n_ch {
                            let b = unsafe { &*buffers_ptr.add(ch) };
                            let src = b.mData as *const f32;
                            let dst = rs.aux_input_scratch[aux_idx][ch].as_mut_ptr();
                            if !src.is_null() && src != dst as *const f32 {
                                let frames =
                                    n_frames.min(b.mDataByteSize as usize / mem::size_of::<f32>());
                                unsafe { ptr::copy_nonoverlapping(src, dst, frames) };
                            }
                        }
                    } else if upstream_error == au::noErr {
                        upstream_error = status;
                    }
                    // Wire scratch into the aux Buffer's slots.
                    unsafe {
                        rs.aux_buffers[aux_idx].set_slices(n_frames, |slots| {
                            for (ch, slot) in slots.iter_mut().enumerate().take(n_ch) {
                                let raw = std::slice::from_raw_parts_mut(
                                    rs.aux_input_scratch[aux_idx][ch].as_mut_ptr(),
                                    n_frames,
                                );
                                *slot = mem::transmute::<&mut [f32], &'static mut [f32]>(raw);
                            }
                        });
                    }
                }
            } else {
                // No callback: zero-fill the aux buffer slots.
                unsafe {
                    rs.aux_buffers[aux_idx].set_slices(n_frames, |slots| {
                        for (ch, slot) in slots.iter_mut().enumerate().take(n_ch) {
                            rs.aux_input_scratch[aux_idx][ch][..n_frames].fill(0.0);
                            let raw = std::slice::from_raw_parts_mut(
                                rs.aux_input_scratch[aux_idx][ch].as_mut_ptr(),
                                n_frames,
                            );
                            *slot = mem::transmute::<&mut [f32], &'static mut [f32]>(raw);
                        }
                    });
                }
            }
        }

        // Snapshot exactly the MIDI events that were queued before this block
        // and order them by sample offset before handing them to the plugin.
        this.midi_input.drain_into(&mut rs.midi, in_number_frames);

        if upstream_error != au::noErr {
            unsafe {
                clear_render_slices(rs);
                zero_buffer_list(io_data, in_number_frames);
            }
            this.set_last_render_error(upstream_error);
            return upstream_error;
        }

        // Bypass: skip Plugin::process entirely. Input has already been
        // copied into io_data above (callback path) or sits there in-place
        // (host path), so the pass-through is implicit — we just don't run
        // the plugin's DSP.

        let process_status = if !this.bypass.load(Ordering::Acquire) {
            // SAFETY: aux_buffers is only accessed here (audio thread, no re-entry).
            // We cast to `&'static mut [Buffer<'static>]` to satisfy
            // AuxiliaryBuffers<'_> — the same lifetime-laundering pattern the
            // main buffer uses for its `&'static mut [f32]` slots. The slices
            // are cleared below before render() returns.
            let aux_inputs_ptr = rs.aux_buffers.as_mut_ptr();
            let aux_inputs_len = rs.aux_buffers.len();
            let mut aux = AuxiliaryBuffers {
                inputs: unsafe { std::slice::from_raw_parts_mut(aux_inputs_ptr, aux_inputs_len) },
                outputs: &mut [],
            };
            // SAFETY: render is not concurrent with Initialize/Uninitialize/Reset
            // and AU does not re-enter render, so this `&mut P` is unique.
            let midi_state = &mut rs.midi;
            let mut process_ctx = AuProcessContext::<P> {
                sink: this.sink.clone(),
                transport,
                input_events: &mut midi_state.input_events,
                output_events: &mut midi_state.output_events,
                _marker: PhantomData,
            };
            unsafe { this.plugin_mut() }.process(&mut rs.buffer, &mut aux, &mut process_ctx)
        } else {
            ProcessStatus::Normal
        };

        let render_status = match process_status {
            ProcessStatus::Error(err) => {
                nih_debug_assert_failure!("Process error: {}", err);
                au::kAudioUnitErr_CannotDoInCurrentContext
            }
            ProcessStatus::Normal => {
                this.set_tail_seconds(0.0);
                au::noErr
            }
            ProcessStatus::Tail(samples) => {
                this.set_tail_seconds(if sr > 0.0 { samples as f64 / sr } else { 0.0 });
                au::noErr
            }
            ProcessStatus::KeepAlive => {
                this.set_tail_seconds(f64::INFINITY);
                au::noErr
            }
        };

        let midi_output_callback = this.midi_output_callback.load();
        let midi_status = if render_status == au::noErr {
            unsafe {
                rs.midi
                    .flush_output(midi_output_callback, in_time_stamp, in_number_frames)
            }
        } else {
            rs.midi.output_events.clear();
            au::noErr
        };

        // Clear the slot slices so the `'static` lifetime can never escape
        // this render call via a stale `&mut [f32]`.
        unsafe { clear_render_slices(rs) };

        let final_status = if render_status != au::noErr {
            render_status
        } else {
            midi_status
        };
        if final_status != au::noErr {
            unsafe { zero_buffer_list(io_data, in_number_frames) };
            this.set_last_render_error(final_status);
            return final_status;
        }

        let latency = this.sink.latency_samples.load(Ordering::Relaxed);
        if latency > 0 && sr > 0.0 {
            let new_l = latency as f64 / sr;
            let prev_l = this.latency_seconds();
            this.set_latency_seconds(new_l);
            if (prev_l - new_l).abs() > f64::EPSILON {
                // We're on the audio thread; just mark pending. The next
                // main-thread property/lifecycle call will drain it.
                this.mark_pending(NOTIFY_LATENCY);
            }
        }

        this.set_last_render_error(au::noErr);
        au::noErr
    }

    unsafe extern "C" fn add_property_listener(
        self_ptr: *mut c_void,
        id: au::AudioUnitPropertyID,
        proc: au::AudioUnitPropertyListenerProc,
        user_data: *mut c_void,
    ) -> au::OSStatus {
        let this = unsafe { Self::from_ptr(self_ptr) };
        if let Ok(mut guard) = this.listeners.lock() {
            guard.push(Listener {
                property_id: id,
                proc,
                user_data,
            });
        }
        au::noErr
    }

    unsafe extern "C" fn remove_property_listener_with_user_data(
        self_ptr: *mut c_void,
        id: au::AudioUnitPropertyID,
        proc: au::AudioUnitPropertyListenerProc,
        user_data: *mut c_void,
    ) -> au::OSStatus {
        let this = unsafe { Self::from_ptr(self_ptr) };
        let proc_addr = proc as usize;
        if let Ok(mut guard) = this.listeners.lock() {
            guard.retain(|l| {
                !(l.property_id == id && (l.proc as usize) == proc_addr && l.user_data == user_data)
            });
        }
        au::noErr
    }
}

/// Build the `AudioUnitParameterInfo` blob the host queries for each
/// parameter. Populates the legacy 52-byte name buffer and the modern CFString
/// slot (AUD-339).
fn build_parameter_info(entry: &ParamEntry) -> au::AudioUnitParameterInfo {
    let mut info: au::AudioUnitParameterInfo = unsafe { std::mem::zeroed() };

    let name_str = unsafe { entry.ptr.name() };
    let max = info.name.len() - 1;
    let bytes = name_str.as_bytes();
    let n = bytes.len().min(max);
    for i in 0..n {
        info.name[i] = bytes[i] as std::os::raw::c_char;
    }

    let normalized_default = unsafe { entry.ptr.default_normalized_value() };
    let default_plain = unsafe { entry.ptr.preview_plain(normalized_default) };
    let min_plain = unsafe { entry.ptr.preview_plain(0.0) };
    let max_plain = unsafe { entry.ptr.preview_plain(1.0) };

    info.minValue = min_plain;
    info.maxValue = max_plain;
    info.defaultValue = default_plain;

    let unit = match unsafe { entry.ptr.step_count() } {
        Some(1) => au::kAudioUnitParameterUnit_Boolean,
        Some(_) => au::kAudioUnitParameterUnit_Indexed,
        None => {
            let unit_str = unsafe { entry.ptr.unit() };
            classify_unit(unit_str)
        }
    };
    info.unit = unit;
    info.unitName = if unit == au::kAudioUnitParameterUnit_Generic {
        let unit_str = unsafe { entry.ptr.unit() };
        if !unit_str.is_empty() {
            string_to_cfstring(unit_str)
        } else {
            ptr::null_mut()
        }
    } else {
        ptr::null_mut()
    };
    info.cfNameString = string_to_cfstring(name_str);
    info.clumpID = 0;

    info.flags = au::kAudioUnitParameterFlag_IsReadable
        | au::kAudioUnitParameterFlag_IsWritable
        | au::kAudioUnitParameterFlag_CanRamp;

    info
}

/// Create a `CFStringRef` whose retain count is +1 and transfer ownership to
/// the caller. Used for `AudioUnitParameterInfo::cfNameString` and `unitName`,
/// which Apple documents as "create" semantics — the host releases the string
/// after reading the parameter info. The `mem::forget` is therefore *not* a
/// leak; dropping the `CFString` here would over-release once the host calls
/// `CFRelease`.
fn string_to_cfstring(s: &str) -> au::CFStringRef {
    let cf_string = CFString::new(s);
    let ptr = cf_string.as_concrete_TypeRef();
    std::mem::forget(cf_string);
    ptr as au::CFStringRef
}

/// Find the declared layout for an AU main output channel count. This also
/// covers instruments/generators whose `main_input_channels` is `None`.
fn layout_for_output<P: AuPlugin>(channels: NonZeroU32) -> Option<&'static AudioIOLayout> {
    P::AUDIO_IO_LAYOUTS
        .iter()
        .find(|layout| layout.main_output_channels == Some(channels))
}

fn current_layout<P: AuPlugin>(output_channels: u32) -> Option<&'static AudioIOLayout> {
    NonZeroU32::new(output_channels).and_then(layout_for_output::<P>)
}

/// Return the channel count for a concrete AU bus. Input buses are laid out as
/// the optional main input followed by auxiliary inputs; output bus zero is the
/// main output (nih-plug does not expose auxiliary outputs through `Buffer`).
fn bus_channel_count<P: AuPlugin>(
    output_channels: u32,
    scope: au::AudioUnitScope,
    element: au::AudioUnitElement,
) -> Option<u32> {
    let layout = current_layout::<P>(output_channels)?;
    match scope {
        au::kAudioUnitScope_Output if element == 0 => {
            layout.main_output_channels.map(NonZeroU32::get)
        }
        au::kAudioUnitScope_Input => {
            let has_main = layout.main_input_channels.is_some();
            if has_main && element == 0 {
                return layout.main_input_channels.map(NonZeroU32::get);
            }
            let aux_base = u32::from(has_main);
            let aux_idx = element.checked_sub(aux_base)? as usize;
            layout
                .aux_input_ports
                .get(aux_idx)
                .copied()
                .map(NonZeroU32::get)
        }
        _ => None,
    }
}

fn classify_unit(unit: &str) -> au::AudioUnitParameterUnit {
    let lower = unit.to_ascii_lowercase();
    if lower.contains("db") || lower.contains("decibel") {
        au::kAudioUnitParameterUnit_Decibels
    } else if lower.contains("hz") || lower.contains("hertz") || lower == "khz" {
        au::kAudioUnitParameterUnit_Hertz
    } else if lower.contains('%') || lower.contains("percent") {
        au::kAudioUnitParameterUnit_Percent
    } else if lower.contains("ms") || lower.contains("sec") || lower.contains("second") {
        // AU has no kAudioUnitParameterUnit_Milliseconds; map "ms" to Seconds.
        // Hosts display the raw value, so plugins must expose values in seconds
        // when they want AU-native time display.
        au::kAudioUnitParameterUnit_Seconds
    } else {
        au::kAudioUnitParameterUnit_Generic
    }
}

pub use Wrapper as AuWrapper;

// ─── CocoaUI view-factory bridge ─────────────────────────────────────────────
//
// AU v2 hosts query `kAudioUnitProperty_CocoaUI` to discover how to open the
// plugin's GUI. The property returns an `AUCocoaViewInfo` containing:
//   1. A CFURLRef pointing at the bundle that contains the view factory class.
//   2. A CFStringRef naming the `NSObject<AUCocoaUIBase>` subclass.
//
// IMPORTANT: objc2's `define_class!` does NOT emit __OBJC_CLASS_PROTOCOLS
// metadata, so `conformsToProtocol:(AUCocoaUIBase)` returns NO in hosts.
// We use a real Objective-C .m shim (src/wrapper/au/cocoaui.m, compiled via
// build.rs + cc crate) which declares `@interface ... : NSObject <AUCocoaUIBase>`.
// This generates proper protocol conformance metadata at compile time.
//
// The shim calls back into Rust via C-ABI functions that look up a reusable
// editor-spawn template by the host's AudioUnit handle.
mod cocoaui {
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::sync::atomic::Ordering as AtomicOrdering;
    use std::sync::{Arc, Mutex, OnceLock};

    use au_sys as au;

    use crate::editor::ParentWindowHandle;
    use crate::plugin::au::AuPlugin;

    use super::{GuiHandleSlot, Wrapper};

    type SpawnFn = Box<dyn Fn(ParentWindowHandle) -> Box<dyn std::any::Any + Send> + Send + Sync>;

    // ── Debug logging (debug builds only) ─────────────────────────────────────

    #[cfg(debug_assertions)]
    fn au_log(msg: &str) {
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/nih_plug_au.log")
        {
            let _ = writeln!(f, "{}", msg);
        }
    }

    macro_rules! au_log {
        ($($arg:tt)*) => {
            #[cfg(debug_assertions)]
            au_log(&format!($($arg)*));
        };
    }

    // ── Globals defined in cocoaui.m ───────────────────────────────────────────
    extern "C" {
        fn nih_plug_au_release_container(container_ns_view: *mut c_void);
        fn nih_plug_au_cocoaui_close_audio_unit_view(audio_unit: *mut c_void);
    }

    // ── Per-AU editor spawn templates ─────────────────────────────────────────
    //
    // An AU host can cache the `AUCocoaUIBase` factory obtained during the
    // first open and invoke it again without asking `kAudioUnitProperty_CocoaUI`
    // a second time. Keep one reusable template per live AU instance rather
    // than consuming a one-shot closure on the first factory call.
    struct SpawnTemplate {
        /// Pointer to `Wrapper<P>::editor_handle` (a `Mutex<Option<…>>`).
        /// Type-erased so the C-ABI callbacks don't need to be generic.
        handle_slot: *const GuiHandleSlot,
        editor_width: u32,
        editor_height: u32,
        spawn_fn: SpawnFn,
    }
    // `handle_slot` points into a live Wrapper. The template is registered by
    // that Wrapper and removed in Wrapper::close before its allocation dies.
    unsafe impl Send for SpawnTemplate {}
    unsafe impl Sync for SpawnTemplate {}

    fn spawn_templates() -> &'static Mutex<HashMap<u64, Arc<SpawnTemplate>>> {
        static SPAWN_TEMPLATES: OnceLock<Mutex<HashMap<u64, Arc<SpawnTemplate>>>> = OnceLock::new();
        SPAWN_TEMPLATES.get_or_init(|| Mutex::new(HashMap::new()))
    }

    #[no_mangle]
    pub unsafe extern "C" fn nih_plug_au_cocoaui_editor_size_for_audio_unit(
        audio_unit: *mut c_void,
        out_width: *mut u32,
        out_height: *mut u32,
    ) -> bool {
        let key = audio_unit as usize as u64;
        let template = spawn_templates()
            .lock()
            .expect("AU CocoaUI pending-spawn mutex poisoned")
            .get(&key)
            .cloned();
        let Some(template) = template else {
            return false;
        };

        if !out_width.is_null() {
            unsafe { *out_width = template.editor_width };
        }
        if !out_height.is_null() {
            unsafe { *out_height = template.editor_height };
        }
        true
    }

    /// Spawn a new editor from the reusable template for this AudioUnit.
    #[no_mangle]
    pub unsafe extern "C" fn nih_plug_au_cocoaui_spawn_for_audio_unit(
        parent_ns_view: *mut c_void,
        audio_unit: *mut c_void,
    ) -> *mut c_void {
        if parent_ns_view.is_null() || audio_unit.is_null() {
            return std::ptr::null_mut();
        }
        let key = audio_unit as usize as u64;
        let template = spawn_templates()
            .lock()
            .expect("AU CocoaUI pending-spawn mutex poisoned")
            .get(&key)
            .cloned();
        let Some(template) = template else {
            return std::ptr::null_mut();
        };

        let parent = ParentWindowHandle::AppKitNsView(parent_ns_view);
        let handle = (template.spawn_fn)(parent);
        let handle_slot = template.handle_slot;
        // SAFETY: see SpawnTemplate's Send/Sync contract above.
        unsafe { (*handle_slot).set(handle) };
        handle_slot as *mut c_void
    }

    pub fn close_audio_unit_view(audio_unit: *mut c_void) {
        if audio_unit.is_null() {
            return;
        }
        let key = audio_unit as usize as u64;
        spawn_templates()
            .lock()
            .expect("AU CocoaUI pending-spawn mutex poisoned")
            .remove(&key);
        unsafe { nih_plug_au_cocoaui_close_audio_unit_view(audio_unit) };
    }

    // ── C-ABI callbacks called by the ObjC shim ────────────────────────────────

    /// Called from NihPlugAuContainerView's -dealloc when the host closes the GUI.
    #[no_mangle]
    pub unsafe extern "C" fn nih_plug_au_cocoaui_close_view(
        container_ns_view: *mut c_void,
        handle_slot: *const c_void,
    ) {
        au_log!(
            "[nih-plug AU] cocoaui_close_view: container={:?}",
            container_ns_view
        );
        if !handle_slot.is_null() {
            unsafe { (*(handle_slot as *const GuiHandleSlot)).clear() };
        }
        unsafe { nih_plug_au_release_container(container_ns_view) };
    }

    // ── Public entry point ─────────────────────────────────────────────────────

    const VIEW_CLASS_NAME: &str = env!("NIH_PLUG_AU_VIEW_CLASS");

    /// Build an `AUCocoaViewInfo` for `this` wrapper.
    pub fn cocoaui_view_info<P: AuPlugin>(this: &Wrapper<P>) -> Option<au::AUCocoaViewInfo> {
        au_log!("[nih-plug AU] cocoaui_view_info: called");
        let editor = this.editor.as_ref()?;
        let gui_ctx_inner = this.gui_context_inner.as_ref()?;

        // Store editor size in the pending packet (Ableton passes preferredSize={0,0}).
        let (ew, eh) = editor.lock().unwrap().size();
        au_log!("[nih-plug AU] cocoaui_view_info: editor_size={}x{}", ew, eh);

        let instance = this.instance.load(AtomicOrdering::Acquire);
        if instance == 0 {
            // A factory call without `Open` cannot be associated with an AU
            // instance safely. Returning no CocoaUI lets the host retry after
            // normal component setup instead of routing the editor to another
            // instance's pending request.
            au_log!("[nih-plug AU] cocoaui_view_info: no opened AudioUnit instance");
            return None;
        }

        let editor_clone = editor.clone();
        let inner_clone = gui_ctx_inner.clone();
        let template = Arc::new(SpawnTemplate {
            handle_slot: &this.editor_handle as *const GuiHandleSlot,
            editor_width: ew,
            editor_height: eh,
            spawn_fn: Box::new(move |parent_handle| {
                let gui_ctx: Arc<dyn crate::context::gui::GuiContext> =
                    Arc::new(crate::wrapper::au::context::AuGuiContext::<P> {
                        inner: inner_clone.clone(),
                        _marker: std::marker::PhantomData,
                    });
                editor_clone.lock().unwrap().spawn(parent_handle, gui_ctx)
            }),
        });
        let key = instance as usize as u64;
        spawn_templates()
            .lock()
            .expect("AU CocoaUI pending-spawn mutex poisoned")
            .insert(key, template);
        au_log!(
            "[nih-plug AU] cocoaui_view_info: spawn template stored for AU={:#x}",
            key
        );

        let bundle_url_ref = match unsafe { bundle_cf_url() } {
            Ok(r) => r,
            Err(()) => {
                spawn_templates()
                    .lock()
                    .expect("AU CocoaUI pending-spawn mutex poisoned")
                    .remove(&key);
                au_log!("[nih-plug AU] cocoaui_view_info: bundle_cf_url failed");
                return None;
            }
        };

        let class_name_ref = unsafe { au::cf_string_create(VIEW_CLASS_NAME) };
        if class_name_ref.is_null() {
            unsafe {
                au::cf_release(bundle_url_ref as *mut c_void);
            }
            spawn_templates()
                .lock()
                .expect("AU CocoaUI pending-spawn mutex poisoned")
                .remove(&key);
            return None;
        }

        au_log!(
            "[nih-plug AU] cocoaui_view_info: class={} bundle={:?}",
            VIEW_CLASS_NAME,
            bundle_url_ref
        );
        Some(au::AUCocoaViewInfo {
            mCocoaAUViewBundleLocation: bundle_url_ref,
            mCocoaAUViewClass: [class_name_ref],
        })
    }

    /// Returns a `CFURLRef` (+1 retain) for the bundle containing this dylib.
    ///
    /// Uses `dladdr` to resolve the path of a symbol inside this compilation
    /// unit, then builds a `file://` CFURL pointing at the `.component`
    /// bundle root (three directories up from `Contents/MacOS/<dylib>`).
    unsafe fn bundle_cf_url() -> Result<au::CFStringRef, ()> {
        #[repr(C)]
        struct DlInfo {
            dli_fname: *const std::os::raw::c_char,
            dli_fbase: *mut c_void,
            dli_sname: *const std::os::raw::c_char,
            dli_saddr: *mut c_void,
        }

        extern "C" {
            fn dladdr(addr: *const c_void, info: *mut DlInfo) -> std::os::raw::c_int;
            fn CFURLCreateFromFileSystemRepresentation(
                allocator: *const c_void,
                buffer: *const u8,
                bufLen: i64,
                isDirectory: u8,
            ) -> *const c_void;
        }

        let anchor: *const c_void = bundle_cf_url as *const c_void;
        let mut info: DlInfo = unsafe { std::mem::zeroed() };
        if unsafe { dladdr(anchor, &mut info) } == 0 || info.dli_fname.is_null() {
            return Err(());
        }

        let dylib_path = unsafe { std::ffi::CStr::from_ptr(info.dli_fname) }
            .to_str()
            .map_err(|_| ())?;

        // Foo.component/Contents/MacOS/Foo → Foo.component/
        let bundle_path = std::path::Path::new(dylib_path)
            .parent()
            .ok_or(())? // MacOS/
            .parent()
            .ok_or(())? // Contents/
            .parent()
            .ok_or(())? // Foo.component/
            .to_str()
            .ok_or(())?;

        let path_bytes = bundle_path.as_bytes();
        let cf_url = unsafe {
            CFURLCreateFromFileSystemRepresentation(
                std::ptr::null(),
                path_bytes.as_ptr(),
                path_bytes.len() as i64,
                1, // isDirectory
            )
        };
        if cf_url.is_null() {
            Err(())
        } else {
            Ok(cf_url as au::CFStringRef)
        }
    }
}

unsafe fn zero_buffer_list(bl: *mut au::AudioBufferList, n_frames: au::UInt32) {
    if bl.is_null() {
        return;
    }
    let bl = unsafe { &mut *bl };
    let n = bl.mNumberBuffers as usize;
    let buffers_ptr = bl.mBuffers.as_mut_ptr();
    for i in 0..n {
        let buf = unsafe { &mut *buffers_ptr.add(i) };
        if buf.mData.is_null() {
            continue;
        }
        let n_samples = n_frames as usize * buf.mNumberChannels as usize;
        // The host's frame count and its buffer size have to agree, or this writes past the end of
        // `mData`. See `buffer_fits_frames()` for why a zero size is not treated as a mismatch.
        if !buffer_fits_frames(buf, n_samples) {
            continue;
        }
        unsafe { ptr::write_bytes(buf.mData as *mut f32, 0, n_samples) };
    }
}

/// Whether `buf` reports enough bytes to hold `n_samples` `f32`s.
///
/// `mDataByteSize` is the host's declaration of how much of `mData` is valid, and the render path
/// derives its slice lengths from `inNumberFrames` instead. A host that disagrees with itself would
/// otherwise cause out of bounds accesses across the C ABI. A reported size of zero is treated as
/// "unspecified" rather than as a mismatch, because hosts commonly leave it unset on output buffers.
#[inline]
fn buffer_fits_frames(buf: &au::AudioBuffer, n_samples: usize) -> bool {
    let reported_bytes = buf.mDataByteSize as usize;

    // Dividing instead of multiplying keeps a host supplied frame count times channel count from
    // overflowing `usize` before the comparison
    reported_bytes == 0 || reported_bytes / mem::size_of::<f32>() >= n_samples
}

/// Clear every host-backed slice before leaving `render()`. `RenderState`
/// stores these with a widened lifetime, so no early-return path may retain a
/// pointer after the host's AudioBufferList goes out of scope.
unsafe fn clear_render_slices<P: AuPlugin>(state: &mut RenderState<P>) {
    unsafe {
        state.buffer.set_slices(0, |slots| {
            for slot in slots.iter_mut() {
                *slot = &mut [];
            }
        });
        for aux_buffer in state.aux_buffers.iter_mut() {
            aux_buffer.set_slices(0, |slots| {
                for slot in slots.iter_mut() {
                    *slot = &mut [];
                }
            });
        }
    }
}

#[cfg(test)]
mod buffer_list_tests {
    use super::*;

    const FRAMES: usize = 8;

    fn buffer(byte_size: usize, data: *mut f32) -> au::AudioBuffer {
        au::AudioBuffer {
            mNumberChannels: 1,
            mDataByteSize: byte_size as au::UInt32,
            mData: data as *mut c_void,
        }
    }

    /// A host that reports fewer valid bytes than the render call's frame count would otherwise be
    /// written past the end of its own buffer.
    #[test]
    fn zero_buffer_list_respects_reported_sizes() {
        let mut storage = vec![0u64; bl_byte_size(3).div_ceil(mem::size_of::<u64>())];
        let bl_ptr = storage.as_mut_ptr() as *mut au::AudioBufferList;

        let mut well_formed = vec![1.0f32; FRAMES];
        let mut undersized = vec![1.0f32; FRAMES];

        unsafe {
            (*bl_ptr).mNumberBuffers = 3;

            let header_offset = mem::offset_of!(au::AudioBufferList, mBuffers);
            let buffers_ptr = (bl_ptr as *mut u8).add(header_offset) as *mut au::AudioBuffer;

            *buffers_ptr.add(0) = buffer(FRAMES * mem::size_of::<f32>(), well_formed.as_mut_ptr());
            // Only claims a single sample's worth of storage
            *buffers_ptr.add(1) = buffer(mem::size_of::<f32>(), undersized.as_mut_ptr());
            // A null payload must be skipped entirely
            *buffers_ptr.add(2) = buffer(FRAMES * mem::size_of::<f32>(), ptr::null_mut());

            zero_buffer_list(bl_ptr, FRAMES as au::UInt32);
        }

        assert_eq!(well_formed, vec![0.0; FRAMES]);
        assert_eq!(undersized, vec![1.0; FRAMES]);
    }

    #[test]
    fn zero_buffer_list_ignores_null_lists() {
        unsafe { zero_buffer_list(ptr::null_mut(), FRAMES as au::UInt32) };
    }

    #[test]
    fn buffer_fits_frames_treats_zero_as_unspecified() {
        // Hosts commonly leave `mDataByteSize` unset on output buffers
        assert!(buffer_fits_frames(&buffer(0, ptr::null_mut()), 512));

        let one_sample = buffer(mem::size_of::<f32>(), ptr::null_mut());
        assert!(buffer_fits_frames(&one_sample, 1));
        assert!(!buffer_fits_frames(&one_sample, 2));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::prelude::*;

    #[derive(Default)]
    struct InstrumentParams;

    unsafe impl Params for InstrumentParams {
        fn param_map(&self) -> Vec<(String, ParamPtr, String)> {
            Vec::new()
        }
    }

    struct TestInstrument {
        params: Arc<InstrumentParams>,
    }

    impl Default for TestInstrument {
        fn default() -> Self {
            Self {
                params: Arc::new(InstrumentParams),
            }
        }
    }

    impl Plugin for TestInstrument {
        const NAME: &'static str = "AU Instrument Test";
        const VENDOR: &'static str = "NIH-plug";
        const URL: &'static str = "https://github.com/robbert-vdh/nih-plug";
        const EMAIL: &'static str = "test@example.com";
        const VERSION: &'static str = "0.0.0";
        const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[
            AudioIOLayout {
                main_input_channels: None,
                main_output_channels: NonZeroU32::new(2),
                ..AudioIOLayout::const_default()
            },
            AudioIOLayout {
                main_input_channels: None,
                main_output_channels: NonZeroU32::new(1),
                ..AudioIOLayout::const_default()
            },
        ];
        const MIDI_INPUT: MidiConfig = MidiConfig::Basic;

        type SysExMessage = ();
        type BackgroundTask = ();

        fn params(&self) -> Arc<dyn Params> {
            self.params.clone()
        }

        fn process(
            &mut self,
            _buffer: &mut Buffer,
            _aux: &mut AuxiliaryBuffers,
            _context: &mut impl ProcessContext<Self>,
        ) -> ProcessStatus {
            ProcessStatus::KeepAlive
        }
    }

    impl AuPlugin for TestInstrument {
        const AU_TYPE: [u8; 4] = *b"aumu";
        const AU_SUBTYPE: [u8; 4] = *b"TstI";
        const AU_MANUFACTURER: [u8; 4] = *b"Test";
    }

    #[test]
    fn instrument_has_no_audio_input_bus_and_exposes_music_device_selectors() {
        let wrapper = Wrapper::<TestInstrument>::new() as *mut c_void;
        let mut count = u32::MAX;
        let mut size = std::mem::size_of::<u32>() as u32;

        assert_eq!(
            unsafe {
                Wrapper::<TestInstrument>::get_property(
                    wrapper,
                    au::kAudioUnitProperty_ElementCount,
                    au::kAudioUnitScope_Input,
                    0,
                    &mut count as *mut u32 as *mut c_void,
                    &mut size,
                )
            },
            au::noErr
        );
        assert_eq!(count, 0);
        assert_eq!(
            bus_channel_count::<TestInstrument>(2, au::kAudioUnitScope_Output, 0),
            Some(2)
        );
        assert_eq!(
            bus_channel_count::<TestInstrument>(2, au::kAudioUnitScope_Input, 0),
            None
        );
        assert!(
            unsafe { Wrapper::<TestInstrument>::lookup(midi::MUSIC_DEVICE_MIDI_EVENT_SELECT) }
                .is_some()
        );
        assert!(
            unsafe { Wrapper::<TestInstrument>::lookup(midi::MUSIC_DEVICE_START_NOTE_SELECT) }
                .is_some()
        );

        assert_eq!(
            unsafe { Wrapper::<TestInstrument>::close(wrapper) },
            au::noErr
        );
    }

    #[test]
    fn fixed_size_properties_reject_undersized_output_buffers() {
        let wrapper = Wrapper::<TestInstrument>::new() as *mut c_void;
        let properties = [
            (
                au::kAudioUnitProperty_ElementCount,
                au::kAudioUnitScope_Input,
            ),
            (au::kAudioUnitProperty_Latency, au::kAudioUnitScope_Global),
            (au::kAudioUnitProperty_TailTime, au::kAudioUnitScope_Global),
            (
                au::kAudioUnitProperty_MaximumFramesPerSlice,
                au::kAudioUnitScope_Global,
            ),
            (
                au::kAudioUnitProperty_BypassEffect,
                au::kAudioUnitScope_Global,
            ),
            (
                au::kAudioUnitProperty_LastRenderError,
                au::kAudioUnitScope_Global,
            ),
            (
                au::kAudioUnitProperty_InPlaceProcessing,
                au::kAudioUnitScope_Global,
            ),
        ];
        let mut output = 0u64;

        for (property, scope) in properties {
            let mut size = 0;
            assert_eq!(
                unsafe {
                    Wrapper::<TestInstrument>::get_property(
                        wrapper,
                        property,
                        scope,
                        0,
                        &mut output as *mut u64 as *mut c_void,
                        &mut size,
                    )
                },
                au::kAudioUnitErr_InvalidPropertyValue,
                "property {property} accepted an undersized output buffer"
            );
        }

        assert_eq!(
            unsafe { Wrapper::<TestInstrument>::close(wrapper) },
            au::noErr
        );
    }

    #[test]
    fn classify_unit_decibels() {
        assert_eq!(classify_unit("dB"), au::kAudioUnitParameterUnit_Decibels);
        assert_eq!(
            classify_unit("decibel"),
            au::kAudioUnitParameterUnit_Decibels
        );
        assert_eq!(classify_unit("dBFS"), au::kAudioUnitParameterUnit_Decibels);
    }

    #[test]
    fn classify_unit_hertz() {
        assert_eq!(classify_unit("Hz"), au::kAudioUnitParameterUnit_Hertz);
        assert_eq!(classify_unit("kHz"), au::kAudioUnitParameterUnit_Hertz);
        assert_eq!(classify_unit("hertz"), au::kAudioUnitParameterUnit_Hertz);
    }

    #[test]
    fn classify_unit_percent() {
        assert_eq!(classify_unit("%"), au::kAudioUnitParameterUnit_Percent);
        assert_eq!(
            classify_unit("percent"),
            au::kAudioUnitParameterUnit_Percent
        );
    }

    #[test]
    fn classify_unit_seconds() {
        assert_eq!(classify_unit("ms"), au::kAudioUnitParameterUnit_Seconds);
        assert_eq!(classify_unit("sec"), au::kAudioUnitParameterUnit_Seconds);
        assert_eq!(
            classify_unit("seconds"),
            au::kAudioUnitParameterUnit_Seconds
        );
    }

    #[test]
    fn classify_unit_generic_fallback() {
        assert_eq!(classify_unit(""), au::kAudioUnitParameterUnit_Generic);
        assert_eq!(classify_unit("ratio"), au::kAudioUnitParameterUnit_Generic);
        assert_eq!(
            classify_unit("semitones"),
            au::kAudioUnitParameterUnit_Generic
        );
    }
}
