// LogicPullHost는 Logic Pro가 실제로 요구한 AUv2 input-pull 계약을 재현한다.
//
// AUD-831의 무음은 단순한 MakeConnection 문제가 아니었다. Logic 스타일
// SetRenderCallback은 유효하지 않은 sample timestamp에 무음을 반환하고,
// caller buffer를 채우는 대신 mData 포인터를 자신의 zero-copy buffer로 교체한다.
// 따라서 wrapper가 timestamp를 0으로 넘기거나 callback 뒤의 BufferList를 다시
// 읽지 않으면 성공 상태(noErr)로도 효과 AU의 출력은 전부 무음이 된다.
//
// 이 host는 두 동작을 함께 재현하고 non-silent output을 요구한다. 실제 Logic
// 앱을 자동화하지는 않지만, 당시의 두 host contract 차이를 결정적으로 pin한다.

import AudioToolbox
import Foundation

struct Args {
    var type = "aufx"
    var subtype = "MPgN"
    var manufacturer = "MoiP"
    var sampleRate = 44_100.0
    var blockSize: UInt32 = 512
    var blocks = 32
    var frequency = 440.0
    var outputPath = "/tmp/nih_plug_logic_pull_result.json"
}

var args = Args()
var arguments = CommandLine.arguments.dropFirst().makeIterator()
while let flag = arguments.next() {
    let value = arguments.next() ?? ""
    switch flag {
    case "--type": args.type = value
    case "--subtype": args.subtype = value
    case "--manufacturer": args.manufacturer = value
    case "--sample-rate": args.sampleRate = Double(value) ?? args.sampleRate
    case "--block-size": args.blockSize = UInt32(value) ?? args.blockSize
    case "--blocks": args.blocks = Int(value) ?? args.blocks
    case "--frequency": args.frequency = Double(value) ?? args.frequency
    case "--out": args.outputPath = value
    default: break
    }
}

func fourCC(_ string: String) -> OSType {
    var value: OSType = 0
    for byte in string.utf8.prefix(4) {
        value = (value << 8) | OSType(byte)
    }
    return value
}

func rms(_ samples: UnsafePointer<Float32>, count: Int) -> Double {
    var sum = 0.0
    for index in 0..<count {
        sum += Double(samples[index]) * Double(samples[index])
    }
    return (sum / Double(max(count, 1))).squareRoot()
}

final class LogicPullContext {
    var phase = 0.0
    let phaseIncrement: Double
    let capacity: Int
    let left: UnsafeMutablePointer<Float32>
    let right: UnsafeMutablePointer<Float32>

    init(frequency: Double, sampleRate: Double, capacity: Int) {
        phaseIncrement = 2.0 * Double.pi * frequency / sampleRate
        self.capacity = capacity
        left = .allocate(capacity: capacity)
        right = .allocate(capacity: capacity)
        left.initialize(repeating: 0, count: capacity)
        right.initialize(repeating: 0, count: capacity)
    }

    deinit {
        left.deallocate()
        right.deallocate()
    }
}

let logicStylePull: AURenderCallback = { reference, _, timestamp, _, frameCount, ioData in
    guard let ioData else { return noErr }
    let context = Unmanaged<LogicPullContext>.fromOpaque(reference).takeUnretainedValue()
    let buffers = UnsafeMutableAudioBufferListPointer(ioData)

    // 결함 1 회귀: wrapper가 sample-time-valid 없이 pull하면 Logic처럼 noErr와
    // silence를 돌려준다. 따라서 양호한 output은 wrapper가 원 timestamp를
    // 그대로 전달했다는 증거다.
    guard timestamp.pointee.mFlags.contains(.sampleTimeValid) else {
        for buffer in buffers {
            if let data = buffer.mData {
                memset(data, 0, Int(buffer.mDataByteSize))
            }
        }
        return noErr
    }

    let frames = min(Int(frameCount), context.capacity)
    for index in 0..<frames {
        let sample = Float32(sin(context.phase) * 0.5)
        context.phase += context.phaseIncrement
        if context.phase > 2.0 * Double.pi {
            context.phase -= 2.0 * Double.pi
        }
        context.left[index] = sample
        context.right[index] = sample
    }

    // 결함 2 회귀: caller scratch를 채우지 않고 mData를 zero-copy source buffer로
    // 교체한다. wrapper가 callback 이전 scratch만 읽으면 downstream은 0을 본다.
    if buffers.count > 0 {
        buffers[0].mData = UnsafeMutableRawPointer(context.left)
        buffers[0].mDataByteSize = UInt32(frames * MemoryLayout<Float32>.size)
    }
    if buffers.count > 1 {
        buffers[1].mData = UnsafeMutableRawPointer(context.right)
        buffers[1].mDataByteSize = UInt32(frames * MemoryLayout<Float32>.size)
    }
    return noErr
}

