#include <AudioToolbox/AudioToolbox.h>
#include <CoreFoundation/CoreFoundation.h>
#include <CoreMIDI/CoreMIDI.h>

#include <math.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

// macOS 26에서는 bare CLI의 서드파티 AudioComponent 탐색이 깨질 수 있다.
// 이 실행 파일은 .app으로 감싸 LaunchServices를 통해 실행한다.

typedef struct {
    UInt32 count;
    MIDITimeStamp timestamps[16];
    UInt16 lengths[16];
    UInt8 bytes[16][3];
} MidiCapture;

static const char *result_path = "/tmp/nih_plug_au_midi_smoke.json";

static void write_error(const char *stage, OSStatus status) {
    FILE *file = fopen(result_path, "w");
    if (file != NULL) {
        fprintf(file, "{\"error\":\"%s\",\"status\":%d}\n", stage, (int)status);
        fclose(file);
    }
}

static void fail(const char *stage, OSStatus status) {
    write_error(stage, status);
    exit(1);
}

static FourCharCode fourcc(const char text[4]) {
    return ((FourCharCode)(UInt8)text[0] << 24) |
           ((FourCharCode)(UInt8)text[1] << 16) |
           ((FourCharCode)(UInt8)text[2] << 8) |
           (FourCharCode)(UInt8)text[3];
}

static OSStatus midi_output_callback(void *user_data,
                                     const AudioTimeStamp *timestamp,
                                     UInt32 output_number,
                                     const MIDIPacketList *packet_list) {
    (void)timestamp;
    (void)output_number;

    MidiCapture *capture = (MidiCapture *)user_data;
    const MIDIPacket *packet = &packet_list->packet[0];
    for (UInt32 index = 0; index < packet_list->numPackets; ++index) {
        if (capture->count < 16) {
            const UInt32 slot = capture->count++;
            capture->timestamps[slot] = packet->timeStamp;
            capture->lengths[slot] = packet->length;
            const UInt16 copy_length = packet->length < 3 ? packet->length : 3;
            memcpy(capture->bytes[slot], packet->data, copy_length);
        }
        packet = MIDIPacketNext(packet);
    }

    return noErr;
}

static AudioUnitParameterID find_parameter(AudioUnit unit, const char *wanted_name) {
    UInt32 size = 0;
    Boolean writable = false;
    OSStatus status = AudioUnitGetPropertyInfo(
        unit, kAudioUnitProperty_ParameterList, kAudioUnitScope_Global, 0, &size, &writable);
    if (status != noErr || size == 0 || size % sizeof(AudioUnitParameterID) != 0) {
        fail("GetPropertyInfo(ParameterList)", status);
    }

    AudioUnitParameterID *ids = malloc(size);
    if (ids == NULL) {
        fail("malloc(ParameterList)", kAudioUnitErr_FailedInitialization);
    }
    status = AudioUnitGetProperty(
        unit, kAudioUnitProperty_ParameterList, kAudioUnitScope_Global, 0, ids, &size);
    if (status != noErr) {
        free(ids);
        fail("GetProperty(ParameterList)", status);
    }

    const UInt32 count = size / sizeof(AudioUnitParameterID);
    AudioUnitParameterID found = UINT32_MAX;
    for (UInt32 index = 0; index < count; ++index) {
        AudioUnitParameterInfo info;
        memset(&info, 0, sizeof(info));
        UInt32 info_size = sizeof(info);
        status = AudioUnitGetProperty(unit,
                                      kAudioUnitProperty_ParameterInfo,
                                      kAudioUnitScope_Global,
                                      ids[index],
                                      &info,
                                      &info_size);
        if (status != noErr) {
            free(ids);
            fail("GetProperty(ParameterInfo)", status);
        }

        if (strncmp(info.name, wanted_name, sizeof(info.name)) == 0) {
            found = ids[index];
        }
        // nih-plug의 AU wrapper는 이 두 문자열을 Create Rule(+1)로 반환한다.
        if (info.cfNameString != NULL) {
            CFRelease(info.cfNameString);
        }
        if (info.unitName != NULL) {
            CFRelease(info.unitName);
        }
        if (found != UINT32_MAX) {
            break;
        }
    }

    free(ids);
    if (found == UINT32_MAX) {
        fail("Use MIDI parameter not found", kAudioUnitErr_InvalidParameter);
    }
    return found;
}

