//! Minimal `InitContext` / `ProcessContext` / `GuiContext` impls for the AU wrapper.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use au_sys as au;

use crate::context::gui::GuiContext;
use crate::context::init::InitContext;
use crate::context::process::{ProcessContext, Transport};
use crate::context::PluginApi;
use crate::params::internals::ParamPtr;
use crate::plugin::Plugin;
use crate::prelude::PluginNoteEvent;
use crate::wrapper::state::PluginState;

/// Cell shared between the wrapper and its `InitContext` / `ProcessContext`
/// so that calls like `set_latency_samples()` can stash a value the wrapper
/// reads after `initialize()` / `process()` returns.
pub(super) struct ContextSink {
    pub latency_samples: AtomicU32,
}

impl ContextSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            latency_samples: AtomicU32::new(0),
        })
    }
}

pub(super) struct AuInitContext<P: Plugin> {
    pub sink: Arc<ContextSink>,
    pub _marker: std::marker::PhantomData<P>,
}

impl<P: Plugin> InitContext<P> for AuInitContext<P> {
    fn plugin_api(&self) -> PluginApi {
        PluginApi::Au
    }

    fn execute(&self, _task: P::BackgroundTask) {
        // No background executor yet.
    }

    fn set_latency_samples(&self, samples: u32) {
        self.sink.latency_samples.store(samples, Ordering::Relaxed);
    }

    fn set_current_voice_capacity(&self, _capacity: u32) {
        // CLAP-only.
    }
}

pub(super) struct AuProcessContext<'a, P: Plugin> {
    pub sink: Arc<ContextSink>,
    pub transport: Transport,
    pub input_events: &'a mut VecDeque<PluginNoteEvent<P>>,
    pub output_events: &'a mut VecDeque<PluginNoteEvent<P>>,
    pub _marker: std::marker::PhantomData<P>,
}

impl<P: Plugin> ProcessContext<P> for AuProcessContext<'_, P> {
    fn plugin_api(&self) -> PluginApi {
        PluginApi::Au
    }

    fn execute_background(&self, _task: P::BackgroundTask) {}

    fn execute_gui(&self, _task: P::BackgroundTask) {}

    fn transport(&self) -> &Transport {
        &self.transport
    }

    fn next_event(&mut self) -> Option<PluginNoteEvent<P>> {
        self.input_events.pop_front()
    }

    fn send_event(&mut self, event: PluginNoteEvent<P>) {
        if self.output_events.len() < self.output_events.capacity() {
            self.output_events.push_back(event);
        } else {
            nih_debug_assert_failure!("The AU MIDI output queue is full, dropping event");
        }
    }

    fn set_latency_samples(&self, samples: u32) {
        self.sink.latency_samples.store(samples, Ordering::Relaxed);
    }

    fn set_current_voice_capacity(&self, _capacity: u32) {}
}

// ─── GuiContext ───────────────────────────────────────────────────────────────

/// Payload shared between `Wrapper` and `AuGuiContext` via `Arc`.
pub(super) struct AuGuiContextInner {
    /// Host's `AudioUnit` opaque handle, stored as raw bits so we can update
    /// it from `open()` without requiring `&mut`. Set once in `open()`.
    pub instance_bits: std::sync::atomic::AtomicU64,
    /// `params_arc` from the plugin — needed for get/set_state.
    pub params_arc: Arc<dyn crate::params::Params>,
    /// (ParamPtr → AU param ID) lookup. Index in `params_by_id` == AU param ID.
    pub params_by_ptr: Vec<(ParamPtr, au::AudioUnitParameterID)>,
    /// The wrapper's sample-rate cell (f64 bits), shared rather than copied so
    /// a GUI write always smooths at the rate the host most recently set.
    pub sample_rate_bits: Arc<AtomicU64>,
}

unsafe impl Send for AuGuiContextInner {}
unsafe impl Sync for AuGuiContextInner {}

pub(super) struct AuGuiContext<P: Plugin> {
    pub inner: Arc<AuGuiContextInner>,
    pub _marker: std::marker::PhantomData<P>,
}

