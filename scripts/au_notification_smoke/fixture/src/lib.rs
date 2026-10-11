// 지연 알림 계약만 검사하는 내부 AU fixture. 오디오 품질 판정용이 아니다.
use nih_plug::prelude::*;
use std::sync::Arc;
#[derive(Params)]
struct Parameters {
    #[id = "bright"]
    bright: FloatParam,
}
impl Default for Parameters {
    fn default() -> Self {
        Self {
            bright: FloatParam::new("Bright", 0.0, FloatRange::Linear { min: 0.0, max: 1.0 }),
        }
    }
}
#[derive(Default)]
struct LatencyFixture {
    params: Arc<Parameters>,
}
impl Plugin for LatencyFixture {
    const NAME: &'static str = "AU latency notification fixture";
    const VENDOR: &'static str = "Intrect QA";
    const URL: &'static str = "https://intrect.io";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = "0.1.0";
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
    fn initialize(
        &mut self,
        _: &AudioIOLayout,
        _: &BufferConfig,
        context: &mut impl InitContext<Self>,
    ) -> bool {
        context.set_latency_samples(9112);
        true
    }
    fn process(
        &mut self,
        _: &mut Buffer,
        _: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        context.set_latency_samples(if self.params.bright.value() > 0.0 {
            28693
        } else {
            9112
        });
        ProcessStatus::Normal
    }
}
impl AuPlugin for LatencyFixture {
    const AU_TYPE: [u8; 4] = *b"aufx";
    const AU_SUBTYPE: [u8; 4] = *b"dlqn";
    const AU_MANUFACTURER: [u8; 4] = *b"INRT";
}
nih_export_au!(LatencyFixture);