static AudioBufferList *make_stereo_buffer_list(UInt32 frames,
                                                 Float32 **left,
                                                 Float32 **right) {
    *left = calloc(frames, sizeof(Float32));
    *right = calloc(frames, sizeof(Float32));
    const size_t list_size = offsetof(AudioBufferList, mBuffers) + 2 * sizeof(AudioBuffer);
    AudioBufferList *list = calloc(1, list_size);
    if (*left == NULL || *right == NULL || list == NULL) {
        fail("calloc(AudioBufferList)", kAudioUnitErr_FailedInitialization);
    }

    list->mNumberBuffers = 2;
    list->mBuffers[0].mNumberChannels = 1;
    list->mBuffers[0].mDataByteSize = frames * sizeof(Float32);
    list->mBuffers[0].mData = *left;
    list->mBuffers[1].mNumberChannels = 1;
    list->mBuffers[1].mDataByteSize = frames * sizeof(Float32);
    list->mBuffers[1].mData = *right;
    return list;
}

static OSStatus render(AudioUnit unit,
                       AudioBufferList *list,
                       UInt32 frames,
                       Float64 sample_time) {
    memset(list->mBuffers[0].mData, 0, frames * sizeof(Float32));
    memset(list->mBuffers[1].mData, 0, frames * sizeof(Float32));
    AudioTimeStamp timestamp;
    memset(&timestamp, 0, sizeof(timestamp));
    timestamp.mSampleTime = sample_time;
    timestamp.mFlags = kAudioTimeStampSampleTimeValid;
    AudioUnitRenderActionFlags flags = 0;
    return AudioUnitRender(unit, &flags, &timestamp, 0, frames, list);
}

static double rms(const Float32 *samples, UInt32 count) {
    double sum = 0.0;
    for (UInt32 index = 0; index < count; ++index) {
        sum += (double)samples[index] * (double)samples[index];
    }
    return sqrt(sum / (double)count);
}

static Boolean captured_message(const MidiCapture *capture,
                                UInt8 status,
                                UInt8 data1,
                                MIDITimeStamp timestamp) {
    for (UInt32 index = 0; index < capture->count; ++index) {
        if (capture->lengths[index] >= 3 && capture->bytes[index][0] == status &&
            capture->bytes[index][1] == data1 && capture->timestamps[index] == timestamp) {
            return true;
        }
    }
    return false;
}

