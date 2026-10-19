//! AudioUnitScheduleParameters ABI, omitted by au-sys 0.1.1.
//! Layout follows AudioToolbox/AUComponent.h (32 bytes, 4-byte alignment).

use au_sys as au;
use std::ffi::c_void;

pub(super) const SCHEDULE_PARAMETERS_SELECT: au::SInt16 = 0x0011;
pub(super) const IMMEDIATE: u32 = 1;
pub(super) type ScheduleProc = unsafe extern "C" fn(*mut c_void, *const Event, u32) -> au::OSStatus;

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Immediate {
    pub buffer_offset: u32,
    pub value: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Ramp {
    pub start_buffer_offset: i32,
    pub duration_frames: u32,
    pub start_value: f32,
    pub end_value: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) union Values {
    pub immediate: Immediate,
    pub ramp: Ramp,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Event {
    pub scope: au::AudioUnitScope,
    pub element: au::AudioUnitElement,
    pub parameter: au::AudioUnitParameterID,
    pub event_type: u32,
    pub values: Values,
}
