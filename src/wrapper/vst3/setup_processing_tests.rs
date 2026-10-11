use crate::prelude::*;
use crate::wrapper::vst3::{vst3_sys, Wrapper};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use vst3_sys::base::kResultOk;
use vst3_sys::vst::{
    AudioBusBuffers, IAudioProcessor, IComponent, ProcessData, ProcessModes, ProcessSetup,
    SymbolicSampleSizes,
};

static SEEN_MODE: AtomicUsize = AtomicUsize::new(usize::MAX);
static SEEN_BLOCK: AtomicU32 = AtomicU32::new(0);
static SEEN_PROCESS_MODE: AtomicUsize = AtomicUsize::new(usize::MAX);
static TEST_LOCK: Mutex<()> = Mutex::new(());

#[derive(Default)]
struct ProbeParams {}

// No parameter pointers are exposed by this observer.
unsafe impl Params for ProbeParams {
    fn param_map(&self) -> Vec<(String, crate::params::internals::ParamPtr, String)> {
        Vec::new()
    }
}

#[derive(Default)]
struct ModeProbe {
    params: Arc<ProbeParams>,
}

fn code(mode: ProcessMode) -> usize {
    match mode {
        ProcessMode::Realtime => 0,
        ProcessMode::Buffered => 1,
        ProcessMode::Offline => 2,
    }
}

impl Plugin for ModeProbe {
    const NAME: &'static str = "Setup mode observer";
    const VENDOR: &'static str = "Intrect QA";
    const URL: &'static str = "";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = "0.0.0";
    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: std::num::NonZeroU32::new(2),
        main_output_channels: std::num::NonZeroU32::new(2),
        aux_input_ports: &[],
        aux_output_ports: &[],
        names: PortNames::const_default(),
    }];
    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn initialize(
        &mut self,
        _: &AudioIOLayout,
        config: &BufferConfig,
        _: &mut impl InitContext<Self>,
    ) -> bool {
        SEEN_MODE.store(code(config.process_mode), Ordering::SeqCst);
        SEEN_BLOCK.store(config.max_buffer_size, Ordering::SeqCst);
        true
    }

    fn process(
        &mut self,
        _: &mut Buffer,
        _: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        SEEN_PROCESS_MODE.store(
            context.host_render_mode().map(code).unwrap_or(usize::MAX),
            Ordering::SeqCst,
        );
        ProcessStatus::Normal
    }
}

impl Vst3Plugin for ModeProbe {
    const VST3_CLASS_ID: [u8; 16] = *b"ModeSetupProbe01";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[Vst3SubCategory::Fx];
}

#[test]
fn activation_receives_current_setup_mode_for_fresh_and_reused_wrappers() {
    let _guard = TEST_LOCK.lock().unwrap();
    let modes = [
        ProcessModes::kRealtime as i32,
        ProcessModes::kPrefetch as i32,
        ProcessModes::kOffline as i32,
    ];
    let mut mismatches = Vec::new();
    for seed in 0..3 {
        for target in 0..3 {
            let wrapper = Wrapper::<ModeProbe>::new();
            for (step, requested) in [seed, target].into_iter().enumerate() {
                let block = 128 + step as u32 * 384;
                let setup = ProcessSetup {
                    process_mode: modes[requested],
                    symbolic_sample_size: SymbolicSampleSizes::kSample32 as i32,
                    max_samples_per_block: block as i32,
                    sample_rate: 48000.0,
                };
                SEEN_MODE.store(usize::MAX, Ordering::SeqCst);
                unsafe {
                    assert_eq!(
                        IAudioProcessor::setup_processing(&*wrapper, &setup),
                        kResultOk
                    );
                    assert_eq!(IComponent::set_active(&*wrapper, 1), kResultOk);
                }
                let actual = SEEN_MODE.load(Ordering::SeqCst);
                assert_eq!(SEEN_BLOCK.load(Ordering::SeqCst), block);
                println!(
                    "seed={seed} target={target} step={step} requested={requested} actual={actual}"
                );
                if actual != requested {
                    mismatches.push((seed, target, step, requested, actual));
                }
                unsafe {
                    assert_eq!(IComponent::set_active(&*wrapper, 0), kResultOk);
                }
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "current setup mode was not delivered: {mismatches:?}"
    );
}

#[test]
fn process_reports_realtime_prefetch_switches_without_new_setup() {
    let _guard = TEST_LOCK.lock().unwrap();
    let wrapper = Wrapper::<ModeProbe>::new();
    let setup = ProcessSetup {
        process_mode: ProcessModes::kRealtime as i32,
        symbolic_sample_size: SymbolicSampleSizes::kSample32 as i32,
        max_samples_per_block: 8,
        sample_rate: 48000.0,
    };
    unsafe {
        assert_eq!(
            IAudioProcessor::setup_processing(&*wrapper, &setup),
            kResultOk
        );
        assert_eq!(IComponent::set_active(&*wrapper, 1), kResultOk);
        assert_eq!(IAudioProcessor::set_processing(&*wrapper, 1), kResultOk);
    }
    let mut input = [[0.25_f32; 8]; 2];
    let mut output = [[0.0_f32; 8]; 2];
    let mut input_ptrs = [input[0].as_mut_ptr().cast(), input[1].as_mut_ptr().cast()];
    let mut output_ptrs = [output[0].as_mut_ptr().cast(), output[1].as_mut_ptr().cast()];
    let mut input_bus = AudioBusBuffers {
        num_channels: 2,
        silence_flags: 0,
        buffers: input_ptrs.as_mut_ptr(),
    };
    let mut output_bus = AudioBusBuffers {
        num_channels: 2,
        silence_flags: 0,
        buffers: output_ptrs.as_mut_ptr(),
    };
    // ProcessData's interface/context fields are nullable SDK pointers.
    let mut data: ProcessData = unsafe { std::mem::zeroed() };
    data.symbolic_sample_size = SymbolicSampleSizes::kSample32 as i32;
    data.num_samples = 8;
    data.num_inputs = 1;
    data.num_outputs = 1;
    data.inputs = &mut input_bus;
    data.outputs = &mut output_bus;
    let mut observed = Vec::new();
    for mode in [
        ProcessModes::kRealtime,
        ProcessModes::kPrefetch,
        ProcessModes::kRealtime,
    ] {
        data.process_mode = mode as i32;
        SEEN_PROCESS_MODE.store(usize::MAX, Ordering::SeqCst);
        unsafe {
            assert_eq!(IAudioProcessor::process(&*wrapper, &mut data), kResultOk);
        }
        observed.push(SEEN_PROCESS_MODE.load(Ordering::SeqCst));
    }
    unsafe {
        assert_eq!(IAudioProcessor::set_processing(&*wrapper, 0), kResultOk);
        assert_eq!(IComponent::set_active(&*wrapper, 0), kResultOk);
    }
    assert_eq!(
        observed,
        vec![0, 1, 0],
        "callback's mode must not remain the setup mode"
    );
}
