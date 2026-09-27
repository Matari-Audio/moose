/// AU v3 Swift implementation - delegates all plugin logic to the Rust
/// framework via C FFI (g_callbacks function pointer table).
import os.log
import AudioToolbox
import CoreMIDI

private let logger = Logger(subsystem: "com.moose.au3", category: "AUExt")
import AVFAudio
import CoreAudioKit

#if os(iOS)
import UIKit
// AppKit `NSView` / `NSSize` / `NSRect` are macOS-only. The iOS
// AUv3 view-controller hosts a UIView; the helpers below alias
// the AppKit types to their UIKit equivalents so most of the
// factory code stays platform-agnostic.
typealias NSView = UIView
typealias NSSize = CGSize
typealias NSRect = CGRect
#else
import AppKit
#endif

// MARK: - UMP helpers

/// `AURenderEventMIDISysEx` / `AURenderEventMIDIEventList` raw values from
/// `AudioToolbox/AudioUnitProperties.h`. Compare the imported event's raw
/// discriminator so the code does not depend on Swift overlay case spelling.
let kAURenderEventMIDISysExRaw: UInt8 = 9
let kAURenderEventMIDIEventListRaw: UInt8 = 10
private let auMIDIDataOffset = MemoryLayout<AUMIDIEvent>.offset(of: \AUMIDIEvent.data)!
private let auMIDIEventListOffset =
    MemoryLayout<AUMIDIEventList>.offset(of: \AUMIDIEventList.eventList)!
private let midiEventListPacketOffset =
    MemoryLayout<MIDIEventList>.offset(of: \MIDIEventList.packet)!
private let midiEventPacketWordsOffset =
    MemoryLayout<MIDIEventPacket>.offset(of: \MIDIEventPacket.words)!

/// The ABI tail version the plugin binary declares, or 0 when the
/// version word lacks its 'TAu\0' magic tag. A pre-2.0 binary has the
/// `create` function pointer at offset 0; without the magic check its
/// low bits would masquerade as a version and every gated tail read
/// would land one slot off. The renamed `moose_au_register_v2` only
/// protects the staticlib/shim link inside the framework; this appex
/// binds the framework through `g_callbacks` / `g_descriptor` /
/// `moose_au_init`, whose names predate 2.0, so version skew across
/// that boundary is caught only by this runtime check.
func mooseAbiTailVersion(_ cb: UnsafePointer<AuCallbacks>) -> UInt32 {
    let word = cb.pointee.abi_version
    guard word & MOOSE_AU_ABI_MAGIC_MASK == MOOSE_AU_ABI_MAGIC else { return 0 }
    return word & 0xFF
}

/// UMP packet length in 32-bit words by message type. Spec: MIDI
/// 2.0 M2-104-UM, §2.1.4 (Message Type field).
@inline(__always) func umpPacketLength(messageType mt: UInt8) -> Int {
    switch mt & 0xF {
    case 0x0, 0x1, 0x2, 0x6, 0x7: return 1
    case 0x3, 0x4, 0x8, 0x9, 0xA: return 2
    case 0xB, 0xC: return 3
    default: return 4
    }
}

@inline(__always) func umpProtocolAccepts(_ protocolID: UInt8, _ messageType: UInt8) -> Bool {
    switch messageType & 0xF {
    case 0x2: return protocolID == 1
    case 0x4: return protocolID == 2
    default: return protocolID == 1 || protocolID == 2
    }
}

@inline(__always) func sampleOffset(
    _ eventTime: AUEventSampleTime, _ bufferStart: Int64, _ frameCount: UInt32
) -> UInt32? {
    let (relative, overflow) = eventTime.subtractingReportingOverflow(bufferStart)
    guard !overflow, relative >= 0, relative < Int64(frameCount) else { return nil }
    return UInt32(relative)
}

/// Forward every UMP in Apple's native `AUMIDIEventList` layout without
/// decoding or changing its protocol, cable, words, or packet width.
func forwardMIDIEventList(
    event: UnsafePointer<AURenderEvent>,
    bufStart: Int64,
    frameCount: UInt32,
    nativeBuf: UnsafeMutablePointer<AuNativeEvent>,
    nativeCount: inout UInt32,
    overflow: inout UInt32
) -> Bool {
    let native = UnsafeRawPointer(event).assumingMemoryBound(to: AUMIDIEventList.self)
    let list = UnsafeRawPointer(native).advanced(by: auMIDIEventListOffset)
        .assumingMemoryBound(to: MIDIEventList.self)
    let rawProtocol = list.pointee.protocol.rawValue
    guard rawProtocol == 1 || rawProtocol == 2 else { return false }
    let protocolID = UInt8(rawProtocol)
    var packet = UnsafeRawPointer(list).advanced(by: midiEventListPacketOffset)
        .assumingMemoryBound(to: MIDIEventPacket.self)
    let numPackets = list.pointee.numPackets
    let eventTime = native.pointee.eventSampleTime
    var packetIdx: UInt32 = 0
    while packetIdx < numPackets {
        let wordCount = packet.pointee.wordCount
        guard wordCount > 0,
              let packetOffset = Int64(exactly: packet.pointee.timeStamp) else {
            return false
        }
        let (packetTime, timeOverflow) = eventTime.addingReportingOverflow(packetOffset)
        guard !timeOverflow,
              let offset = sampleOffset(packetTime, bufStart, frameCount) else { return false }
        let words = UnsafeRawPointer(packet).advanced(by: midiEventPacketWordsOffset)
            .assumingMemoryBound(to: UInt32.self)
        var i: UInt32 = 0
        while i < wordCount {
            let w0 = words[Int(i)]
            let mt = UInt8((w0 >> 28) & 0xF)
            let packetWords = UInt32(umpPacketLength(messageType: mt))
            if packetWords > wordCount - i { return false }
            if nativeCount < 256 {
                var out = AuNativeEvent()
                out.sample_offset = offset
                out.port = UInt16(native.pointee.cable)
                out.kind = UInt8(AU_NATIVE_EVENT_UMP)
                out.protocol = protocolID
                out.data_len = packetWords
                out.words = (
                    w0,
                    packetWords > 1 ? words[Int(i + 1)] : 0,
                    packetWords > 2 ? words[Int(i + 2)] : 0,
                    packetWords > 3 ? words[Int(i + 3)] : 0)
                nativeBuf[Int(nativeCount)] = out
                nativeCount += 1
            } else {
                overflow = 1
            }
            i += packetWords
        }
        packet = UnsafePointer(MIDIEventPacketNext(packet))
        packetIdx += 1
    }
    return true
}

// A deinterleaved float32 format for `channels`. `standardFormat` only
// defines mono/stereo layouts and returns nil for wider counts, so a
// surround bus (declared via a multi-entry `bus_layouts()`) needs an
// explicit channel layout or the force-unwrap would trap at init and the
// host would see the appex fail to open (OpenAComponent 4097).
func mooseAudioFormat(sampleRate: Double, channels: AVAudioChannelCount) -> AVAudioFormat? {
    if channels <= 2 {
        return AVAudioFormat(standardFormatWithSampleRate: sampleRate, channels: channels)
    }
    let tag: AudioChannelLayoutTag
    switch channels {
    case 3: tag = kAudioChannelLayoutTag_MPEG_3_0_A
    case 4: tag = kAudioChannelLayoutTag_Quadraphonic
    case 5: tag = kAudioChannelLayoutTag_MPEG_5_0_A
    case 6: tag = kAudioChannelLayoutTag_MPEG_5_1_A
    case 7: tag = kAudioChannelLayoutTag_MPEG_6_1_A
    case 8: tag = kAudioChannelLayoutTag_MPEG_7_1_A
    default: return nil
    }
    guard let layout = AVAudioChannelLayout(layoutTag: tag) else { return nil }
    return AVAudioFormat(standardFormatWithSampleRate: sampleRate, channelLayout: layout)
}

// MARK: - AUAudioUnit subclass

class MooseAUAudioUnit: AUAudioUnit {
    private(set) var rustCtx: UnsafeMutableRawPointer?
    /// Set to true during GUI→host param sync to prevent observer feedback.
    var isSyncingToHost = false

    private var _inputBusArray: AUAudioUnitBusArray!
    private var _outputBusArray: AUAudioUnitBusArray!
    private var _parameterTree: AUParameterTree?
    private var _sampleRate: Double = 44100.0
    private var _maxFrames: UInt32 = 1024
    /// Last latency (samples) pushed to the host via KVO. The framework
    /// refreshes its latency cache each block; a main-thread timer
    /// compares against this and fires KVO on `latency` when it moves, so
    /// a plugin that varies its latency reaches the host.
    private var _lastLatencySamples: UInt32 = 0
    /// Polls `latency` while render resources are allocated, so a latency
    /// change driven by host automation (no editor open) still notifies.
    /// Slow (a few Hz) - latency moves on mode switches, not per block.
    private var _latencyTimer: Timer?

