import SwiftUI
import UIKit

/// How a finger drives the remote pointer.
enum PointerMode: String {
    /// Like a laptop's trackpad: the finger moves a pointer drawn over the picture, and
    /// a tap clicks where the pointer is. Precise on a small screen; the default.
    case trackpad
    /// Like a touchscreen: a tap clicks where the finger is.
    case touch
}

/// The host's picture, drawn by the core into this view's own CAMetalLayer. It only
/// shows the picture: zooming and every gesture belong to `SessionCanvas`.
final class VideoUIView: UIView {
    override class var layerClass: AnyClass { CAMetalLayer.self }
    var metalLayer: CAMetalLayer { layer as! CAMetalLayer }

    weak var core: NexaCore?
    private var attached = false
    /// While a pinch is under way the picture is only stretched; the core hears the new
    /// size once, when it ends (each size is a new picture from the host).
    var deferResize = false
    private var sentSize: (w: Int, h: Int) = (0, 0)

    /// The longest side, in pixels, the view asks for: zoomed in 3× on a 3× screen it
    /// would otherwise want a 7,000-pixel picture, more than any host sends and a lot
    /// of memory for the drawables.
    static let maxSidePx: CGFloat = 4096

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = .black
        isUserInteractionEnabled = false
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    /// Pixels per point: the screen's, less when the view is very large.
    var pixelScale: CGFloat {
        let screen = window?.screen.scale ?? UIScreen.main.scale
        let side = max(bounds.width, bounds.height)
        return side > 0 ? min(screen, Self.maxSidePx / side) : screen
    }

    private var pixelSize: (w: Int, h: Int) {
        let s = pixelScale
        return (Int(bounds.width * s), Int(bounds.height * s))
    }

    func attachIfReady() {
        guard !attached, window != nil, bounds.width > 0, let core else { return }
        let p = pixelSize
        // A failed attach is tried again at the next layout.
        attached = core.attach(layer: metalLayer, widthPx: p.w, heightPx: p.h, scale: pixelScale)
        if attached { sentSize = p }
    }

    func detach() {
        guard attached else { return }
        core?.detach()
        attached = false
    }

    /// Tells the core the view's size, if it changed since it was last told.
    func commitSize() {
        guard attached, !deferResize else { return }
        let p = pixelSize
        guard p != sentSize, p.w > 0, p.h > 0 else { return }
        sentSize = p
        core?.resize(widthPx: p.w, heightPx: p.h, scale: pixelScale)
    }

    override func didMoveToWindow() {
        super.didMoveToWindow()
        if window == nil { detach() } else { attachIfReady() }
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        if attached { commitSize() } else { attachIfReady() }
    }

    /// Where `point` (in this view) is, in the view pixels the core's commands use.
    func pixels(_ point: CGPoint) -> (Double, Double) {
        let s = pixelScale
        return (Double(point.x * s), Double(point.y * s))
    }
}

/// The session's picture and everything a finger does on it.
///
/// In both modes a two-finger tap is a right click, a two-finger drag scrolls, and a
/// pinch zooms (up to 3×, sharper as the host sends more pixels); moving the fingers
/// while pinching pans.
/// - Trackpad: one finger moves the pointer, a tap clicks at it, touch and hold then
///   move drags; when zoomed in the picture follows the pointer.
/// - Touch: a tap clicks under the finger, a one-finger drag drags, touch and hold is a
///   right click.
final class SessionCanvas: UIView, UIGestureRecognizerDelegate {
    let video = VideoUIView()
    private let pointerView = UIImageView()

    weak var core: NexaCore? {
        didSet { video.core = core }
    }
    var mode: PointerMode = .trackpad {
        didSet { if mode != oldValue { updatePointer() } }
    }
    /// The host's picture in pixels, which fills the video view letterboxed.
    var streamSize: CGSize = .zero {
        didSet { if streamSize != oldValue { clampPointer(); updatePointer() } }
    }

