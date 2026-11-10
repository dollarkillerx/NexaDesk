// M0 S3 (macOS): answers the VideoToolbox questions in DETAILED_DESIGN §6.5/§15.
// Input is synthetic IOSurface frames, so no Screen Recording permission is needed;
// the capture segment is therefore NOT measured here.
import CoreMedia
import CoreVideo
import Foundation
import Metal
import VideoToolbox

let W = 1920, H = 1080

func us() -> Double { Double(DispatchTime.now().uptimeNanoseconds) / 1000 }
func pct(_ v: [Double], _ p: Double) -> Double {
    if v.isEmpty { return .nan }
    let s = v.sorted(); return s[Int(Double(s.count - 1) * p)]
}
func R(_ s: String) { print("RESULT " + s); fflush(stdout) }

// Scrolling textured background plus a moving block: compressible but never static.
let texture: [UInt8] = {
    var t = [UInt8](repeating: 0, count: (W * 2) * H)
    var seed: UInt32 = 12345
    var tiles = [UInt8](repeating: 0, count: 128 * 128)
    for i in 0..<tiles.count { seed = seed &* 1664525 &+ 1013904223; tiles[i] = UInt8(40 + (seed >> 24) % 150) }
    for y in 0..<H { for x in 0..<(W * 2) {
        let tile = tiles[((y >> 4) & 127) * 128 + ((x >> 4) & 127)]
        t[y * W * 2 + x] = tile &+ UInt8(((x ^ y) & 7) * 3)
    } }
    return t
}()

func makePool(_ fmt: OSType) -> CVPixelBufferPool {
    let attrs: [CFString: Any] = [
        kCVPixelBufferPixelFormatTypeKey: fmt, kCVPixelBufferWidthKey: W, kCVPixelBufferHeightKey: H,
        kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary, kCVPixelBufferMetalCompatibilityKey: true,
    ]
    var p: CVPixelBufferPool?
    CVPixelBufferPoolCreate(nil, nil, attrs as CFDictionary, &p)
    return p!
}

func fill(_ pb: CVPixelBuffer, _ n: Int) {
    CVPixelBufferLockBaseAddress(pb, [])
    defer { CVPixelBufferUnlockBaseAddress(pb, []) }
    let y = CVPixelBufferGetBaseAddressOfPlane(pb, 0)!.assumingMemoryBound(to: UInt8.self)
    let stride = CVPixelBufferGetBytesPerRowOfPlane(pb, 0)
    let shift = (n * 3) % W
    texture.withUnsafeBufferPointer { t in
        for r in 0..<H { memcpy(y + r * stride, t.baseAddress! + r * W * 2 + shift, W) }
    }
    let bx = (n * 11) % (W - 320), by = (n * 5) % (H - 320)
    for r in by..<(by + 320) { memset(y + r * stride + bx, Int32(200 + (n % 50)), 320) }
    let uv = CVPixelBufferGetBaseAddressOfPlane(pb, 1)!
    memset(uv, 128, CVPixelBufferGetBytesPerRowOfPlane(pb, 1) * CVPixelBufferGetHeightOfPlane(pb, 1))
}

func luma8(_ pb: CVPixelBuffer) -> [UInt8] {
    CVPixelBufferLockBaseAddress(pb, .readOnly)
    defer { CVPixelBufferUnlockBaseAddress(pb, .readOnly) }
    let y = CVPixelBufferGetBaseAddressOfPlane(pb, 0)!.assumingMemoryBound(to: UInt8.self)
    let s = CVPixelBufferGetBytesPerRowOfPlane(pb, 0)
    var o = [UInt8](); o.reserveCapacity(W * H / 64)
    for r in Swift.stride(from: 0, to: H, by: 8) { for c in Swift.stride(from: 0, to: W, by: 8) { o.append(y[r * s + c]) } }
    return o
}
func psnr(_ a: [UInt8], _ b: [UInt8]) -> Double {
    var e = 0.0
    for i in 0..<min(a.count, b.count) { let d = Double(a[i]) - Double(b[i]); e += d * d }
    e /= Double(a.count)
    return e == 0 ? 99 : 10 * log10(255 * 255 / e)
}

