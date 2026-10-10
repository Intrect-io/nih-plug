use super::*;
use crate::prelude::{ClapFeature, GuiContext, ParentWindowHandle, ProcessContext};

#[derive(Default)]
struct EmptyParams {}
unsafe impl Params for EmptyParams {
    fn param_map(&self) -> Vec<(String, ParamPtr, String)> {
        Vec::new()
    }
}

#[derive(Default)]
struct GuiPlugin;
impl Plugin for GuiPlugin {
    const NAME: &'static str = "GUI visibility fixture";
    const VENDOR: &'static str = "Intrect";
    const URL: &'static str = "https://intrect.io";
    const EMAIL: &'static str = "test@intrect.io";
    const VERSION: &'static str = "0.0.0";
    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[];
    type SysExMessage = ();
    type BackgroundTask = ();
    fn params(&self) -> Arc<dyn Params> {
        Arc::new(EmptyParams::default())
    }
    fn process(
        &mut self,
        _: &mut crate::buffer::Buffer,
        _: &mut AuxiliaryBuffers,
        _: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        ProcessStatus::Normal
    }
}
impl ClapPlugin for GuiPlugin {
    const CLAP_ID: &'static str = "io.intrect.gui-visibility-test";
    const CLAP_DESCRIPTION: Option<&'static str> = None;
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[];
}

struct VisibleEditor;
impl Editor for VisibleEditor {
    fn spawn(&self, _: ParentWindowHandle, _: Arc<dyn GuiContext>) -> Box<dyn Any + Send> {
        Box::new(true)
    }
    fn size(&self) -> (u32, u32) {
        (100, 100)
    }
    fn set_scale_factor(&self, _: f32) -> bool {
        false
    }
    fn param_value_changed(&self, _: &str, _: f32) {}
    fn param_modulation_changed(&self, _: &str, _: f32) {}
    fn param_values_changed(&self) {}
}

struct ControlledEditor;
impl Editor for ControlledEditor {
    fn spawn(
        &self,
        parent: ParentWindowHandle,
        context: Arc<dyn GuiContext>,
    ) -> Box<dyn Any + Send> {
        VisibleEditor.spawn(parent, context)
    }
    fn set_visible(&self, handle: &mut (dyn Any + Send), visible: bool) -> bool {
        let Some(state) = handle.downcast_mut::<bool>() else {
            return false;
        };
        *state = visible;
        true
    }
    fn size(&self) -> (u32, u32) {
        (100, 100)
    }
    fn set_scale_factor(&self, _: f32) -> bool {
        false
    }
    fn param_value_changed(&self, _: &str, _: f32) {}
    fn param_modulation_changed(&self, _: &str, _: f32) {}
    fn param_values_changed(&self) {}
}

#[test]
fn clap_gui_show_accepts_an_already_visible_editor_and_preserves_control_failures() {
    unsafe extern "C" fn no_request(_: *const clap_host) {}
    let host = clap_host {
        clap_version: clap_sys::version::CLAP_VERSION,
        host_data: std::ptr::null_mut(),
        name: c"GUI fixture".as_ptr(),
        vendor: c"Intrect".as_ptr(),
        url: c"https://intrect.io".as_ptr(),
        version: c"0".as_ptr(),
        get_extension: None,
        request_restart: Some(no_request),
        request_process: Some(no_request),
        request_callback: Some(no_request),
    };
    let wrapper = unsafe { Wrapper::<GuiPlugin>::new(&host) };
    let plugin = wrapper.clap_plugin.borrow();
    let gui = &wrapper.clap_plugin_gui;
    let show = || unsafe { (gui.show.unwrap())(&*plugin) };
    let hide = || unsafe { (gui.hide.unwrap())(&*plugin) };
    assert!(!show());
    assert!(!hide());

    *wrapper.editor.borrow_mut() = Some(Mutex::new(Box::new(VisibleEditor)));
    assert!(!show()); // The editor exists, but has not been attached.
    *wrapper.editor_handle.lock() = Some(Box::new(true));
    assert!(show());
    assert!(!hide());
    assert!(show());

    *wrapper.editor.borrow_mut() = Some(Mutex::new(Box::new(ControlledEditor)));
    assert!(hide());
    assert_eq!(
        wrapper
            .editor_handle
            .lock()
            .as_ref()
            .unwrap()
            .downcast_ref::<bool>(),
        Some(&false)
    );
    assert!(show());
    assert_eq!(
        wrapper
            .editor_handle
            .lock()
            .as_ref()
            .unwrap()
            .downcast_ref::<bool>(),
        Some(&true)
    );
    // An override's real error must not be converted to successful show().
    *wrapper.editor_handle.lock() = Some(Box::new(()));
    assert!(!show());
    assert!(!hide());
}