    override init(componentDescription: AudioComponentDescription,
                  options: AudioComponentInstantiationOptions = []) throws {
        try super.init(componentDescription: componentDescription, options: options)

        guard let callbacks = g_callbacks, let descriptor = g_descriptor else { return }

        // A pre-2.0 framework loads cleanly (the globals above kept
        // their names) but lays out `AuCallbacks` differently -
        // calling `create` there would jump through the wrong slot.
        // Refuse instantiation instead of crashing the extension.
        guard mooseAbiTailVersion(callbacks) >= 7 else {
            logger.error("AU init: plugin binary lacks the native event ABI; rebuild the framework and appex with the same moose version")
            throw NSError(domain: NSOSStatusErrorDomain,
                          code: Int(kAudioUnitErr_FailedInitialization))
        }

        rustCtx = callbacks.pointee.create()
        logger.info("AU init: in=\(descriptor.pointee.num_inputs) out=\(descriptor.pointee.num_outputs)")
        let numIn = descriptor.pointee.num_inputs
        let numOut = descriptor.pointee.num_outputs

        if numIn > 0 {
            guard let inputFmt = mooseAudioFormat(sampleRate: 44100, channels: numIn) else {
                throw NSError(domain: NSOSStatusErrorDomain,
                              code: Int(kAudioUnitErr_FormatNotSupported))
            }
            // Bus 0 is the main input. A declared sidechain adds bus 1 so
            // the host can route a separate source to it; the render block
            // pulls it and concatenates its channels after the main ones.
            let mainBus = try AUAudioUnitBus(format: inputFmt)
            mainBus.name = "Input"
            var inBusses = [mainBus]
            let scChans = descriptor.pointee.sidechain_in_channels
            if scChans > 0 {
                guard let scFmt = mooseAudioFormat(sampleRate: 44100, channels: scChans) else {
                    throw NSError(domain: NSOSStatusErrorDomain,
                                  code: Int(kAudioUnitErr_FormatNotSupported))
                }
                let scBus = try AUAudioUnitBus(format: scFmt)
                scBus.name = "Sidechain"
                inBusses.append(scBus)
            }
            _inputBusArray = AUAudioUnitBusArray(audioUnit: self, busType: .input, busses: inBusses)
        } else {
            _inputBusArray = AUAudioUnitBusArray(audioUnit: self, busType: .input, busses: [])
        }

        // Even `aumi` (MIDI Processor, num_outputs=0) needs an
        // output bus: Apple's AUv3 framework uses it to negotiate
        // the sample rate for MIDI timing, and rejects plugins
        // with an empty output bus array via -10868
        // (kAudioUnitErr_FormatNotSupported) at instantiation.
        // The output bus exists purely so the framework can read a
        // sample rate from its format - no audio is ever written
        // to it. The render block below memsets the output buffer
        // to 0 for aumi plugins to satisfy strict hosts that audit
        // for stale data.
        // (numOut=0 in `render()` skips the output pointer setup).
        let outChans = numOut > 0 ? numOut : 2
        guard let outputFmt = mooseAudioFormat(sampleRate: 44100, channels: outChans) else {
            throw NSError(domain: NSOSStatusErrorDomain,
                          code: Int(kAudioUnitErr_FormatNotSupported))
        }
        let outBus = try AUAudioUnitBus(format: outputFmt)
        _outputBusArray = AUAudioUnitBusArray(audioUnit: self, busType: .output, busses: [outBus])

        buildParameterTree()

    }

    deinit {
        if let ctx = rustCtx, let callbacks = g_callbacks {
            callbacks.pointee.destroy(ctx)
        }
    }

    override var inputBusses: AUAudioUnitBusArray { _inputBusArray }
    override var outputBusses: AUAudioUnitBusArray { _outputBusArray }

    // MARK: - AU v3 view resizing

    /// Accept every view configuration the host proposes. This is what
    /// surfaces the resize / expand affordance in hosts like GarageBand -
    /// without it the host treats the AU v3 view as a single fixed size and
    /// never offers to enlarge it, regardless of the `resizable`
    /// AudioComponents tag. Returning all indices says "we can render at any
    /// size the host offers"; the embedded editor reflows to the host bounds
    /// in the view controller's layout pass (`fitGUIToSafeArea`).
    override func supportedViewConfigurations(
        _ availableViewConfigurations: [AUAudioUnitViewConfiguration]
    ) -> IndexSet {
        IndexSet(integersIn: availableViewConfigurations.indices)
    }

    /// Host picked one of the configurations reported above. The hosted view
    /// tracks its parent's bounds and refits on the next layout pass, so this
    /// only needs to exist for the host's `select` call to succeed.
    override func select(_ viewConfiguration: AUAudioUnitViewConfiguration) {}

    override var parameterTree: AUParameterTree? {
        get { _parameterTree }
        set { _parameterTree = newValue }
    }

    /// MIDI output ports exposed to the host, gated on the plugin's
    /// `emits_midi` capability (`has_midi_output` in the descriptor;
    /// note-effect default, overridable via `midi_output` in
    /// moose.toml). `aumi` MIDI Processors must advertise one for
    /// Apple's AU infrastructure to accept instantiation; an
    /// instrument or effect that opts in gets one too, and a plugin
    /// that emits no MIDI returns an empty array so hosts don't
    /// surface a phantom port.
    override var midiOutputNames: [String] {
        guard let d = g_descriptor?.pointee, d.has_midi_output != 0 else { return [] }
        // One named output per declared MIDI output port. The plugin
        // routes each event to a port via `Event::port`, which the
        // render drain passes as the `cable` to `midiOutputEventBlock`.
        // Numbered only when there's more than one, so the common
        // single-port case keeps the plain "MIDI Out" label.
        let n = max(1, Int(d.midi_output_ports))
        if n == 1 { return ["MIDI Out"] }
        return (1...n).map { "MIDI Out \($0)" }
    }

    override var virtualMIDICableCount: Int {
        Int(g_descriptor?.pointee.midi_input_ports ?? 0)
    }

    /// MIDI protocol the host delivers *input* in. Declaring 2.0 makes the
    /// host send native UMP MIDI 2.0 (NoteOn2 / PerNoteCC / ...) through the
    /// render-event MIDI list, which the Rust side decodes; declaring 1.0
    /// makes the host down-convert first. Gated on `midi2_input` only - a
    /// 1.0->2.0 promoter (`midi2_output` without `midi2_input`) wants 1.0
    /// input it can read, and emits 2.0 on its own *output* stream, which is
    /// negotiated separately (see the output drain below). A plugin that
    /// didn't ask for MIDI 2.0 input never sees the 2.0 variants - the same
    /// contract as CLAP (which only advertises `CLAP_NOTE_DIALECT_MIDI2`
    /// when opted in). Without this override the default is 1.0, so the Rust
    /// 2.0 decode path would stay dormant.
    @available(macOS 12.0, iOS 15.0, *)
    override var audioUnitMIDIProtocol: MIDIProtocolID {
        if let d = g_descriptor?.pointee, d.midi2_input != 0 {
            return ._2_0
        }
        return ._1_0
    }