// GPU binding stage: NV12 → BGRA through CVMetalTextureCache, offscreen.
final class Binder {
    let dev = MTLCreateSystemDefaultDevice()!
    let q: MTLCommandQueue
    let pipe: MTLRenderPipelineState
    var cache: CVMetalTextureCache?
    let target: MTLTexture
    init() {
        q = dev.makeCommandQueue()!
        let src = """
        #include <metal_stdlib>
        using namespace metal;
        struct V { float4 p [[position]]; float2 uv; };
        vertex V vs(uint i [[vertex_id]]) { float2 u = float2((i<<1)&2, i&2); V o; o.p = float4(u*float2(2,-2)+float2(-1,1),0,1); o.uv=u; return o; }
        fragment float4 fs(V in [[stage_in]], texture2d<float> y [[texture(0)]], texture2d<float> c [[texture(1)]]) {
            constexpr sampler s(filter::linear);
            float Y = y.sample(s, in.uv).r - 0.0625; float2 C = c.sample(s, in.uv).rg - 0.5;
            return float4(1.164*Y+1.793*C.y, 1.164*Y-0.213*C.x-0.533*C.y, 1.164*Y+2.112*C.x, 1);
        }
        """
        let lib = try! dev.makeLibrary(source: src, options: nil)
        let d = MTLRenderPipelineDescriptor()
        d.vertexFunction = lib.makeFunction(name: "vs"); d.fragmentFunction = lib.makeFunction(name: "fs")
        d.colorAttachments[0].pixelFormat = .bgra8Unorm
        pipe = try! dev.makeRenderPipelineState(descriptor: d)
        CVMetalTextureCacheCreate(nil, nil, dev, nil, &cache)
        let t = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: .bgra8Unorm, width: W, height: H, mipmapped: false)
        t.usage = [.renderTarget]; t.storageMode = .private
        target = dev.makeTexture(descriptor: t)!
    }
    func tex(_ pb: CVPixelBuffer, _ plane: Int, _ f: MTLPixelFormat) -> CVMetalTexture? {
        var o: CVMetalTexture?
        CVMetalTextureCacheCreateTextureFromImage(nil, cache!, pb, nil, f, CVPixelBufferGetWidthOfPlane(pb, plane), CVPixelBufferGetHeightOfPlane(pb, plane), plane, &o)
        return o
    }
    func draw(_ pb: CVPixelBuffer) -> Bool {
        guard let y = tex(pb, 0, .r8Unorm), let c = tex(pb, 1, .rg8Unorm) else { return false }
        let p = MTLRenderPassDescriptor()
        p.colorAttachments[0].texture = target; p.colorAttachments[0].loadAction = .dontCare; p.colorAttachments[0].storeAction = .store
        let cb = q.makeCommandBuffer()!, e = cb.makeRenderCommandEncoder(descriptor: p)!
        e.setRenderPipelineState(pipe)
        e.setFragmentTexture(CVMetalTextureGetTexture(y), index: 0); e.setFragmentTexture(CVMetalTextureGetTexture(c), index: 1)
        e.drawPrimitives(type: .triangle, vertexStart: 0, vertexCount: 3); e.endEncoding()
        cb.commit(); cb.waitUntilCompleted()
        return cb.status == .completed
    }
}
let binder = Binder()

struct Cfg {
    let name: String; let codec: CMVideoCodecType; var lowLatency = false; var ltr = false
    var fmt: OSType = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
    var profile: CFString? = nil; var bps = 20_000_000; var fps = 60
}

func setProp(_ s: VTCompressionSession, _ k: CFString, _ v: CFTypeRef, _ bad: inout [String]) {
    let st = VTSessionSetProperty(s, key: k, value: v)
    if st != noErr { bad.append("\(k)=\(st)") }
}

func makeEncoder(_ c: Cfg) -> (VTCompressionSession?, OSStatus, [String]) {
    var spec: [CFString: Any] = [:]
    if c.lowLatency { spec[kVTVideoEncoderSpecification_EnableLowLatencyRateControl] = true }
    else { spec[kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder] = true }
    var s: VTCompressionSession?
    let st = VTCompressionSessionCreate(allocator: nil, width: Int32(W), height: Int32(H), codecType: c.codec, encoderSpecification: spec as CFDictionary, imageBufferAttributes: nil, compressedDataAllocator: nil, outputCallback: nil, refcon: nil, compressionSessionOut: &s)
    guard st == noErr, let e = s else { return (nil, st, []) }
    var bad = [String]()
    setProp(e, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue, &bad)
    setProp(e, kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse, &bad)
    setProp(e, kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, kCFBooleanTrue, &bad)
    setProp(e, kVTCompressionPropertyKey_ExpectedFrameRate, c.fps as CFNumber, &bad)
    setProp(e, kVTCompressionPropertyKey_AverageBitRate, c.bps as CFNumber, &bad)
    setProp(e, kVTCompressionPropertyKey_MaxFrameDelayCount, 0 as CFNumber, &bad)
    if !c.lowLatency { setProp(e, kVTCompressionPropertyKey_MaxKeyFrameInterval, 0 as CFNumber, &bad) }
    if let p = c.profile { setProp(e, kVTCompressionPropertyKey_ProfileLevel, p, &bad) }
    if c.ltr { setProp(e, kVTCompressionPropertyKey_EnableLTR, kCFBooleanTrue, &bad) }
    VTCompressionSessionPrepareToEncodeFrames(e)
    return (e, noErr, bad)
}

