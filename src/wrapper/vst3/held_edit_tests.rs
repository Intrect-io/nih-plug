//! Editor edits made while the host is processing audio, driven through the wrapper's VST3
//! interfaces with a mock component handler and mock parameter change queues.

use super::*;
use crate::prelude::*;
use std::sync::Mutex;
use vst3_sys::utils::StaticVstPtr;
use vst3_sys::vst::{IComponentHandler, ProcessSetup, SymbolicSampleSizes};

const GAIN_ID: &str = "gain";
const ENABLED_ID: &str = "enabled";

struct TestParams {
    gain: FloatParam,
    enabled: BoolParam,
}

impl Default for TestParams {
    fn default() -> Self {
        Self {
            gain: FloatParam::new("Gain", 0.25, FloatRange::Linear { min: 0.0, max: 1.0 }),
            enabled: BoolParam::new("Enabled", false),
        }
    }
}

// The pointers point to fields of this object, which the wrapper keeps alive.
unsafe impl Params for TestParams {
    fn param_map(&self) -> Vec<(String, ParamPtr, String)> {
        vec![
            (GAIN_ID.to_owned(), self.gain.as_ptr(), String::new()),
            (ENABLED_ID.to_owned(), self.enabled.as_ptr(), String::new()),
        ]
    }
}

#[derive(Default)]
struct TestPlugin {
    params: Arc<TestParams>,
}

impl Plugin for TestPlugin {
    const NAME: &'static str = "Held edit test";
    const VENDOR: &'static str = "Intrect QA";
    const URL: &'static str = "";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = "0.0.0";
    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: NonZeroU32::new(2),
        main_output_channels: NonZeroU32::new(2),
        aux_input_ports: &[],
        aux_output_ports: &[],
        names: PortNames::const_default(),
    }];
    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn process(
        &mut self,
        _: &mut Buffer,
        _: &mut AuxiliaryBuffers,
        _: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        ProcessStatus::Normal
    }
}

impl Vst3Plugin for TestPlugin {
    const VST3_CLASS_ID: [u8; 16] = *b"HeldEditTest0001";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[Vst3SubCategory::Fx];
}

/// Records the edits the plugin sends to the host.
#[VST3(implements(IComponentHandler))]
struct MockComponentHandler {
    performed: Mutex<Vec<(u32, f64)>>,
    ended: Mutex<Vec<u32>>,
}

impl IComponentHandler for MockComponentHandler {
    unsafe fn begin_edit(&self, _id: u32) -> tresult {
        kResultOk
    }

    unsafe fn perform_edit(&self, id: u32, value_normalized: f64) -> tresult {
        self.performed.lock().unwrap().push((id, value_normalized));
        kResultOk
    }

    unsafe fn end_edit(&self, id: u32) -> tresult {
        self.ended.lock().unwrap().push(id);
        kResultOk
    }

    unsafe fn restart_component(&self, _flags: i32) -> tresult {
        kResultOk
    }
}

/// One parameter's points, all at sample offset 0, in the order the host sends them.
#[VST3(implements(IParamValueQueue))]
struct MockParamValueQueue {
    id: u32,
    values: Vec<f64>,
}

impl IParamValueQueue for MockParamValueQueue {
    unsafe fn get_parameter_id(&self) -> u32 {
        self.id
    }

    unsafe fn get_point_count(&self) -> i32 {
        self.values.len() as i32
    }

    unsafe fn get_point(&self, index: i32, sample_offset: *mut i32, value: *mut f64) -> tresult {
        match self.values.get(index as usize) {
            Some(point) => {
                *sample_offset = 0;
                *value = *point;
                kResultOk
            }
            None => kInvalidArgument,
        }
    }

    unsafe fn add_point(&self, _sample_offset: i32, _value: f64, _index: *mut i32) -> tresult {
        kResultFalse
    }
}

#[VST3(implements(IParameterChanges))]
struct MockParameterChanges {
    queues: Vec<&'static MockParamValueQueue>,
}

impl IParameterChanges for MockParameterChanges {
    unsafe fn get_parameter_count(&self) -> i32 {
        self.queues.len() as i32
    }

    unsafe fn get_parameter_data(&self, index: i32) -> StaticVstPtr<dyn IParamValueQueue> {
        match self.queues.get(index as usize) {
            // `StaticVstPtr` is a transparent pointer to the object's vtable pointer, see view.rs
            Some(queue) => mem::transmute(&queue.__iparamvaluequeuevptr as *const *const _),
            None => mem::zeroed(),
        }
    }

    unsafe fn add_parameter_data(
        &self,
        _id: *const u32,
        _index: *mut i32,
    ) -> StaticVstPtr<dyn IParamValueQueue> {
        mem::zeroed()
    }
}

