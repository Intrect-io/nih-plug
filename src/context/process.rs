//! A context passed during the process function.

use super::PluginApi;
use crate::prelude::{Plugin, PluginNoteEvent};

/// Contains both context data and callbacks the plugin can use during processing. Most notably this
/// is how a plugin sends and receives note events, gets transport information, and accesses
/// sidechain inputs and auxiliary outputs. This is passed to the plugin during as part of
/// [`Plugin::process()`][crate::plugin::Plugin::process()].
//
// # Safety
//
// The implementing wrapper needs to be able to handle concurrent requests, and it should perform
// the actual callback within [MainThreadQueue::schedule_gui].
pub trait ProcessContext<P: Plugin> {
    /// Get the current plugin API.
    fn plugin_api(&self) -> PluginApi;

    /// Execute a task on a background thread using `[Plugin::task_executor]`. This allows you to
    /// defer expensive tasks for later without blocking either the process function or the GUI
    /// thread. As long as creating the `task` is realtime-safe, this operation is too.
    ///
    /// # Note
    ///
    /// Scheduling the same task multiple times will cause those duplicate tasks to pile up. Try to
    /// either prevent this from happening, or check whether the task still needs to be completed in
    /// your task executor.
    fn execute_background(&self, task: P::BackgroundTask);

    /// Execute a task on a background thread using `[Plugin::task_executor]`. As long as creating
    /// the `task` is realtime-safe, this operation is too.
    ///
    /// # Note
    ///
    /// Scheduling the same task multiple times will cause those duplicate tasks to pile up. Try to
    /// either prevent this from happening, or check whether the task still needs to be completed in
    /// your task executor.
    fn execute_gui(&self, task: P::BackgroundTask);

    /// Get information about the current transport position and status.
    fn transport(&self) -> &Transport;

    /// Returns the next note event, if there is one. Use
    /// [`NoteEvent::timing()`][crate::prelude::NoteEvent::timing()] to get the event's timing
    /// within the buffer. Only available when
    /// [`Plugin::MIDI_INPUT`][crate::prelude::Plugin::MIDI_INPUT] is set.
    ///
    /// # Usage
    ///
    /// You will likely want to use this with a loop, since there may be zero, one, or more events
    /// for a sample:
    ///
    /// ```ignore
    /// let mut next_event = context.next_event();
    /// for (sample_id, channel_samples) in buffer.iter_samples().enumerate() {
    ///     while let Some(event) = next_event {
    ///         if event.timing() != sample_id as u32 {
    ///             break;
    ///         }
    ///
    ///         match event {
    ///             NoteEvent::NoteOn { note, velocity, .. } => { ... },
    ///             NoteEvent::NoteOff { note, .. } if note == 69 => { ... },
    ///             NoteEvent::PolyPressure { note, pressure, .. } { ... },
    ///             _ => (),
    ///         }
    ///
    ///         next_event = context.next_event();
    ///     }
    ///
    ///     // Do something with `channel_samples`...
    /// }
    ///
    /// ProcessStatus::Normal
    /// ```
    fn next_event(&mut self) -> Option<PluginNoteEvent<P>>;

    /// Send an event to the host. Only available when
    /// [`Plugin::MIDI_OUTPUT`][crate::prelude::Plugin::MIDI_OUTPUT] is set. Will not do anything
    /// otherwise.
    fn send_event(&mut self, event: PluginNoteEvent<P>);

    /// Update the current latency of the plugin. If the plugin is currently processing audio, then
    /// this may cause audio playback to be restarted.
    fn set_latency_samples(&self, samples: u32);

    /// Set the current voice **capacity** for this plugin (so not the number of currently active
    /// voices). This may only be called if
    /// [`ClapPlugin::CLAP_POLY_MODULATION_CONFIG`][crate::prelude::ClapPlugin::CLAP_POLY_MODULATION_CONFIG]
    /// is set. `capacity` must be between 1 and the configured maximum capacity. Changing this at
    /// runtime allows the host to better optimize polyphonic modulation, or to switch to strictly
    /// monophonic modulation when dropping the capacity down to 1.
    fn set_current_voice_capacity(&self, capacity: u32);