    private func buildParameterTree() {
        // We need `rustCtx` to be non-nil (the body uses `rustCtx!`
        // below) but the value is only consumed through the closures
        // that capture it explicitly, so the `let ctx = ...` binding
        // would be unused. `case .some` matches both the
        // existence check and silences the unused-binding warning
        // without forcing a runtime !-unwrap here.
        guard let callbacks = g_callbacks, case .some = rustCtx else { return }
        let rawCtx = rustCtx!
        let cb = callbacks.pointee
        var params: [AUParameter] = []
        var groups: [String: [AUParameter]] = [:]

        for i in 0..<g_num_params {
            let desc = g_param_descriptors.advanced(by: Int(i)).pointee
            let name = String(cString: desc.name)
            let group = String(cString: desc.group)

            // step_count is values-1: 1 => boolean-like, >=2 => an indexed
            // list. Reporting the unit (not .generic) makes the host
            // quantize automation to the steps and offer step navigation;
            // valueStrings carries the per-index names from the plugin's
            // own format_value, so an enum shows its variant names.
            var auUnit: AudioUnitParameterUnit = .generic
            var valueStrings: [String]? = nil
            if desc.step_count >= 1 {
                auUnit = desc.step_count == 1 ? .boolean : .indexed
                var names: [String] = []
                for step in 0...desc.step_count {
                    let plain = desc.min + Double(step)
                    var buf = [CChar](repeating: 0, count: 128)
                    let len = cb.param_format_value(rawCtx, desc.id, plain, &buf, 128)
                    names.append(len > 0 ? String(cString: buf) : "\(step)")
                }
                valueStrings = names
            }

            let param = AUParameterTree.createParameter(
                withIdentifier: "param\(desc.id)", name: name,
                address: AUParameterAddress(desc.id),
                min: AUValue(desc.min), max: AUValue(desc.max),
                unit: auUnit, unitName: nil,
                flags: [.flag_IsWritable, .flag_IsReadable],
                valueStrings: valueStrings, dependentParameters: nil)
            param.value = AUValue(desc.default_value)
            if group.isEmpty { params.append(param) }
            else { groups[group, default: []].append(param) }
        }

        var children: [AUParameterNode] = params
        for (gn, gp) in groups {
            children.append(AUParameterTree.createGroup(withIdentifier: gn, name: gn, children: gp))
        }
        _parameterTree = AUParameterTree.createTree(withChildren: children)

        _parameterTree?.implementorValueObserver = { [weak self] p, v in
            guard self?.isSyncingToHost != true else { return }
            cb.param_set_value(rawCtx, UInt32(p.address), Double(v))
        }
        _parameterTree?.implementorValueProvider = { p in
            AUValue(cb.param_get_value(rawCtx, UInt32(p.address)))
        }
        _parameterTree?.implementorStringFromValueCallback = { p, vp in
            let val = vp?.pointee ?? p.value
            var buf = [CChar](repeating: 0, count: 128)
            let len = cb.param_format_value(rawCtx, UInt32(p.address), Double(val), &buf, 128)
            return len > 0 ? String(cString: buf) : String(format: "%.2f", val)
        }
        _parameterTree?.implementorValueFromStringCallback = { p, str in
            // `param_parse_value` is an ABI v5 tail callback: on an older
            // plugin binary the pointer is past its tail, so gate on the
            // version and fall back to a plain float parse (what AU does
            // by default) rather than calling through it.
            if mooseAbiTailVersion(callbacks) >= 5 {
                var plain = 0.0
                let ok = str.withCString { cstr in
                    cb.param_parse_value(rawCtx, UInt32(p.address), cstr, &plain)
                }
                if ok != 0 { return AUValue(plain) }
            }
            return AUValue(Float(str) ?? p.value)
        }
    }

    // MARK: Render

    override func allocateRenderResources() throws {
        try super.allocateRenderResources()
        if _outputBusArray.count > 0 { _sampleRate = _outputBusArray[0].format.sampleRate }
        _maxFrames = maximumFramesToRender
        if let ctx = rustCtx, let cb = g_callbacks {
            // Forward the host's offline-render flag before prep so the
            // plugin's reset / process observe the right ProcessMode
            // (0 realtime, 2 offline). `set_render_mode` is an ABI v4
            // tail callback - gate so an older plugin binary is never
            // called through a pointer past its tail.
            if mooseAbiTailVersion(cb) >= 4 {
                let mode: UInt32 = isRenderingOffline ? 2 : 0
                cb.pointee.set_render_mode(ctx, mode)
            }
            cb.pointee.reset(ctx, _sampleRate, _maxFrames)
        }
        // Watch for dynamic-latency changes on the main thread while
        // rendering (KVO must fire off the audio thread). Scheduled on
        // the main run loop even if the host allocates on another thread.
        DispatchQueue.main.async { [weak self] in
            guard let self = self else { return }
            self._latencyTimer?.invalidate()
            self._latencyTimer = Timer.scheduledTimer(withTimeInterval: 0.2, repeats: true) {
                [weak self] _ in
                self?.notifyLatencyIfChanged()
            }
        }
    }

    override func deallocateRenderResources() {
        DispatchQueue.main.async { [weak self] in
            self?._latencyTimer?.invalidate()
            self?._latencyTimer = nil
        }
        super.deallocateRenderResources()
    }

