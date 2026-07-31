//! Special handling for note expressions, because VST3 makes this a lot more complicated than it
//! needs to be. We only support the predefined expressions.

use vst3_sys::vst::{NoteExpressionValueEvent, NoteOffEvent, NoteOnEvent};

use crate::prelude::{NoteEvent, SysExMessage};

type MidiNote = u8;
type MidiChannel = u8;
type NoteId = i32;

/// A note ID registered by a note on event, along with the note and channel it maps back to.
#[derive(Clone, Copy, Debug)]
struct NoteMapping {
    note_id: NoteId,
    note: MidiNote,
    channel: MidiChannel,
    /// Set on note off. Released notes still resolve, because hosts may keep sending expressions
    /// during a note's release phase, but their slots are the first ones reused.
    released: bool,
}

/// How many note ID mappings we can track at once. Released mappings are reused before any live one
/// is overwritten, so this bounds the number of simultaneously sounding notes rather than the number
/// of notes played in a session.
const NOTE_IDS_LEN: usize = 128;

/// `kVolumeTypeID`
pub const VOLUME_EXPRESSION_ID: u32 = 0;
/// `kPanTypeId`
pub const PAN_EXPRESSION_ID: u32 = 1;
/// `kTuningTypeID`
pub const TUNING_EXPRESSION_ID: u32 = 2;
/// `kVibratoTypeID`
pub const VIBRATO_EXPRESSION_ID: u32 = 3;
/// `kExpressionTypeID`
pub const EXPRESSION_EXPRESSION_ID: u32 = 4;
/// `kBrightnessTypeID`
pub const BRIGHTNESS_EXPRESSION_ID: u32 = 5;

/// The note expressions we support. It's completely undocumented, but apparently VST3 plugins need
/// to specifically define a custom note expression for the predefined note expressions for them to
/// work.
pub const KNOWN_NOTE_EXPRESSIONS: [NoteExpressionInfo; 6] = [
    NoteExpressionInfo {
        type_id: VOLUME_EXPRESSION_ID,
        title: "Volume",
        unit: "dB",
    },
    NoteExpressionInfo {
        type_id: PAN_EXPRESSION_ID,
        title: "Pan",
        unit: "",
    },
    NoteExpressionInfo {
        type_id: TUNING_EXPRESSION_ID,
        title: "Tuning",
        unit: "semitones",
    },
    NoteExpressionInfo {
        type_id: VIBRATO_EXPRESSION_ID,
        title: "Vibrato",
        unit: "",
    },
    NoteExpressionInfo {
        type_id: EXPRESSION_EXPRESSION_ID,
        title: "Expression",
        unit: "",
    },
    NoteExpressionInfo {
        type_id: BRIGHTNESS_EXPRESSION_ID,
        title: "Brightness",
        unit: "",
    },
];

/// VST3 has predefined note expressions just like CLAP, but unlike the other note events these
/// expressions are identified only with a note ID. To account for that, we'll keep track of the
/// most recent note IDs we've encountered so we can later map those IDs back to a note and channel
/// combination.
#[derive(Debug)]
pub struct NoteExpressionController {
    /// The note IDs that are currently sounding. We'll do a linear search every time we receive a
    /// note expression value event to find the matching note and channel.
    ///
    /// `None` marks a free slot. Distinguishing free slots from used ones matters: with a plain
    /// zero initialized array, an expression for note ID 0 would match an unused entry and be
    /// attributed to note 0 on channel 0.
    note_ids: [Option<NoteMapping>; NOTE_IDS_LEN],
    /// The index in the `note_ids` ring buffer the next event should be inserted at when every slot
    /// is occupied, wraps back around to 0 when reaching the end.
    note_ids_idx: usize,
}

impl Default for NoteExpressionController {
    fn default() -> Self {
        Self {
            note_ids: [None; NOTE_IDS_LEN],
            note_ids_idx: 0,
        }
    }
}

/// This is used to register a (predefined) note expression in the `INoteExpressionController`. The
/// data is kept in this module to keep everything related to VST3 note expressions in one place.
///
/// This does not contain value descriptions because those are also predefined as normalized `[0,
/// 1]` values.
pub struct NoteExpressionInfo {
    /// The predefined VST3 note expression type ID for this note expression.
    pub type_id: u32,
    /// The title for the note expression. Also used for the short title because why not.
    pub title: &'static str,
    /// The unit for the note expression.
    pub unit: &'static str,
}

