use super::*;
use crate::prelude::{ClapFeature, FloatParam, FloatRange, InitContext, Param, ProcessContext};
use clap_sys::audio_buffer::clap_audio_buffer;
use std::ptr;

struct TestParams {
    amount: FloatParam,
}
unsafe impl Params for TestParams {
    fn param_map(&self) -> Vec<(String, ParamPtr, String)> {
        vec![("amount".into(), self.amount.as_ptr(), String::new())]
    }
}
struct TestPlugin {
    params: Arc<TestParams>,
}
impl Default for TestPlugin {
    fn default() -> Self {
        Self {
            params: Arc::new(TestParams {
                amount: FloatParam::new("Amount", 0.25, FloatRange::Linear { min: 0.0, max: 1.0 }),
            }),
        }
    }
}
impl TestPlugin {
    fn latency(&self) -> u32 {
        if self.params.amount.value() > 0.0 {
            256
        } else {
            128
        }
    }
}
impl Plugin for TestPlugin {
    const NAME: &'static str = "CLAP host contract fixture";
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
    fn initialize(
        &mut self,
        _: &AudioIOLayout,
        config: &BufferConfig,
        context: &mut impl InitContext<Self>,
    ) -> bool {
        context.set_latency_samples(self.latency());
        config.sample_rate != 8_000.0
    }
    fn process(
        &mut self,
        _: &mut crate::buffer::Buffer,
        _: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        context.set_latency_samples(self.latency());
        ProcessStatus::Normal
    }
}
impl ClapPlugin for TestPlugin {
    const CLAP_ID: &'static str = "io.intrect.clap-host-contract-test";
    const CLAP_DESCRIPTION: Option<&'static str> = None;
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[ClapFeature::AudioEffect];
}

#[derive(Default)]
struct HostData {
    being_activated: AtomicBool,
    audio_role: AtomicBool,
    latency_changed: AtomicU32,
    illegal_latency_changed: AtomicU32,
    restarts: AtomicU32,
    rescans: AtomicU32,
}
unsafe fn data(host: *const clap_host) -> &'static HostData {
    &*((*host).host_data as *const HostData)
}
unsafe extern "C" fn changed(host: *const clap_host) {
    let d = data(host);
    d.latency_changed.fetch_add(1, Ordering::SeqCst);
    if !d.being_activated.load(Ordering::SeqCst) || d.audio_role.load(Ordering::SeqCst) {
        d.illegal_latency_changed.fetch_add(1, Ordering::SeqCst);
    }
}
unsafe extern "C" fn restart(host: *const clap_host) {
    data(host).restarts.fetch_add(1, Ordering::SeqCst);
}
unsafe extern "C" fn rescan(host: *const clap_host, flags: u32) {
    data(host).rescans.fetch_or(flags, Ordering::SeqCst);
}
unsafe extern "C" fn no_request(_: *const clap_host) {}
unsafe extern "C" fn no_clear(_: *const clap_host, _: clap_id, _: u32) {}
unsafe extern "C" fn main_thread(host: *const clap_host) -> bool {
    !data(host).audio_role.load(Ordering::SeqCst)
}
unsafe extern "C" fn audio_thread(host: *const clap_host) -> bool {
    data(host).audio_role.load(Ordering::SeqCst)
}
static HOST_LATENCY: clap_host_latency = clap_host_latency {
    changed: Some(changed),
};
static HOST_PARAMS: clap_host_params = clap_host_params {
    rescan: Some(rescan),
    clear: Some(no_clear),
    request_flush: Some(no_request),
};
static HOST_THREADS: clap_host_thread_check = clap_host_thread_check {
    is_main_thread: Some(main_thread),
    is_audio_thread: Some(audio_thread),
};
unsafe extern "C" fn extension(_: *const clap_host, id: *const c_char) -> *const c_void {
    let id = CStr::from_ptr(id);
    if id == CLAP_EXT_LATENCY {
        &HOST_LATENCY as *const _ as *const _
    } else if id == CLAP_EXT_PARAMS {
        &HOST_PARAMS as *const _ as *const _
    } else if id == CLAP_EXT_THREAD_CHECK {
        &HOST_THREADS as *const _ as *const _
    } else {
        ptr::null()
    }
}

