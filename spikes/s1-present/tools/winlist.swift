// Lists on-screen windows of one process. Bounds and layer need no Screen Recording permission.
import CoreGraphics
import Foundation

let pid = Int(CommandLine.arguments[1])!
let opts: CGWindowListOption = [.optionOnScreenOnly, .excludeDesktopElements]
let list = CGWindowListCopyWindowInfo(opts, kCGNullWindowID) as! [[String: Any]]
for (order, w) in list.enumerated() where (w[kCGWindowOwnerPID as String] as? Int) == pid {
    let b = w[kCGWindowBounds as String] as! [String: Any]
    print("z=\(order) layer=\(w[kCGWindowLayer as String]!) x=\(b["X"]!) y=\(b["Y"]!) w=\(b["Width"]!) h=\(b["Height"]!)")
}
