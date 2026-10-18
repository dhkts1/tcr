import Foundation
import ServiceManagement

/// TcrBar's side of the root fan helper (`crates/tcr-fan-helper`): its
/// registration with `SMAppService.daemon`, and `TcrBar --fan-helper
/// status|register|unregister`.
///
/// TcrBar never talks to the helper. The proxy does (`tcr`, signed
/// `io.github.dhkts1.tcr`), and the helper accepts nobody else. What TcrBar
/// owns is the registration, because only an app can register a daemon from
/// its own bundle, and only macOS knows whether the operator approved it.
///
/// Same rule as ``LoginItem``: **macOS owns the registration bit.** Every read
/// goes to `SMAppService.status`; nothing caches an "installed" Bool.
///
/// The plist is `Contents/Library/LaunchDaemons/<label>.plist`, placed by
/// `scripts/build-tcrbar.sh`, which also writes the label it shipped into
/// the app's `Info.plist` as ``infoPlistKey``. That key is the only source of
/// the label here: the script picks `.dev` from the signing tier as well as
/// `TCRBAR_DEV_BUILD`, so an Apple Development build of the shipping bundle id
/// ships the `.dev` helper, and a label guessed from the bundle id would name
/// one the bundle does not carry.
public enum FanHelperRegistration {
    /// The `Info.plist` key `build-tcrbar.sh` writes the helper's label into.
    public static let infoPlistKey = "TcrFanHelperLabel"

    /// Our reading of `SMAppService.Status`, as ``LoginItem/Status`` reads the
    /// main app's.
    public enum Status: Equatable, Sendable {
        /// Registered and allowed to run.
        case enabled
        /// Never installed, or unregistered.
        case notRegistered
        /// Registered, waiting for the operator to allow it in System Settings
        /// ▸ General ▸ Login Items.
        case requiresApproval
        /// macOS cannot find the plist or the bundle it registered: an
        /// unbundled build, a moved app, or a build without the helper. Also
        /// what macOS answered for a dev bundle whose helper plist was in place
        /// but had never been registered (2026-10-18), so before a first
        /// `register` this reads as "not installed", not as a broken bundle.
        case notFound
        /// A case Apple added after this was written. Surfaced, never guessed at.
        case unrecognised(rawValue: Int)
        /// This app's `Info.plist` names no helper label (``infoPlistKey``
        /// absent, or not one of the two labels): a `swift run`, the render
        /// harness, or a bundle not built by `build-tcrbar.sh`. macOS is not
        /// asked, because there is no label to ask about.
        case noLabel

        /// The word `--fan-helper status` prints.
        public var word: String {
            switch self {
            case .noLabel: return "noLabel"
            case .enabled: return "enabled"
            case .notRegistered: return "notRegistered"
            case .requiresApproval: return "requiresApproval"
            case .notFound: return "notFound"
            case .unrecognised(let raw): return "unrecognised(\(raw))"
            }
        }
    }

    public nonisolated static func classify(_ status: SMAppService.Status) -> Status {
        switch status {
        case .enabled: return .enabled
        case .notRegistered: return .notRegistered
        case .requiresApproval: return .requiresApproval
        case .notFound: return .notFound
        @unknown default: return .unrecognised(rawValue: status.rawValue)
        }
    }

    /// The shipping helper's launchd label, Mach service name and signing
    /// identifier. Must equal `FanIdentity::Release.helper_label()` in
    /// `crates/tcr-fan/src/identity.rs` and the plist in
    /// `apps/macos/LaunchDaemons/`.
    public static let releaseLabel = "io.github.dhkts1.tcrbar.fanhelper"
    /// The dev build's: the same with `.dev`.
    public static let devLabel = releaseLabel + ".dev"

    /// The label an `Info.plist` names under ``infoPlistKey``, or nil when it
    /// names none. Only the two labels the build script can write are taken;
    /// any other value is a bundle this build does not understand, and is
    /// reported as no label rather than registered under a name nothing ships.
    public static func label(fromInfo info: [String: Any]?) -> String? {
        guard let value = info?[infoPlistKey] as? String else { return nil }
        return value == releaseLabel || value == devLabel ? value : nil
    }

