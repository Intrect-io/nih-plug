//! AUv2 MIDI input/output plumbing.
//!
//! `au-sys` 0.1.1 does not expose the MusicDevice selectors or the legacy
//! MIDI-output callback structs, so the small C ABI surface needed by the AU
//! wrapper is mirrored here from the macOS SDK headers.

use std::borrow::Borrow;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

use au_sys as au;
use crossbeam::queue::ArrayQueue;

use crate::midi::{MidiResult, NoteEvent};
use crate::plugin::Plugin;
use crate::prelude::{MidiConfig, PluginNoteEvent};

pub(super) const MUSIC_DEVICE_MIDI_EVENT_SELECT: au::SInt16 = 0x0101;
pub(super) const MUSIC_DEVICE_SYS_EX_SELECT: au::SInt16 = 0x0102;
pub(super) const MUSIC_DEVICE_START_NOTE_SELECT: au::SInt16 = 0x0105;
pub(super) const MUSIC_DEVICE_STOP_NOTE_SELECT: au::SInt16 = 0x0106;
pub(super) const MUSIC_DEVICE_PROPERTY_SUPPORTS_START_STOP_NOTE: au::AudioUnitPropertyID = 1014;

const MUSIC_DEVICE_SAMPLE_FRAME_MASK: u32 = 0x00ff_ffff;
const MIDI_EVENT_QUEUE_CAPACITY: usize = 1024;
const MIDI_RENDER_EVENT_CAPACITY: usize = MIDI_EVENT_QUEUE_CAPACITY * 2;
const ACTIVE_NOTE_CAPACITY: usize = 1024;
const STOPPING_NOTE_BIT: u64 = 1 << 63;
const MIDI_PACKET_LIST_BYTES: usize = 65_536;
const MIDI_PACKET_CHUNK_BYTES: usize = 60_000;

pub(super) type MusicDeviceMidiEventProc = unsafe extern "C" fn(
    *mut c_void,
    au::UInt32,
    au::UInt32,
    au::UInt32,
    au::UInt32,
) -> au::OSStatus;
pub(super) type MusicDeviceSysExProc =
    unsafe extern "C" fn(*mut c_void, *const u8, au::UInt32) -> au::OSStatus;
pub(super) type MusicDeviceStartNoteProc = unsafe extern "C" fn(
    *mut c_void,
    au::UInt32,
    au::UInt32,
    *mut au::UInt32,
    au::UInt32,
    *const MusicDeviceNoteParams,
) -> au::OSStatus;
pub(super) type MusicDeviceStopNoteProc =
    unsafe extern "C" fn(*mut c_void, au::UInt32, au::UInt32, au::UInt32) -> au::OSStatus;

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct NoteParamsControlValue {
    pub id: au::AudioUnitParameterID,
    pub value: au::AudioUnitParameterValue,
}

/// Variable-length in C. The wrapper only consumes the required pitch and
/// velocity prefix, so one trailing control is enough to mirror its ABI.
#[repr(C)]
pub(super) struct MusicDeviceNoteParams {
    pub arg_count: au::UInt32,
    pub pitch: au::Float32,
    pub velocity: au::Float32,
    pub controls: [NoteParamsControlValue; 1],
}

pub(super) type AuMidiOutputCallback = unsafe extern "C" fn(
    *mut c_void,
    *const au::AudioTimeStamp,
    au::UInt32,
    *const c_void,
) -> au::OSStatus;

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct AuMidiOutputCallbackStruct {
    pub callback: Option<AuMidiOutputCallback>,
    pub user_data: *mut c_void,
}

// The host owns `user_data`; the wrapper only forwards it to the callback.
unsafe impl Send for AuMidiOutputCallbackStruct {}
unsafe impl Sync for AuMidiOutputCallbackStruct {}

/// Lock-free render-side storage for the host's MIDI output callback.
///
/// Property writes happen on a control thread and may allocate or wait. The
/// render thread announces the brief pointer-copy section with a reader count,
/// and the serialized writer only reclaims the replaced immutable record once
/// that count reaches zero. The render path therefore never locks, allocates,
/// waits, or performs reference counting. Hosts retain responsibility for
/// keeping the opaque `user_data` target alive while callbacks may be in flight,
/// as required by the AU callback contract.
struct MidiOutputCallbackRecord {
    callback: AuMidiOutputCallbackStruct,
    #[cfg(test)]
    live_records: std::sync::Arc<AtomicUsize>,
}

