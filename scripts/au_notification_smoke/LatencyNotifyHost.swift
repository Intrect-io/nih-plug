// Bright의 지연 변경 알림이 호스트의 추가 property 조회 없이 도착하는지 확인한다.
// 실제 DAW의 PDC/오디오 품질 검사를 대체하지 않는 AU 계약 회귀 검사다.
import AudioToolbox
import Foundation
import Darwin

final class Notifications {
    var events: [[String: Any]] = []
    var unitToDispose: AudioUnit?
    var disposeOnNext = false
    var disposalStatus: OSStatus?
    var disposeOnWorker = false
    var removeOnNext = false
    var removalStatus: OSStatus?
    var removalWasOnMain: Bool?
    var disposalWasOnMain: Bool?
    var workerTimedOut = false
}

final class WorkerOutcome {
    let lock = NSLock()
    var status: OSStatus = -99999
    var onMain = true
}
func onWorker(_ operation: @escaping () -> OSStatus) -> (OSStatus, Bool, Bool) {
    let result = WorkerOutcome()
    let done = DispatchSemaphore(value: 0)
    DispatchQueue.global().async {
        let status = operation()
        result.lock.lock()
        result.status = status
        result.onMain = Thread.isMainThread
        result.lock.unlock()
        done.signal()
    }
    let timedOut = done.wait(timeout: .now() + 2) != .success
    result.lock.lock()
    defer { result.lock.unlock() }
    return (result.status, result.onMain, timedOut)
}

let propertyListener: AudioUnitPropertyListenerProc = { ref, _, property, scope, element in
    let state = Unmanaged<Notifications>.fromOpaque(ref).takeUnretainedValue()
    state.events.append(["property": property, "scope": scope, "element": element,
                         "main_thread": Thread.isMainThread])
    if state.removeOnNext, let unit = state.unitToDispose {
        state.removeOnNext = false
        let (status, main, timeout) = onWorker {
            AudioUnitRemovePropertyListenerWithUserData(unit, kAudioUnitProperty_Latency,
                propertyListener, ref)
        }
        state.removalStatus = status
        state.removalWasOnMain = main
        state.workerTimedOut = timeout
    }
    if state.disposeOnNext, let unit = state.unitToDispose {
        state.disposeOnNext = false
        state.unitToDispose = nil
        if state.disposeOnWorker {
            let (status, main, timeout) = onWorker { AudioComponentInstanceDispose(unit) }
            state.disposalStatus = status
            state.disposalWasOnMain = main
            state.workerTimedOut = state.workerTimedOut || timeout
        } else {
            state.disposalStatus = AudioComponentInstanceDispose(unit)
            state.disposalWasOnMain = Thread.isMainThread
        }
    }
}

// 입력을 제공하되 청감/출력 품질에 관한 판정은 하지 않는다.
let inputCallback: AURenderCallback = { _, _, _, _, frames, data in
    guard let data = data else { return kAudio_ParamError }
    for buffer in UnsafeMutableAudioBufferListPointer(data) {
        guard let samples = buffer.mData?.assumingMemoryBound(to: Float.self),
              buffer.mDataByteSize >= frames * 4 else { return kAudio_ParamError }
        for i in 0..<Int(frames) { samples[i] = Float(sin(Double(i) * 0.05) * 0.25) }
    }
    return noErr
}

struct HostError: Error { let operation: String; let status: OSStatus }
func check(_ status: OSStatus, _ operation: String) throws {
    if status != noErr { throw HostError(operation: operation, status: status) }
}

func fourCC(_ value: String) -> OSType {
    value.utf8.reduce(0) { ($0 << 8) | OSType($1) }
}