    // TODO: Add this, this works similar to [GuiContext::set_parameter] but it adds the parameter
    //       change to a queue (or directly to the VST3 plugin's parameter output queues) instead of
    //       using main thread host automation (and all the locks involved there).
    // fn set_parameter<P: Param>(&self, param: &P, value: P::Plain);
}

/// Information about the plugin's transport. Depending on the plugin API and the host not all
/// fields may be available.
#[derive(Debug)]
pub struct Transport {
    /// Whether the transport is currently running.
    pub playing: bool,
    /// Whether recording is enabled in the project.
    pub recording: bool,
    /// Whether the pre-roll is currently active, if the plugin API reports this information.
    pub preroll_active: Option<bool>,

    /// The sample rate in Hertz. Also passed in
    /// [`Plugin::initialize()`][crate::prelude::Plugin::initialize()], so if you need this then you
    /// can also store that value.
    pub sample_rate: f32,
    /// The project's tempo in beats per minute.
    pub tempo: Option<f64>,
    /// The time signature's numerator.
    pub time_sig_numerator: Option<i32>,
    /// The time signature's denominator.
    pub time_sig_denominator: Option<i32>,

    // XXX: VST3 also has a continuous time in samples that ignores loops, but we can't reconstruct
    //      something similar in CLAP so it may be best to just ignore that so you can't rely on it
    /// The position in the song in samples. Can be used to calculate the time in seconds if needed.
    pub(crate) pos_samples: Option<i64>,
    /// The position in the song in seconds. Can be used to calculate the time in samples if needed.
    pub(crate) pos_seconds: Option<f64>,
    /// The position in the song in quarter notes. Can be calculated from the time in seconds and
    /// the tempo if needed.
    pub(crate) pos_beats: Option<f64>,
    /// The last bar's start position in beats. Can be calculated from the beat position and time
    /// signature if needed.
    pub(crate) bar_start_pos_beats: Option<f64>,
    /// The number of the bar at `bar_start_pos_beats`. This starts at 0 for the very first bar at
    /// the start of the song. Can be calculated from the beat position and time signature if
    /// needed.
    pub(crate) bar_number: Option<i32>,

    /// The loop range in samples, if the loop is active and this information is available. None of
    /// the plugin API docs mention whether this is exclusive or inclusive, but just assume that the
    /// end is exclusive. Can be calculated from the other loop range information if needed.
    pub(crate) loop_range_samples: Option<(i64, i64)>,
    /// The loop range in seconds, if the loop is active and this information is available. None of
    /// the plugin API docs mention whether this is exclusive or inclusive, but just assume that the
    /// end is exclusive. Can be calculated from the other loop range information if needed.
    pub(crate) loop_range_seconds: Option<(f64, f64)>,
    /// The loop range in quarter notes, if the loop is active and this information is available.
    /// None of the plugin API docs mention whether this is exclusive or inclusive, but just assume
    /// that the end is exclusive. Can be calculated from the other loop range information if
    /// needed.
    pub(crate) loop_range_beats: Option<(f64, f64)>,
}

impl Transport {
    /// Initialize the transport struct without any information.
    pub(crate) fn new(sample_rate: f32) -> Self {
        Self {
            playing: false,
            recording: false,
            preroll_active: None,

            sample_rate,
            tempo: None,
            time_sig_numerator: None,
            time_sig_denominator: None,

            pos_samples: None,
            pos_seconds: None,
            pos_beats: None,
            bar_start_pos_beats: None,
            bar_number: None,

            loop_range_samples: None,
            loop_range_seconds: None,
            loop_range_beats: None,
        }
    }

    /// The sample rate as an `f64`, but only if the host reported a value that can be divided by or
    /// multiplied with. Hosts have been known to report zero before the plugin is activated.
    #[inline]
    fn valid_sample_rate(&self) -> Option<f64> {
        let sample_rate = self.sample_rate as f64;
        if sample_rate.is_finite() && sample_rate > 0.0 {
            Some(sample_rate)
        } else {
            None
        }
    }

    /// The tempo, but only if the host reported a value that can be divided by.
    #[inline]
    fn valid_tempo(&self) -> Option<f64> {
        self.tempo
            .filter(|tempo| tempo.is_finite() && *tempo > 0.0)
    }