int main(int argc, const char *argv[]) {
    for (int index = 1; index + 1 < argc; index += 2) {
        if (strcmp(argv[index], "--out") == 0) {
            result_path = argv[index + 1];
        }
    }

    AudioComponentDescription description = {
        .componentType = fourcc("aumu"),
        .componentSubType = fourcc("MPsn"),
        .componentManufacturer = fourcc("MoiP"),
        .componentFlags = 0,
        .componentFlagsMask = 0,
    };
    AudioComponent component = AudioComponentFindNext(NULL, &description);
    if (component == NULL) {
        fail("AudioComponentFindNext", kAudioUnitErr_InvalidProperty);
    }

    AudioUnit unit = NULL;
    OSStatus status = AudioComponentInstanceNew(component, &unit);
    if (status != noErr || unit == NULL) {
        fail("AudioComponentInstanceNew", status);
    }

    UInt32 input_count = UINT32_MAX;
    UInt32 output_count = UINT32_MAX;
    UInt32 uint_size = sizeof(UInt32);
    status = AudioUnitGetProperty(unit,
                                  kAudioUnitProperty_ElementCount,
                                  kAudioUnitScope_Input,
                                  0,
                                  &input_count,
                                  &uint_size);
    if (status != noErr || input_count != 0) {
        fail("instrument input bus count", status);
    }
    uint_size = sizeof(UInt32);
    status = AudioUnitGetProperty(unit,
                                  kAudioUnitProperty_ElementCount,
                                  kAudioUnitScope_Output,
                                  0,
                                  &output_count,
                                  &uint_size);
    if (status != noErr || output_count != 1) {
        fail("instrument output bus count", status);
    }

    UInt32 supports_start_stop = 0;
    uint_size = sizeof(UInt32);
    status = AudioUnitGetProperty(
        unit, 1014, kAudioUnitScope_Global, 0, &supports_start_stop, &uint_size);
    if (status != noErr || supports_start_stop != 1) {
        fail("SupportsStartStopNote", status);
    }

    CFArrayRef output_names = NULL;
    UInt32 output_names_size = sizeof(output_names);
    status = AudioUnitGetProperty(unit,
                                  kAudioUnitProperty_MIDIOutputCallbackInfo,
                                  kAudioUnitScope_Global,
                                  0,
                                  &output_names,
                                  &output_names_size);
    if (status != noErr || output_names == NULL || CFArrayGetCount(output_names) != 1) {
        fail("MIDIOutputCallbackInfo", status);
    }
    CFRelease(output_names);

    MidiCapture capture;
    memset(&capture, 0, sizeof(capture));
    AUMIDIOutputCallbackStruct output_callback = {
        .midiOutputCallback = midi_output_callback,
        .userData = &capture,
    };
    status = AudioUnitSetProperty(unit,
                                  kAudioUnitProperty_MIDIOutputCallback,
                                  kAudioUnitScope_Global,
                                  0,
                                  &output_callback,
                                  sizeof(output_callback));
    if (status != noErr) {
        fail("SetProperty(MIDIOutputCallback)", status);
    }

    AudioStreamBasicDescription format = {
        .mSampleRate = 48000.0,
        .mFormatID = kAudioFormatLinearPCM,
        .mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagIsNonInterleaved,
        .mBytesPerPacket = sizeof(Float32),
        .mFramesPerPacket = 1,
        .mBytesPerFrame = sizeof(Float32),
        .mChannelsPerFrame = 2,
        .mBitsPerChannel = 32,
        .mReserved = 0,
    };
    status = AudioUnitSetProperty(unit,
                                  kAudioUnitProperty_StreamFormat,
                                  kAudioUnitScope_Output,
                                  0,
                                  &format,
                                  sizeof(format));
    if (status != noErr) {
        fail("SetProperty(StreamFormat)", status);
    }

    UInt32 max_frames = 512;
    status = AudioUnitSetProperty(unit,
                                  kAudioUnitProperty_MaximumFramesPerSlice,
                                  kAudioUnitScope_Global,
                                  0,
                                  &max_frames,
                                  sizeof(max_frames));
    if (status != noErr) {
        fail("SetProperty(MaximumFramesPerSlice)", status);
    }

    const AudioUnitParameterID use_midi = find_parameter(unit, "Use MIDI");
    status = AudioUnitSetParameter(unit, use_midi, kAudioUnitScope_Global, 0, 1.0f, 0);
    if (status != noErr) {
        fail("SetParameter(Use MIDI)", status);
    }

    status = AudioUnitInitialize(unit);
    if (status != noErr) {
        fail("AudioUnitInitialize", status);
    }

    Float32 *left = NULL;
    Float32 *right = NULL;
    AudioBufferList *list = make_stereo_buffer_list(max_frames, &left, &right);
    status = render(unit, list, max_frames, 0.0);
    if (status != noErr) {
        fail("pre-note AudioUnitRender", status);
    }
    const double pre_note_rms = rms(left, max_frames);

    status = MusicDeviceMIDIEvent(unit, 0x90, 69, 100, 64);
    if (status != noErr) {
        fail("MusicDeviceMIDIEvent(NoteOn)", status);
    }
    status = render(unit, list, max_frames, 512.0);
    if (status != noErr) {
        fail("note-on AudioUnitRender", status);
    }
    const double note_on_rms = rms(left, max_frames);

    status = MusicDeviceMIDIEvent(unit, 0x80, 69, 0, 16);
    if (status != noErr) {
        fail("MusicDeviceMIDIEvent(NoteOff)", status);
    }
    status = render(unit, list, max_frames, 1024.0);
    if (status != noErr) {
        fail("note-off AudioUnitRender", status);
    }

    const UInt8 sysex[] = {0xF0, 0x7E, 0x00, 0xF7};
    status = MusicDeviceSysEx(unit, sysex, sizeof(sysex));
    if (status != noErr) {
        fail("MusicDeviceSysEx", status);
    }

    MusicDeviceStdNoteParams note_params = {
        .argCount = 2,
        .mPitch = 72.5f,
        .mVelocity = 96.0f,
    };
    NoteInstanceID note_id = 0;
    status = MusicDeviceStartNote(unit,
                                  kMusicNoteEvent_UseGroupInstrument,
                                  0,
                                  &note_id,
                                  32,
                                  (const MusicDeviceNoteParams *)&note_params);
    if (status != noErr || note_id == 0) {
        fail("MusicDeviceStartNote", status);
    }
    status = render(unit, list, max_frames, 1536.0);
    if (status != noErr) {
        fail("start-note AudioUnitRender", status);
    }
    status = MusicDeviceStopNote(unit, 0, note_id, 24);
    if (status != noErr) {
        fail("MusicDeviceStopNote", status);
    }
    status = render(unit, list, max_frames, 2048.0);
    if (status != noErr) {
        fail("stop-note AudioUnitRender", status);
    }

    OSStatus last_render_error = -1;
    UInt32 status_size = sizeof(last_render_error);
    status = AudioUnitGetProperty(unit,
                                  kAudioUnitProperty_LastRenderError,
                                  kAudioUnitScope_Global,
                                  0,
                                  &last_render_error,
                                  &status_size);
    if (status != noErr || last_render_error != noErr) {
        fail("LastRenderError", status);
    }

    Float64 tail_time = 0.0;
    UInt32 tail_time_size = sizeof(tail_time);
    status = AudioUnitGetProperty(unit,
                                  kAudioUnitProperty_TailTime,
                                  kAudioUnitScope_Global,
                                  0,
                                  &tail_time,
                                  &tail_time_size);
    const Boolean tail_time_infinite = status == noErr && isinf(tail_time);
    if (!tail_time_infinite) {
        fail("TailTime", status);
    }

    const Boolean note_on_echo = captured_message(&capture, 0x90, 69, 64);
    const Boolean note_off_echo = captured_message(&capture, 0x80, 69, 16);
    const Boolean audio_ok = pre_note_rms < 0.000001 && note_on_rms > 0.001;

    AudioUnitUninitialize(unit);
    AudioComponentInstanceDispose(unit);
    free(list);
    free(left);
    free(right);

    FILE *file = fopen(result_path, "w");
    if (file == NULL) {
        return 1;
    }
    fprintf(file,
            "{\"component\":\"aumu/MPsn/MoiP\",\"input_buses\":%u,"
            "\"output_buses\":%u,\"pre_note_rms\":%.9f,\"note_on_rms\":%.9f,"
            "\"audio_ok\":%s,\"midi_output_packets\":%u,\"note_on_echo\":%s,"
            "\"note_off_echo\":%s,\"start_stop_note_id\":%u,"
            "\"last_render_error\":%d,\"tail_time_infinite\":%s}\n",
            input_count,
            output_count,
            pre_note_rms,
            note_on_rms,
            audio_ok ? "true" : "false",
            capture.count,
            note_on_echo ? "true" : "false",
            note_off_echo ? "true" : "false",
            note_id,
            (int)last_render_error,
            tail_time_infinite ? "true" : "false");
    fclose(file);

    return audio_ok && note_on_echo && note_off_echo ? 0 : 1;
}