    public static func plistName(forLabel label: String) -> String {
        label + ".plist"
    }

    /// This app's label, from its own `Info.plist`. Nil for a `swift run` or
    /// the render harness, which have no bundle the script wrote.
    public static var label: String? {
        label(fromInfo: Bundle.main.infoDictionary)
    }

    private static func service(_ label: String) -> SMAppService {
        SMAppService.daemon(plistName: plistName(forLabel: label))
    }

    /// Read fresh from macOS every time; ``Status/noLabel`` without asking it
    /// when there is no label.
    public static func status(label: String? = label) -> Status {
        guard let label else { return .noLabel }
        return classify(service(label).status)
    }

    /// Why ``register(label:)`` or ``unregister(label:)`` did nothing.
    public struct NoLabelError: Error, CustomStringConvertible {
        public var description: String {
            "this app's Info.plist names no fan helper (\(FanHelperRegistration.infoPlistKey)); build it with build-tcrbar.sh"
        }
    }

    /// Install the helper. macOS then usually answers ``Status/requiresApproval``
    /// until the operator allows it in Login Items.
    public static func register(label: String? = label) throws {
        guard let label else { throw NoLabelError() }
        try service(label).register()
    }

    /// Remove the registration. For testing only; the panel offers no such
    /// control.
    public static func unregister(label: String? = label) throws {
        guard let label else { throw NoLabelError() }
        try service(label).unregister()
    }

    /// System Settings ▸ General ▸ Login Items, where "Allow in the
    /// Background" lists TcrBar.
    public static func openApproval() {
        SMAppService.openSystemSettingsLoginItems()
    }

    // MARK: `TcrBar --fan-helper status|register|unregister`

    public static let flag = "--fan-helper"

    /// What the command line asked for. `nil` from ``request(_:)`` means the
    /// flag is absent and the app should start normally.
    public enum Request: Equatable {
        case status
        case register
        case unregister
        /// The flag with no verb or an unknown one: printed and refused rather
        /// than starting the app, as ``KeepAwakeProbe`` does.
        case usage(problem: String)
    }

    public static func request(_ arguments: [String] = CommandLine.arguments) -> Request? {
        guard let i = arguments.firstIndex(of: flag) else { return nil }
        guard i + 1 < arguments.count else {
            return .usage(problem: "\(flag) needs one of: status, register, unregister")
        }
        switch arguments[i + 1] {
        case "status": return .status
        case "register": return .register
        case "unregister": return .unregister
        case let other:
            return .usage(problem: "\(flag): unknown verb '\(other)' (status, register, unregister)")
        }
    }

    /// Print, act, exit. Starts no server and draws nothing.
    public static func run(_ request: Request) -> Never {
        if case .usage(let problem) = request {
            write(problem, to: FileHandle.standardError)
            exit(2)
        }
        guard let label = self.label else {
            // No label: say so on the status line, and refuse a register or
            // unregister rather than guess one.
            print("\(infoPlistKey) absent: \(Status.noLabel.word)")
            if request != .status {
                write(NoLabelError().description, to: FileHandle.standardError)
                exit(1)
            }
            exit(0)
        }
        switch request {
        case .usage:
            exit(2)
        case .status:
            print("\(label): \(status(label: label).word)")
            exit(0)
        case .register, .unregister:
            do {
                if request == .register {
                    try register(label: label)
                } else {
                    try unregister(label: label)
                }
            } catch {
                write(
                    "\(label): \(request == .register ? "register" : "unregister") failed: \(error)",
                    to: FileHandle.standardError)
                print("\(label): \(status(label: label).word)")
                exit(1)
            }
            print("\(label): \(status(label: label).word)")
            exit(0)
        }
    }

    private static func write(_ line: String, to handle: FileHandle) {
        handle.write(Data((line + "\n").utf8))
    }
}