    /// The length of a bar in quarter notes, but only if the host reported a usable time signature.
    #[inline]
    fn quarter_note_bar_length(&self) -> Option<f64> {
        let time_sig_numerator = self.time_sig_numerator?;
        let time_sig_denominator = self.time_sig_denominator?;
        if time_sig_numerator <= 0 || time_sig_denominator <= 0 {
            return None;
        }

        let bar_length = time_sig_numerator as f64 / time_sig_denominator as f64 * 4.0;
        if bar_length.is_finite() && bar_length > 0.0 {
            Some(bar_length)
        } else {
            None
        }
    }

    /// The position in the song in samples. Will be calculated from other information if needed.
    /// Returns `None` if the host didn't report the information needed for the conversion, or if it
    /// reported values that would result in a nonsensical position.
    pub fn pos_samples(&self) -> Option<i64> {
        if let Some(pos_samples) = self.pos_samples {
            return Some(pos_samples);
        }

        // Both remaining conversions need a usable sample rate
        let sample_rate = self.valid_sample_rate()?;
        if let Some(pos_seconds) = self.pos_seconds {
            return f64_to_i64((pos_seconds * sample_rate).round());
        }
        if let (Some(pos_beats), Some(tempo)) = (self.pos_beats, self.valid_tempo()) {
            return f64_to_i64((pos_beats / tempo * 60.0 * sample_rate).round());
        }

        None
    }

    /// The position in the song in seconds. Can be used to calculate the time in samples if needed.
    /// Returns `None` if the host didn't report the information needed for the conversion, or if it
    /// reported values that would result in a nonsensical position.
    pub fn pos_seconds(&self) -> Option<f64> {
        if let Some(pos_seconds) = self.pos_seconds {
            return Some(pos_seconds);
        }
        if let (Some(pos_samples), Some(sample_rate)) = (self.pos_samples, self.valid_sample_rate())
        {
            return Some(pos_samples as f64 / sample_rate);
        }
        if let (Some(pos_beats), Some(tempo)) = (self.pos_beats, self.valid_tempo()) {
            return Some(pos_beats / tempo * 60.0);
        }

        None
    }

    /// The position in the song in quarter notes. Will be calculated from other information if
    /// needed. Returns `None` if the host didn't report the information needed for the conversion,
    /// or if it reported values that would result in a nonsensical position.
    pub fn pos_beats(&self) -> Option<f64> {
        if let Some(pos_beats) = self.pos_beats {
            return Some(pos_beats);
        }

        // Both remaining conversions need a usable tempo
        let tempo = self.valid_tempo()?;
        if let Some(pos_seconds) = self.pos_seconds {
            return Some(pos_seconds / 60.0 * tempo);
        }
        if let (Some(pos_samples), Some(sample_rate)) = (self.pos_samples, self.valid_sample_rate())
        {
            return Some(pos_samples as f64 / sample_rate / 60.0 * tempo);
        }

        None
    }

    /// The last bar's start position in beats. Will be calculated from other information if needed.
    /// Returns `None` if the host reported an invalid time signature.
    pub fn bar_start_pos_beats(&self) -> Option<f64> {
        if self.bar_start_pos_beats.is_some() {
            return self.bar_start_pos_beats;
        }

        let quarter_note_bar_length = self.quarter_note_bar_length()?;
        let pos_beats = self.pos_beats()?;

        Some((pos_beats / quarter_note_bar_length).floor() * quarter_note_bar_length)
    }

    /// The number of the bar at `bar_start_pos_beats`. This starts at 0 for the very first bar at
    /// the start of the song. Will be calculated from other information if needed. Returns `None` if
    /// the host reported an invalid time signature.
    pub fn bar_number(&self) -> Option<i32> {
        if self.bar_number.is_some() {
            return self.bar_number;
        }

        let quarter_note_bar_length = self.quarter_note_bar_length()?;
        let pos_beats = self.pos_beats()?;

        f64_to_i32((pos_beats / quarter_note_bar_length).floor())
    }