    private static func render(
        ctx: UnsafeMutableRawPointer, cb: UnsafePointer<AuCallbacks>,
        numIn: UInt32, numOut: UInt32,
        hasAudioOutput: Bool,
        timestamp: UnsafePointer<AudioTimeStamp>, frameCount: UInt32,
        outputData: UnsafeMutablePointer<AudioBufferList>,
        events: UnsafePointer<AURenderEvent>?, pull: AURenderPullInputBlock?,
        inPtrs: UnsafeMutablePointer<UnsafePointer<Float>?>,
        outPtrs: UnsafeMutablePointer<UnsafeMutablePointer<Float>?>,
        nativeBuf: UnsafeMutablePointer<AuNativeEvent>,
        scCh: Int,
        mainInputEnabled: Bool,
        sidechainEnabled: Bool,
        mainOutputEnabled: Bool,
        scScratch: UnsafeMutablePointer<Float>?,
        scABL: UnsafeMutableAudioBufferListPointer?,
        scMaxFrames: Int,
        mainInScratch: UnsafeMutablePointer<Float>?,
        mainInABL: UnsafeMutableAudioBufferListPointer?,
        paramBuf: UnsafeMutablePointer<AuParamEvent>,
        transportBuf: UnsafeMutablePointer<AuTransportSnapshot>,
        sysexOutScratch: UnsafeMutablePointer<UInt8>,
        sysexOutScratchCap: Int,
        musicalContext: AUHostMusicalContextBlock?,
        transportState: AUHostTransportStateBlock?,
        midiOutputBlock: AUMIDIOutputEventBlock?,
        // Type-erased `AUMIDIEventListBlock?` (the UMP output block).
        // Passed as `Any?` so this signature doesn't reference the
        // macOS-12 / iOS-15-only type; cast back under `#available`.
        midiOutputListBlock: Any?,
        midiOutputProtocol: UInt32
    ) -> AUAudioUnitStatus {
        // Reject a block larger than the scratch was sized for: writing
        // frameCount frames at the scMaxFrames stride would overrun the
        // main-input / sidechain scratch. Mirrors the v2 shim's
        // kAudioUnitErr_TooManyFramesToProcess guard (au_v2_shim.c).
        if frameCount > UInt32(scMaxFrames) {
            return kAudioUnitErr_TooManyFramesToProcess
        }
        let abl = UnsafeMutableAudioBufferListPointer(outputData)
        if !hasAudioOutput {
            // AUv3 requires a dummy output bus for input-only and MIDI-only
            // units. It is transport-only: silence the host's negotiated
            // buffers here, but never expose their arbitrary width to Rust.
            for index in 0..<abl.count {
                let buffer = abl[index]
                let requestedBytes = Int(frameCount)
                    * max(Int(buffer.mNumberChannels), 1)
                    * MemoryLayout<Float>.size
                let bytes = min(requestedBytes, Int(buffer.mDataByteSize))
                if bytes > 0, let data = buffer.mData {
                    memset(data, 0, bytes)
                }
            }
        }
        if numIn > 0, let pull = pull {
            var f = AudioUnitRenderActionFlags()
            if let mainInScratch = mainInScratch, let mainInABL = mainInABL {
                // numIn > numOut: pull all input channels into the dedicated
                // scratch, not the too-narrow output ABL (which would drop
                // the channels past numOut).
                for c in 0..<Int(numIn) {
                    mainInABL[c] = AudioBuffer(
                        mNumberChannels: 1,
                        mDataByteSize: frameCount * UInt32(MemoryLayout<Float>.size),
                        mData: UnsafeMutableRawPointer(
                            mainInScratch.advanced(by: c * scMaxFrames)))
                }
                let s = pull(&f, timestamp, frameCount, 0, mainInABL.unsafeMutablePointer)
                if s != noErr { return s }
            } else {
                // numIn <= numOut: zero-copy in-place pull into the output ABL.
                let s = pull(&f, timestamp, frameCount, 0, outputData)
                if s != noErr { return s }
            }
        }
        var numNative: UInt32 = 0
        var nativeOverflow: UInt32 = 0
        var numParam: UInt32 = 0
        var paramOverflow: UInt32 = 0
        let sampleTime = timestamp.pointee.mSampleTime
        guard sampleTime.isFinite,
              sampleTime.rounded(.towardZero) == sampleTime,
              sampleTime >= Double(Int64.min),
              sampleTime < Double(Int64.max) else {
            return kAudio_ParamError
        }
        let bufStart = Int64(sampleTime)
        let maxAbsoluteOffset: UInt32 = bufStart < 0
            ? UInt32.max
            : UInt32(clamping: Int64.max - bufStart)
        var ev = events
        while let event = ev {
            let head = event.pointee.head
            if head.eventType == .MIDI {
                let native = UnsafeRawPointer(event).assumingMemoryBound(to: AUMIDIEvent.self)
                let m = native.pointee
                guard let offset = sampleOffset(m.eventSampleTime, bufStart, frameCount),
                      m.length > 0, m.length <= 3 else { return kAudio_ParamError }
                if numNative < 256 {
                    var out = AuNativeEvent()
                    out.sample_offset = offset
                    out.port = UInt16(m.cable)
                    out.kind = UInt8(AU_NATIVE_EVENT_MIDI1)
                    out.data_len = UInt32(m.length)
                    out.midi = m.data
                    nativeBuf[Int(numNative)] = out
                    numNative += 1
                } else {
                    nativeOverflow = 1
                }
            } else if head.eventType.rawValue == kAURenderEventMIDISysExRaw {
                let native = UnsafeRawPointer(event).assumingMemoryBound(to: AUMIDIEvent.self)
                let m = native.pointee
                let data = UnsafeRawPointer(native).advanced(by: auMIDIDataOffset)
                    .assumingMemoryBound(to: UInt8.self)
                guard let offset = sampleOffset(m.eventSampleTime, bufStart, frameCount),
                      m.length >= 2,
                      data[0] == 0xF0,
                      data[Int(m.length) - 1] == 0xF7 else {
                    return kAudio_ParamError
                }
                if numNative < 256 {
                    var out = AuNativeEvent()
                    out.sample_offset = offset
                    out.port = UInt16(m.cable)
                    out.kind = UInt8(AU_NATIVE_EVENT_SYSEX)
                    out.data_len = UInt32(m.length)
                    out.sysex = data
                    nativeBuf[Int(numNative)] = out
                    numNative += 1
                } else {
                    nativeOverflow = 1
                }
            } else if head.eventType.rawValue == kAURenderEventMIDIEventListRaw {
                guard forwardMIDIEventList(
                    event: event,
                    bufStart: bufStart,
                    frameCount: frameCount,
                    nativeBuf: nativeBuf,
                    nativeCount: &numNative,
                    overflow: &nativeOverflow) else { return kAudio_ParamError }
            } else if head.eventType == .parameter || head.eventType == .parameterRamp {
                // Decode .parameter / .parameterRamp into AuParamEvent
                // with the proper within-block sample offset so the
                // Rust chunker can split the audio block at each
                // automation point. Ramp events get treated as a step
                // at the ramp's start (eventSampleTime); the plugin's
                // own smoother handles the actual interpolation. This
                // matches moose-vst3's step-at-point treatment of VST3
                // parameter queues.
                //
                let absTime = event.pointee.parameter.eventSampleTime
                guard let offset = sampleOffset(absTime, bufStart, frameCount),
                      let paramID = UInt32(exactly: event.pointee.parameter.parameterAddress)
                else { return kAudio_ParamError }
                if numParam < 256 {
                    paramBuf[Int(numParam)] = AuParamEvent(
                        sample_offset: offset,
                        param_id: paramID,
                        value: event.pointee.parameter.value)
                    numParam += 1
                } else {
                    paramOverflow = 1
                }
            } else {
                return kAudioUnitErr_FormatNotSupported
            }
            ev = UnsafePointer(head.next)
        }
        let bufCount = abl.count
        // The host may run a multi-layout plugin at a narrower width than
        // its first declared layout (the descriptor's numIn / numOut), so
        // the negotiated bus - reflected in the buffer count - is the
        // authority. Clamp to it and hand the plugin the real widths, not
        // the descriptor's, so it never sees nil channel pointers.
        let actualOut = hasAudioOutput ? UInt32(min(Int(numOut), bufCount)) : 0
        for i in 0..<32 { inPtrs[i] = nil; outPtrs[i] = nil }
        let actualIn: UInt32
        if let mainInScratch = mainInScratch {
            // Input staged separately (numIn > numOut): hand the plugin all
            // numIn channels from the scratch, not the narrower output ABL.
            actualIn = min(numIn, 32)
            for c in 0..<Int(actualIn) {
                inPtrs[c] = UnsafePointer(mainInScratch.advanced(by: c * scMaxFrames))
            }
        } else {
            // In-place: input aliases the output ABL. The negotiated bus
            // width (bufCount) is the authority for a multi-layout plugin
            // running narrower than its first declared layout.
            actualIn = numIn > 0 ? UInt32(min(Int(numIn), bufCount)) : 0
            for c in 0..<Int(actualIn) {
                let p: UnsafeMutablePointer<Float>? =
                    abl[c].mData?.assumingMemoryBound(to: Float.self)
                inPtrs[c] = UnsafePointer(p)
            }
        }
        for c in 0..<Int(actualOut) {
            outPtrs[c] = abl[c].mData?.assumingMemoryBound(to: Float.self)
        }

        // Pull the sidechain input (bus 1) only after the host enables it,
        // then append its fixed-width channels so the flat array stays
        // [main..., sidechain...]. A disabled or failed pull reads silence.
        var scActual = 0
        if scCh > 0, let scScratch = scScratch, let scABL = scABL {
            for c in 0..<scCh {
                scABL[c] = AudioBuffer(
                    mNumberChannels: 1,
                    mDataByteSize: frameCount * UInt32(MemoryLayout<Float>.size),
                    mData: UnsafeMutableRawPointer(scScratch.advanced(by: c * scMaxFrames)))
            }
            var pulled = false
            if sidechainEnabled, let pull = pull {
                var f = AudioUnitRenderActionFlags()
                pulled = pull(&f, timestamp, frameCount, 1, scABL.unsafeMutablePointer) == noErr
            }
            if !pulled {
                for c in 0..<scCh {
                    memset(scScratch.advanced(by: c * scMaxFrames), 0,
                           Int(frameCount) * MemoryLayout<Float>.size)
                }
            }
            let n = min(scCh, max(0, 32 - Int(actualIn)))
            for c in 0..<n {
                inPtrs[Int(actualIn) + c] =
                    UnsafePointer(scABL[c].mData?.assumingMemoryBound(to: Float.self))
            }
            scActual = n
        }

        // Fill the transport snapshot from the host-provided blocks.
        // Both are optional: hosts that don't place the plugin in a
        // musical context leave them nil.
        transportBuf.pointee = AuTransportSnapshot(
            valid: 0, playing: 0, recording: 0, loop_active: 0,
            time_sig_num: 0, time_sig_den: 0,
            tempo: 0, position_samples: 0, position_beats: 0,
            bar_start_beats: 0, loop_start_beats: 0, loop_end_beats: 0)
        if let musical = musicalContext {
            var tempo: Double = 0
            var tsigNum: Double = 0
            var tsigDen: Int = 0
            var beat: Double = 0
            var nextBeat: Int = 0
            var downbeat: Double = 0
            if musical(&tempo, &tsigNum, &tsigDen, &beat, &nextBeat, &downbeat) {
                transportBuf.pointee.tempo = tempo
                transportBuf.pointee.time_sig_num = Int32(tsigNum)
                transportBuf.pointee.time_sig_den = Int32(tsigDen)
                transportBuf.pointee.position_beats = beat
                transportBuf.pointee.bar_start_beats = downbeat
                transportBuf.pointee.valid = 1
            }
        }
        if let state = transportState {
            var flags = AUHostTransportStateFlags(rawValue: 0)
            var samplePos: Double = 0
            var cycleStart: Double = 0
            var cycleEnd: Double = 0
            if state(&flags, &samplePos, &cycleStart, &cycleEnd) {
                transportBuf.pointee.playing =
                    flags.contains(.moving) ? 1 : 0
                transportBuf.pointee.recording =
                    flags.contains(.recording) ? 1 : 0
                transportBuf.pointee.loop_active =
                    flags.contains(.cycling) ? 1 : 0
                transportBuf.pointee.position_samples = samplePos
                transportBuf.pointee.loop_start_beats = cycleStart
                transportBuf.pointee.loop_end_beats = cycleEnd
                transportBuf.pointee.valid = 1
            }
        }
        if transportBuf.pointee.valid == 0 {
            let ts = timestamp.pointee
            if (ts.mFlags.rawValue &
                AudioTimeStampFlags.sampleTimeValid.rawValue) != 0 {
                transportBuf.pointee.position_samples = ts.mSampleTime
                transportBuf.pointee.valid = 1
            }
        }

        guard mooseAbiTailVersion(cb) >= 11,
              let processNative = cb.pointee.process_native_v11,
              let beginOutput = cb.pointee.begin_output_events_v10,
              let nextOutput = cb.pointee.next_output_event,
              let finishOutput = cb.pointee.finish_output_events,
              let commitOutputParams = cb.pointee.commit_output_params else {
            return kAudio_ParamError
        }
        if nativeOverflow != 0 || paramOverflow != 0 {
            return kAudioUnitErr_MIDIOutputBufferFull
        }
        let processResult = processNative(
            ctx, inPtrs, outPtrs, actualIn + UInt32(scActual), actualOut,
            (mainInputEnabled ? 1 : 0) | (sidechainEnabled ? 2 : 0),
            (hasAudioOutput && mainOutputEnabled) ? 1 : 0,
            frameCount, nativeBuf, numNative, nativeOverflow,
            paramBuf, numParam, paramOverflow, transportBuf)
        if processResult != UInt32(AU_PROCESS_OK) {
            finishOutput(ctx, UInt32(AU_OUTPUT_INVALID))
            if processResult == UInt32(AU_PROCESS_QUEUE_FULL) {
                return kAudioUnitErr_MIDIOutputBufferFull
            }
            return kAudio_ParamError
        }

        var outputStatus = UInt32(AU_OUTPUT_EMITTED)
        defer { finishOutput(ctx, outputStatus) }

        var carrierMask: UInt32 = midiOutputBlock == nil ? 0 : UInt32(AU_NATIVE_CARRIER_BYTES)
        if #available(macOS 12.0, iOS 15.0, *), midiOutputListBlock != nil {
            carrierMask |= UInt32(AU_NATIVE_CARRIER_UMP)
        }
        beginOutput(ctx, carrierMask, frameCount, midiOutputProtocol,
                    maxAbsoluteOffset, 1, 0)
        while true {
            var out = AuNativeEvent()
            let result = nextOutput(ctx, &out)
            if result == UInt32(AU_OUTPUT_END) { break }
            if result == UInt32(AU_OUTPUT_UNSUPPORTED) {
                outputStatus = UInt32(AU_OUTPUT_UNSUPPORTED)
                return kAudioUnitErr_FormatNotSupported
            }
            if result == UInt32(AU_OUTPUT_INVALID) {
                outputStatus = UInt32(AU_OUTPUT_INVALID)
                return kAudio_ParamError
            }
            if result == UInt32(AU_OUTPUT_QUEUE_FULL) {
                outputStatus = UInt32(AU_OUTPUT_QUEUE_FULL)
                return kAudioUnitErr_MIDIOutputBufferFull
            }
            guard result == UInt32(AU_OUTPUT_EMITTED),
                  out.sample_offset < frameCount,
                  out.port <= UInt16(UInt8.max) else {
                outputStatus = UInt32(AU_OUTPUT_INVALID)
                return kAudio_ParamError
            }

            let (absoluteTime, timeOverflow) =
                bufStart.addingReportingOverflow(Int64(out.sample_offset))
            guard !timeOverflow else {
                outputStatus = UInt32(AU_OUTPUT_INVALID)
                return kAudio_ParamError
            }
            let eventTime = AUEventSampleTime(absoluteTime)
            let cable = UInt8(out.port)
            if out.kind == UInt8(AU_NATIVE_EVENT_MIDI1), let outputBlock = midiOutputBlock {
                guard out.data_len > 0, out.data_len <= 3 else {
                    outputStatus = UInt32(AU_OUTPUT_INVALID)
                    return kAudio_ParamError
                }
                let status: OSStatus = withUnsafeBytes(of: out.midi) { raw in
                    outputBlock(eventTime, cable, Int(out.data_len),
                                raw.baseAddress!.assumingMemoryBound(to: UInt8.self))
                }
                if status != noErr {
                    outputStatus = UInt32(AU_OUTPUT_QUEUE_FULL)
                    return status
                }
            } else if out.kind == UInt8(AU_NATIVE_EVENT_SYSEX), let outputBlock = midiOutputBlock {
                guard let bytes = out.sysex else {
                    outputStatus = UInt32(AU_OUTPUT_INVALID)
                    return kAudio_ParamError
                }
                let payloadLen = Int(out.data_len)
                guard payloadLen <= sysexOutScratchCap - 2 else {
                    outputStatus = UInt32(AU_OUTPUT_QUEUE_FULL)
                    return kAudioUnitErr_MIDIOutputBufferFull
                }
                sysexOutScratch[0] = 0xF0
                sysexOutScratch.advanced(by: 1).update(from: bytes, count: payloadLen)
                sysexOutScratch[payloadLen + 1] = 0xF7
                let status = outputBlock(
                    eventTime, cable, payloadLen + 2, UnsafePointer(sysexOutScratch))
                if status != noErr {
                    outputStatus = UInt32(AU_OUTPUT_QUEUE_FULL)
                    return status
                }
            } else if out.kind == UInt8(AU_NATIVE_EVENT_UMP) {
                guard #available(macOS 12.0, iOS 15.0, *),
                      let listBlock = midiOutputListBlock as? AUMIDIEventListBlock else {
                    outputStatus = UInt32(AU_OUTPUT_UNSUPPORTED)
                    return kAudioUnitErr_FormatNotSupported
                }
                // Preserve the source list protocol. Apple's AU boundary
                // converts translatable messages to hostMIDIProtocol.
                let protocolID: MIDIProtocolID
                if out.protocol == 1 {
                    protocolID = ._1_0
                } else if out.protocol == 2 {
                    protocolID = ._2_0
                } else {
                    outputStatus = UInt32(AU_OUTPUT_INVALID)
                    return kAudio_ParamError
                }
                guard out.data_len > 0, out.data_len <= 4 else {
                    outputStatus = UInt32(AU_OUTPUT_INVALID)
                    return kAudio_ParamError
                }
                let messageType = UInt8((out.words.0 >> 28) & 0xF)
                guard Int(out.data_len) == umpPacketLength(messageType: messageType),
                      midiOutputProtocol == 1 || midiOutputProtocol == 2,
                      umpProtocolAccepts(out.protocol, messageType) else {
                    outputStatus = UInt32(AU_OUTPUT_INVALID)
                    return kAudio_ParamError
                }
                var list = MIDIEventList()
                let packet = MIDIEventListInit(&list, protocolID)
                let added: UnsafeMutablePointer<MIDIEventPacket>? =
                    withUnsafeBytes(of: out.words) { raw in
                        MIDIEventListAdd(
                            &list, MemoryLayout<MIDIEventList>.size, packet, 0,
                            Int(out.data_len),
                            raw.baseAddress!.assumingMemoryBound(to: UInt32.self))
                    }
                guard added != nil else {
                    outputStatus = UInt32(AU_OUTPUT_QUEUE_FULL)
                    return kAudioUnitErr_MIDIOutputBufferFull
                }
                let status = listBlock(eventTime, cable, &list)
                if status != noErr {
                    outputStatus = UInt32(AU_OUTPUT_QUEUE_FULL)
                    return status
                }
            } else {
                outputStatus = UInt32(AU_OUTPUT_UNSUPPORTED)
                return kAudio_ParamError
            }
        }
        let paramResult = commitOutputParams(ctx)
        if paramResult == UInt32(AU_OUTPUT_UNSUPPORTED) {
            outputStatus = UInt32(AU_OUTPUT_UNSUPPORTED)
            return kAudioUnitErr_FormatNotSupported
        }
        if paramResult == UInt32(AU_OUTPUT_INVALID) {
            outputStatus = UInt32(AU_OUTPUT_INVALID)
            return kAudio_ParamError
        }
        if paramResult == UInt32(AU_OUTPUT_QUEUE_FULL) {
            outputStatus = UInt32(AU_OUTPUT_QUEUE_FULL)
            return kAudioUnitErr_MIDIOutputBufferFull
        }
        guard paramResult == UInt32(AU_OUTPUT_END)
                || paramResult == UInt32(AU_OUTPUT_EMITTED) else {
            outputStatus = UInt32(AU_OUTPUT_INVALID)
            return kAudio_ParamError
        }
        return noErr
    }

    override var internalRenderBlock: AUInternalRenderBlock {
        let ctx = rustCtx!
        let cb = g_callbacks!
        // Widths from the NEGOTIATED bus formats, not the descriptor's first
        // layout: a multi-layout plugin the host ran narrower (or wider)
        // than layout 0 must stage its main input at the width actually set
        // on bus 0. Using the descriptor width here would build a pull ABL
        // wider than the bus - a stale/dropped channel, or a failed pull -
        // and pick the wrong in-place vs separate-staging branch. Fall back
        // to the descriptor when a direction has no bus (an instrument's
        // input).
        let numIn = _inputBusArray.count > 0
            ? UInt32(_inputBusArray[0].format.channelCount)
            : (g_descriptor?.pointee.num_inputs ?? 0)
        let numOut = _outputBusArray.count > 0
            ? UInt32(_outputBusArray[0].format.channelCount)
            : (g_descriptor?.pointee.num_outputs ?? 2)
        let hasAudioOutput = (g_descriptor?.pointee.num_outputs ?? 0) > 0
        let inPtrs = UnsafeMutablePointer<UnsafePointer<Float>?>.allocate(capacity: 32)
        let outPtrs = UnsafeMutablePointer<UnsafeMutablePointer<Float>?>.allocate(capacity: 32)
        let nativeBuf = UnsafeMutablePointer<AuNativeEvent>.allocate(capacity: 256)
        // Per-block scratch for host-side parameter automation
        // events. AURenderEvent's `.parameter` / `.parameterRamp`
        // entries land here with a within-block `sample_offset` so
        // the Rust chunker splits the audio block at each
        // automation point. This capacity is independent of the native
        // event lane, so filling one never truncates the other.
        let paramBuf = UnsafeMutablePointer<AuParamEvent>.allocate(capacity: 256)
        let transportBuf = UnsafeMutablePointer<AuTransportSnapshot>.allocate(capacity: 1)
        let sysexOutScratchCap = Int(MOOSE_SYSEX_POOL_PREALLOC) + 2
        let sysexOutScratch =
            UnsafeMutablePointer<UInt8>.allocate(capacity: sysexOutScratchCap)
        // Sidechain input (bus 1) staging. The main input is pulled in
        // place into the output ABL, so the sidechain needs its own
        // scratch to pull into and append after the main channels. Sized
        // once to the render-graph's max frame count.
        let scCh = Int(g_descriptor?.pointee.sidechain_in_channels ?? 0)
        // AUAudioUnitBus.isEnabled is the format's connection/activation
        // contract. Snapshot it while the host builds the render graph; the
        // realtime block must not query Objective-C state.
        let mainInputEnabled = !_inputBusArray.isEmpty && _inputBusArray[0].isEnabled
        let sidechainEnabled = _inputBusArray.count > 1 && _inputBusArray[1].isEnabled
        let mainOutputEnabled = !_outputBusArray.isEmpty && _outputBusArray[0].isEnabled
        // Size to the authoritative max captured in allocateRenderResources
        // (`_maxFrames`), not `maximumFramesToRender` sampled now: the host
        // may have finalized the max after a smaller default, and this
        // getter can run before that. Never below 4096. The render block
        // rejects a frameCount past this, so an over-declared render can't
        // overrun the scratch.
        let scMaxFrames = max(Int(_maxFrames), Int(self.maximumFramesToRender), 4096)
        let scScratch: UnsafeMutablePointer<Float>? =
            scCh > 0 ? UnsafeMutablePointer<Float>.allocate(capacity: scCh * scMaxFrames) : nil
        let scABL: UnsafeMutableAudioBufferListPointer? =
            scCh > 0 ? AudioBufferList.allocate(maximumBuffers: scCh) : nil

        // Main-input staging for N-in/M-out layouts with N>M (a 2->1 sum, a
        // 4->2 downmix), and for input-only units whose AUv3 output is only
        // a transport dummy. Never pull real input into a dummy buffer that
        // must be silenced before returning it to the host.
        let mainInSeparate = numIn > 0 && (!hasAudioOutput || numIn > numOut)
        let mainInScratch: UnsafeMutablePointer<Float>? =
            mainInSeparate
                ? UnsafeMutablePointer<Float>.allocate(capacity: Int(numIn) * scMaxFrames) : nil
        let mainInABL: UnsafeMutableAudioBufferListPointer? =
            mainInSeparate ? AudioBufferList.allocate(maximumBuffers: Int(numIn)) : nil

        // Snapshot the host blocks at render-graph compile time. AU v3
        // guarantees these are realtime-safe to call from the render
        // block; hosts may set them post-initialization, so this copy
        // will be nil for plugins instantiated outside a musical context.
        let musicalContext = self.musicalContextBlock
        let transportState = self.transportStateBlock
        let midiOutputBlock = self.midiOutputEventBlock
        // The UMP output block (MIDI 2.0), captured type-erased so the
        // deployment target can stay below macOS 12 / iOS 15. `render`
        // casts it back under an availability check.
        let midiOutputListBlock: Any?
        let midiOutputProtocol: UInt32
        if #available(macOS 12.0, iOS 15.0, *) {
            midiOutputListBlock = self.midiOutputEventListBlock
            midiOutputProtocol = UInt32(exactly: self.hostMIDIProtocol.rawValue) ?? 0
        } else {
            midiOutputListBlock = nil
            midiOutputProtocol = 0
        }
        return { _, timestamp, frameCount, _, outputData, events, pull in
            return MooseAUAudioUnit.render(
                ctx: ctx, cb: cb, numIn: numIn, numOut: numOut,
                hasAudioOutput: hasAudioOutput,
                timestamp: timestamp, frameCount: frameCount,
                outputData: outputData, events: events, pull: pull,
                inPtrs: inPtrs, outPtrs: outPtrs, nativeBuf: nativeBuf,
                scCh: scCh, mainInputEnabled: mainInputEnabled,
                sidechainEnabled: sidechainEnabled,
                mainOutputEnabled: mainOutputEnabled,
                scScratch: scScratch, scABL: scABL, scMaxFrames: scMaxFrames,
                mainInScratch: mainInScratch, mainInABL: mainInABL,
                paramBuf: paramBuf,
                transportBuf: transportBuf,
                sysexOutScratch: sysexOutScratch,
                sysexOutScratchCap: sysexOutScratchCap,
                musicalContext: musicalContext,
                transportState: transportState,
                midiOutputBlock: midiOutputBlock,
                midiOutputListBlock: midiOutputListBlock,
                midiOutputProtocol: midiOutputProtocol)
        }
    }

    // MARK: State

    override var fullState: [String: Any]? {
        get {
            var state = super.fullState ?? [:]
            guard let ctx = rustCtx, let cb = g_callbacks else { return state }
            var data: UnsafeMutablePointer<UInt8>? = nil
            var len: UInt32 = 0
            cb.pointee.state_save(ctx, &data, &len)
            if let data = data, len > 0 {
                state["truce_state"] = Data(bytes: data, count: Int(len))
                cb.pointee.state_free(data, len)
            }
            return state
        }
        set {
            super.fullState = newValue
            guard let ctx = rustCtx, let cb = g_callbacks else { return }
            if let blob = newValue?["truce_state"] as? Data {
                blob.withUnsafeBytes { ptr in
                    cb.pointee.state_load(ctx, ptr.baseAddress?.assumingMemoryBound(to: UInt8.self), UInt32(blob.count))
                }
                syncParameterTreeFromRust()
                return
            }
            // No moose entry: a pre-moose build stored its state under
            // its own dictionary key. Probe the keys declared in
            // moose.toml's [plugin.legacy_state] (first present +
            // accepted wins) so the plugin's migrate_state hook can
            // translate the old session. These callbacks live at the
            // struct tail (AU ABI version 1); this appex may be newer
            // than the plugin binary, so gate on the reported version
            // before reading them.
            guard mooseAbiTailVersion(cb) >= 1,
                  let dict = newValue,
                  let keyCount = cb.pointee.legacy_state_key_count,
                  let keyAt = cb.pointee.legacy_state_key_at,
                  let loadForeign = cb.pointee.state_load_foreign else { return }
            for i in 0..<keyCount(ctx) {
                guard let cKey = keyAt(ctx, i),
                      let blob = dict[String(cString: cKey)] as? Data else { continue }
                let accepted = blob.withUnsafeBytes { ptr in
                    loadForeign(ctx, cKey, ptr.baseAddress?.assumingMemoryBound(to: UInt8.self), UInt32(blob.count))
                }
                if accepted != 0 {
                    syncParameterTreeFromRust()
                    return
                }
            }
        }
    }

    /// Push the Rust side's current parameter values into the AU
    /// parameter tree so host UIs reflect a state / preset load.
    private func syncParameterTreeFromRust() {
        guard let ctx = rustCtx, let cb = g_callbacks, let tree = _parameterTree else { return }
        for param in tree.allParameters {
            param.value = AUValue(cb.pointee.param_get_value(ctx, UInt32(param.address)))
        }
    }

    // MARK: Factory presets

    /// Backed by the `.trucepreset` files `cargo moose install`
    /// bundles into the framework's `Resources/Presets/` - the same
    /// library the AU v2 component serves through
    /// `kAudioUnitProperty_FactoryPresets`.
    override var factoryPresets: [AUAudioUnitPreset]? {
        guard let ctx = rustCtx, let cb = g_callbacks else { return nil }
        let n = cb.pointee.factory_preset_count(ctx)
        guard n > 0 else { return nil }
        return (0..<n).map { i in
            let preset = AUAudioUnitPreset()
            preset.number = Int(i)
            if let cName = cb.pointee.factory_preset_name(ctx, i) {
                preset.name = String(cString: cName)
            }
            return preset
        }
    }

    private var _currentPreset: AUAudioUnitPreset?
    override var currentPreset: AUAudioUnitPreset? {
        get { _currentPreset }
        set {
            guard let preset = newValue else {
                _currentPreset = nil
                return
            }
            if preset.number >= 0 {
                // Factory preset - the same apply path session
                // restore takes. A failed load (bad index, missing
                // file) leaves the current preset unchanged.
                guard let ctx = rustCtx, let cb = g_callbacks,
                      cb.pointee.factory_preset_load(ctx, UInt32(preset.number)) != 0
                else { return }
                syncParameterTreeFromRust()
                _currentPreset = preset
            } else {
                // User preset: replay the host-stored document state.
                guard let state = try? presetState(for: preset) else { return }
                fullStateForDocument = state
                _currentPreset = preset
            }
        }
    }

    // Without this the host (Logic / GarageBand) never offers "Save as
    // user preset" and never drives the number < 0 recall branch above,
    // so the implemented user-preset path stays dead. `fullStateForDocument`
    // is what a user preset serializes / restores.
    override var supportsUserPresets: Bool { true }

    // MARK: Capabilities

    override var channelCapabilities: [NSNumber]? {
        guard let d = g_descriptor?.pointee else { return nil }
        // Multiple declared bus_layouts(): one [in, out] pair per layout,
        // flattened, so the host can pick any supported channel config.
        if d.num_layouts > 0, let ins = d.layout_in_channels, let outs = d.layout_out_channels {
            var caps: [NSNumber] = []
            caps.reserveCapacity(Int(d.num_layouts) * 2)
            for i in 0..<Int(d.num_layouts) {
                caps.append(NSNumber(value: ins[i]))
                caps.append(NSNumber(value: outs[i]))
            }
            return caps
        }
        // aumi (MIDI Processor, zero audio I/O): advertise 0 inputs
        // and "any" (-1) outputs - the output bus is a dummy kept
        // alive only so AUv3 can negotiate a sample rate. This
        // `[0, -1]` shape is the AUChannelInfo Apple's framework
        // requires for AU MIDI effects.
        if d.num_inputs == 0 && d.num_outputs == 0 {
            return [0, -1]
        }
        if d.num_inputs == 0 {
            return [0, NSNumber(value: d.num_outputs)]
        }
        return [NSNumber(value: d.num_inputs), NSNumber(value: d.num_outputs)]
    }
    override var isMusicDeviceOrEffect: Bool { true }
    override var canProcessInPlace: Bool { g_descriptor?.pointee.num_inputs ?? 0 > 0 }
    // Report the plugin's latency / release tail so the host aligns
    // delay compensation. Samples come from a cache the framework
    // refreshes each block; divide by the sample rate for seconds. The
    // `latency_samples` / `tail_samples` callbacks are ABI version 3,
    // so gate on the (magic-validated) reported version - this appex
    // may be newer than the framework binary it binds.
    override var latency: TimeInterval {
        guard let ctx = rustCtx, let cb = g_callbacks,
              mooseAbiTailVersion(cb) >= 3, _sampleRate > 0 else { return 0 }
        return TimeInterval(cb.pointee.latency_samples(ctx)) / _sampleRate
    }
    override var tailTime: TimeInterval {
        guard let ctx = rustCtx, let cb = g_callbacks,
              mooseAbiTailVersion(cb) >= 3, _sampleRate > 0 else { return 0 }
        return TimeInterval(cb.pointee.tail_samples(ctx)) / _sampleRate
    }
    override var shouldBypassEffect: Bool { get { false } set { } }

    // AUAudioUnit.latency is KVO-observed by hosts for delay
    // compensation, but the value comes from a callback (a computed
    // property) so re-assignment can't fire the notification. Called on
    // the main thread from the param-sync timer: when the framework's
    // reported latency moves, fire KVO manually so the host re-reads the
    // fresh `latency`. This is AU v3's push path - the audio thread only
    // refreshes the cache, never touches KVO.
    func notifyLatencyIfChanged() {
        guard let ctx = rustCtx, let cb = g_callbacks,
              mooseAbiTailVersion(cb) >= 3 else { return }
        let now = cb.pointee.latency_samples(ctx)
        if now != _lastLatencySamples {
            _lastLatencySamples = now
            willChangeValue(forKey: "latency")
            didChangeValue(forKey: "latency")
        }
    }
}

