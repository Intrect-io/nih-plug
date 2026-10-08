use super::*;
use crate::prelude::{ClapFeature, Enum, EnumParam, FloatParam, FloatRange, Param, ProcessContext};
use std::ptr;

#[derive(Enum, Debug, PartialEq, Clone, Copy)]
enum SingleVariant {
    Only,
}

struct TestParams {
    value: FloatParam,
    single: Option<EnumParam<SingleVariant>>,
}
unsafe impl Params for TestParams {
    fn param_map(&self) -> Vec<(String, ParamPtr, String)> {
        let mut params = vec![("value".into(), self.value.as_ptr(), String::new())];
        if let Some(single) = &self.single {
            params.push(("single".into(), single.as_ptr(), String::new()));
        }
        params
    }
}
struct TestPlugin<const ENABLED: bool> {
    params: Arc<TestParams>,
}
impl<const ENABLED: bool> Default for TestPlugin<ENABLED> {
    fn default() -> Self {
        Self {
            params: Arc::new(TestParams {
                value: FloatParam::new("Value", 0.25, FloatRange::Linear { min: 0.0, max: 1.0 }),
                // IntRange rejects zero-width ranges in debug tests, but release
                // EnumParam still advertises its single variant as CLAP [0, 0].
                single: if cfg!(debug_assertions) {
                    None
                } else {
                    Some(EnumParam::new("Single", SingleVariant::Only))
                },
            }),
        }
    }
}