    /// The loop range in samples, if the loop is active and this information is available. None of
    /// the plugin API docs mention whether this is exclusive or inclusive, but just assume that the
    /// end is exclusive. Will be calculated from other information if needed.
    pub fn loop_range_samples(&self) -> Option<(i64, i64)> {
        if let Some(loop_range_samples) = self.loop_range_samples {
            return Some(loop_range_samples);
        }

        // Both remaining conversions need a usable sample rate
        let sample_rate = self.valid_sample_rate()?;
        if let Some((start_seconds, end_seconds)) = self.loop_range_seconds {
            return Some((
                f64_to_i64((start_seconds * sample_rate).round())?,
                f64_to_i64((end_seconds * sample_rate).round())?,
            ));
        }
        if let (Some((start_beats, end_beats)), Some(tempo)) =
            (self.loop_range_beats, self.valid_tempo())
        {
            return Some((
                f64_to_i64((start_beats / tempo * 60.0 * sample_rate).round())?,
                f64_to_i64((end_beats / tempo * 60.0 * sample_rate).round())?,
            ));
        }

        None
    }

    /// The loop range in seconds, if the loop is active and this information is available. None of
    /// the plugin API docs mention whether this is exclusive or inclusive, but just assume that the
    /// end is exclusive. Will be calculated from other information if needed.
    pub fn loop_range_seconds(&self) -> Option<(f64, f64)> {
        if let Some(loop_range_seconds) = self.loop_range_seconds {
            return Some(loop_range_seconds);
        }
        if let (Some((start_samples, end_samples)), Some(sample_rate)) =
            (self.loop_range_samples, self.valid_sample_rate())
        {
            return Some((
                start_samples as f64 / sample_rate,
                end_samples as f64 / sample_rate,
            ));
        }
        if let (Some((start_beats, end_beats)), Some(tempo)) =
            (self.loop_range_beats, self.valid_tempo())
        {
            return Some((start_beats / tempo * 60.0, end_beats / tempo * 60.0));
        }

        None
    }

    /// The loop range in quarter notes, if the loop is active and this information is available.
    /// None of the plugin API docs mention whether this is exclusive or inclusive, but just assume
    /// that the end is exclusive. Will be calculated from other information if needed.
    pub fn loop_range_beats(&self) -> Option<(f64, f64)> {
        if let Some(loop_range_beats) = self.loop_range_beats {
            return Some(loop_range_beats);
        }

        // Both remaining conversions need a usable tempo
        let tempo = self.valid_tempo()?;
        if let Some((start_seconds, end_seconds)) = self.loop_range_seconds {
            return Some((start_seconds / 60.0 * tempo, end_seconds / 60.0 * tempo));
        }
        if let (Some((start_samples, end_samples)), Some(sample_rate)) =
            (self.loop_range_samples, self.valid_sample_rate())
        {
            return Some((
                start_samples as f64 / sample_rate / 60.0 * tempo,
                end_samples as f64 / sample_rate / 60.0 * tempo,
            ));
        }

        None
    }
}

/// Convert a computed position to an `i64`. `as` casts saturate at the integer's bounds and map NaN
/// to zero, which would turn invalid host data into a plausible looking position.
#[inline]
fn f64_to_i64(value: f64) -> Option<i64> {
    // `i64::MAX as f64` rounds up to 2^63, which is not representable as an `i64`, so the upper
    // bound has to be exclusive. `i64::MIN` is exactly representable.
    if value.is_finite() && value >= i64::MIN as f64 && value < i64::MAX as f64 {
        Some(value as i64)
    } else {
        None
    }
}