// MARK: - Factory

// `@objc(AudioUnitFactory)` pins the runtime class name to
// `AudioUnitFactory` (no module prefix) and - critically - forces
// Swift's optimizer to keep the class in `__objc_classlist`.
// Without this, `swiftc -O` strips the class because nothing in
// the module references it directly; NSExtension's runtime lookup
// (via `NSExtensionPrincipalClass`) is invisible to the optimizer
// and the appex launches with no principal class, dying with
// XPC error 4097 (`NSXPCConnectionInvalid`). The matching
// Info.plist key is `<key>NSExtensionPrincipalClass</key>
// <string>AudioUnitFactory</string>` (no `$(PRODUCT_MODULE_NAME).`
// prefix).
@objc(AudioUnitFactory)
class AudioUnitFactory: AUViewController, AUAudioUnitFactory {
    private var auInstance: MooseAUAudioUnit?

    public func createAudioUnit(with componentDescription: AudioComponentDescription) throws -> AUAudioUnit {
        let au = try MooseAUAudioUnit(componentDescription: componentDescription, options: [])
        auInstance = au
        logger.info("factory createAudioUnit")
        // If the view is already loaded (host called loadView before
        // createAudioUnit), set up the GUI now that we have an instance.
        // Must dispatch to main thread - NSView operations require it.
        if isViewLoaded {
            DispatchQueue.main.async { [weak self] in
                self?.setupGUIIfReady()
            }
        }
        return au
    }