#[cfg(not(debug_assertions))]
#[test]
fn zero_width_advertised_enum_accepts_zero_and_rejects_invalid_batches() {
    let host = host();
    let wrapper = unsafe { Wrapper::<TestPlugin<true>>::new(&host) };
    let plugin = wrapper.clap_plugin.borrow();
    let value_id = wrapper.param_id_to_hash["value"];
    let single_id = wrapper.param_id_to_hash["single"];
    let ext = &Wrapper::<TestPlugin<true>>::AAX_PARAMETER_DELIVERY;
    let mut info = unsafe { mem::zeroed::<clap_param_info>() };
    assert!(unsafe { Wrapper::<TestPlugin<true>>::ext_params_get_info(&*plugin, 1, &mut info) });
    assert_eq!(info.id, single_id);
    assert_eq!(
        (info.min_value, info.max_value, info.default_value),
        (0.0, 0.0, 0.0)
    );
    assert_eq!(
        wrapper
            .plugin
            .lock()
            .params
            .single
            .as_ref()
            .unwrap()
            .step_count(),
        Some(0)
    );
    let mut received = Vec::<u16>::new();
    let output = clap_output_events {
        ctx: &mut received as *mut _ as *mut _,
        try_push: Some(output_push),
    };
    for zero in [0.0, -0.0] {
        let events = vec![value(value_id, 0.8), value(single_id, zero)];
        let input = clap_input_events {
            ctx: &events as *const _ as *mut _,
            size: Some(input_size),
            get: Some(input_get),
        };
        unsafe {
            let token = (ext.try_begin)(&*plugin);
            assert_ne!(token, 0);
            assert_eq!((ext.flush)(&*plugin, token, &input, &output), 2);
            assert!((ext.end)(&*plugin, token));
        }
        let params = wrapper.plugin.lock().params.clone();
        assert_eq!(params.value.value(), 0.8);
        let single = params.single.as_ref().unwrap();
        assert_eq!(single.value(), SingleVariant::Only);
        assert_eq!(single.unmodulated_normalized_value(), 0.0);
        let mut plain = f64::NAN;
        assert!(unsafe {
            Wrapper::<TestPlugin<true>>::ext_params_get_value(&*plugin, single_id, &mut plain)
        });
        assert_eq!(plain, 0.0);
    }
    for invalid in [
        f64::EPSILON,
        -f64::EPSILON,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ] {
        let events = vec![value(value_id, 0.2), value(single_id, invalid)];
        let input = clap_input_events {
            ctx: &events as *const _ as *mut _,
            size: Some(input_size),
            get: Some(input_get),
        };
        unsafe {
            let token = (ext.try_begin)(&*plugin);
            assert_ne!(token, 0);
            assert_eq!((ext.flush)(&*plugin, token, &input, &output), 0);
            assert!((ext.end)(&*plugin, token));
        }
        assert_eq!(wrapper.plugin.lock().params.value.value(), 0.8);
    }
    assert!(received.is_empty());
}
impl<const ENABLED: bool> Plugin for TestPlugin<ENABLED> {
    const NAME: &'static str = "AAX delivery fixture";
    const VENDOR: &'static str = "Intrect";
    const URL: &'static str = "https://intrect.io";
    const EMAIL: &'static str = "test@intrect.io";
    const VERSION: &'static str = "0.0.0";
    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: NonZeroU32::new(1),
        main_output_channels: NonZeroU32::new(1),
        ..AudioIOLayout::const_default()
    }];
    type SysExMessage = ();
    type BackgroundTask = ();
    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }
    fn process(
        &mut self,
        buffer: &mut crate::buffer::Buffer,
        _: &mut AuxiliaryBuffers,
        _: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        for channel in buffer.as_slice() {
            for sample in channel.iter_mut() {
                *sample *= self.params.value.value();
            }
        }
        ProcessStatus::Normal
    }
}
impl<const ENABLED: bool> ClapPlugin for TestPlugin<ENABLED> {
    const CLAP_ID: &'static str = "io.intrect.aax-delivery-test";
    const CLAP_DESCRIPTION: Option<&'static str> = None;
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[ClapFeature::AudioEffect];
    const CLAP_AAX_CONCURRENT_PARAMETER_DELIVERY: bool = ENABLED;
}
unsafe extern "C" fn no_extension(_: *const clap_host, _: *const c_char) -> *const c_void {
    ptr::null()
}
unsafe extern "C" fn no_request(_: *const clap_host) {}
fn host() -> clap_host {
    clap_host {
        clap_version: clap_sys::version::CLAP_VERSION,
        host_data: ptr::null_mut(),
        name: c"AAX fixture".as_ptr(),
        vendor: c"Intrect".as_ptr(),
        url: c"https://intrect.io".as_ptr(),
        version: c"0".as_ptr(),
        get_extension: Some(no_extension),
        request_restart: Some(no_request),
        request_process: Some(no_request),
        request_callback: Some(no_request),
    }
}
unsafe extern "C" fn input_size(input: *const clap_input_events) -> u32 {
    (&*((*input).ctx as *const Vec<clap_event_param_value>)).len() as u32
}
unsafe extern "C" fn input_get(
    input: *const clap_input_events,
    index: u32,
) -> *const clap_event_header {
    let events = &*((*input).ctx as *const Vec<clap_event_param_value>);
    events
        .get(index as usize)
        .map_or(ptr::null(), |event| &event.header)
}
unsafe extern "C" fn output_push(
    output: *const clap_output_events,
    event: *const clap_event_header,
) -> bool {
    let result = &mut *((*output).ctx as *mut Vec<u16>);
    result.push((*event).type_);
    true
}
fn value(id: u32, plain: f64) -> clap_event_param_value {
    clap_event_param_value {
        header: clap_event_header {
            size: mem::size_of::<clap_event_param_value>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_PARAM_VALUE,
            flags: 0,
        },
        param_id: id,
        cookie: ptr::null_mut(),
        note_id: -1,
        port_index: -1,
        channel: -1,
        key: -1,
        value: plain,
    }
}
#[test]
fn extension_requires_explicit_opt_in() {
    let host = host();
    let disabled = unsafe { Wrapper::<TestPlugin<false>>::new(&host) };
    let enabled = unsafe { Wrapper::<TestPlugin<true>>::new(&host) };
    for (expected, plugin) in [
        (false, disabled.clap_plugin.borrow()),
        (true, enabled.clap_plugin.borrow()),
    ] {
        let extension = unsafe {
            (plugin.get_extension.unwrap())(&*plugin, AAX_PARAMETER_DELIVERY_ID.as_ptr())
        };
        assert_eq!(!extension.is_null(), expected);
    }
}
#[test]
fn bridge_avoids_note_buffers_and_preserves_parameter_gestures() {
    let host = host();
    let wrapper = unsafe { Wrapper::<TestPlugin<true>>::new(&host) };
    let plugin = wrapper.clap_plugin.borrow();
    let id = wrapper.param_id_to_hash["value"];
    let events = vec![value(id, 0.75)];
    let input = clap_input_events {
        ctx: &events as *const _ as *mut _,
        size: Some(input_size),
        get: Some(input_get),
    };
    let mut received = Vec::new();
    let output = clap_output_events {
        ctx: &mut received as *mut Vec<u16> as *mut _,
        try_push: Some(output_push),
    };
    wrapper
        .output_parameter_events
        .push(OutputParamEvent::BeginGesture { param_hash: id })
        .unwrap();
    wrapper
        .output_parameter_events
        .push(OutputParamEvent::SetValue {
            param_hash: id,
            clap_plain_value: 0.5,
        })
        .unwrap();
    wrapper
        .output_parameter_events
        .push(OutputParamEvent::EndGesture { param_hash: id })
        .unwrap();
    let _held_input = wrapper.input_events.borrow_mut();
    let _held_output = wrapper.output_events.borrow_mut();
    let ext = &Wrapper::<TestPlugin<true>>::AAX_PARAMETER_DELIVERY;
    unsafe {
        let token = (ext.try_begin)(&*plugin);
        assert_ne!(token, 0);
        assert_eq!((ext.try_begin)(&*plugin), 0);
        assert_eq!((ext.flush)(&*plugin, token, &input, &output), 2);
        assert!((ext.end)(&*plugin, token));
    }
    assert_eq!(wrapper.plugin.lock().params.value.value(), 0.5);
    assert_eq!(
        received,
        [
            CLAP_EVENT_PARAM_GESTURE_BEGIN,
            CLAP_EVENT_PARAM_VALUE,
            CLAP_EVENT_PARAM_GESTURE_END
        ]
    );
    assert!(wrapper.output_parameter_events.is_empty());
}
#[test]
fn rejected_batch_applies_nothing_and_audio_output_defers_without_consuming() {
    let host = host();
    let wrapper = unsafe { Wrapper::<TestPlugin<true>>::new(&host) };
    let plugin = wrapper.clap_plugin.borrow();
    let id = wrapper.param_id_to_hash["value"];
    let mut events = vec![value(id, 0.8), value(id, 0.9)];
    events[1].header.size = mem::size_of::<clap_event_header>() as u32;
    let input = clap_input_events {
        ctx: &events as *const _ as *mut _,
        size: Some(input_size),
        get: Some(input_get),
    };
    let mut received = Vec::new();
    let output = clap_output_events {
        ctx: &mut received as *mut Vec<u16> as *mut _,
        try_push: Some(output_push),
    };
    wrapper
        .output_parameter_events
        .push(OutputParamEvent::SetValue {
            param_hash: id,
            clap_plain_value: 0.6,
        })
        .unwrap();
    let ext = &Wrapper::<TestPlugin<true>>::AAX_PARAMETER_DELIVERY;
    let token = unsafe { (ext.try_begin)(&*plugin) };
    assert_ne!(token, 0);
    unsafe {
        assert_eq!((ext.flush)(&*plugin, token, &input, &output), 0);
        wrapper.handle_out_events(&output, 0, 64);
    }
    assert_eq!(wrapper.plugin.lock().params.value.value(), 0.25);
    assert!(received.is_empty());
    assert_eq!(wrapper.output_parameter_events.len(), 1);
    unsafe {
        assert!((ext.end)(&*plugin, token));
        wrapper.handle_out_events(&output, 0, 64);
    }
    assert_eq!(wrapper.plugin.lock().params.value.value(), 0.6);
    assert_eq!(received, [CLAP_EVENT_PARAM_VALUE]);
}