func probe(disposeOnWorker: Bool) throws -> [String: Any] {
    var description = AudioComponentDescription(componentType: fourCC("aufx"),
        componentSubType: fourCC("dlrt"), componentManufacturer: fourCC("INRT"),
        componentFlags: 0, componentFlagsMask: 0)
    guard CommandLine.arguments.count == 3 else { throw HostError(operation: "fixture path required", status: -1) }
    do {
        description.componentSubType = fourCC("dlqn")
        guard let handle = dlopen(CommandLine.arguments[2], RTLD_NOW | RTLD_LOCAL),
              let symbol = dlsym(handle, "nih_plug_au_factory") else {
            throw HostError(operation: "load diagnostic fixture factory", status: -1)
        }
        let factory = unsafeBitCast(symbol, to: AudioComponentFactoryFunction.self)
        _ = AudioComponentRegister(&description, "Notification fixture" as CFString, 1, factory)
        // 등록한 factory는 프로세스 종료까지 유효하게 유지한다.
    }
    guard let component = AudioComponentFindNext(nil, &description) else {
        throw HostError(operation: "find de-leak", status: -1)
    }
    var instance: AudioUnit?
    try check(AudioComponentInstanceNew(component, &instance), "instantiate")
    guard let unit = instance else { throw HostError(operation: "null instance", status: -1) }
    let notifications = Notifications()
    notifications.unitToDispose = unit
    notifications.disposeOnWorker = disposeOnWorker
    defer { if notifications.unitToDispose != nil { AudioComponentInstanceDispose(unit) } }
    var format = AudioStreamBasicDescription(mSampleRate: 48000,
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsNonInterleaved,
        mBytesPerPacket: 4, mFramesPerPacket: 1, mBytesPerFrame: 4,
        mChannelsPerFrame: 2, mBitsPerChannel: 32, mReserved: 0)
    for scope in [kAudioUnitScope_Input, kAudioUnitScope_Output] {
        try check(AudioUnitSetProperty(unit, kAudioUnitProperty_StreamFormat, scope, 0,
            &format, UInt32(MemoryLayout.size(ofValue: format))), "stream format")
    }
    var callback = AURenderCallbackStruct(inputProc: inputCallback, inputProcRefCon: nil)
    try check(AudioUnitSetProperty(unit, kAudioUnitProperty_SetRenderCallback,
        kAudioUnitScope_Input, 0, &callback, UInt32(MemoryLayout.size(ofValue: callback))), "input")
    // 파라미터 ID와 이름을 함께 확인해 순서 변경 시 다른 값을 조작하지 않는다.
    var info = AudioUnitParameterInfo()
    var infoSize = UInt32(MemoryLayout.size(ofValue: info))
    try check(AudioUnitGetProperty(unit, kAudioUnitProperty_ParameterInfo,
        kAudioUnitScope_Global, 0, &info, &infoSize), "Bright info")
    defer { info.cfNameString?.release(); info.unitName?.release() }
    let name = withUnsafePointer(to: &info.name) {
        $0.withMemoryRebound(to: CChar.self, capacity: 52) { String(cString: $0) }
    }
    guard name == "Bright", info.minValue == 0, info.maxValue == 1 else {
        throw HostError(operation: "Bright identity/range: \(name)", status: -1)
    }
    try check(AudioUnitInitialize(unit), "initialize")
    defer { if notifications.unitToDispose != nil { AudioUnitUninitialize(unit) } }
    let left = UnsafeMutablePointer<Float>.allocate(capacity: 512)
    let right = UnsafeMutablePointer<Float>.allocate(capacity: 512)
    defer { left.deallocate(); right.deallocate() }
    let buffers = AudioBufferList.allocate(maximumBuffers: 2)
    defer { free(buffers.unsafeMutablePointer) }
    buffers.count = 2
    var timestamp = AudioTimeStamp()
    timestamp.mFlags = .sampleTimeValid
    func render() throws {
        for _ in 0..<4 {
            buffers[0] = AudioBuffer(mNumberChannels: 1, mDataByteSize: 2048, mData: left)
            buffers[1] = AudioBuffer(mNumberChannels: 1, mDataByteSize: 2048, mData: right)
            var flags = AudioUnitRenderActionFlags()
            try check(AudioUnitRender(unit, &flags, &timestamp, 0, 512,
                buffers.unsafeMutablePointer), "render")
            timestamp.mSampleTime += 512
        }
    }
    func latency() throws -> Double {
        var seconds = 0.0
        var size = UInt32(MemoryLayout.size(ofValue: seconds))
        try check(AudioUnitGetProperty(unit, kAudioUnitProperty_Latency,
            kAudioUnitScope_Global, 0, &seconds, &size), "latency")
        return seconds
    }
    try render()
    let base = try latency()
    let ref = Unmanaged.passUnretained(notifications).toOpaque()
    try check(AudioUnitAddPropertyListener(unit, kAudioUnitProperty_Latency,
        propertyListener, ref), "add listener")
    defer {
        if notifications.unitToDispose != nil {
            AudioUnitRemovePropertyListenerWithUserData(unit, kAudioUnitProperty_Latency,
                propertyListener, ref)
        }
    }
    var transitions: [[String: Any]] = []
    for bright: Float in [1, 0] {
        let before = notifications.events.count
        try check(AudioUnitSetParameter(unit, 0, kAudioUnitScope_Global, 0, bright, 0), "Bright")
        try render()
        // 이 구간에는 AU property 호출이 없다. 알림 자체가 도착해야 한다.
        let deadline = Date().addingTimeInterval(0.3)
        while Date() < deadline {
            RunLoop.main.run(until: Date().addingTimeInterval(0.01))
        }
        let withoutQuery = notifications.events.count - before
        let seconds = try latency()
        transitions.append(["bright": bright, "latency_seconds": seconds,
            "notifications_without_query": withoutQuery,
            "notifications_after_query": notifications.events.count - before])
    }
    let positive = transitions[0]["latency_seconds"] as! Double
    let restored = transitions[1]["latency_seconds"] as! Double
    // The main callback waits for a worker removal. It must return an error
    // promptly instead of waiting for the callback that is waiting for it.
    notifications.removeOnNext = true
    try check(AudioUnitSetParameter(unit, 0, kAudioUnitScope_Global, 0, 1, 0), "Bright before busy removal")
    try render()
    RunLoop.main.run(until: Date().addingTimeInterval(0.3))
    let busyRemovalRejected = notifications.removalStatus == kAudioUnitErr_CannotDoInCurrentContext &&
        notifications.removalWasOnMain == false && !notifications.workerTimedOut
    let beforePreserved = notifications.events.count
    try check(AudioUnitSetParameter(unit, 0, kAudioUnitScope_Global, 0, 0, 0), "Bright after busy removal")
    try render()
    RunLoop.main.run(until: Date().addingTimeInterval(0.3))
    let listenerPreserved = notifications.events.count > beforePreserved
    let retryRemoval = AudioUnitRemovePropertyListenerWithUserData(unit, kAudioUnitProperty_Latency,
        propertyListener, ref)
    try check(retryRemoval, "retry removal after callback")
    try check(AudioUnitAddPropertyListener(unit, kAudioUnitProperty_Latency, propertyListener, ref), "restore listener")

    // Actual AudioComponent disposal from its own notification callback must
    // return safely, and no later timer callback may reach this listener.
    notifications.disposeOnNext = true
    try check(AudioUnitSetParameter(unit, 0, kAudioUnitScope_Global, 0, 1, 0), "Bright before disposal")
    try render()
    RunLoop.main.run(until: Date().addingTimeInterval(0.3))
    let disposed = notifications.unitToDispose == nil && notifications.disposalStatus == noErr
    let countAfterDispose = notifications.events.count
    RunLoop.main.run(until: Date().addingTimeInterval(0.1))
    let noPostDispose = notifications.events.count == countAfterDispose
    let passed = busyRemovalRejected && listenerPreserved && disposed && noPostDispose &&
        !notifications.workerTimedOut && notifications.disposalWasOnMain == !disposeOnWorker && positive > base && abs(restored - base) < 1e-9 &&
        transitions.allSatisfy { ($0["notifications_without_query"] as! Int) > 0 } &&
        notifications.events.allSatisfy { ($0["main_thread"] as! Bool) }
    return ["status": passed ? "PASS" : "FAIL", "base_latency_seconds": base,
            "transitions": transitions, "notifications": notifications.events,
            "reentrant_disposal": disposed, "disposal_status": notifications.disposalStatus ?? -1,
            "no_post_dispose_notifications": noPostDispose,
            "close_mode": disposeOnWorker ? "worker" : "main",
            "disposal_on_main": notifications.disposalWasOnMain ?? true,
            "busy_removal_status": notifications.removalStatus ?? -1,
            "busy_removal_rejected": busyRemovalRejected, "listener_preserved": listenerPreserved,
            "retry_removal_status": retryRemoval, "worker_timeout": notifications.workerTimedOut,
            "limit": "AU notification contract only; not DAW/audio acceptance"]
}

let output = CommandLine.arguments.dropFirst().first
guard let output = output else { fatalError("result JSON path required") }
var result: [String: Any]
do {
    let cases = try [probe(disposeOnWorker: false), probe(disposeOnWorker: true)]
    result = ["status": cases.allSatisfy { $0["status"] as? String == "PASS" } ? "PASS" : "FAIL",
              "cases": cases]
}
catch let error as HostError {
    result = ["status": "ERROR", "operation": error.operation, "osstatus": error.status]
} catch { result = ["status": "ERROR", "error": String(describing: error)] }
let data = try JSONSerialization.data(withJSONObject: result, options: [.prettyPrinted, .sortedKeys])
try data.write(to: URL(fileURLWithPath: output), options: .atomic)
exit(result["status"] as? String == "PASS" ? 0 : 1)