impl NoteExpressionController {
    /// Register the note ID from a note on event so it can later be retrieved when handling a note
    /// expression value event.
    pub fn register_note(&mut self, event: &NoteOnEvent) {
        let mapping = NoteMapping {
            note_id: event.note_id,
            note: event.pitch as u8,
            channel: event.channel as u8,
            released: false,
        };

        // Reuse this note ID's own slot if the host restarted the note without a note off in
        // between, then a never used slot, then one belonging to an already released note. Only
        // when every slot holds a sounding note does this fall back to overwriting the oldest
        // entry, which is what loses expressions for a note that is still playing.
        let slot_idx = self
            .note_ids
            .iter()
            .position(|slot| matches!(slot, Some(m) if m.note_id == event.note_id))
            .or_else(|| self.note_ids.iter().position(Option::is_none))
            .or_else(|| {
                self.note_ids
                    .iter()
                    .position(|slot| matches!(slot, Some(m) if m.released))
            });

        match slot_idx {
            Some(slot_idx) => self.note_ids[slot_idx] = Some(mapping),
            None => {
                nih_debug_assert_failure!(
                    "More than {} notes are sounding, note expressions for the oldest note will \
                     be lost",
                    NOTE_IDS_LEN
                );

                self.note_ids[self.note_ids_idx] = Some(mapping);
                self.note_ids_idx = (self.note_ids_idx + 1) % NOTE_IDS_LEN;
            }
        }
    }

    /// Mark a note ID as released so its slot can be reused.
    ///
    /// The mapping is deliberately kept resolvable: hosts may keep sending expressions during a
    /// note's release phase, and dropping them here would silently lose that modulation.
    pub fn unregister_note(&mut self, event: &NoteOffEvent) {
        if let Some(mapping) = self
            .note_ids
            .iter_mut()
            .flatten()
            .find(|mapping| mapping.note_id == event.note_id)
        {
            mapping.released = true;
        }
    }

    /// Translate the note expression value event into an internal NIH-plug event, if we handle the
    /// expression type from the note expression value event. The timing is provided here because we
    /// may be splitting buffers on inter-buffer parameter changes.
    pub fn translate_event<S: SysExMessage>(
        &self,
        timing: u32,
        event: &NoteExpressionValueEvent,
    ) -> Option<NoteEvent<S>> {
        // We're calling it a voice ID, VST3 (and CLAP) calls it a note ID
        let NoteMapping {
            note_id,
            note,
            channel,
            ..
        } = *self
            .note_ids
            .iter()
            .flatten()
            .find(|mapping| mapping.note_id == event.note_id)?;

        match event.type_id {
            VOLUME_EXPRESSION_ID => Some(NoteEvent::PolyVolume {
                timing,
                voice_id: Some(note_id),
                channel,
                note,
                // Because expression values in VST3 are always in the `[0, 1]` range, they added a
                // 4x scaling factor here to allow the values to go from -infinity to +12 dB
                gain: event.value as f32 * 4.0,
            }),
            PAN_EXPRESSION_ID => Some(NoteEvent::PolyPan {
                timing,
                voice_id: Some(note_id),
                channel,
                note,
                // Our panning expressions are symmetrical around 0
                pan: (event.value as f32 * 2.0) - 1.0,
            }),
            TUNING_EXPRESSION_ID => Some(NoteEvent::PolyTuning {
                timing,
                voice_id: Some(note_id),
                channel,
                note,
                // This denormalized to the same [-120, 120] range used by CLAP and our expression
                // events
                tuning: 240.0 * (event.value as f32 - 0.5),
            }),
            VIBRATO_EXPRESSION_ID => Some(NoteEvent::PolyVibrato {
                timing,
                voice_id: Some(note_id),
                channel,
                note,
                vibrato: event.value as f32,
            }),
            EXPRESSION_EXPRESSION_ID => Some(NoteEvent::PolyBrightness {
                timing,
                voice_id: Some(note_id),
                channel,
                note,
                brightness: event.value as f32,
            }),
            BRIGHTNESS_EXPRESSION_ID => Some(NoteEvent::PolyExpression {
                timing,
                voice_id: Some(note_id),
                channel,
                note,
                expression: event.value as f32,
            }),
            _ => None,
        }
    }