    private var guiSetUp = false
    private var guiContainer: NSView?
    private var guiPtSize: NSSize = .zero
    private var paramSyncTimer: Timer?
    /// Latch flipped after the host's first `viewDidLayout` pass.
    /// On first layout Logic Pro lays our `view` out at its plug-in
    /// pane size (typically wider than the editor's natural), and
    /// propagating that to `gui_set_size` makes the editor canvas
    /// grow to fill - widgets stay at natural cell positions but
    /// the empty trailing space looks "stretched". Skipping the
    /// first layout keeps the editor at its built natural size on
    /// open; subsequent layouts (genuine user resize) still
    /// propagate.
    private var didInitialLayout = false

    override func loadView() {
        // Query editor size using a temporary Rust context.
        var size = NSSize(width: 200, height: 150) // fallback
        if let cb = g_callbacks {
            let tmpCtx = cb.pointee.create()
            if let ctx = tmpCtx {
                if cb.pointee.gui_has_editor(ctx) != 0 {
                    var w: UInt32 = 0, h: UInt32 = 0
                    cb.pointee.gui_get_size(ctx, &w, &h)
                    if w > 0 && h > 0 {
                        // w/h are in logical points - use directly.
                        size = NSSize(width: CGFloat(w), height: CGFloat(h))
                    }
                }
                cb.pointee.destroy(ctx)
            }
        }
        let v = NSView(frame: NSRect(origin: .zero, size: size))
        #if os(macOS)
        v.wantsLayer = true
        v.layer?.backgroundColor = CGColor(red: 0.15, green: 0.15, blue: 0.15, alpha: 1)
        // Without an autoresize mask, Logic Pro's bigger plug-in
        // container leaves our view pinned at its initial natural
        // size and `viewDidLayoutSubviews` never fires (our bounds
        // never change). Width / height sizable makes the view
        // follow the host container; `propagateHostResize` then
        // sees `bounds != guiPtSize` and calls `gui_set_size`, and
        // the inner `guiContainer` follows because it inherits the
        // mask too (set on the container in `setupGUIIfReady`).
        v.autoresizingMask = [.width, .height]
        #else
        // UIView always has a backing layer; set the BG directly.
        v.backgroundColor = UIColor(red: 0.15, green: 0.15, blue: 0.15, alpha: 1)
        v.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        #endif
        self.view = v
        self.preferredContentSize = size
        logger.info("loadView: \(size.width)x\(size.height)")
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        logger.info("viewDidLoad: view.frame=\(self.view.frame.width)x\(self.view.frame.height) auInstance=\(self.auInstance != nil)")
        setupGUIIfReady()
    }

