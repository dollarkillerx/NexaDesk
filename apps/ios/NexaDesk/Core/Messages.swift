import Foundation

/// Why a session ended, in the person's language: a title, a blank line, what to do.
/// Mirrors the desktop app's Texts.Disconnect.* (ui/NexaDesk.App/Localization), with
/// "the computer" for "the other Mac", since an iPhone may control a Mac or a PC.
enum Messages {
    static func disconnected(_ d: Nexa_Client_Disconnected) -> String {
        let (title, detail): (String, String)
        switch d.reason {
        case .disconnectClosedLocally:
            (title, detail) = (String(localized: "Disconnected"), String(localized: "You ended this session."))
        case .disconnectHostEnded:
            (title, detail) = (String(localized: "The computer ended the session"),
                               String(localized: "The host disconnected or stopped allowing control."))
        case .disconnectReplaced:
            (title, detail) = (String(localized: "Session taken over"),
                               String(localized: "Another device connected to the computer, and the person there agreed to the takeover."))
        case .disconnectPipelineFailed:
            (title, detail) = (String(localized: "The computer can't share its screen"),
                               String(localized: "Screen capture or video encoding failed on the host. On a Mac, NexaDesk needs the Screen Recording permission."))
        case .disconnectNetwork:
            (title, detail) = (String(localized: "Network connection failed"),
                               String(localized: "Can't reach the computer. Check the address and port, that it has “Allow remote control” turned on, and the firewalls on both sides."))
        case .disconnectPinMismatch:
            (title, detail) = (String(localized: "The computer's identity doesn't match the saved pairing"),
                               String(localized: "NexaDesk may have been reinstalled there, or someone may be impersonating it. Check first, then pair again with a one-time code."))
        case .disconnectNotPaired:
            (title, detail) = (String(localized: "Not paired with this device"),
                               String(localized: "The computer has no saved pairing with this device. Enter the one-time code shown on its screen."))
        case .disconnectAuthFailed:
            (title, detail) = (String(localized: "Incorrect one-time code or password"),
                               String(localized: "Check the 8-character code shown on the computer's screen, or its fixed password. After 5 wrong codes in a row, the code changes automatically."))
        case .disconnectRateLimited:
            (title, detail) = (String(localized: "Too many attempts"),
                               String(localized: "To prevent guessing, the computer is refusing new attempts for now. Wait a minute and try again."))
        case .disconnectConsentDenied:
            (title, detail) = (String(localized: "The connection was not accepted"),
                               String(localized: "The person at the computer declined, didn't respond in time, or nobody is there to accept."))
        case .disconnectNoCommonCodec:
            (title, detail) = (String(localized: "No video codec both sides support"),
                               String(localized: "The two devices' hardware codecs don't match, so no video stream can be set up."))
        case .disconnectVersionMismatch:
            (title, detail) = (String(localized: "Incompatible NexaDesk versions"),
                               String(localized: "Update NexaDesk on both sides to the same version."))
        case .disconnectTimedOut:
            (title, detail) = (String(localized: "Connection timed out"),
                               String(localized: "The computer didn't respond in time. Make sure the address is right and the computer is online, then try again."))
        case .disconnectInvalidRequest:
            (title, detail) = (String(localized: "Invalid connection request"),
                               String(localized: "The address or one-time code isn't in the right format."))
        case .disconnectInternal:
            (title, detail) = (String(localized: "NexaDesk internal error"),
                               String(localized: "Something unexpected went wrong. Try again; if it keeps happening, restart NexaDesk."))
        case .disconnectOffline:
            (title, detail) = (String(localized: "The computer is offline"),
                               String(localized: "This device ID isn't connected to the signaling server right now. Make sure the computer has “Allow remote control” turned on and both sides use the same signaling server."))
        case .disconnectNoPath:
            (title, detail) = (String(localized: "No working network path"),
                               String(localized: "Direct, hole punching and relay all failed. A firewall may be blocking UDP, or the signaling server has no relay configured."))
        case .disconnectSignalServer:
            (title, detail) = (String(localized: "Can't use the signaling server"),
                               String(localized: "Couldn't reach the signaling server, or it refused the request. Check the server address and your network."))
        default:
            (title, detail) = (String(localized: "Connection lost"), String(localized: "The connection dropped unexpectedly."))
        }
        return "\(title)\n\n\(detail)"
    }
}