/// A wrapper that is active and processing, with a component handler set, and the handler.
///
/// The mock COM objects are leaked so they outlive any reference the wrapper holds to them.
fn processing_wrapper() -> (Box<Wrapper<TestPlugin>>, &'static MockComponentHandler) {
    let wrapper = Wrapper::<TestPlugin>::new();
    let handler: &'static MockComponentHandler = Box::leak(MockComponentHandler::allocate(
        Mutex::new(Vec::new()),
        Mutex::new(Vec::new()),
    ));
    let setup = ProcessSetup {
        process_mode: ProcessModes::kRealtime as i32,
        symbolic_sample_size: SymbolicSampleSizes::kSample32 as i32,
        max_samples_per_block: 64,
        sample_rate: 48000.0,
    };
    unsafe {
        let handler_ptr: SharedVstPtr<dyn IComponentHandler> =
            mem::transmute(&handler.__icomponenthandlervptr as *const *const _);
        assert_eq!(wrapper.set_component_handler(handler_ptr), kResultOk);
        assert_eq!(wrapper.setup_processing(&setup), kResultOk);
        assert_eq!(wrapper.set_active(1), kResultOk);
        assert_eq!(wrapper.set_processing(1), kResultOk);
    }

    (wrapper, handler)
}

fn hash(wrapper: &Wrapper<TestPlugin>, id: &str) -> u32 {
    wrapper.inner.param_id_to_hash[id]
}

/// The parameter's actual value, as the plugin sees it.
fn value(wrapper: &Wrapper<TestPlugin>, id: &str) -> f32 {
    unsafe { wrapper.inner.param_by_hash[&hash(wrapper, id)].unmodulated_normalized_value() }
}

/// A begin/set/end gesture made in the plugin's editor.
fn editor_edit(wrapper: &Wrapper<TestPlugin>, id: &str, normalized: f32) {
    let gui_context = wrapper.inner.clone().make_gui_context();
    let param = wrapper.inner.param_by_hash[&hash(wrapper, id)];
    unsafe {
        gui_context.raw_begin_set_parameter(param);
        gui_context.raw_set_parameter_normalized(param, normalized);
        gui_context.raw_end_set_parameter(param);
    }
}

/// A process call that only carries parameter changes (a VST3 parameter flush), with each
/// `(hash, values)` pair delivered as one parameter's queue.
fn process(wrapper: &Wrapper<TestPlugin>, changes: Vec<(u32, Vec<f64>)>) {
    let queues = changes
        .into_iter()
        .map(|(id, values)| &*Box::leak(MockParamValueQueue::allocate(id, values)))
        .collect();
    let param_changes: &'static MockParameterChanges =
        Box::leak(MockParameterChanges::allocate(queues));

    let mut data: vst3_sys::vst::ProcessData = unsafe { mem::zeroed() };
    data.process_mode = ProcessModes::kRealtime as i32;
    data.symbolic_sample_size = SymbolicSampleSizes::kSample32 as i32;
    data.num_samples = 0;
    data.input_param_changes =
        unsafe { mem::transmute(&param_changes.__iparameterchangesvptr as *const *const _) };
    assert_eq!(unsafe { wrapper.process(&mut data) }, kResultOk);
}

fn stop(wrapper: &Wrapper<TestPlugin>) {
    unsafe {
        assert_eq!(wrapper.set_processing(0), kResultOk);
        assert_eq!(wrapper.set_active(0), kResultOk);
    }
}

/// Cubase 15 and Nuendo read the controller value back during `endEdit()` and send what they read
/// to the processor after the `performEdit()` value. Reading it right after the gesture is the same
/// as reading it from within the mock's `end_edit()`, since no process call happens in between.
#[test]
fn cubase_endedit_readback_keeps_the_edit() {
    let (wrapper, handler) = processing_wrapper();
    let gain = hash(&wrapper, GAIN_ID);
    let enabled = hash(&wrapper, ENABLED_ID);
    assert_eq!(value(&wrapper, GAIN_ID), 0.25);
    assert_eq!(value(&wrapper, ENABLED_ID), 0.0);

    let mut changes = Vec::new();
    for (id, hash, normalized) in [(ENABLED_ID, enabled, 1.0), (GAIN_ID, gain, 0.75)] {
        editor_edit(&wrapper, id, normalized);
        let read_back = unsafe { wrapper.get_param_normalized(hash) };
        println!("{id}: performEdit={normalized} getParamNormalized at endEdit={read_back}");
        changes.push((hash, vec![normalized as f64, read_back]));
    }
    assert_eq!(
        *handler.performed.lock().unwrap(),
        vec![(enabled, 1.0), (gain, 0.75)]
    );
    assert_eq!(*handler.ended.lock().unwrap(), vec![enabled, gain]);

    process(&wrapper, changes);
    let (enabled_value, gain_value) = (value(&wrapper, ENABLED_ID), value(&wrapper, GAIN_ID));
    println!("after process: enabled={enabled_value} gain={gain_value}");
    assert_eq!(enabled_value, 1.0, "the BoolParam edit was undone");
    assert_eq!(gain_value, 0.75, "the FloatParam edit was undone");
    stop(&wrapper);
}