    static let maxZoom: CGFloat = 3
    private var zoom: CGFloat = 1
    /// Where the zoomed picture's top-left corner is, in this view.
    private var offset: CGPoint = .zero
    private var pinchStart: (zoom: CGFloat, offset: CGPoint, centroid: CGPoint)?
    private var pinching = false
    /// The pointer as a fraction of the video view, so it stays put through zooms.
    private var pointer = CGPoint(x: 0.5, y: 0.5)
    /// Where the latest touch went down: a drag is recognized only after the finger has
    /// moved a little, and the press (or the movement) starts where it went down.
    private var touchDown = CGPoint.zero
    private var lastDrag = CGPoint.zero
    private var lastScroll = CGPoint.zero
    private let tapFeedback = UIImpactFeedbackGenerator(style: .light)
    private let holdFeedback = UIImpactFeedbackGenerator(style: .medium)

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = .black
        clipsToBounds = true
        isMultipleTouchEnabled = true
        addSubview(video)

        let config = UIImage.SymbolConfiguration(pointSize: 20, weight: .regular)
        pointerView.image = UIImage(systemName: "cursorarrow", withConfiguration: config)?
            .withTintColor(.white, renderingMode: .alwaysOriginal)
        pointerView.layer.shadowColor = UIColor.black.cgColor
        pointerView.layer.shadowOpacity = 1
        pointerView.layer.shadowRadius = 1
        pointerView.layer.shadowOffset = .zero
        pointerView.sizeToFit()
        pointerView.isHidden = true
        addSubview(pointerView)