#[test]
fn failed_stale_and_cross_thread_tokens_cannot_release_another_owner() {
    let host = host();
    let wrapper = unsafe { Wrapper::<TestPlugin<true>>::new(&host) };
    let ext = &Wrapper::<TestPlugin<true>>::AAX_PARAMETER_DELIVERY;
    let plugin = wrapper.clap_plugin.borrow();
    let first = unsafe { (ext.try_begin)(&*plugin) };
    assert_ne!(first, 0);
    assert_eq!(unsafe { (ext.try_begin)(&*plugin) }, 0);
    assert!(!unsafe { (ext.end)(&*plugin, 0) });
    thread::scope(|scope| {
        scope
            .spawn(|| {
                let plugin = wrapper.clap_plugin.borrow();
                assert!(!unsafe { (ext.end)(&*plugin, first) });
                assert_eq!(unsafe { (ext.try_begin)(&*plugin) }, 0);
            })
            .join()
            .unwrap();
    });
    assert!(unsafe { (ext.end)(&*plugin, first) });
    let second = unsafe { (ext.try_begin)(&*plugin) };
    assert!(second > first);
    assert!(!unsafe { (ext.end)(&*plugin, first) });
    assert_eq!(unsafe { (ext.try_begin)(&*plugin) }, 0);
    assert!(unsafe { (ext.end)(&*plugin, second) });
}