impl Drop for MidiOutputCallbackRecord {
    fn drop(&mut self) {
        #[cfg(test)]
        self.live_records.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(super) struct MidiOutputCallbackSlot {
    current: AtomicPtr<MidiOutputCallbackRecord>,
    readers: AtomicUsize,
    writer: Mutex<()>,
    #[cfg(test)]
    live_records: std::sync::Arc<AtomicUsize>,
}

impl MidiOutputCallbackSlot {
    pub fn new() -> Self {
        Self {
            current: AtomicPtr::new(std::ptr::null_mut()),
            readers: AtomicUsize::new(0),
            writer: Mutex::new(()),
            #[cfg(test)]
            live_records: std::sync::Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn store(&self, callback: Option<AuMidiOutputCallbackStruct>) -> Result<(), ()> {
        let _writer = self.writer.lock().map_err(|_| ())?;
        let replacement = match callback.filter(|callback| callback.callback.is_some()) {
            Some(callback) => {
                let record = Box::new(MidiOutputCallbackRecord {
                    callback,
                    #[cfg(test)]
                    live_records: self.live_records.clone(),
                });
                #[cfg(test)]
                self.live_records.fetch_add(1, Ordering::SeqCst);
                Box::into_raw(record)
            }
            None => std::ptr::null_mut(),
        };

        let retired = self.current.swap(replacement, Ordering::SeqCst);
        while self.readers.load(Ordering::SeqCst) != 0 {
            std::thread::yield_now();
        }

        if !retired.is_null() {
            // SAFETY: writers are serialized, and the reader count reached
            // zero after this record stopped being current. A later reader can
            // therefore only observe `replacement`.
            unsafe { drop(Box::from_raw(retired)) };
        }

        Ok(())
    }

    #[inline]
    pub fn load(&self) -> Option<AuMidiOutputCallbackStruct> {
        self.readers.fetch_add(1, Ordering::SeqCst);
        let record = self.current.load(Ordering::SeqCst);
        let callback = if record.is_null() {
            None
        } else {
            // SAFETY: the reader count prevents the writer from reclaiming the
            // immutable record until after this copy has completed.
            Some(unsafe { (*record).callback })
        };
        self.readers.fetch_sub(1, Ordering::SeqCst);
        callback
    }

    #[cfg(test)]
    fn live_record_count(&self) -> usize {
        self.live_records.load(Ordering::SeqCst)
    }
}

impl Drop for MidiOutputCallbackSlot {
    fn drop(&mut self) {
        debug_assert_eq!(*self.readers.get_mut(), 0);
        let record = *self.current.get_mut();
        if !record.is_null() {
            // SAFETY: `&mut self` proves that no safe reader or writer can still
            // access this slot. AU teardown is also serialized against render.
            unsafe { drop(Box::from_raw(record)) };
        }
    }
}

pub(super) struct QueuedMidiEvents<P: Plugin> {
    sequence: u64,
    first: PluginNoteEvent<P>,
    second: Option<PluginNoteEvent<P>>,
}

/// Multi-producer queue used by the MusicDevice selector calls. Hosts may call
/// these selectors from either their render thread or a control thread, while
/// `AudioUnitRender` is the single consumer.
pub(super) struct MidiInputState<P: Plugin> {
    queue: ArrayQueue<QueuedMidiEvents<P>>,
    sequence: AtomicU64,
    next_note_id: AtomicU32,
    active_notes: Box<[AtomicU64]>,
}

impl<P: Plugin> MidiInputState<P> {
    pub fn new() -> Self {
        Self {
            queue: ArrayQueue::new(MIDI_EVENT_QUEUE_CAPACITY),
            sequence: AtomicU64::new(0),
            next_note_id: AtomicU32::new(1),
            active_notes: (0..ACTIVE_NOTE_CAPACITY)
                .map(|_| AtomicU64::new(0))
                .collect(),
        }
    }

    fn push(&self, first: PluginNoteEvent<P>, second: Option<PluginNoteEvent<P>>) -> au::OSStatus {
        let event = QueuedMidiEvents {
            sequence: self.sequence.fetch_add(1, Ordering::Relaxed),
            first,
            second,
        };

        if self.queue.push(event).is_err() {
            au::kAudioUnitErr_TooManyFramesToProcess
        } else {
            au::noErr
        }
    }

    pub fn push_midi_event(
        &self,
        status: au::UInt32,
        data_1: au::UInt32,
        data_2: au::UInt32,
        offset: au::UInt32,
    ) -> au::OSStatus {
        if P::MIDI_INPUT < MidiConfig::Basic {
            return au::noErr;
        }
        if status > u8::MAX as u32
            || !(0x80..0xf0).contains(&(status as u8))
            || data_1 > 127
            || data_2 > 127
        {
            return au::kAudioUnitErr_InvalidParameter;
        }

        let timing = offset & MUSIC_DEVICE_SAMPLE_FRAME_MASK;
        let bytes = [status as u8, data_1 as u8, data_2 as u8];
        match NoteEvent::from_midi(timing, &bytes) {
            Ok(event) if input_event_allowed::<P>(&event) => self.push(event, None),
            // Hosts should not need to special-case a plugin's MIDI dialect.
            // Unsupported messages are accepted and ignored, matching the
            // CLAP/VST3 wrappers.
            _ => au::noErr,
        }
    }

    pub unsafe fn push_sysex(&self, data: *const u8, length: au::UInt32) -> au::OSStatus {
        if P::MIDI_INPUT < MidiConfig::Basic {
            return au::noErr;
        }
        if data.is_null() || length < 2 {
            return au::kAudioUnitErr_InvalidParameter;
        }

        let bytes = unsafe { std::slice::from_raw_parts(data, length as usize) };
        if bytes.first() != Some(&0xf0) || bytes.last() != Some(&0xf7) {
            return au::kAudioUnitErr_InvalidParameter;
        }

        match NoteEvent::from_midi(0, bytes) {
            Ok(event @ NoteEvent::MidiSysEx { .. }) => self.push(event, None),
            _ => au::noErr,
        }
    }

    pub unsafe fn start_note(
        &self,
        group: au::UInt32,
        out_note_id: *mut au::UInt32,
        offset: au::UInt32,
        params: *const MusicDeviceNoteParams,
    ) -> au::OSStatus {
        if P::MIDI_INPUT < MidiConfig::Basic {
            return au::kAudioUnitErr_CannotDoInCurrentContext;
        }
        if group >= 16 || out_note_id.is_null() || params.is_null() {
            return au::kAudioUnitErr_InvalidParameter;
        }

        let params = unsafe { &*params };
        if params.arg_count < 2
            || !params.pitch.is_finite()
            || !(0.0..128.0).contains(&params.pitch)
            || !params.velocity.is_finite()
            || !(0.0..=127.0).contains(&params.velocity)
        {
            return au::kAudioUnitErr_InvalidParameter;
        }

        let rounded_pitch = params.pitch.round().clamp(0.0, 127.0);
        let note = rounded_pitch as u8;
        let (note_id, slot, packed) = match self.reserve_active_note(group as u8, note) {
            Some(active) => active,
            None => return au::kAudioUnitErr_CannotDoInCurrentContext,
        };
        let timing = offset & MUSIC_DEVICE_SAMPLE_FRAME_MASK;
        let voice_id = Some(note_id as i32);
        let note_on = NoteEvent::NoteOn {
            timing,
            voice_id,
            channel: group as u8,
            note,
            velocity: params.velocity / 127.0,
        };
        let tuning = params.pitch - rounded_pitch;
        let tuning_event = (tuning.abs() > f32::EPSILON).then_some(NoteEvent::PolyTuning {
            timing,
            voice_id,
            channel: group as u8,
            note,
            tuning,
        });

        let status = self.push(note_on, tuning_event);
        if status != au::noErr {
            let _ = self.active_notes[slot].compare_exchange(
                packed,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            return status;
        }

        unsafe { *out_note_id = note_id };
        au::noErr
    }

    pub fn stop_note(
        &self,
        group: au::UInt32,
        note_id: au::UInt32,
        offset: au::UInt32,
    ) -> au::OSStatus {
        if P::MIDI_INPUT < MidiConfig::Basic {
            return au::kAudioUnitErr_CannotDoInCurrentContext;
        }
        if group >= 16 || note_id == 0 || note_id > i32::MAX as u32 {
            return au::kAudioUnitErr_InvalidParameter;
        }

        for slot in self.active_notes.iter() {
            let packed = slot.load(Ordering::Acquire);
            if packed & STOPPING_NOTE_BIT != 0
                || unpack_note_id(packed) != note_id
                || unpack_channel(packed) != group as u8
            {
                continue;
            }
            let claimed = packed | STOPPING_NOTE_BIT;
            if slot
                .compare_exchange(packed, claimed, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }

            let status = self.push(
                NoteEvent::NoteOff {
                    timing: offset & MUSIC_DEVICE_SAMPLE_FRAME_MASK,
                    voice_id: Some(note_id as i32),
                    channel: group as u8,
                    note: unpack_note(packed),
                    velocity: 0.0,
                },
                None,
            );
            if status == au::noErr {
                slot.store(0, Ordering::Release);
            } else {
                let _ = slot.compare_exchange(claimed, packed, Ordering::AcqRel, Ordering::Acquire);
            }
            return status;
        }

        au::kAudioUnitErr_InvalidParameter
    }

    fn reserve_active_note(&self, channel: u8, note: u8) -> Option<(u32, usize, u64)> {
        for _ in 0..ACTIVE_NOTE_CAPACITY {
            let note_id = self
                .next_note_id
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    Some(if current >= i32::MAX as u32 {
                        1
                    } else {
                        current + 1
                    })
                })
                .ok()?;
            if self
                .active_notes
                .iter()
                .any(|slot| unpack_note_id(slot.load(Ordering::Acquire)) == note_id)
            {
                continue;
            }

            let packed = pack_active_note(note_id, channel, note);
            for (idx, slot) in self.active_notes.iter().enumerate() {
                if slot
                    .compare_exchange(0, packed, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return Some((note_id, idx, packed));
                }
            }
            return None;
        }
        None
    }

    pub fn drain_into(&self, state: &mut MidiRenderState<P>, number_frames: u32) {
        state.input_batches.clear();
        state.input_events.clear();
        state.output_events.clear();

        // Do not consume events that arrive after this render started. They are
        // intended for the next block and remain queued.
        let queued_at_start = self.queue.len().min(MIDI_EVENT_QUEUE_CAPACITY);
        for _ in 0..queued_at_start {
            if let Some(batch) = self.queue.pop() {
                state.input_batches.push(batch);
            }
        }
        state
            .input_batches
            .sort_unstable_by_key(|batch| (batch.first.timing(), batch.sequence));

        let last_frame = number_frames.saturating_sub(1);
        for mut batch in state.input_batches.drain(..) {
            batch.first.clamp_timing(last_frame);
            state.input_events.push_back(batch.first);
            if let Some(mut second) = batch.second {
                second.clamp_timing(last_frame);
                state.input_events.push_back(second);
            }
        }
    }

    pub fn clear(&self) {
        while self.queue.pop().is_some() {}
        for slot in self.active_notes.iter() {
            slot.store(0, Ordering::Release);
        }
    }
}

fn input_event_allowed<P: Plugin>(event: &PluginNoteEvent<P>) -> bool {
    match event {
        NoteEvent::NoteOn { .. }
        | NoteEvent::NoteOff { .. }
        | NoteEvent::PolyPressure { .. }
        | NoteEvent::MidiSysEx { .. } => P::MIDI_INPUT >= MidiConfig::Basic,
        NoteEvent::MidiChannelPressure { .. }
        | NoteEvent::MidiPitchBend { .. }
        | NoteEvent::MidiCC { .. }
        | NoteEvent::MidiProgramChange { .. } => P::MIDI_INPUT >= MidiConfig::MidiCCs,
        _ => false,
    }
}

fn output_event_allowed<P: Plugin>(event: &PluginNoteEvent<P>) -> bool {
    match event {
        NoteEvent::NoteOn { .. }
        | NoteEvent::NoteOff { .. }
        | NoteEvent::PolyPressure { .. }
        | NoteEvent::MidiSysEx { .. } => P::MIDI_OUTPUT >= MidiConfig::Basic,
        NoteEvent::MidiChannelPressure { .. }
        | NoteEvent::MidiPitchBend { .. }
        | NoteEvent::MidiCC { .. }
        | NoteEvent::MidiProgramChange { .. } => P::MIDI_OUTPUT >= MidiConfig::MidiCCs,
        _ => false,
    }
}

fn pack_active_note(note_id: u32, channel: u8, note: u8) -> u64 {
    ((note_id as u64) << 32) | ((channel as u64) << 8) | note as u64
}

fn unpack_note_id(packed: u64) -> u32 {
    ((packed & !STOPPING_NOTE_BIT) >> 32) as u32
}

fn unpack_channel(packed: u64) -> u8 {
    ((packed >> 8) & 0xff) as u8
}

fn unpack_note(packed: u64) -> u8 {
    (packed & 0xff) as u8
}

pub(super) struct MidiRenderState<P: Plugin> {
    input_batches: Vec<QueuedMidiEvents<P>>,
    pub input_events: VecDeque<PluginNoteEvent<P>>,
    pub output_events: VecDeque<PluginNoteEvent<P>>,
    packet_storage: Vec<u64>,
}

impl<P: Plugin> MidiRenderState<P> {
    pub fn new() -> Self {
        Self {
            input_batches: Vec::with_capacity(MIDI_EVENT_QUEUE_CAPACITY),
            input_events: VecDeque::with_capacity(MIDI_RENDER_EVENT_CAPACITY),
            output_events: VecDeque::with_capacity(MIDI_EVENT_QUEUE_CAPACITY),
            packet_storage: vec![0; MIDI_PACKET_LIST_BYTES / mem::size_of::<u64>()],
        }
    }

    pub fn clear(&mut self) {
        self.input_batches.clear();
        self.input_events.clear();
        self.output_events.clear();
    }

    pub unsafe fn flush_output(
        &mut self,
        callback: Option<AuMidiOutputCallbackStruct>,
        time_stamp: *const au::AudioTimeStamp,
        number_frames: u32,
    ) -> au::OSStatus {
        let callback = match callback.filter(|cb| cb.callback.is_some()) {
            Some(callback) if P::MIDI_OUTPUT >= MidiConfig::Basic => callback,
            _ => {
                self.output_events.clear();
                return au::noErr;
            }
        };

        let mut builder =
            unsafe { MidiPacketListBuilder::new(callback, time_stamp, &mut self.packet_storage) };
        while let Some(mut event) = self.output_events.pop_front() {
            if !output_event_allowed::<P>(&event) {
                nih_debug_assert_failure!(
                    "Invalid AU output event for the current MIDI_OUTPUT setting"
                );
                continue;
            }
            event.clamp_timing(number_frames.saturating_sub(1));
            let timing = event.timing() as u64;
            match event.as_midi() {
                Some(MidiResult::Basic(bytes)) => {
                    let length = match bytes[0] & 0xf0 {
                        0xc0 | 0xd0 => 2,
                        _ => 3,
                    };
                    if let Err(status) = unsafe { builder.push_message(timing, &bytes[..length]) } {
                        return status;
                    }
                }
                Some(MidiResult::SysEx(buffer, length)) => {
                    let bytes = buffer.borrow();
                    if let Err(status) = unsafe { builder.push_message(timing, &bytes[..length]) } {
                        return status;
                    }
                }
                None => {
                    nih_debug_assert_failure!("AU cannot encode this note expression as MIDI 1.0");
                }
            }
        }

        match unsafe { builder.flush() } {
            Ok(()) => au::noErr,
            Err(status) => status,
        }
    }
}

struct MidiPacketListBuilder<'a> {
    callback: AuMidiOutputCallbackStruct,
    time_stamp: *const au::AudioTimeStamp,
    storage: &'a mut [u64],
    current_packet: *mut c_void,
    has_packets: bool,
}

impl<'a> MidiPacketListBuilder<'a> {
    unsafe fn new(
        callback: AuMidiOutputCallbackStruct,
        time_stamp: *const au::AudioTimeStamp,
        storage: &'a mut [u64],
    ) -> Self {
        let current_packet = unsafe { midi_packet_list_init(storage.as_mut_ptr() as *mut c_void) };
        Self {
            callback,
            time_stamp,
            storage,
            current_packet,
            has_packets: false,
        }
    }

    unsafe fn reset(&mut self) {
        self.current_packet =
            unsafe { midi_packet_list_init(self.storage.as_mut_ptr() as *mut c_void) };
        self.has_packets = false;
    }

    unsafe fn push_message(&mut self, timing: u64, data: &[u8]) -> Result<(), au::OSStatus> {
        let mut offset = 0;
        while offset < data.len() {
            let end = (offset + MIDI_PACKET_CHUNK_BYTES).min(data.len());
            let chunk = &data[offset..end];
            let mut added = unsafe {
                midi_packet_list_add(
                    self.storage.as_mut_ptr() as *mut c_void,
                    MIDI_PACKET_LIST_BYTES,
                    self.current_packet,
                    timing,
                    chunk.len(),
                    chunk.as_ptr(),
                )
            };
            if added.is_null() && self.has_packets {
                unsafe { self.flush()? };
                added = unsafe {
                    midi_packet_list_add(
                        self.storage.as_mut_ptr() as *mut c_void,
                        MIDI_PACKET_LIST_BYTES,
                        self.current_packet,
                        timing,
                        chunk.len(),
                        chunk.as_ptr(),
                    )
                };
            }
            if added.is_null() {
                return Err(au::kAudioUnitErr_CannotDoInCurrentContext);
            }
            self.current_packet = added;
            self.has_packets = true;
            offset = end;
            if offset < data.len() {
                unsafe { self.flush()? };
            }
        }
        Ok(())
    }

    unsafe fn flush(&mut self) -> Result<(), au::OSStatus> {
        if !self.has_packets {
            return Ok(());
        }
        let callback = self
            .callback
            .callback
            .expect("callback checked by constructor caller");
        let status = unsafe {
            callback(
                self.callback.user_data,
                self.time_stamp,
                0,
                self.storage.as_ptr() as *const c_void,
            )
        };
        if status != au::noErr {
            return Err(status);
        }
        unsafe { self.reset() };
        Ok(())
    }
}

#[link(name = "CoreMIDI", kind = "framework")]
extern "C" {
    #[link_name = "MIDIPacketListInit"]
    fn midi_packet_list_init(packet_list: *mut c_void) -> *mut c_void;
    #[link_name = "MIDIPacketListAdd"]
    fn midi_packet_list_add(
        packet_list: *mut c_void,
        list_size: usize,
        current_packet: *mut c_void,
        time: u64,
        data_length: usize,
        data: *const u8,
    ) -> *mut c_void;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::prelude::*;

    #[derive(Debug, Clone, Copy, PartialEq)]
    struct TestSysEx([u8; 4]);

    impl SysExMessage for TestSysEx {
        type Buffer = [u8; 4];

        fn from_buffer(buffer: &[u8]) -> Option<Self> {
            (buffer.len() == 4).then(|| Self(buffer.try_into().unwrap()))
        }

        fn to_buffer(self) -> (Self::Buffer, usize) {
            (self.0, self.0.len())
        }
    }

    #[derive(Default)]
    struct TestParams {}

    unsafe impl Params for TestParams {
        fn param_map(&self) -> Vec<(String, ParamPtr, String)> {
            Vec::new()
        }
    }

    struct TestPlugin {
        params: Arc<TestParams>,
    }

    impl Default for TestPlugin {
        fn default() -> Self {
            Self {
                params: Arc::new(TestParams::default()),
            }
        }
    }

    impl Plugin for TestPlugin {
        const NAME: &'static str = "AU MIDI Test";
        const VENDOR: &'static str = "NIH-plug";
        const URL: &'static str = "https://github.com/robbert-vdh/nih-plug";
        const EMAIL: &'static str = "test@example.com";
        const VERSION: &'static str = "0.0.0";
        const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[];
        const MIDI_INPUT: MidiConfig = MidiConfig::MidiCCs;
        const MIDI_OUTPUT: MidiConfig = MidiConfig::MidiCCs;

        type SysExMessage = TestSysEx;
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

    #[test]
    fn music_device_channel_messages_are_sorted_and_clamped() {
        let input = MidiInputState::<TestPlugin>::new();
        let mut render = MidiRenderState::<TestPlugin>::new();

        assert_eq!(input.push_midi_event(0x91, 60, 100, 20), au::noErr);
        assert_eq!(input.push_midi_event(0x81, 60, 64, 1), au::noErr);
        assert_eq!(input.push_midi_event(0xa1, 60, 32, 2), au::noErr);
        assert_eq!(input.push_midi_event(0xb1, 7, 100, 3), au::noErr);
        assert_eq!(input.push_midi_event(0xc1, 12, 0, 4), au::noErr);
        assert_eq!(input.push_midi_event(0xd1, 48, 0, 5), au::noErr);
        assert_eq!(input.push_midi_event(0xe1, 0, 64, 6), au::noErr);

        input.drain_into(&mut render, 8);
        assert_eq!(render.input_events.len(), 7);
        let timings: Vec<_> = render.input_events.iter().map(NoteEvent::timing).collect();
        assert_eq!(timings, vec![1, 2, 3, 4, 5, 6, 7]);
        assert!(matches!(render.input_events[0], NoteEvent::NoteOff { .. }));
        assert!(matches!(
            render.input_events[1],
            NoteEvent::PolyPressure { .. }
        ));
        assert!(matches!(render.input_events[2], NoteEvent::MidiCC { .. }));
        assert!(matches!(
            render.input_events[3],
            NoteEvent::MidiProgramChange { .. }
        ));
        assert!(matches!(
            render.input_events[4],
            NoteEvent::MidiChannelPressure { .. }
        ));
        assert!(matches!(
            render.input_events[5],
            NoteEvent::MidiPitchBend { .. }
        ));
        assert!(matches!(render.input_events[6], NoteEvent::NoteOn { .. }));
    }

    #[test]
    fn music_device_sysex_uses_the_plugin_parser() {
        let input = MidiInputState::<TestPlugin>::new();
        let mut render = MidiRenderState::<TestPlugin>::new();
        let message = [0xf0, 0x01, 0x02, 0xf7];

        assert_eq!(
            unsafe { input.push_sysex(message.as_ptr(), message.len() as u32) },
            au::noErr
        );
        input.drain_into(&mut render, 32);
        assert_eq!(
            render.input_events.pop_front(),
            Some(NoteEvent::MidiSysEx {
                timing: 0,
                message: TestSysEx(message),
            })
        );
    }

    #[test]
    fn midi_input_queue_fails_closed_when_full() {
        let input = MidiInputState::<TestPlugin>::new();
        for _ in 0..MIDI_EVENT_QUEUE_CAPACITY {
            assert_eq!(input.push_midi_event(0x90, 60, 100, 0), au::noErr);
        }
        assert_eq!(
            input.push_midi_event(0x90, 61, 100, 0),
            au::kAudioUnitErr_TooManyFramesToProcess
        );
    }

    #[test]
    fn extended_start_stop_preserves_voice_identity_and_fractional_tuning() {
        let input = MidiInputState::<TestPlugin>::new();
        let mut render = MidiRenderState::<TestPlugin>::new();
        let params = MusicDeviceNoteParams {
            arg_count: 2,
            pitch: 60.25,
            velocity: 63.5,
            controls: [NoteParamsControlValue { id: 0, value: 0.0 }],
        };
        let mut note_id = 0;

        assert_eq!(
            unsafe { input.start_note(3, &mut note_id, 4, &params) },
            au::noErr
        );
        assert_ne!(note_id, 0);
        assert_eq!(input.stop_note(3, note_id, 9), au::noErr);

        input.drain_into(&mut render, 16);
        assert_eq!(render.input_events.len(), 3);
        assert!(matches!(
            render.input_events[0],
            NoteEvent::NoteOn {
                timing: 4,
                voice_id: Some(id),
                channel: 3,
                note: 60,
                ..
            } if id == note_id as i32
        ));
        assert!(matches!(
            render.input_events[1],
            NoteEvent::PolyTuning {
                timing: 4,
                voice_id: Some(id),
                tuning,
                ..
            } if id == note_id as i32 && (tuning - 0.25).abs() < f32::EPSILON
        ));
        assert!(matches!(
            render.input_events[2],
            NoteEvent::NoteOff {
                timing: 9,
                voice_id: Some(id),
                channel: 3,
                note: 60,
                ..
            } if id == note_id as i32
        ));
    }

    #[repr(C)]
    struct PacketCapture {
        calls: u32,
        count: u32,
        timestamps: [u64; 4],
        lengths: [u16; 4],
        data: [[u8; 3]; 4],
    }

    unsafe extern "C" fn capture_packets(
        user_data: *mut c_void,
        _time_stamp: *const au::AudioTimeStamp,
        midi_output_number: au::UInt32,
        packet_list: *const c_void,
    ) -> au::OSStatus {
        assert_eq!(midi_output_number, 0);
        let capture = unsafe { &mut *(user_data as *mut PacketCapture) };
        capture.calls += 1;
        let base = packet_list as *const u8;
        let count = unsafe { std::ptr::read_unaligned(base as *const u32) };
        let mut offset = 4usize;
        for _ in 0..count {
            let idx = capture.count as usize;
            capture.timestamps[idx] =
                unsafe { std::ptr::read_unaligned(base.add(offset) as *const u64) };
            let length = unsafe { std::ptr::read_unaligned(base.add(offset + 8) as *const u16) };
            capture.lengths[idx] = length;
            let bytes =
                unsafe { std::slice::from_raw_parts(base.add(offset + 10), length as usize) };
            capture.data[idx][..bytes.len()].copy_from_slice(bytes);
            capture.count += 1;
            let next = offset + 10 + length as usize;
            #[cfg(target_arch = "aarch64")]
            {
                offset = (next + 3) & !3;
            }
            #[cfg(not(target_arch = "aarch64"))]
            {
                offset = next;
            }
        }
        au::noErr
    }

    #[test]
    fn midi_output_callback_receives_sample_offset_packets() {
        let mut render = MidiRenderState::<TestPlugin>::new();
        render.output_events.push_back(NoteEvent::NoteOn {
            timing: 3,
            voice_id: None,
            channel: 2,
            note: 64,
            velocity: 1.0,
        });
        render
            .output_events
            .push_back(NoteEvent::MidiProgramChange {
                timing: 7,
                channel: 2,
                program: 12,
            });
        let mut capture = PacketCapture {
            calls: 0,
            count: 0,
            timestamps: [0; 4],
            lengths: [0; 4],
            data: [[0; 3]; 4],
        };
        let callback = AuMidiOutputCallbackStruct {
            callback: Some(capture_packets),
            user_data: &mut capture as *mut PacketCapture as *mut c_void,
        };

        assert_eq!(
            unsafe { render.flush_output(Some(callback), std::ptr::null(), 16) },
            au::noErr
        );
        assert_eq!(capture.calls, 1);
        assert_eq!(capture.count, 2);
        assert_eq!(capture.timestamps[..2], [3, 7]);
        assert_eq!(capture.lengths[..2], [3, 2]);
        assert_eq!(capture.data[0], [0x92, 64, 127]);
        assert_eq!(capture.data[1][..2], [0xc2, 12]);
    }

    #[test]
    fn midi_output_callback_replacement_reclaims_records_without_blocking_render_loads() {
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
        use std::thread;

        const UPDATE_COUNT: usize = 4096;
        let slot = Arc::new(MidiOutputCallbackSlot::new());
        slot.store(Some(AuMidiOutputCallbackStruct {
            callback: Some(capture_packets),
            user_data: 1usize as *mut c_void,
        }))
        .unwrap();

        let writer_slot = slot.clone();
        let writer_done = Arc::new(AtomicBool::new(false));
        let writer_done_clone = writer_done.clone();
        let writer = thread::spawn(move || {
            for value in 2..=UPDATE_COUNT {
                writer_slot
                    .store(Some(AuMidiOutputCallbackStruct {
                        callback: Some(capture_packets),
                        user_data: value as *mut c_void,
                    }))
                    .unwrap();
            }
            writer_done_clone.store(true, AtomicOrdering::Release);
        });

        while !writer_done.load(AtomicOrdering::Acquire) {
            let callback = slot.load().expect("an installed callback disappeared");
            assert!(callback.callback.is_some());
            let value = callback.user_data as usize;
            assert!((1..=UPDATE_COUNT).contains(&value));
            thread::yield_now();
        }
        writer.join().unwrap();

        assert_eq!(slot.load().unwrap().user_data as usize, UPDATE_COUNT);
        assert_eq!(slot.live_record_count(), 1);
        slot.store(None).unwrap();
        assert!(slot.load().is_none());
        assert_eq!(slot.live_record_count(), 0);
    }
}