    /// Translate a NIH-plug note expression event a VST3 `NoteExpressionValueEvent`. Will return
    /// `None` if the event is not a polyphonic expression event, i.e. one of the events handled by
    /// `translate_event()`.
    pub fn translate_event_reverse(
        note_id: i32,
        event: &NoteEvent<impl SysExMessage>,
    ) -> Option<NoteExpressionValueEvent> {
        match &event {
            NoteEvent::PolyVolume { gain, .. } => Some(NoteExpressionValueEvent {
                type_id: VOLUME_EXPRESSION_ID,
                note_id,
                value: *gain as f64 / 4.0,
            }),
            NoteEvent::PolyPan { pan, .. } => Some(NoteExpressionValueEvent {
                type_id: PAN_EXPRESSION_ID,
                note_id,
                value: (*pan as f64 + 1.0) / 2.0,
            }),
            NoteEvent::PolyTuning { tuning, .. } => Some(NoteExpressionValueEvent {
                type_id: TUNING_EXPRESSION_ID,
                note_id,
                value: (*tuning as f64 / 240.0) + 0.5,
            }),
            NoteEvent::PolyVibrato { vibrato, .. } => Some(NoteExpressionValueEvent {
                type_id: VIBRATO_EXPRESSION_ID,
                note_id,
                value: *vibrato as f64,
            }),
            NoteEvent::PolyExpression { expression, .. } => Some(NoteExpressionValueEvent {
                type_id: EXPRESSION_EXPRESSION_ID,
                note_id,
                value: *expression as f64,
            }),
            NoteEvent::PolyBrightness { brightness, .. } => Some(NoteExpressionValueEvent {
                type_id: BRIGHTNESS_EXPRESSION_ID,
                note_id,
                value: *brightness as f64,
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note_on(note_id: NoteId, pitch: i16, channel: i16) -> NoteOnEvent {
        NoteOnEvent {
            note_id,
            pitch,
            channel,
            ..Default::default()
        }
    }

    fn note_off(note_id: NoteId) -> NoteOffEvent {
        NoteOffEvent {
            note_id,
            ..Default::default()
        }
    }

    /// Resolve a note ID through a volume expression, returning the note and channel it mapped to.
    fn resolve(controller: &NoteExpressionController, note_id: NoteId) -> Option<(MidiNote, MidiChannel)> {
        let event = NoteExpressionValueEvent {
            type_id: VOLUME_EXPRESSION_ID,
            note_id,
            value: 0.25,
        };

        match controller.translate_event::<()>(0, &event)? {
            NoteEvent::PolyVolume { note, channel, .. } => Some((note, channel)),
            event => panic!("Unexpected event: {event:?}"),
        }
    }

    /// The zero initialized entries used to make note ID 0 resolve to note 0 on channel 0 before any
    /// note was registered.
    #[test]
    fn unregistered_note_ids_do_not_resolve() {
        let controller = NoteExpressionController::default();

        assert_eq!(resolve(&controller, 0), None);
        assert_eq!(resolve(&controller, 42), None);
    }

    /// Released notes must keep resolving: hosts may send expressions during the release phase, and
    /// dropping them at note off would silently lose that modulation.
    #[test]
    fn released_notes_still_resolve() {
        let mut controller = NoteExpressionController::default();
        controller.register_note(&note_on(7, 60, 3));

        assert_eq!(resolve(&controller, 7), Some((60, 3)));

        controller.unregister_note(&note_off(7));
        assert_eq!(resolve(&controller, 7), Some((60, 3)));
    }

    /// Every note that fits must keep its mapping. The old 32 entry overwrite-only ring silently
    /// dropped expressions for older notes well before that.
    #[test]
    fn all_simultaneous_notes_keep_their_mapping() {
        let mut controller = NoteExpressionController::default();
        for note_id in 0..NOTE_IDS_LEN as NoteId {
            controller.register_note(&note_on(note_id, (note_id % 128) as i16, 0));
        }

        for note_id in 0..NOTE_IDS_LEN as NoteId {
            assert_eq!(
                resolve(&controller, note_id),
                Some(((note_id % 128) as MidiNote, 0)),
                "note ID {note_id}"
            );
        }
    }

    /// Released slots are recycled before any sounding note is overwritten, so a long session never
    /// falls back to the overwrite-only ring.
    #[test]
    fn released_slots_are_reused_before_sounding_ones() {
        let mut controller = NoteExpressionController::default();

        // Hold one note for the whole run. It must never lose its mapping to the churn below.
        const HELD: NoteId = 9999;
        controller.register_note(&note_on(HELD, 24, 2));

        for note_id in 0..(NOTE_IDS_LEN as NoteId * 4) {
            controller.register_note(&note_on(note_id, 60, 0));
            assert_eq!(
                resolve(&controller, note_id),
                Some((60, 0)),
                "note ID {note_id}"
            );
            controller.unregister_note(&note_off(note_id));

            assert_eq!(resolve(&controller, HELD), Some((24, 2)), "note ID {note_id}");
        }
    }

    #[test]
    fn restarted_note_ids_reuse_their_slot() {
        let mut controller = NoteExpressionController::default();
        controller.register_note(&note_on(1, 60, 0));
        controller.register_note(&note_on(1, 72, 1));

        // The second note on must overwrite the first rather than adding a duplicate entry
        assert_eq!(resolve(&controller, 1), Some((72, 1)));

        // Occupy every remaining slot with a sounding note, then release note 1 and register one
        // more. Its slot is the only one that may be taken, which only holds if note ID 1 never
        // had a second entry.
        for note_id in 2..=NOTE_IDS_LEN as NoteId {
            controller.register_note(&note_on(note_id, 60, 0));
        }
        controller.unregister_note(&note_off(1));
        controller.register_note(&note_on(1000, 36, 0));

        assert_eq!(resolve(&controller, 1), None);
        assert_eq!(resolve(&controller, 1000), Some((36, 0)));
    }
}