#[test]
fn rejected_output_is_retained_in_order_without_replaying_accepted_gestures() {
    struct Sink {
        accepted: Vec<u16>,
        reject_value_once: bool,
    }
    unsafe extern "C" fn push(
        out: *const clap_output_events,
        event: *const clap_event_header,
    ) -> bool {
        let sink = &mut *((*out).ctx as *mut Sink);
        if (*event).type_ == CLAP_EVENT_PARAM_VALUE && sink.reject_value_once {
            sink.reject_value_once = false;
            return false;
        }
        sink.accepted.push((*event).type_);
        true
    }
    let host = host();
    let wrapper = unsafe { Wrapper::<TestPlugin<true>>::new(&host) };
    let plugin = wrapper.clap_plugin.borrow();
    let id = wrapper.param_id_to_hash["value"];
    let events = Vec::<clap_event_param_value>::new();
    let input = clap_input_events {
        ctx: &events as *const _ as *mut _,
        size: Some(input_size),
        get: Some(input_get),
    };
    let mut sink = Sink {
        accepted: Vec::new(),
        reject_value_once: true,
    };
    let output = clap_output_events {
        ctx: &mut sink as *mut _ as *mut _,
        try_push: Some(push),
    };
    for event in [
        OutputParamEvent::BeginGesture { param_hash: id },
        OutputParamEvent::SetValue {
            param_hash: id,
            clap_plain_value: 0.6,
        },
        OutputParamEvent::EndGesture { param_hash: id },
    ] {
        wrapper.output_parameter_events.push(event).unwrap();
    }
    let ext = &Wrapper::<TestPlugin<true>>::AAX_PARAMETER_DELIVERY;
    unsafe {
        let token = (ext.try_begin)(&*plugin);
        assert_ne!(token, 0);
        assert_eq!((ext.flush)(&*plugin, token, &input, &output), 1);
        assert_eq!(sink.accepted, [CLAP_EVENT_PARAM_GESTURE_BEGIN]);
        assert!(wrapper.pending_parameter_output.borrow().is_some());
        assert_eq!(wrapper.output_parameter_events.len(), 1);
        assert_eq!((ext.flush)(&*plugin, token, &input, &output), 2);
        assert!((ext.end)(&*plugin, token));
    }
    assert_eq!(
        sink.accepted,
        [
            CLAP_EVENT_PARAM_GESTURE_BEGIN,
            CLAP_EVENT_PARAM_VALUE,
            CLAP_EVENT_PARAM_GESTURE_END
        ]
    );
    assert!(wrapper.pending_parameter_output.borrow().is_none());
    assert!(wrapper.output_parameter_events.is_empty());
}