/// See [`f64_to_i64()`].
#[inline]
fn f64_to_i32(value: f64) -> Option<i32> {
    if value.is_finite() && value >= i32::MIN as f64 && value <= i32::MAX as f64 {
        Some(value as i32)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RATE: f32 = 48_000.0;

    fn new_transport() -> Transport {
        Transport::new(SAMPLE_RATE)
    }

    #[test]
    fn f64_to_i64_rejects_unrepresentable_values() {
        assert_eq!(f64_to_i64(0.0), Some(0));
        assert_eq!(f64_to_i64(-1.0), Some(-1));
        assert_eq!(f64_to_i64(i64::MIN as f64), Some(i64::MIN));

        // `i64::MAX as f64` rounds up to 2^63, so it is not a valid `i64` and saturating on it is
        // exactly the fabricated position this helper exists to prevent
        assert_eq!(f64_to_i64(i64::MAX as f64), None);
        assert_eq!(f64_to_i64(f64::NAN), None);
        assert_eq!(f64_to_i64(f64::INFINITY), None);
        assert_eq!(f64_to_i64(f64::NEG_INFINITY), None);
    }

    #[test]
    fn f64_to_i32_rejects_unrepresentable_values() {
        assert_eq!(f64_to_i32(0.0), Some(0));
        assert_eq!(f64_to_i32(i32::MIN as f64), Some(i32::MIN));
        assert_eq!(f64_to_i32(i32::MAX as f64), Some(i32::MAX));

        assert_eq!(f64_to_i32(i32::MAX as f64 + 1.0), None);
        assert_eq!(f64_to_i32(f64::NAN), None);
    }

    /// The reported values take precedence over anything derived from them.
    #[test]
    fn reported_positions_win_over_derived_ones() {
        let mut transport = new_transport();
        transport.pos_samples = Some(1234);
        transport.pos_seconds = Some(99.0);
        transport.pos_beats = Some(7.0);
        transport.tempo = Some(120.0);

        assert_eq!(transport.pos_samples(), Some(1234));
        assert_eq!(transport.pos_seconds(), Some(99.0));
        assert_eq!(transport.pos_beats(), Some(7.0));
    }

    /// Each position can be derived from the others, in a fixed order of preference.
    #[test]
    fn positions_are_derived_in_order() {
        let mut transport = new_transport();
        transport.pos_seconds = Some(2.0);
        transport.tempo = Some(120.0);

        assert_eq!(transport.pos_samples(), Some(96_000));
        // 2 seconds at 120 BPM is 4 quarter notes
        assert_eq!(transport.pos_beats(), Some(4.0));

        let mut transport = new_transport();
        transport.pos_beats = Some(4.0);
        transport.tempo = Some(120.0);

        assert_eq!(transport.pos_seconds(), Some(2.0));
        assert_eq!(transport.pos_samples(), Some(96_000));
    }

    /// A host that reports a nonsensical sample rate or tempo must not produce a fabricated
    /// position. The conversions used to divide by them unconditionally.
    #[test]
    fn invalid_sample_rates_and_tempos_do_not_produce_positions() {
        for sample_rate in [0.0, -48_000.0, f32::NAN, f32::INFINITY] {
            let mut transport = Transport::new(sample_rate);
            transport.pos_seconds = Some(2.0);

            assert_eq!(transport.pos_samples(), None, "sample rate {sample_rate}");
        }

        for tempo in [0.0, -120.0, f64::NAN, f64::INFINITY] {
            let mut transport = new_transport();
            transport.pos_seconds = Some(2.0);
            transport.tempo = Some(tempo);

            assert_eq!(transport.pos_beats(), None, "tempo {tempo}");
        }
    }

    /// Deriving a bar position divides by the time signature, which the host may not have filled in
    /// sensibly.
    #[test]
    fn invalid_time_signatures_do_not_produce_bar_positions() {
        for (numerator, denominator) in [(0, 4), (4, 0), (-4, 4), (4, -4), (0, 0)] {
            let mut transport = new_transport();
            transport.pos_beats = Some(9.0);
            transport.time_sig_numerator = Some(numerator);
            transport.time_sig_denominator = Some(denominator);

            assert_eq!(
                transport.bar_start_pos_beats(),
                None,
                "time signature {numerator}/{denominator}"
            );
            assert_eq!(
                transport.bar_number(),
                None,
                "time signature {numerator}/{denominator}"
            );
        }
    }

    #[test]
    fn bar_positions_are_derived_from_the_time_signature() {
        let mut transport = new_transport();
        transport.pos_beats = Some(9.0);
        transport.time_sig_numerator = Some(4);
        transport.time_sig_denominator = Some(4);

        // A 4/4 bar is four quarter notes, so beat 9 sits in the third bar
        assert_eq!(transport.bar_start_pos_beats(), Some(8.0));
        assert_eq!(transport.bar_number(), Some(2));
    }

    #[test]
    fn loop_ranges_are_derived_like_positions() {
        let mut transport = new_transport();
        transport.loop_range_seconds = Some((1.0, 3.0));
        transport.tempo = Some(120.0);

        assert_eq!(transport.loop_range_samples(), Some((48_000, 144_000)));
        assert_eq!(transport.loop_range_beats(), Some((2.0, 6.0)));

        let mut transport = Transport::new(0.0);
        transport.loop_range_seconds = Some((1.0, 3.0));
        assert_eq!(transport.loop_range_samples(), None);
    }
}
