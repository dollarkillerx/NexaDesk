// Posts move, click and wheel events to one process with CGEventPostToPid.
// The real cursor does not move and no other app receives anything.
// Usage: postmouse <pid> <client-height-points> <x-in-client> <y-in-client>
import CoreGraphics
import Foundation

let a = CommandLine.arguments
let pid = pid_t(a[1])!
let clientH = Double(a[2])!, cx = Double(a[3])!, cy = Double(a[4])!

let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as! [[String: Any]]
let wins = list.filter { ($0[kCGWindowOwnerPID as String] as? Int32) == pid && ($0[kCGWindowLayer as String] as? Int) == 0 }
guard let win = wins.max(by: { (($0[kCGWindowBounds as String] as! [String: Double])["Height"]!) < (($1[kCGWindowBounds as String] as! [String: Double])["Height"]!) }) else {
    print("no window"); exit(1)
}
let b = win[kCGWindowBounds as String] as! [String: Double]
let num = Int64(win[kCGWindowNumber as String] as! Int)
// Content sits below the title bar: window height minus client height.
let p = CGPoint(x: b["X"]! + cx, y: b["Y"]! + (b["Height"]! - clientH) + cy)

let src = CGEventSource(stateID: .privateState)
func post(_ e: CGEvent) {
    e.setIntegerValueField(.mouseEventWindowUnderMousePointer, value: num)
    e.setIntegerValueField(.mouseEventWindowUnderMousePointerThatCanHandleThisEvent, value: num)
    e.postToPid(pid)
    usleep(40_000)
}
for t in [CGEventType.mouseMoved, .leftMouseDown, .leftMouseUp] {
    post(CGEvent(mouseEventSource: src, mouseType: t, mouseCursorPosition: p, mouseButton: .left)!)
}
let wheel = CGEvent(scrollWheelEvent2Source: src, units: .pixel, wheelCount: 1, wheel1: -30, wheel2: 0, wheel3: 0)!
wheel.location = p
post(wheel)
print("window=\(num) at=\(Int(p.x)),\(Int(p.y))")