#[test]
fn held_edit_applies_without_host_echo() {
    let (wrapper, _handler) = processing_wrapper();
    editor_edit(&wrapper, GAIN_ID, 0.75);
    editor_edit(&wrapper, ENABLED_ID, 1.0);
    // Not applied in the middle of processing
    assert_eq!(value(&wrapper, GAIN_ID), 0.25);
    assert_eq!(value(&wrapper, ENABLED_ID), 0.0);

    process(&wrapper, Vec::new());
    assert_eq!(value(&wrapper, GAIN_ID), 0.75);
    assert_eq!(value(&wrapper, ENABLED_ID), 1.0);
    stop(&wrapper);
}

#[test]
fn host_changes_in_the_same_call_win_over_held_edits() {
    let (wrapper, _handler) = processing_wrapper();
    let gain = hash(&wrapper, GAIN_ID);
    editor_edit(&wrapper, GAIN_ID, 0.75);

    process(&wrapper, vec![(gain, vec![0.5])]);
    assert_eq!(value(&wrapper, GAIN_ID), 0.5);
    assert_eq!(unsafe { wrapper.get_param_normalized(gain) }, 0.5);
    stop(&wrapper);
}

#[test]
fn set_processing_false_flushes_held_edits() {
    let (wrapper, _handler) = processing_wrapper();
    editor_edit(&wrapper, GAIN_ID, 0.75);
    editor_edit(&wrapper, ENABLED_ID, 1.0);

    unsafe { assert_eq!(wrapper.set_processing(0), kResultOk) };
    assert_eq!(value(&wrapper, GAIN_ID), 0.75);
    assert_eq!(value(&wrapper, ENABLED_ID), 1.0);
    unsafe { assert_eq!(wrapper.set_active(0), kResultOk) };
}

#[test]
fn state_restore_discards_held_edits() {
    let (wrapper, _handler) = processing_wrapper();
    let gain = hash(&wrapper, GAIN_ID);
    // A state with gain at 0.5
    process(&wrapper, vec![(gain, vec![0.5])]);
    let mut state = wrapper.inner.get_state_object();
    process(&wrapper, vec![(gain, vec![0.25])]);
    assert_eq!(value(&wrapper, GAIN_ID), 0.25);

    editor_edit(&wrapper, GAIN_ID, 0.75);
    // The same `set_state_inner_exclusive()` that `IComponent::setState()` and the editor's
    // preset restore use
    assert!(wrapper.inner.set_state_inner(&mut state));
    assert_eq!(unsafe { wrapper.get_param_normalized(gain) }, 0.5);

    process(&wrapper, Vec::new());
    assert_eq!(value(&wrapper, GAIN_ID), 0.5);
    stop(&wrapper);
}

#[test]
fn edit_while_not_processing_clears_stale_hold() {
    let (wrapper, _handler) = processing_wrapper();
    editor_edit(&wrapper, GAIN_ID, 0.75);

    // `set_processing(false)` would flush the held edit, so the window in which a host has
    // stopped processing without saying so (REAPER bypassing the plugin) is created directly
    wrapper.inner.is_processing.store(false, Ordering::SeqCst);
    editor_edit(&wrapper, GAIN_ID, 0.5);
    assert_eq!(value(&wrapper, GAIN_ID), 0.5);
    wrapper.inner.is_processing.store(true, Ordering::SeqCst);

    process(&wrapper, Vec::new());
    assert_eq!(
        value(&wrapper, GAIN_ID),
        0.5,
        "the older held edit was applied"
    );
    stop(&wrapper);
}

#[test]
fn host_set_param_normalized_keeps_an_echo_and_clears_a_different_value() {
    let (wrapper, _handler) = processing_wrapper();
    let gain = hash(&wrapper, GAIN_ID);

    // The host echoing the editor's own edit to the controller keeps it held
    editor_edit(&wrapper, GAIN_ID, 0.75);
    unsafe { assert_eq!(wrapper.set_param_normalized(gain, 0.75), kResultOk) };
    assert_eq!(unsafe { wrapper.get_param_normalized(gain) }, 0.75);
    process(&wrapper, Vec::new());
    assert_eq!(value(&wrapper, GAIN_ID), 0.75);

    // A different value from the host discards the held edit, so the edit cannot later be
    // applied over the value the host sends to the processor. The host would send 0.5 with the
    // next process call. It is left out here so that only the discarded edit could change the
    // value.
    editor_edit(&wrapper, GAIN_ID, 0.1);
    unsafe { assert_eq!(wrapper.set_param_normalized(gain, 0.5), kResultOk) };
    assert_eq!(unsafe { wrapper.get_param_normalized(gain) }, 0.75);
    process(&wrapper, Vec::new());
    assert_eq!(
        value(&wrapper, GAIN_ID),
        0.75,
        "the discarded edit was applied"
    );
    stop(&wrapper);
}