func usingHW(_ e: VTCompressionSession) -> String {
    var v: CFTypeRef?
    let st = withUnsafeMutablePointer(to: &v) { VTSessionCopyProperty(e, key: kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder, allocator: nil, valueOut: UnsafeMutableRawPointer($0)) }
    return st == noErr ? "\(v as? Bool ?? false)" : "unknown"
}

func encodeOne(_ e: VTCompressionSession, _ pb: CVPixelBuffer, _ n: Int, _ fps: Int, _ opts: [CFString: Any]?) -> (CMSampleBuffer?, OSStatus) {
    let sem = DispatchSemaphore(value: 0)
    var out: CMSampleBuffer?, status: OSStatus = noErr
    let st = VTCompressionSessionEncodeFrame(e, imageBuffer: pb, presentationTimeStamp: CMTime(value: CMTimeValue(n), timescale: CMTimeScale(fps)), duration: CMTime(value: 1, timescale: CMTimeScale(fps)), frameProperties: opts as CFDictionary?, infoFlagsOut: nil) { s, _, sb in
        status = s; out = sb; sem.signal()
    }
    if st != noErr { return (nil, st) }
    sem.wait()
    return (out, status)
}

func isKey(_ sb: CMSampleBuffer) -> Bool {
    guard let a = CMSampleBufferGetSampleAttachmentsArray(sb, createIfNecessary: false) as? [[CFString: Any]], let f = a.first else { return true }
    return !(f[kCMSampleAttachmentKey_NotSync] as? Bool ?? false)
}
func ltrToken(_ sb: CMSampleBuffer) -> NSNumber? {
    guard let a = CMSampleBufferGetSampleAttachmentsArray(sb, createIfNecessary: false) as? [[CFString: Any]], let f = a.first else { return nil }
    return f[kVTSampleAttachmentKey_RequireLTRAcknowledgementToken] as? NSNumber
}

final class Dec {
    var s: VTDecompressionSession?
    let fmt: OSType
    init(_ f: OSType) { fmt = f }
    func ensure(_ fd: CMFormatDescription) -> OSStatus {
        if let x = s, VTDecompressionSessionCanAcceptFormatDescription(x, formatDescription: fd) { return noErr }
        // Explicit format: left unset, Apple Silicon hands back GPU-compressed buffers the CPU cannot read for PSNR.
        let attrs: [CFString: Any] = [kCVPixelBufferPixelFormatTypeKey: fmt, kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary, kCVPixelBufferMetalCompatibilityKey: true]
        return VTDecompressionSessionCreate(allocator: nil, formatDescription: fd, decoderSpecification: nil, imageBufferAttributes: attrs as CFDictionary, outputCallback: nil, decompressionSessionOut: &s)
    }
    func decode(_ sb: CMSampleBuffer) -> (CVPixelBuffer?, OSStatus) {
        let st0 = ensure(CMSampleBufferGetFormatDescription(sb)!)
        if st0 != noErr { return (nil, st0) }
        var pb: CVPixelBuffer?, status: OSStatus = noErr
        let st = VTDecompressionSessionDecodeFrame(s!, sampleBuffer: sb, flags: [], infoFlagsOut: nil) { s, _, img, _, _ in status = s; pb = img }
        if st != noErr { return (nil, st) }
        return (pb, status)
    }
}

