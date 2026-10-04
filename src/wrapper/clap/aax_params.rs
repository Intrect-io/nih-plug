//! Intrect AAX-only parameter delivery, separate from standard CLAP `params.flush`.
//!
//! A host must serialize all incoming parameter batches against this extension. While it owns
//! this gate, concurrent `process` calls may contain audio/transport but no parameter/modulation
//! input events. Lifecycle/state callbacks must not overlap this delivery. Output parameter
//! values/gestures use the same gate in `process`, which skips the drain instead of waiting.
//! No audio or MIDI event buffers are accessed by the extension.

use clap_sys::events::{clap_input_events, clap_output_events};
use clap_sys::plugin::clap_plugin;
use std::ffi::CStr;
use std::sync::atomic::{AtomicBool, Ordering};

pub const AAX_PARAMETER_DELIVERY_ID: &CStr =
    unsafe { CStr::from_bytes_with_nul_unchecked(b"io.intrect.aax-parameter-delivery/1\0") };

/// Private extension ABI. All callbacks use the CLAP C calling convention.
///
/// # Safety
/// `try_begin` is called only on the host's serialized background parameter service. If it
/// returns a nonzero token, that caller must call `end` exactly once with that token on the same thread, including on a
/// failed `flush`. A zero token indicates failed acquisition. Stale tokens and calls from another thread are rejected. Input events must remain
/// stable throughout `flush`, and output callbacks must accept only parameter values/gestures.
/// `flush` returns 0 for invalid/unowned input (retain the input), 1 for consumed input with
/// parameter output still pending (discard the input and retry output), or 2 for complete delivery.
/// Rejected output stays queued ahead of later GUI events; accepted events are not replayed.
#[repr(C)]
pub struct AaxParameterDelivery {
    pub try_begin: unsafe extern "C" fn(*const clap_plugin) -> u64,
    pub flush: unsafe extern "C" fn(
        *const clap_plugin,
        u64,
        *const clap_input_events,
        *const clap_output_events,
    ) -> u32,
    pub end: unsafe extern "C" fn(*const clap_plugin, u64) -> bool,
}

pub(super) struct ParameterDeliveryGuard<'a>(&'a AtomicBool);

impl<'a> ParameterDeliveryGuard<'a> {
    pub(super) fn try_acquire(busy: &'a AtomicBool) -> Option<Self> {
        busy.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| Self(busy))
    }
}

impl Drop for ParameterDeliveryGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