var targetFound = false
var callbackStatus: OSStatus = -99_999
var initializeStatus: OSStatus = -99_999
var renderStatus: OSStatus = noErr
var firstNonSilentSample = -1
var error = ""

func run() {
    var description = AudioComponentDescription(
        componentType: fourCC(args.type),
        componentSubType: fourCC(args.subtype),
        componentManufacturer: fourCC(args.manufacturer),
        componentFlags: 0,
        componentFlagsMask: 0
    )
    guard let component = AudioComponentFindNext(nil, &description) else {
        error = "target AudioComponent not found"
        return
    }
    targetFound = true

    var target: AudioUnit?
    guard AudioComponentInstanceNew(component, &target) == noErr, let unit = target else {
        error = "AudioComponentInstanceNew failed"
        return
    }

    var format = AudioStreamBasicDescription(
        mSampleRate: args.sampleRate,
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsNonInterleaved,
        mBytesPerPacket: 4,
        mFramesPerPacket: 1,
        mBytesPerFrame: 4,
        mChannelsPerFrame: 2,
        mBitsPerChannel: 32,
        mReserved: 0
    )
    let formatSize = UInt32(MemoryLayout<AudioStreamBasicDescription>.size)
    let context = LogicPullContext(
        frequency: args.frequency,
        sampleRate: args.sampleRate,
        capacity: Int(args.blockSize) * 4
    )
    var callback = AURenderCallbackStruct(
        inputProc: logicStylePull,
        inputProcRefCon: Unmanaged.passUnretained(context).toOpaque()
    )

    let inputFormatStatus = AudioUnitSetProperty(
        unit,
        kAudioUnitProperty_StreamFormat,
        kAudioUnitScope_Input,
        0,
        &format,
        formatSize
    )
    let outputFormatStatus = AudioUnitSetProperty(
        unit,
        kAudioUnitProperty_StreamFormat,
        kAudioUnitScope_Output,
        0,
        &format,
        formatSize
    )
    guard inputFormatStatus == noErr, outputFormatStatus == noErr else {
        error = "AudioUnitSetProperty(StreamFormat) failed"
        return
    }

    callbackStatus = AudioUnitSetProperty(
        unit,
        kAudioUnitProperty_SetRenderCallback,
        kAudioUnitScope_Input,
        0,
        &callback,
        UInt32(MemoryLayout<AURenderCallbackStruct>.size)
    )
    guard callbackStatus == noErr else {
        error = "AudioUnitSetProperty(SetRenderCallback) failed"
        return
    }

    initializeStatus = AudioUnitInitialize(unit)
    guard initializeStatus == noErr else {
        error = "AudioUnitInitialize failed"
        return
    }
    defer { AudioUnitUninitialize(unit) }

    let frameCount = Int(args.blockSize)
    let left = UnsafeMutablePointer<Float32>.allocate(capacity: frameCount)
    let right = UnsafeMutablePointer<Float32>.allocate(capacity: frameCount)
    defer {
        left.deallocate()
        right.deallocate()
    }
    let list = AudioBufferList.allocate(maximumBuffers: 2)
    defer { free(list.unsafeMutablePointer) }

    var timestamp = AudioTimeStamp()
    timestamp.mSampleTime = 0
    timestamp.mFlags = .sampleTimeValid

    for block in 0..<args.blocks {
        list.count = 2
        list[0] = AudioBuffer(
            mNumberChannels: 1,
            mDataByteSize: args.blockSize * UInt32(MemoryLayout<Float32>.size),
            mData: UnsafeMutableRawPointer(left)
        )
        list[1] = AudioBuffer(
            mNumberChannels: 1,
            mDataByteSize: args.blockSize * UInt32(MemoryLayout<Float32>.size),
            mData: UnsafeMutableRawPointer(right)
        )

        var flags = AudioUnitRenderActionFlags()
        renderStatus = AudioUnitRender(
            unit,
            &flags,
            &timestamp,
            0,
            args.blockSize,
            list.unsafeMutablePointer
        )
        if renderStatus != noErr {
            break
        }
        if rms(UnsafePointer(left), count: frameCount) > 0.0001,
           firstNonSilentSample < 0 {
            firstNonSilentSample = block * frameCount
        }
        timestamp.mSampleTime += Double(args.blockSize)
    }
}

run()

let silent = firstNonSilentSample < 0
let json = """
{"target_found":\(targetFound),"callback_status":\(callbackStatus),"initialize_status":\(initializeStatus),"render_status":\(renderStatus),"first_nonsilent_sample":\(firstNonSilentSample),"rendered_samples":\(args.blocks * Int(args.blockSize)),"silent":\(silent),"error":"\(error)"}
"""
try? json.write(toFile: args.outputPath, atomically: true, encoding: .utf8)
print(json)