/// What the encoder really produced: hvcC byte 16 holds chroma_format_idc (1 = 4:2:0, 3 = 4:4:4).
func reportBitstream(_ name: String, _ sb: CMSampleBuffer) {
    guard let fd = CMSampleBufferGetFormatDescription(sb) else { return }
    let sub = CMFormatDescriptionGetMediaSubType(fd)
    let fcc = String(bytes: withUnsafeBytes(of: sub.bigEndian) { Array($0) }, encoding: .ascii) ?? "?"
    var extra = ""
    if let atoms = CMFormatDescriptionGetExtension(fd, extensionKey: kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms) as? [String: Any],
       let hvcC = atoms["hvcC"] as? Data, hvcC.count > 16 {
        let b = [UInt8](hvcC)
        extra = " profile_idc=\(b[1] & 0x1f) chroma_format_idc=\(b[16] & 3)"
    }
    R("bitstream \(name): codec=\(fcc)\(extra)")
}

func latency(_ c: Cfg, secs: Int = 6) {
    let (enc, st, bad) = makeEncoder(c)
    guard let e = enc else { R("\(c.name): encoder create FAILED status=\(st)"); return }
    let pool = makePool(c.fmt); let dec = Dec(c.fmt)
    var en = [Double](), de = [Double](), re = [Double](), total = [Double]()
    var sizes = [Double](), keys = 0, errs = 0, bindFail = 0
    var last: Double = 0, psnrs = [Double]()
    let interval = 1_000_000.0 / Double(c.fps)
    let start = us()
    for n in 0..<(secs * c.fps) {
        let due = start + Double(n) * interval
        while us() < due { usleep(100) }
        var pb: CVPixelBuffer?
        CVPixelBufferPoolCreatePixelBuffer(nil, pool, &pb)
        fill(pb!, n)
        let t0 = us()
        let (sb, s) = encodeOne(e, pb!, n, c.fps, nil)
        guard s == noErr, let sample = sb else { errs += 1; continue }
        let t1 = us()
        if isKey(sample) { keys += 1 }
        if n == 0 { reportBitstream(c.name, sample) }
        sizes.append(Double(CMSampleBufferGetTotalSampleSize(sample)))
        let (img, ds) = dec.decode(sample)
        guard ds == noErr, let out = img else { errs += 1; continue }
        let t2 = us()
        if !binder.draw(out) { bindFail += 1 }
        let t3 = us()
        if n >= 10 { en.append((t1 - t0) / 1000); de.append((t2 - t1) / 1000); re.append((t3 - t2) / 1000); total.append((t3 - t0) / 1000) }
        if n % 30 == 0 { psnrs.append(psnr(luma8(pb!), luma8(out))) }
        last = t3
    }
    _ = last
    let mbps = sizes.reduce(0, +) * 8 * Double(c.fps) / Double(max(sizes.count, 1)) / 1e6
    R(String(format: "%@: hw=%@ enc p50=%.2f p99=%.2f | dec p50=%.2f p99=%.2f | gpu-bind p50=%.2f p99=%.2f | total p50=%.2f p99=%.2f ms | %.1f Mbps keys=%d errs=%d bindFail=%d psnr=%.1f rejectedProps=%@",
             c.name, usingHW(e), pct(en, 0.5), pct(en, 0.99), pct(de, 0.5), pct(de, 0.99), pct(re, 0.5), pct(re, 0.99), pct(total, 0.5), pct(total, 0.99), mbps, keys, errs, bindFail, psnrs.reduce(0, +) / Double(max(psnrs.count, 1)), bad.joined(separator: ",")))
    VTCompressionSessionInvalidate(e)
}