// SAFETY: AuGuiContext holds no P data; only Arc<AuGuiContextInner> which is Send+Sync.
unsafe impl<P: Plugin> Send for AuGuiContext<P> {}
unsafe impl<P: Plugin> Sync for AuGuiContext<P> {}

impl<P: Plugin> GuiContext for AuGuiContext<P> {
    fn plugin_api(&self) -> PluginApi {
        PluginApi::Au
    }

    fn request_resize(&self) -> bool {
        // AUv2 has no host-resize API.
        false
    }

    unsafe fn raw_begin_set_parameter(&self, _param: ParamPtr) {
        // AUv2 has no begin-gesture notification.
    }

    unsafe fn raw_set_parameter_normalized(&self, param: ParamPtr, normalized: f32) {
        // Write the value directly (same path as AU SetParameter from the host).
        unsafe { param.set_normalized_value(normalized) };

        // Retarget the smoother, exactly as `Wrapper::set_parameter` does for
        // host-driven writes. `set_normalized_value` only stores the value; it
        // deliberately leaves the smoother alone. Without this the audio thread
        // keeps reading whatever `.smoothed` held since `initialize`, so every
        // parameter a plugin reads through `.smoothed` ignores its own GUI —
        // knobs appear dead while toggles (read via `.value()`) still work.
        let sr = super::wrapper::unpack_f64(
            self.inner.sample_rate_bits.load(Ordering::Acquire),
        ) as f32;
        if sr > 0.0 {
            unsafe { param.update_smoother(sr, false) };
        }

        // Notify host listeners via AUParameterListenerNotify so automation
        // records the GUI-driven change.
        if let Some(&param_id) = self
            .inner
            .params_by_ptr
            .iter()
            .find(|(p, _)| *p == param)
            .map(|(_, id)| id)
        {
            let instance = self
                .inner
                .instance_bits
                .load(std::sync::atomic::Ordering::Acquire) as usize
                as au::AudioUnit;
            let au_param = AUParameter {
                mAudioUnit: instance,
                mParameterID: param_id,
                mScope: au::kAudioUnitScope_Global,
                mElement: 0,
            };
            // SAFETY: AUParameterListenerNotify is safe from any thread.
            unsafe {
                AUParameterListenerNotify(std::ptr::null_mut(), std::ptr::null_mut(), &au_param)
            };
        }
    }

    unsafe fn raw_end_set_parameter(&self, _param: ParamPtr) {
        // AUv2 has no end-gesture notification.
    }

    fn get_state(&self) -> PluginState {
        let params_arc = self.inner.params_arc.clone();
        let param_map = params_arc.param_map();
        unsafe {
            crate::wrapper::state::serialize_object::<P>(
                params_arc.clone(),
                param_map.iter().map(|(id_str, ptr, _group)| (id_str, *ptr)),
            )
        }
    }

    fn set_state(&self, mut state: PluginState) {
        let params_arc = self.inner.params_arc.clone();
        let param_map = params_arc.param_map();
        let getter = move |id: &str| {
            param_map
                .iter()
                .find(|(id_str, _, _)| id_str.as_str() == id)
                .map(|(_, ptr, _)| *ptr)
        };
        unsafe {
            crate::wrapper::state::deserialize_object::<P>(&mut state, params_arc, getter, None);
        }
    }
}

/// `AudioUnitParameter` — used by `AUParameterListenerNotify`.
#[repr(C)]
#[allow(non_snake_case)]
pub(super) struct AUParameter {
    pub mAudioUnit: au::AudioUnit,
    pub mParameterID: au::AudioUnitParameterID,
    pub mScope: au::AudioUnitScope,
    pub mElement: au::AudioUnitElement,
}