struct Fixture {
    wrapper: Arc<Wrapper<TestPlugin>>,
    _host: Box<clap_host>,
    data: Box<HostData>,
    active: bool,
    processing: bool,
}
impl Fixture {
    fn new() -> Self {
        let mut d = Box::<HostData>::default();
        let host = Box::new(clap_host {
            clap_version: clap_sys::version::CLAP_VERSION,
            host_data: &mut *d as *mut _ as *mut _,
            name: c"Host contract fixture".as_ptr(),
            vendor: c"Intrect".as_ptr(),
            url: c"https://intrect.io".as_ptr(),
            version: c"0".as_ptr(),
            get_extension: Some(extension),
            request_restart: Some(restart),
            request_process: Some(no_request),
            request_callback: Some(no_request),
        });
        let wrapper = unsafe { Wrapper::<TestPlugin>::new(&*host) };
        let result = Self {
            wrapper,
            _host: host,
            data: d,
            active: false,
            processing: false,
        };
        assert!(unsafe { ((*result.plugin()).init.unwrap())(result.plugin()) });
        result
    }
    fn plugin(&self) -> *const clap_plugin {
        &*self.wrapper.clap_plugin.borrow() as *const _
    }
    fn params(&self) -> &clap_plugin_params {
        unsafe {
            &*(((*self.plugin()).get_extension.unwrap())(self.plugin(), CLAP_EXT_PARAMS.as_ptr())
                as *const clap_plugin_params)
        }
    }
    fn state(&self) -> &clap_plugin_state {
        unsafe {
            &*(((*self.plugin()).get_extension.unwrap())(self.plugin(), CLAP_EXT_STATE.as_ptr())
                as *const clap_plugin_state)
        }
    }
    fn activate(&mut self, rate: f64) -> bool {
        self.data.being_activated.store(true, Ordering::SeqCst);
        let ok = unsafe { ((*self.plugin()).activate.unwrap())(self.plugin(), rate, 1, 32) };
        self.data.being_activated.store(false, Ordering::SeqCst);
        self.active = ok;
        ok
    }
    fn deactivate(&mut self) {
        if self.processing {
            self.data.audio_role.store(true, Ordering::SeqCst);
            unsafe { ((*self.plugin()).stop_processing.unwrap())(self.plugin()) };
            self.data.audio_role.store(false, Ordering::SeqCst);
            self.processing = false;
        }
        if self.active {
            unsafe { ((*self.plugin()).deactivate.unwrap())(self.plugin()) };
            self.active = false;
        }
    }
    fn set(&self, value: f64) {
        let mut info = unsafe { mem::zeroed::<clap_param_info>() };
        assert!(unsafe { (self.params().get_info.unwrap())(self.plugin(), 0, &mut info) });
        let event = clap_event_param_value {
            header: clap_event_header {
                size: mem::size_of::<clap_event_param_value>() as u32,
                time: 0,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_PARAM_VALUE,
                flags: 0,
            },
            param_id: info.id,
            cookie: ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            value,
        };
        let input = clap_input_events {
            ctx: &event as *const _ as *mut _,
            size: Some(one_event),
            get: Some(get_event),
        };
        let output = clap_output_events {
            ctx: ptr::null_mut(),
            try_push: Some(accept_event),
        };
        self.data.audio_role.store(self.active, Ordering::SeqCst);
        unsafe { (self.params().flush.unwrap())(self.plugin(), &input, &output) };
        self.data.audio_role.store(false, Ordering::SeqCst);
    }
    fn value(&self) -> f64 {
        let mut info = unsafe { mem::zeroed::<clap_param_info>() };
        assert!(unsafe { (self.params().get_info.unwrap())(self.plugin(), 0, &mut info) });
        let mut value = f64::NAN;
        assert!(unsafe { (self.params().get_value.unwrap())(self.plugin(), info.id, &mut value) });
        value
    }
    fn save(&self) -> Vec<u8> {
        let mut bytes = Vec::<u8>::new();
        let stream = clap_ostream {
            ctx: &mut bytes as *mut _ as *mut _,
            write: Some(write),
        };
        assert!(unsafe { (self.state().save.unwrap())(self.plugin(), &stream) });
        bytes
    }
    fn load(&self, bytes: &[u8], max_read: usize) -> bool {
        let mut reader = Reader {
            bytes,
            cursor: 0,
            max_read,
        };
        let stream = clap_istream {
            ctx: &mut reader as *mut _ as *mut _,
            read: Some(read),
        };
        unsafe { (self.state().load.unwrap())(self.plugin(), &stream) }
    }
    fn process(&mut self) {
        self.data.audio_role.store(true, Ordering::SeqCst);
        if !self.processing {
            assert!(unsafe { ((*self.plugin()).start_processing.unwrap())(self.plugin()) });
            self.processing = true;
        }
        let mut audio = [0.5_f32; 16];
        let mut channel = audio.as_mut_ptr();
        let input_buffer = clap_audio_buffer {
            data32: &mut channel,
            data64: ptr::null_mut(),
            channel_count: 1,
            latency: 0,
            constant_mask: 0,
        };
        let mut output_buffer = clap_audio_buffer {
            data32: &mut channel,
            data64: ptr::null_mut(),
            channel_count: 1,
            latency: 0,
            constant_mask: 0,
        };
        let input = clap_input_events {
            ctx: ptr::null_mut(),
            size: Some(no_events),
            get: Some(get_event),
        };
        let output = clap_output_events {
            ctx: ptr::null_mut(),
            try_push: Some(accept_event),
        };
        let process = clap_process {
            steady_time: 0,
            frames_count: 16,
            transport: ptr::null(),
            audio_inputs: &input_buffer,
            audio_outputs: &mut output_buffer,
            audio_inputs_count: 1,
            audio_outputs_count: 1,
            in_events: &input,
            out_events: &output,
        };
        assert_ne!(
            unsafe { ((*self.plugin()).process.unwrap())(self.plugin(), &process) },
            CLAP_PROCESS_ERROR
        );
        self.data.audio_role.store(false, Ordering::SeqCst);
        unsafe { ((*self.plugin()).on_main_thread.unwrap())(self.plugin()) };
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.deactivate();
    }
}
unsafe extern "C" fn one_event(_: *const clap_input_events) -> u32 {
    1
}
unsafe extern "C" fn no_events(_: *const clap_input_events) -> u32 {
    0
}
unsafe extern "C" fn get_event(
    events: *const clap_input_events,
    _: u32,
) -> *const clap_event_header {
    (*events).ctx as *const _
}
unsafe extern "C" fn accept_event(
    _: *const clap_output_events,
    _: *const clap_event_header,
) -> bool {
    true
}
unsafe extern "C" fn write(stream: *const clap_ostream, bytes: *const c_void, size: u64) -> i64 {
    let output = &mut *((*stream).ctx as *mut Vec<u8>);
    output.extend_from_slice(std::slice::from_raw_parts(
        bytes as *const u8,
        size as usize,
    ));
    size as i64
}
struct Reader<'a> {
    bytes: &'a [u8],
    cursor: usize,
    max_read: usize,
}
unsafe extern "C" fn read(stream: *const clap_istream, bytes: *mut c_void, size: u64) -> i64 {
    let input = &mut *((*stream).ctx as *mut Reader<'_>);
    let n = (size as usize)
        .min(input.max_read)
        .min(input.bytes.len() - input.cursor);
    ptr::copy_nonoverlapping(input.bytes[input.cursor..].as_ptr(), bytes as *mut u8, n);
    input.cursor += n;
    n as i64
}
fn zero_state() -> Vec<u8> {
    let f = Fixture::new();
    f.set(0.0);
    f.save()
}

#[test]
fn host_contract_activation_notifies_latency_inside_activate() {
    let mut f = Fixture::new();
    assert!(f.activate(48_000.0));
    assert_eq!(f.data.latency_changed.load(Ordering::SeqCst), 1);
    assert_eq!(f.data.illegal_latency_changed.load(Ordering::SeqCst), 0);
    assert_eq!(f.data.restarts.load(Ordering::SeqCst), 0);
}
#[test]
fn host_contract_active_idle_state_change_requests_restart_and_rescan() {
    let state = zero_state();
    let mut f = Fixture::new();
    assert!(f.activate(48_000.0));
    assert!(f.load(&state, usize::MAX));
    assert_eq!(f.value(), 0.0);
    assert_eq!(f.data.illegal_latency_changed.load(Ordering::SeqCst), 0);
    assert_eq!(f.data.restarts.load(Ordering::SeqCst), 1);
    // CLAP's public ABI assigns bit 0 to a parameter value rescan.
    assert_eq!(f.data.rescans.load(Ordering::SeqCst), 1);
}
#[test]
fn host_contract_processing_latency_change_requests_main_thread_restart() {
    let mut f = Fixture::new();
    assert!(f.activate(48_000.0));
    f.set(0.0);
    f.process();
    assert_eq!(f.data.illegal_latency_changed.load(Ordering::SeqCst), 0);
    assert_eq!(f.data.restarts.load(Ordering::SeqCst), 1);
    // The AAX restart bridge queries this pending value before restarting DSP.
    let ext = unsafe {
        &*(((*f.plugin()).get_extension.unwrap())(f.plugin(), CLAP_EXT_LATENCY.as_ptr())
            as *const clap_plugin_latency)
    };
    assert_eq!(unsafe { (ext.get.unwrap())(f.plugin()) }, 128);
}
#[test]
fn host_contract_deactivated_state_change_waits_for_next_activation() {
    let state = zero_state();
    let mut f = Fixture::new();
    assert!(f.activate(48_000.0));
    f.deactivate();
    assert!(f.load(&state, 3));
    assert_eq!(f.data.illegal_latency_changed.load(Ordering::SeqCst), 0);
    assert_eq!(f.data.restarts.load(Ordering::SeqCst), 0);
    assert!(f.activate(48_000.0));
    assert_eq!(f.data.latency_changed.load(Ordering::SeqCst), 2);
}
#[test]
fn host_contract_fresh_state_restore_requests_rescan_and_is_byte_reproducible() {
    let state = zero_state();
    let f = Fixture::new();
    assert!(f.load(&state, 3));
    assert_eq!(f.value(), 0.0);
    assert_eq!(f.save(), state);
    assert_eq!(f.data.rescans.load(Ordering::SeqCst), 1);
}
// Malformed state deliberately trips the wrapper's debug assertion at the FFI boundary.
#[cfg(not(debug_assertions))]
#[test]
fn host_contract_failed_state_load_does_not_request_rescan() {
    let f = Fixture::new();
    assert!(!f.load(&[0, 1], 3));
    assert_eq!(f.data.rescans.load(Ordering::SeqCst), 0);
    assert_eq!(f.value(), 0.25);
}
#[test]
fn host_contract_failed_activation_does_not_leave_active_notification_state() {
    let state = zero_state();
    let mut f = Fixture::new();
    assert!(!f.activate(8_000.0));
    let calls = f.data.latency_changed.load(Ordering::SeqCst);
    assert!(f.load(&state, usize::MAX));
    assert_eq!(f.data.latency_changed.load(Ordering::SeqCst), calls);
    assert_eq!(f.data.illegal_latency_changed.load(Ordering::SeqCst), 0);
    assert_eq!(f.data.restarts.load(Ordering::SeqCst), 0);
}
