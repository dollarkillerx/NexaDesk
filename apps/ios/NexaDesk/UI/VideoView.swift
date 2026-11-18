import SwiftUI
import UIKit

/// The host's picture, drawn by the core into this view's own CAMetalLayer.
///
/// Touch works like a touchscreen on the remote desktop:
/// - tap: click there; two-finger tap: right click;
/// - one-finger drag: press, move, release (drag a window, select text);
/// - two-finger drag: scroll.
/// Keys come from `KeyInputView` and the keyboard bar.
final class VideoUIView: UIView {
    override class var layerClass: AnyClass { CAMetalLayer.self }
    var metalLayer: CAMetalLayer { layer as! CAMetalLayer }

    weak var core: NexaCore?
    private var attached = false
    private var lastScroll = CGPoint.zero
    /// Where the latest touch went down: a drag is recognized only after the finger
    /// has moved a little, and the press belongs where it started.
    private var touchDown = CGPoint.zero

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        if let t = touches.first { touchDown = t.location(in: self) }
        super.touchesBegan(touches, with: event)
    }

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = .black
        isMultipleTouchEnabled = true

        let tap = UITapGestureRecognizer(target: self, action: #selector(onTap(_:)))
        let twoFingerTap = UITapGestureRecognizer(target: self, action: #selector(onTwoFingerTap(_:)))
        twoFingerTap.numberOfTouchesRequired = 2
        tap.require(toFail: twoFingerTap)
        let drag = UIPanGestureRecognizer(target: self, action: #selector(onDrag(_:)))
        drag.maximumNumberOfTouches = 1
        let scroll = UIPanGestureRecognizer(target: self, action: #selector(onScroll(_:)))
        scroll.minimumNumberOfTouches = 2
        scroll.maximumNumberOfTouches = 2
        [tap, twoFingerTap, drag, scroll].forEach(addGestureRecognizer)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    // MARK: Attaching

    private var pixelSize: (w: Int, h: Int, scale: CGFloat) {
        let scale = window?.screen.scale ?? UIScreen.main.scale
        return (Int(bounds.width * scale), Int(bounds.height * scale), scale)
    }

    func attachIfReady() {
        guard !attached, window != nil, bounds.width > 0, let core else { return }
        let p = pixelSize
        metalLayer.contentsScale = p.scale
        // A failed attach is tried again at the next layout.
        attached = core.attach(layer: metalLayer, widthPx: p.w, heightPx: p.h, scale: p.scale)
    }

    func detach() {
        guard attached else { return }
        core?.detach()
        attached = false
    }

    override func didMoveToWindow() {
        super.didMoveToWindow()
        if window == nil { detach() } else { attachIfReady() }
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        if attached {
            let p = pixelSize
            core?.resize(widthPx: p.w, heightPx: p.h, scale: p.scale)
        } else {
            attachIfReady()
        }
    }

    // MARK: Touch → pointer (view pixels)

    private func pixels(_ point: CGPoint) -> (Double, Double) {
        let scale = Double(pixelSize.scale)
        return (Double(point.x) * scale, Double(point.y) * scale)
    }

    private func moveTo(_ point: CGPoint) {
        let (x, y) = pixels(point)
        core?.move(x: x, y: y)
    }

    @objc private func onTap(_ g: UITapGestureRecognizer) {
        moveTo(g.location(in: self))
        core?.button(0, down: true)
        core?.button(0, down: false)
    }

    @objc private func onTwoFingerTap(_ g: UITapGestureRecognizer) {
        moveTo(g.location(in: self))
        core?.button(1, down: true)
        core?.button(1, down: false)
    }

    @objc private func onDrag(_ g: UIPanGestureRecognizer) {
        let point = g.location(in: self)
        switch g.state {
        case .began:
            // Where the finger first touched, not where the drag was recognized.
            moveTo(touchDown)
            core?.button(0, down: true)
            moveTo(point)
        case .changed:
            moveTo(point)
        default:
            moveTo(point)
            core?.button(0, down: false)
        }
    }

    @objc private func onScroll(_ g: UIPanGestureRecognizer) {
        let t = g.translation(in: self)
        if g.state == .began {
            moveTo(g.location(in: self))
            lastScroll = .zero
        }
        // About one notch (120) per 40 points, natural direction like the trackpad.
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

    func makeUIView(context: Context) -> VideoUIView {
        let view = VideoUIView()
        view.core = core
        return view
    }

    func updateUIView(_ view: VideoUIView, context: Context) {
        view.attachIfReady()
    }

    static func dismantleUIView(_ view: VideoUIView, coordinator: ()) {
        view.detach()
    }
}