/// Drop frames 100..<103 from the decoder, then recover as the design would.
enum Recovery { case none, forceKey, ltr }
func lossTest(_ c: Cfg, _ rec: Recovery) {
    let (enc, st, bad) = makeEncoder(c)
    guard let e = enc else { R("\(c.name): encoder create FAILED status=\(st)"); return }
    let pool = makePool(c.fmt); let dec = Dec(c.fmt)
    var acked = [NSNumber](), tokens = 0, decodeErrs = 0, keysAfter = 0
    var size = [Int: Int](), ps = [Int: Double]()
    let lost = 100..<103, resume = 103
    for n in 0..<200 {
        var pb: CVPixelBuffer?
        CVPixelBufferPoolCreatePixelBuffer(nil, pool, &pb)
        fill(pb!, n)
        var o = [CFString: Any]()
        if rec == .ltr, !acked.isEmpty { o[kVTEncodeFrameOptionKey_AcknowledgedLTRTokens] = acked; acked = [] }
        if n == resume {
            if rec == .ltr { o[kVTEncodeFrameOptionKey_ForceLTRRefresh] = true }
            if rec == .forceKey { o[kVTEncodeFrameOptionKey_ForceKeyFrame] = true }
        }
        let (sb, s) = encodeOne(e, pb!, n, c.fps, o.isEmpty ? nil : o)
        guard s == noErr, let sample = sb else { R("\(c.name): encode error at \(n) status=\(s)"); return }
        size[n] = CMSampleBufferGetTotalSampleSize(sample)
        if let t = ltrToken(sample) { tokens += 1; if n < lost.lowerBound || n >= resume { acked.append(t) } }
        if n >= resume && isKey(sample) { keysAfter += 1 }
        if lost.contains(n) { continue }
        let (img, ds) = dec.decode(sample)
        if ds != noErr || img == nil { decodeErrs += 1; if n <= resume + 3 { R("  \(c.name): decode status \(ds) at frame \(n)") }; continue }
        if n >= resume - 6 && n <= resume + 20 || n % 20 == 0 { ps[n] = psnr(luma8(pb!), luma8(img!)) }
    }
    let avg = Double((10..<100).compactMap { size[$0] }.reduce(0, +)) / 90
    let before = ps[99] ?? .nan, firstAfter = ps[resume] ?? .nan, later = ps[resume + 10] ?? .nan
    R(String(format: "%@ recovery=%@: ltr_tokens=%d decode_errs=%d keyframes_after_loss=%d resume_frame_size=%.1fx avg psnr before=%.1f resume=%.1f resume+10=%.1f rejectedProps=%@",
             c.name, "\(rec)", tokens, decodeErrs, keysAfter, Double(size[resume] ?? 0) / max(avg, 1), before, firstAfter, later, bad.joined(separator: ",")))
    VTCompressionSessionInvalidate(e)
}

// ---- capability inventory
var list: CFArray?
VTCopyVideoEncoderList(nil, &list)
for e in (list as? [[CFString: Any]]) ?? [] where (e[kVTVideoEncoderList_IsHardwareAccelerated] as? Bool) == true {
    let c = e[kVTVideoEncoderList_CodecType] as! UInt32
    let fcc = String(bytes: withUnsafeBytes(of: c.bigEndian) { Array($0) }, encoding: .ascii) ?? "?"
    R("hw encoder: \(fcc) \(e[kVTVideoEncoderList_DisplayName] ?? "")")
}
for (n, c) in [("h264", kCMVideoCodecType_H264), ("hevc", kCMVideoCodecType_HEVC), ("av1", kCMVideoCodecType_AV1)] {
    R("hw decode \(n): \(VTIsHardwareDecodeSupported(c))")
}

let h264 = kCMVideoCodecType_H264, hevc = kCMVideoCodecType_HEVC
latency(Cfg(name: "h264-lowlatency", codec: h264, lowLatency: true, profile: kVTProfileLevel_H264_High_AutoLevel))
latency(Cfg(name: "hevc-lowlatency", codec: hevc, lowLatency: true, profile: kVTProfileLevel_HEVC_Main_AutoLevel))
latency(Cfg(name: "h264-realtime", codec: h264, profile: kVTProfileLevel_H264_High_AutoLevel))
latency(Cfg(name: "hevc-realtime", codec: hevc, profile: kVTProfileLevel_HEVC_Main_AutoLevel))
latency(Cfg(name: "hevc-444-realtime", codec: hevc, fmt: kCVPixelFormatType_444YpCbCr8BiPlanarVideoRange, profile: kVTProfileLevel_HEVC_Main_AutoLevel))
latency(Cfg(name: "h264-lowlatency-120fps", codec: h264, lowLatency: true, profile: kVTProfileLevel_H264_High_AutoLevel, bps: 40_000_000, fps: 120))

lossTest(Cfg(name: "h264-lowlatency", codec: h264, lowLatency: true, profile: kVTProfileLevel_H264_High_AutoLevel), .none)
lossTest(Cfg(name: "h264-lowlatency", codec: h264, lowLatency: true, profile: kVTProfileLevel_H264_High_AutoLevel), .forceKey)
lossTest(Cfg(name: "h264-lowlatency+ltr", codec: h264, lowLatency: true, ltr: true, profile: kVTProfileLevel_H264_High_AutoLevel), .ltr)
lossTest(Cfg(name: "hevc-lowlatency+ltr", codec: hevc, lowLatency: true, ltr: true, profile: kVTProfileLevel_HEVC_Main_AutoLevel), .ltr)
lossTest(Cfg(name: "hevc-realtime+ltr", codec: hevc, ltr: true, profile: kVTProfileLevel_HEVC_Main_AutoLevel), .ltr)
R("done")