#[link(name = "AudioToolbox", kind = "framework")]
extern "C" {
    /// Notifies all listeners registered for the given parameter.
    /// Declared here because `au-sys` does not expose this AudioToolbox API.
    pub(super) fn AUParameterListenerNotify(
        inSendingListener: *mut std::ffi::c_void,
        inSendingObject: *mut std::ffi::c_void,
        inParameter: *const AUParameter,
    );
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    use super::{AuGuiContext, AuGuiContextInner};
    use crate::context::gui::GuiContext;
    use crate::params::internals::ParamPtr;
    use crate::params::ParamMut;
    use crate::prelude::*;

    #[derive(Default)]
    struct EmptyParams;

    unsafe impl Params for EmptyParams {
        fn param_map(&self) -> Vec<(String, ParamPtr, String)> {
            Vec::new()
        }
    }

    struct TestPlugin {
        params: Arc<EmptyParams>,
    }

    impl Default for TestPlugin {
        fn default() -> Self {
            Self {
                params: Arc::new(EmptyParams),
            }
        }
    }

    impl Plugin for TestPlugin {
        const NAME: &'static str = "AU GUI Smoother Test";
        const VENDOR: &'static str = "NIH-plug";
        const URL: &'static str = "https://github.com/robbert-vdh/nih-plug";
        const EMAIL: &'static str = "test@example.com";
        const VERSION: &'static str = "0.0.0";
        const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
            main_input_channels: NonZeroU32::new(2),
            main_output_channels: NonZeroU32::new(2),
            ..AudioIOLayout::const_default()
        }];

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
            ProcessStatus::Normal
        }
    }

    /// A GUI-driven write must retarget the smoother, not just store the value.
    ///
    /// The audio thread reads `.smoothed`; if only the plain value moves, every
    /// knob in the plugin's own editor is inert while the DSP keeps running on
    /// whatever `initialize` left behind. `params_by_ptr` is left empty so the
    /// host-notify branch is skipped and this stays a pure unit test.
    #[test]
    fn gui_parameter_write_retargets_the_smoother() {
        const SAMPLE_RATE: f32 = 48_000.0;

        let param = FloatParam::new("gain", 1.0, FloatRange::Linear { min: 0.0, max: 1.0 })
            .with_smoother(SmoothingStyle::Linear(1.0));
        param.update_smoother(SAMPLE_RATE, true);
        assert_eq!(param.smoothed.next(), 1.0, "smoother starts at the default");

        let inner = Arc::new(AuGuiContextInner {
            instance_bits: AtomicU64::new(0),
            params_arc: Arc::new(EmptyParams),
            params_by_ptr: Vec::new(),
            sample_rate_bits: Arc::new(AtomicU64::new(
                super::super::wrapper::pack_f64(SAMPLE_RATE as f64),
            )),
        });
        let ctx = AuGuiContext::<TestPlugin> {
            inner,
            _marker: std::marker::PhantomData,
        };

        unsafe { ctx.raw_set_parameter_normalized(ParamPtr::FloatParam(&param as *const _ as *mut _), 0.0) };

        assert_eq!(param.value(), 0.0, "plain value follows the GUI write");

        // Linear(1.0) over 48 kHz = 48 steps; drain more than that and the
        // smoother must have arrived. Without the retarget it never moves.
        for _ in 0..256 {
            param.smoothed.next();
        }
        assert_eq!(
            param.smoothed.next(),
            0.0,
            "smoother must converge on the GUI-written value"
        );
    }

    /// A zero/unset sample rate must not poison the smoother.
    #[test]
    fn gui_parameter_write_tolerates_unset_sample_rate() {
        let param = FloatParam::new("gain", 1.0, FloatRange::Linear { min: 0.0, max: 1.0 })
            .with_smoother(SmoothingStyle::Linear(1.0));

        let inner = Arc::new(AuGuiContextInner {
            instance_bits: AtomicU64::new(0),
            params_arc: Arc::new(EmptyParams),
            params_by_ptr: Vec::new(),
            sample_rate_bits: Arc::new(AtomicU64::new(super::super::wrapper::pack_f64(0.0))),
        });
        let ctx = AuGuiContext::<TestPlugin> {
            inner,
            _marker: std::marker::PhantomData,
        };

        unsafe { ctx.raw_set_parameter_normalized(ParamPtr::FloatParam(&param as *const _ as *mut _), 0.25) };

        assert_eq!(param.value(), 0.25, "plain value still lands");
        assert!(
            ctx.inner.sample_rate_bits.load(Ordering::Acquire) == super::super::wrapper::pack_f64(0.0),
            "sample rate cell untouched"
        );
    }
}