#[test]
fn actual_process_completes_while_background_parameter_output_is_blocked() {
    use clap_sys::audio_buffer::clap_audio_buffer;
    use std::sync::mpsc;

    struct BlockingSink {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }
    unsafe extern "C" fn hold_output(
        output: *const clap_output_events,
        _: *const clap_event_header,
    ) -> bool {
        let sink = &*((*output).ctx as *const BlockingSink);
        sink.entered.send(()).unwrap();
        sink.release.recv().unwrap();
        true
    }
    let host = host();
    let wrapper = unsafe { Wrapper::<TestPlugin<true>>::new(&host) };
    {
        let plugin = wrapper.clap_plugin.borrow();
        unsafe {
            assert!((plugin.init.unwrap())(&*plugin));
            assert!((plugin.activate.unwrap())(&*plugin, 48000.0, 64, 64));
            assert!((plugin.start_processing.unwrap())(&*plugin));
        }
    }
    let id = wrapper.param_id_to_hash["value"];
    wrapper
        .output_parameter_events
        .push(OutputParamEvent::SetValue {
            param_hash: id,
            clap_plain_value: 0.6,
        })
        .unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (audio_tx, audio_rx) = mpsc::channel();
    let completed_while_held = thread::scope(|scope| {
        let background = scope.spawn(|| {
            let plugin = wrapper.clap_plugin.borrow();
            let events = Vec::<clap_event_param_value>::new();
            let input = clap_input_events {
                ctx: &events as *const _ as *mut _,
                size: Some(input_size),
                get: Some(input_get),
            };
            let sink = BlockingSink {
                entered: entered_tx,
                release: release_rx,
            };
            let output = clap_output_events {
                ctx: &sink as *const _ as *mut _,
                try_push: Some(hold_output),
            };
            let ext = &Wrapper::<TestPlugin<true>>::AAX_PARAMETER_DELIVERY;
            unsafe {
                let token = (ext.try_begin)(&*plugin);
                assert_ne!(token, 0);
                let result = (ext.flush)(&*plugin, token, &input, &output);
                assert!((ext.end)(&*plugin, token));
                result
            }
        });
        // If setup fails, release and join before asserting so a failed test cannot hang.
        let entered = entered_rx.recv_timeout(Duration::from_secs(2)).is_ok();
        let audio = scope.spawn(|| {
            let plugin = wrapper.clap_plugin.borrow();
            let mut input = [0.5f32; 64];
            let mut output = [-99.0f32; 64];
            let mut ip = input.as_mut_ptr();
            let mut op = output.as_mut_ptr();
            let in_buffer = clap_audio_buffer {
                data32: &mut ip,
                data64: ptr::null_mut(),
                channel_count: 1,
                latency: 0,
                constant_mask: 0,
            };
            let mut out_buffer = clap_audio_buffer {
                data32: &mut op,
                data64: ptr::null_mut(),
                channel_count: 1,
                latency: 0,
                constant_mask: 0,
            };
            let mut received = Vec::<u16>::new();
            let events = clap_output_events {
                ctx: &mut received as *mut _ as *mut _,
                try_push: Some(output_push),
            };
            let process = clap_process {
                steady_time: 0,
                frames_count: 64,
                transport: ptr::null(),
                audio_inputs: &in_buffer,
                audio_outputs: &mut out_buffer,
                audio_inputs_count: 1,
                audio_outputs_count: 1,
                in_events: ptr::null(),
                out_events: &events,
            };
            let result = unsafe { (plugin.process.unwrap())(&*plugin, &process) };
            audio_tx.send((result, output)).unwrap();
        });
        let completed = audio_rx.recv_timeout(Duration::from_secs(2)).ok();
        release_tx.send(()).unwrap();
        let flush_result = background.join().unwrap();
        audio.join().unwrap();
        assert!(entered, "background output callback did not enter");
        assert_eq!(flush_result, 2);
        completed
    });
    let (status, output) =
        completed_while_held.expect("DSP waited for background parameter output");
    assert_ne!(status, CLAP_PROCESS_ERROR);
    assert!(output.iter().all(|sample| (*sample - 0.3).abs() < 1e-6));
    let plugin = wrapper.clap_plugin.borrow();
    unsafe {
        (plugin.stop_processing.unwrap())(&*plugin);
        (plugin.deactivate.unwrap())(&*plugin);
    }
}