        let tap = UITapGestureRecognizer(target: self, action: #selector(onTap(_:)))
        let twoFingerTap = UITapGestureRecognizer(target: self, action: #selector(onTwoFingerTap(_:)))
        twoFingerTap.numberOfTouchesRequired = 2
        tap.require(toFail: twoFingerTap)
        let hold = UILongPressGestureRecognizer(target: self, action: #selector(onHold(_:)))
        hold.minimumPressDuration = 0.4
        let drag = UIPanGestureRecognizer(target: self, action: #selector(onDrag(_:)))
        drag.maximumNumberOfTouches = 1
        let scroll = UIPanGestureRecognizer(target: self, action: #selector(onScroll(_:)))
        scroll.minimumNumberOfTouches = 2
        scroll.maximumNumberOfTouches = 2
        let pinch = UIPinchGestureRecognizer(target: self, action: #selector(onPinch(_:)))
        [scroll, pinch].forEach { $0.delegate = self }
        [tap, twoFingerTap, hold, drag, scroll, pinch].forEach(addGestureRecognizer)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    // Two fingers may be scrolling or pinching: both are recognized, and once the
    // pinch has begun the scroll stands aside.
    func gestureRecognizer(_ g: UIGestureRecognizer,
                           shouldRecognizeSimultaneouslyWith other: UIGestureRecognizer) -> Bool {
        let pair = [g, other]
        return pair.contains { $0 is UIPinchGestureRecognizer }
            && pair.contains { ($0 as? UIPanGestureRecognizer)?.minimumNumberOfTouches == 2 }
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        if let t = touches.first { touchDown = t.location(in: self) }
        tapFeedback.prepare()
        super.touchesBegan(touches, with: event)
    }

    // MARK: Zoom

    override func layoutSubviews() {
        super.layoutSubviews()
        clampOffset()
        video.frame = CGRect(origin: offset, size: CGSize(width: bounds.width * zoom, height: bounds.height * zoom))
        updatePointer()
    }

    /// Keeps the picture, not its letterbox, on screen: on an axis where the zoomed
    /// picture is larger than the screen it covers it, else it is centered.
    private func clampOffset() {
        let r = Self.fit(streamSize, in: bounds.size)
        func clamp(_ o: CGFloat, view: CGFloat, start: CGFloat, length: CGFloat) -> CGFloat {
            let lo = start * zoom, size = length * zoom
            if size <= view { return (view - size) / 2 - lo }
            return min(-lo, max(view - lo - size, o))
        }
        offset.x = clamp(offset.x, view: bounds.width, start: r.minX, length: r.width)
        offset.y = clamp(offset.y, view: bounds.height, start: r.minY, length: r.height)
    }

    @objc private func onPinch(_ g: UIPinchGestureRecognizer) {
        switch g.state {
        case .began:
            pinching = true
            video.deferResize = true
            pinchStart = (zoom, offset, g.location(in: self))
        case .changed:
            // With one finger lifted the centroid jumps: wait for both, or the end.
            guard let start = pinchStart, g.numberOfTouches >= 2 else { return }
            let centroid = g.location(in: self)
            zoom = min(Self.maxZoom, max(1, start.zoom * g.scale))
            // The spot that was under the fingers stays under them, and follows them.
            let spot = CGPoint(x: (start.centroid.x - start.offset.x) / start.zoom,
                               y: (start.centroid.y - start.offset.y) / start.zoom)
            offset = CGPoint(x: centroid.x - spot.x * zoom, y: centroid.y - spot.y * zoom)
            setNeedsLayout()
        default:
            pinching = false
            pinchStart = nil
            if zoom < 1.1 { zoom = 1 }
            setNeedsLayout()
            layoutIfNeeded()
            video.deferResize = false
            video.commitSize()
        }
    }

    // MARK: The pointer

    /// Where the picture is in the video view (letterboxed), in the video view's points.
    private var pictureRect: CGRect { Self.fit(streamSize, in: video.bounds.size) }

    /// A picture of `size` fitted into `area`, letterboxed; all of `area` when either is empty.
    static func fit(_ size: CGSize, in area: CGSize) -> CGRect {
        guard size.width > 0, size.height > 0, area.width > 0, area.height > 0 else {
            return CGRect(origin: .zero, size: area)
        }
        let k = min(area.width / size.width, area.height / size.height)
        let w = size.width * k, h = size.height * k
        return CGRect(x: (area.width - w) / 2, y: (area.height - h) / 2, width: w, height: h)
    }

    private var pointerInVideo: CGPoint {
        CGPoint(x: pointer.x * video.bounds.width, y: pointer.y * video.bounds.height)
    }

    /// Puts the pointer at `p` (in the video view), kept on the picture.
    private func setPointer(inVideo p: CGPoint) {
        let b = video.bounds
        guard b.width > 0, b.height > 0 else { return }
        let r = pictureRect
        let x = min(r.maxX - 0.5, max(r.minX, p.x))
        let y = min(r.maxY - 0.5, max(r.minY, p.y))
        pointer = CGPoint(x: x / b.width, y: y / b.height)
    }

    private func clampPointer() {
        setPointer(inVideo: pointerInVideo)
    }

    private func updatePointer() {
        pointerView.isHidden = mode != .trackpad || streamSize == .zero
        let p = video.convert(pointerInVideo, to: self)
        // The symbol's tip is near its top left.
        pointerView.frame.origin = CGPoint(x: p.x - pointerView.bounds.width * 0.28,
                                           y: p.y - pointerView.bounds.height * 0.12)
    }

    /// Zoomed in, the picture pans so the pointer stays on screen.
    private func follow() {
        guard zoom > 1 else { return }
        let p = video.convert(pointerInVideo, to: self)
        let inner = bounds.insetBy(dx: min(48, bounds.width / 4), dy: min(48, bounds.height / 4))
        if p.x < inner.minX { offset.x += inner.minX - p.x }
        if p.x > inner.maxX { offset.x -= p.x - inner.maxX }
        if p.y < inner.minY { offset.y += inner.minY - p.y }
        if p.y > inner.maxY { offset.y -= p.y - inner.maxY }
        setNeedsLayout()
    }

    private func sendPointer() {
        let (x, y) = video.pixels(pointerInVideo)
        core?.move(x: x, y: y)
        updatePointer()
    }

    /// The pointer to where the finger is, in touch mode.
    private func point(at location: CGPoint) {
        setPointer(inVideo: convert(location, to: video))
        sendPointer()
    }

    /// The pointer moved by a finger's movement on screen: the same on screen at any
    /// zoom, and farther when the finger is fast, as on a trackpad.
    private func nudge(by delta: CGPoint, velocity: CGPoint) {
        let speed = hypot(velocity.x, velocity.y)
        let gain = 1.0 + min(speed / 800, 1.5)
        let p = video.convert(pointerInVideo, to: self)
        let moved = CGPoint(x: p.x + delta.x * gain, y: p.y + delta.y * gain)
        setPointer(inVideo: convert(moved, to: video))
        sendPointer()
        follow()
    }

    private func click(_ button: UInt32) {
        core?.button(button, down: true)
        core?.button(button, down: false)
        tapFeedback.impactOccurred()
    }

    // MARK: Gestures

    /// The pointer to where a gesture happens: under the finger, or where it already is.
    private func aim(_ g: UIGestureRecognizer) {
        if mode == .touch { point(at: g.location(in: self)) } else { sendPointer() }
    }

    @objc private func onTap(_ g: UITapGestureRecognizer) {
        aim(g)
        click(0)
    }

    @objc private func onTwoFingerTap(_ g: UITapGestureRecognizer) {
        aim(g)
        click(1)
    }

    @objc private func onHold(_ g: UILongPressGestureRecognizer) {
        let location = g.location(in: self)
        switch mode {
        case .touch:
            if g.state == .began {
                point(at: location)
                click(1)
            }
        case .trackpad:
            // Touch and hold, then move: a drag with the button held.
            switch g.state {
            case .began:
                holdFeedback.impactOccurred()
                sendPointer()
                core?.button(0, down: true)
                lastDrag = location
            case .changed:
                nudge(by: CGPoint(x: location.x - lastDrag.x, y: location.y - lastDrag.y), velocity: .zero)
                lastDrag = location
            default:
                core?.button(0, down: false)
            }
        }
    }

    @objc private func onDrag(_ g: UIPanGestureRecognizer) {
        let location = g.location(in: self)
        switch mode {
        case .trackpad:
            if g.state == .began { lastDrag = touchDown }
            guard g.state == .began || g.state == .changed else { return }
            nudge(by: CGPoint(x: location.x - lastDrag.x, y: location.y - lastDrag.y), velocity: g.velocity(in: self))
            lastDrag = location
        case .touch:
            switch g.state {
            case .began:
                // Pressed where the finger first touched, not where the drag was recognized.
                point(at: touchDown)
                core?.button(0, down: true)
                point(at: location)
            case .changed:
                point(at: location)
            default:
                point(at: location)
                core?.button(0, down: false)
            }
        }
    }

    @objc private func onScroll(_ g: UIPanGestureRecognizer) {
        let t = g.translation(in: self)
        if g.state == .began {
            aim(g)
            lastScroll = .zero
        }
        // Fingers that are zooming do not scroll.
        guard !pinching else { lastScroll = t; return }
        // About one notch (120) per 40 points, in the natural direction, as on a trackpad.
        let dx = Int32((t.x - lastScroll.x) * 3)
        let dy = Int32((t.y - lastScroll.y) * 3)
        if dx != 0 || dy != 0 {
            core?.wheel(dx: -dx, dy: dy)
            lastScroll = t
        }
    }
}

struct VideoView: UIViewRepresentable {
    let core: NexaCore
    let mode: PointerMode
    /// A value of its own, not read from `core`, so SwiftUI updates the view when it changes.
    let stream: CGSize

    func makeUIView(context: Context) -> SessionCanvas {
        let canvas = SessionCanvas()
        canvas.core = core
        return canvas
    }

    func updateUIView(_ canvas: SessionCanvas, context: Context) {
        canvas.mode = mode
        canvas.streamSize = stream
        canvas.video.attachIfReady()
    }

    static func dismantleUIView(_ canvas: SessionCanvas, coordinator: ()) {
        canvas.video.detach()
    }
}