    #if os(macOS)
    override func viewWillAppear() {
        super.viewWillAppear()
        logger.info("viewWillAppear: view.frame=\(self.view.frame.width)x\(self.view.frame.height)")
        setupGUIIfReady()
    }
    #else
    override func viewWillAppear(_ animated: Bool) {
        super.viewWillAppear(animated)
        logger.info("viewWillAppear: view.frame=\(self.view.frame.width)x\(self.view.frame.height)")
        setupGUIIfReady()
    }
    #endif


    /// Rust context for THIS instance's AU (per-instance).
    private var myCtx: UnsafeMutableRawPointer? {
        auInstance?.rustCtx
    }

    private func setupGUIIfReady() {
        logger.info("setupGUIIfReady: guiSetUp=\(self.guiSetUp) auInstance=\(self.auInstance != nil)")
        guard !guiSetUp,
              let ctx = myCtx,
              let cb = g_callbacks,
              cb.pointee.gui_has_editor(ctx) != 0 else { return }

        var w: UInt32 = 0
        var h: UInt32 = 0
        cb.pointee.gui_get_size(ctx, &w, &h)
        guard w > 0, h > 0 else { return }

        // w/h are in logical points - use directly.
        guiPtSize = NSSize(width: CGFloat(w), height: CGFloat(h))
        logger.info("setupGUI: \(w)x\(h) view=\(self.view.frame.width)x\(self.view.frame.height)")

        let container = NSView(frame: NSRect(origin: .zero, size: guiPtSize))
        cb.pointee.gui_open(ctx, Unmanaged.passUnretained(container).toOpaque())

        for sub in self.view.subviews { sub.removeFromSuperview() }
        self.view.addSubview(container)
        guiContainer = container
        self.preferredContentSize = guiPtSize
        guiSetUp = true
        #if os(iOS)
        // Fit the editor to the host's safe-area frame so its responsive
        // layout reflows to the real device viewport.
        fitGUIToSafeArea()
        #else
        // Center the GUI in the host's view (which may be oversized).
        centerGUI()
        #endif

        // Sync Rust param values → AUParameterTree at ~30fps.
        // This ensures the host sees GUI-initiated param changes (KVO).
        startParamSync()
    }