#[test]
fn out_of_range_and_nonfinite_values_reject_the_entire_input_batch() {
    let host = host();
    let wrapper = unsafe { Wrapper::<TestPlugin<true>>::new(&host) };
    let plugin = wrapper.clap_plugin.borrow();
    let id = wrapper.param_id_to_hash["value"];
    let ext = &Wrapper::<TestPlugin<true>>::AAX_PARAMETER_DELIVERY;
    let mut received = Vec::<u16>::new();
    let output = clap_output_events {
        ctx: &mut received as *mut _ as *mut _,
        try_push: Some(output_push),
    };
    for invalid in [
        f64::MAX,
        -f64::MAX,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
        -0.1,
        1.0 + f64::EPSILON,
    ] {
        let events = vec![value(id, 0.8), value(id, invalid)];
        let input = clap_input_events {
            ctx: &events as *const _ as *mut _,
            size: Some(input_size),
            get: Some(input_get),
        };
        unsafe {
            let token = (ext.try_begin)(&*plugin);
            assert_ne!(token, 0);
            assert_eq!((ext.flush)(&*plugin, token, &input, &output), 0);
            assert!((ext.end)(&*plugin, token));
        }
        assert_eq!(wrapper.plugin.lock().params.value.value(), 0.25);
        assert!(received.is_empty());
    }
}

#[test]
fn lifecycle_contention_rejects_activation_and_silences_audio_without_borrowing() {
    let host = host();
    let wrapper = unsafe { Wrapper::<TestPlugin<true>>::new(&host) };
    let plugin = wrapper.clap_plugin.borrow();
    let guard = wrapper.processing_gate.lock();
    // These live borrows used to abort activate() at the C ABI boundary.
    let buffers = wrapper.buffer_manager.borrow_mut();
    let events = wrapper.input_events.borrow_mut();
    assert!(!unsafe { Wrapper::<TestPlugin<true>>::activate(&*plugin, 48000.0, 1, 16) });
    let mut samples = [1.0f32; 16];
    let mut pointers = [samples.as_mut_ptr()];
    let mut output = clap_sys::audio_buffer::clap_audio_buffer {
        data32: pointers.as_mut_ptr(),
        data64: ptr::null_mut(),
        channel_count: 1,
        latency: 0,
        constant_mask: 0,
    };
    let process = clap_process {
        steady_time: 0,
        frames_count: 16,
        transport: ptr::null(),
        audio_inputs: ptr::null(),
        audio_outputs: &mut output,
        audio_inputs_count: 0,
        audio_outputs_count: 1,
        in_events: ptr::null(),
        out_events: ptr::null(),
    };
    assert_eq!(
        unsafe { Wrapper::<TestPlugin<true>>::process(&*plugin, &process) },
        CLAP_PROCESS_ERROR
    );
    assert_eq!(samples, [0.0; 16]);
    unsafe { Wrapper::<TestPlugin<true>>::reset(&*plugin) };
    assert!(wrapper.reset_pending.load(Ordering::Acquire));
    drop(events);
    drop(buffers);
    drop(guard);
    assert!(unsafe { Wrapper::<TestPlugin<true>>::activate(&*plugin, 48000.0, 1, 16) });
}