    #if os(macOS)
    override func viewDidDisappear() {
        super.viewDidDisappear()
        teardownGUI()
    }

    override func viewDidLayout() {
        super.viewDidLayout()
        if didInitialLayout {
            propagateHostResize()
        } else {
            didInitialLayout = true
        }
        centerGUI()
    }
    #else
    override func viewDidDisappear(_ animated: Bool) {
        super.viewDidDisappear(animated)
        teardownGUI()
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        // iOS always fits to the host's safe-area frame - unlike the macOS
        // `didInitialLayout` skip (which avoids Logic stretching the desktop
        // editor on first layout), fitting on the very first layout is the
        // whole point on iOS: the host hands us a pane sized to the device
        // and we reflow into it immediately.
        fitGUIToSafeArea()
    }
    #endif

    /// When the host's container view changes our bounds (drag-
    /// resize), forward to `gui_set_size` so the editor follows.
    /// No-op when the editor opted out of resize - in that case
    /// `centerGUI` keeps the inner container at its original size.
    private func propagateHostResize() {
        guard guiSetUp,
              let ctx = myCtx,
              let cb = g_callbacks,
              cb.pointee.gui_can_resize(ctx) != 0
        else { return }
        let hostW = self.view.bounds.width
        let hostH = self.view.bounds.height
        guard hostW > 0, hostH > 0,
              (hostW, hostH) != (guiPtSize.width, guiPtSize.height)
        else { return }
        let newW = UInt32(max(1, hostW.rounded()))
        let newH = UInt32(max(1, hostH.rounded()))
        cb.pointee.gui_set_size(ctx, newW, newH)
        // Re-query: the editor may have clamped the request against
        // its `min_size` / `max_size` (the built-in `GridLayout`
        // does), so the stored size differs from `(newW, newH)`.
        // Using the requested values for `guiPtSize` makes
        // `centerGUI` mis-position the inner container: when host
        // bounds are *smaller* than the editor's min, the container
        // would be set to host bounds while the editor's actual
        // surface stays at min, leaving its bottom-left at the
        // host's bottom-left and the layout's TOP (GAIN header)
        // clipping off the host's top edge.
        var actW: UInt32 = 0, actH: UInt32 = 0
        cb.pointee.gui_get_size(ctx, &actW, &actH)
        guiPtSize = NSSize(width: CGFloat(max(1, actW)),
                           height: CGFloat(max(1, actH)))
        guiContainer?.frame = NSRect(origin: .zero, size: guiPtSize)
    }

    #if os(iOS)
    /// Fit the editor to the host plug-in pane's safe-area frame. AU v3
    /// hosts (GarageBand, AUM, Logic for iPad) hand us a UIView whose
    /// bounds track their pane; we drive the Rust editor to the safe-area
    /// size via `gui_set_size` and pin the container inside the safe-area
    /// insets so the editor's responsive layout reflows to the real device
    /// viewport instead of sitting at its built portrait size. When the
    /// editor opted out of resize (`gui_can_resize == 0`) we only position
    /// the natural-size container, matching the desktop `centerGUI`.
    private func fitGUIToSafeArea() {
        guard guiSetUp, let container = guiContainer, guiPtSize.width > 0 else { return }
        // `safeAreaLayoutGuide.layoutFrame` excludes the notch /
        // home-indicator insets; fall back to raw bounds before the safe
        // area resolves (it is zero until the view is in a window
        // hierarchy).
        let safeFrame = self.view.safeAreaLayoutGuide.layoutFrame
        let layoutFrame = (safeFrame.width > 0 && safeFrame.height > 0) ? safeFrame : self.view.bounds
        let hostW = layoutFrame.width
        let hostH = layoutFrame.height
        guard hostW > 0, hostH > 0 else { return }

        if let ctx = myCtx, let cb = g_callbacks,
           cb.pointee.gui_can_resize(ctx) != 0,
           (hostW, hostH) != (guiPtSize.width, guiPtSize.height) {
            let reqW = UInt32(max(1, hostW.rounded()))
            let reqH = UInt32(max(1, hostH.rounded()))
            cb.pointee.gui_set_size(ctx, reqW, reqH)
            // Re-query: the Rust side clamps the request against the
            // editor's min / max, so the size it actually adopted may
            // differ from what we asked for. Position the container to the
            // clamped size, not the request.
            var actW: UInt32 = 0, actH: UInt32 = 0
            cb.pointee.gui_get_size(ctx, &actW, &actH)
            guiPtSize = NSSize(width: CGFloat(max(1, actW)),
                               height: CGFloat(max(1, actH)))
            self.preferredContentSize = guiPtSize
        }

        // Center the editor within the safe-area frame (UIKit origin is
        // top-left). A resizable editor that filled the safe area has zero
        // offset; a fixed-size (or min-clamped) editor smaller than the
        // pane sits centered instead of pinned to a corner. `max(0, ...)`
        // keeps the top-left visible when the editor is larger than the
        // pane (better to clip the far edge than the labels).
        let offsetX = max(0, (hostW - guiPtSize.width) / 2)
        let offsetY = max(0, (hostH - guiPtSize.height) / 2)
        container.frame = NSRect(x: layoutFrame.minX + offsetX,
                                 y: layoutFrame.minY + offsetY,
                                 width: guiPtSize.width,
                                 height: guiPtSize.height)
    }
    #endif

    private func teardownGUI() {
        // Close the GUI when the host hides the plugin window.
        // This stops the repaint timer. setupGUIIfReady will
        // re-create the GUI when the window is shown again.
        stopParamSync()
        if guiSetUp, let ctx = myCtx, let cb = g_callbacks {
            cb.pointee.gui_close(ctx)
        }
        guiContainer?.removeFromSuperview()
        guiContainer = nil
        guiSetUp = false
    }

    private func centerGUI() {
        guard let container = guiContainer, guiPtSize.width > 0 else { return }
        let hostW = self.view.bounds.width
        let hostH = self.view.bounds.height
        // Horizontal: center the editor when the host's view is
        // wider; clamp to 0 so the LEFT edge stays visible when the
        // host is narrower (better to clip the right edge than the
        // left where labels sit).
        let x = max(0, (hostW - guiPtSize.width) / 2)
        // Vertical: anchor to the TOP of the host view in unflipped
        // Cocoa coordinates. `(host - gui)` is positive when the
        // editor fits (centers vertically by symmetry) and negative
        // when the editor is taller than the host (e.g. Logic's UI
        // zoom shrinks the host below our editor's `min_size`); in
        // that case `(host - gui)` is negative, so `y` ends up
        // below 0 and the editor's TOP edge sits at the host's TOP
        // edge with the BOTTOM clipping off-screen. Without this,
        // `max(0, ...)` pinned the bottom-left to the host's bottom-
        // left and the GAIN header at the layout's top fell off the
        // top edge of the visible plug-in window.
        #if os(macOS)
        let y = hostH - guiPtSize.height
        #else
        // UIKit uses flipped coords (origin top-left): the natural
        // anchor for "top" is already y = 0, so the `max(0, ...)`
        // clamp keeps the top visible.
        let y = max(0, (hostH - guiPtSize.height) / 2)
        #endif
        container.frame = NSRect(x: x, y: y,
                                 width: guiPtSize.width, height: guiPtSize.height)
    }

    // MARK: - Parameter sync (GUI ↔ host)

    private func startParamSync() {
        paramSyncTimer = Timer.scheduledTimer(withTimeInterval: 1.0/30.0, repeats: true) { [weak self] _ in
            self?.syncParamsToHost()
        }
    }

    private func stopParamSync() {
        paramSyncTimer?.invalidate()
        paramSyncTimer = nil
    }

    /// Push current Rust param values to the AUParameterTree so the host
    /// sees changes made via the custom GUI (triggers KVO notification).
    private var syncCount = 0

    private func syncParamsToHost() {
        guard let au = auInstance,
              let ctx = au.rustCtx,
              let cb = g_callbacks,
              let tree = au.parameterTree else { return }

        au.isSyncingToHost = true
        for param in tree.allParameters {
            let rustVal = AUValue(cb.pointee.param_get_value(ctx, UInt32(param.address)))
            if abs(param.value - rustVal) > 1e-4 {
                param.setValue(rustVal, originator: nil)
            }
        }
        au.isSyncingToHost = false

        // Push a latency change (if any) to the host on the same
        // main-thread tick.
        au.notifyLatencyIfChanged()
    }
}
