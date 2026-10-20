import Foundation

// The "Tailscale" row on Settings > Peers > This Mac: one picker with three
// choices (Off, the Tailscale app, tcr's own node) and one state line under it.
//
// Pure data, in `TcrBarCore` for the reason `PeerInternet.swift` gives: the
// test target links this module alone, so every sentence the row prints is a
// value a test can build.
//
// The wire shape is `tcr peer tailnet --json`, fixed by the tailnet contract
// (section 2): `mode`, `nodeSupported`, and `held`, which is what the SERVING
// process recorded on its last tailnet tick.

/// The setting, as `tcr-peers.json` holds it and `tcr peer tailnet <mode>`
/// writes it.
public enum PeerTailnetMode: String, CaseIterable, Equatable, Sendable {
    case off
    case app
    case node

    /// The picker's label, closed and in the menu.
    ///
    /// No ellipsis on the own node yet, where the mockup's scene 2 draws one:
    /// an ellipsis promises a sheet, and the join sheet is not built yet.
    /// Until then choosing it runs the verb and opens nothing.
    public var title: String {
        switch self {
        case .off: return "Off"
        case .app: return "Tailscale app"
        case .node: return "tcr's own node"
        }
    }

    /// The menu item's second line.
    public func menuDetail(nodeSupported: Bool) -> String {
        switch self {
        case .off: return "A pasted key still works over a tailnet."
        case .app: return "Finds your own Macs through the app."
        case .node:
            return nodeSupported ? "Experimental. Joins by itself, no app." : "Not in this build"
        }
    }

    /// Whether the menu offers this choice. Only the own node can be
    /// missing, on a `tcr` built without the `tailnet-node` feature.
    public func isOffered(nodeSupported: Bool) -> Bool {
        self != .node || nodeSupported
    }
}

/// One `tcr peer tailnet --json` answer.
public struct PeerTailnetReading: Equatable, Sendable {
    /// The setting as the file says now.
    public var mode: PeerTailnetMode
    /// This `tcr` was built with the `tailnet-node` feature.
    public var nodeSupported: Bool
    /// What the serving process last recorded, or `nil` when none has.
    public var held: Held?

    public init(mode: PeerTailnetMode, nodeSupported: Bool, held: Held?) {
        self.mode = mode
        self.nodeSupported = nodeSupported
        self.held = held
    }

    /// The serving process's own record, read off `peer-state.json`.
    public struct Held: Equatable, Sendable {
        /// The mode the server acted on. Lags ``PeerTailnetReading/mode`` by
        /// up to 20 seconds.
        public var mode: PeerTailnetMode
        public var state: State
        /// This Mac's tailnet IPv4 in that mode.
        public var address: String?
        /// Node mode only: the name on the tailnet.
        public var nodeName: String?
        /// App mode: own Macs that sent a beacon in the last 60 s.
        public var found: Int?
        /// Node mode: paired Macs with a live connection through the node.
        public var connected: Int?
        /// One plain sentence for a failure state.
        public var detail: String?
        /// Unix milliseconds. A reader treats a record older than
        /// ``PeerTailnetState/staleAfterMs`` as stale.
        public var recordedAtMs: Int64

        public init(
            mode: PeerTailnetMode, state: State, address: String? = nil,
            nodeName: String? = nil, found: Int? = nil, connected: Int? = nil,
            detail: String? = nil, recordedAtMs: Int64
        ) {
            self.mode = mode
            self.state = state
            self.address = address
            self.nodeName = nodeName
            self.found = found
            self.connected = connected
            self.detail = detail
            self.recordedAtMs = recordedAtMs
        }
    }

    /// Every `held.state` the contract names, plus the raw token for one
    /// this build does not know, which draws as itself rather than as a
    /// state it is not.
    public enum State: Equatable, Sendable {
        case off
        case ready
        case appMissing
        case appNotRunning
        case readFailed
        case nodeNotJoined
        case nodeConnecting
        case nodeUnsupported
        case unknown(String)

        public init(token: String) {
            switch token {
            case "off": self = .off
            case "ready": self = .ready
            case "appMissing": self = .appMissing
            case "appNotRunning": self = .appNotRunning
            case "readFailed": self = .readFailed
            case "nodeNotJoined": self = .nodeNotJoined
            case "nodeConnecting": self = .nodeConnecting
            case "nodeUnsupported": self = .nodeUnsupported
            default: self = .unknown(token)
            }
        }
    }

    /// Decode one `tcr peer tailnet --json` payload.
    ///
    /// `mode` is required and must be one of the three: a setting this build
    /// cannot name is not a setting the picker can show, so it throws and the
    /// row says it could not read the answer. `held` is optional, and a
    /// `held` missing `mode`, `state` or `recordedAtMs` reads as no record:
    /// a partial record is not a state to draw.
    public static func decode(_ data: Data) throws -> PeerTailnetReading {
        let top = try JSONSerialization.jsonObject(with: data)
        guard let object = top as? [String: Any] else {
            throw DecodingError.dataCorrupted(
                .init(
                    codingPath: [],
                    debugDescription:
                        "tcr peer tailnet --json emits a JSON object; got \(type(of: top))"))
        }
        guard let token = object["mode"] as? String, let mode = PeerTailnetMode(rawValue: token)
        else {
            throw DecodingError.dataCorrupted(
                .init(
                    codingPath: [],
                    debugDescription:
                        "tcr peer tailnet --json named no mode this build knows: "
                        + String(describing: object["mode"] ?? "nothing")))
        }
        return PeerTailnetReading(
            mode: mode,
            nodeSupported: object["nodeSupported"] as? Bool ?? false,
            held: held(object["held"]))
    }

    static func held(_ value: Any?) -> Held? {
        guard let object = value as? [String: Any],
            let modeToken = object["mode"] as? String,
            let mode = PeerTailnetMode(rawValue: modeToken),
            let stateToken = object["state"] as? String,
            let recorded = (object["recordedAtMs"] as? NSNumber)?.int64Value
        else { return nil }
        return Held(
            mode: mode,
            state: State(token: stateToken),
            address: object["address"] as? String,
            nodeName: object["nodeName"] as? String,
            found: object["found"] as? Int,
            connected: object["connected"] as? Int,
            detail: object["detail"] as? String,
            recordedAtMs: recorded)
    }
}

/// What the row has to go on: nothing yet, a `tcr` without the verb, an
/// answer it could not read, or a reading.
public enum PeerTailnetRead: Equatable, Sendable {
    case notRead
    /// This `tcr` has no `peer tailnet` verb. Not a failure: the row says
    /// so and the picker is disabled.
    case unsupported
    /// `tcr` refused, or answered something this build cannot read. Its own
    /// words.
    case unreadable(String)
    case read(PeerTailnetReading)

    /// Classify what the verb printed.
    public static func output(_ text: String) -> PeerTailnetRead {
        do {
            return .read(try PeerTailnetReading.decode(Data(text.utf8)))
        } catch {
            return .unreadable("tcr peer tailnet answered something this build cannot read")
        }
    }

    /// Classify a failed run. An older `tcr` exits through clap with
    /// "unrecognized subcommand", which is the same test the Peers tab's
    /// own `peer ls` read applies.
    public static func failure(_ message: String) -> PeerTailnetRead {
        let lowered = message.lowercased()
        if lowered.contains("unrecognized subcommand") || lowered.contains("unexpected argument") {
            return .unsupported
        }
        return .unreadable(message)
    }

    public var reading: PeerTailnetReading? {
        if case .read(let reading) = self { return reading }
        return nil
    }

    /// The setting the picker shows as chosen, or `nil` when nothing was read.
    public var mode: PeerTailnetMode? { reading?.mode }

    public var nodeSupported: Bool { reading?.nodeSupported ?? false }

    /// Whether the picker can be used. Not before the first answer, and not
    /// on a `tcr` that has no verb to run.
    public var canPick: Bool {
        switch self {
        case .notRead, .unsupported: return false
        case .unreadable, .read: return true
        }
    }

    /// The picker's closed label.
    public var pickerTitle: String {
        switch self {
        case .notRead: return "Not read yet"
        case .unsupported: return PeerTailnetMode.off.title
        case .unreadable: return "Not read"
        case .read(let reading): return reading.mode.title
        }
    }
}

/// The press a state line can carry after its sentence.
public enum PeerTailnetLink: Equatable, Sendable {
    case openApp
    case join
    case details

    public var title: String {
        switch self {
        case .openApp: return "Open Tailscale"
        case .join: return "Join\u{2026}"
        case .details: return "Details\u{2026}"
        }
    }
}

/// A press on the row that did not do what it said.
public enum PeerTailnetPressFailure: Equatable, Sendable {
    /// `tcr peer tailnet <mode>` was refused; `tcr`'s own words.
    case pickRefused(String)
    /// "Open Tailscale" found no app, or the app would not open.
    case appNotOpened(String?)

    public var line: String {
        switch self {
        case .pickRefused(let message): return "Not changed: \(message)"
        case .appNotOpened(let reason?): return "Could not open the Tailscale app: \(reason)"
        case .appNotOpened(nil): return "The Tailscale app was not found on this Mac."
        }
    }
}

/// The state the row's line is in, and the only thing that decides what it
/// says.
public enum PeerTailnetState: Equatable, Sendable {
    case notRead
    case unsupportedTcr
    case unreadable(String)
    /// A press on this row failed: a refused pick or an app that would not
    /// open. The whole sentence (``PeerTailnetPressFailure``), shown on the
    /// row that was pressed until the next press, because the Settings pane
    /// has no other place a refused verb lands.
    case pressFailed(String)
    case off
    /// A mode is chosen and no serving process has recorded anything.
    case notReported
    /// The last record is older than ``staleAfterMs``.
    case stale
    /// The server is still acting on a different mode than the one chosen.
    case applying
    case appReady(address: String?, found: Int?)
    case appMissing
    case appNotRunning
    case readFailed(String?)
    case nodeReady(name: String?, address: String?, connected: Int?)
    case nodeNotJoined
    case nodeConnecting
    case nodeUnsupported
    case unknownState(String)

    /// A record older than this is stale (contract section 2).
    public static let staleAfterMs: Int64 = 60_000

    public enum Tone: Equatable, Sendable {
        case plain
        case ok
        case warn
    }

    /// Which state a read puts the row in, judged at `now`.
    ///
    /// The setting decides Off on its own: turning Tailscale off needs no
    /// server to agree. Every other mode needs a record, a fresh one, made
    /// for the mode that is chosen; only then does the record's own state
    /// speak.
    public static func state(
        _ read: PeerTailnetRead, pressFailure: PeerTailnetPressFailure? = nil, now: Date
    ) -> PeerTailnetState {
        if let pressFailure { return .pressFailed(pressFailure.line) }
        let reading: PeerTailnetReading
        switch read {
        case .notRead: return .notRead
        case .unsupported: return .unsupportedTcr
        case .unreadable(let message): return .unreadable(message)
        case .read(let r): reading = r
        }
        guard reading.mode != .off else { return .off }
        guard let held = reading.held else { return .notReported }
        let nowMs = Int64((now.timeIntervalSince1970 * 1000).rounded())
        guard nowMs - held.recordedAtMs <= staleAfterMs else { return .stale }
        guard held.mode == reading.mode else { return .applying }
        switch held.state {
        case .off: return .applying
        case .ready:
            return held.mode == .node
                ? .nodeReady(name: held.nodeName, address: held.address, connected: held.connected)
                : .appReady(address: held.address, found: held.found)
        case .appMissing: return .appMissing
        case .appNotRunning: return .appNotRunning
        case .readFailed: return .readFailed(held.detail)
        case .nodeNotJoined: return .nodeNotJoined
        case .nodeConnecting: return .nodeConnecting
        case .nodeUnsupported: return .nodeUnsupported
        case .unknown(let token): return .unknownState(token)
        }
    }

    /// The one line under the picker. Always a sentence: the line's height
    /// is reserved in every state, so it is never empty.
    public var line: String {
        switch self {
        case .notRead:
            return "Reading the Tailscale setting\u{2026}"
        case .unsupportedTcr:
            return "This tcr has no Tailscale setting. Update tcr to use it."
        case .unreadable(let message):
            return "Could not read the Tailscale setting: \(message)"
        case .pressFailed(let line):
            return line
        case .off:
            return "Off. A pasted key still works over a tailnet."
        case .notReported:
            return "No running tcr has reported on Tailscale yet."
        case .stale:
            return "The running tcr has not reported on Tailscale for over a minute."
        case .applying:
            return "Applying. The running tcr picks this up within 20 seconds."
        case .appReady(let address, let found):
            return ["Through the Tailscale app", address, found.map(Self.foundPhrase)]
                .compactMap { $0 }.joined(separator: " \u{00B7} ")
        case .appMissing:
            return "No Tailscale app was found on this Mac."
        case .appNotRunning:
            return "The Tailscale app is not running on this Mac."
        case .readFailed(let detail):
            return detail.map { "Could not read the Tailscale app: \($0)" }
                ?? "Could not read the Tailscale app."
        case .nodeReady(let name, let address, let connected):
            return ["tcr's own node", name, address, connected.map(Self.connectedPhrase)]
                .compactMap { $0 }.joined(separator: " \u{00B7} ")
        case .nodeNotJoined:
            return "Not on a tailnet yet."
        case .nodeConnecting:
            return "Connecting to Tailscale\u{2026}"
        case .nodeUnsupported:
            return "tcr's own node is not in this build of tcr."
        case .unknownState(let token):
            return "tcr reported a Tailscale state this build cannot read: \(token)."
        }
    }

    public var tone: Tone {
        switch self {
        case .appReady, .nodeReady: return .ok
        case .unreadable, .pressFailed, .stale, .appMissing, .appNotRunning, .readFailed, .nodeNotJoined,
            .nodeUnsupported, .unknownState:
            return .warn
        case .notRead, .unsupportedTcr, .off, .notReported, .applying, .nodeConnecting:
            return .plain
        }
    }

    /// The press after the sentence, when the state has one.
    public var link: PeerTailnetLink? {
        switch self {
        case .appNotRunning: return .openApp
        case .nodeNotJoined: return .join
        case .nodeReady: return .details
        default: return nil
        }
    }

    static func foundPhrase(_ count: Int) -> String {
        count == 0 ? "none of your Macs found yet" : "\(count) of your Macs found"
    }

    static func connectedPhrase(_ count: Int) -> String {
        switch count {
        case 0: return "no paired Mac connected"
        case 1: return "1 paired Mac connected"
        default: return "\(count) paired Macs connected"
        }
    }

    /// The row's sub-line, the same in every state.
    public static let rowDetail = "Find your own Macs and reach them from any network."
}

// MARK: - The pills (tailnet contract section 3)

/// A source that saw a found Mac in the last 60 s.
public enum PeerSource: Equatable, Sendable {
    /// mDNS on this network.
    case lan
    case tailnet
    case unknown(String)

    public init(token: String) {
        switch token {
        case "lan": self = .lan
        case "tailnet": self = .tailnet
        default: self = .unknown(token)
        }
    }
}

/// A trusted row's `tailnet` object: the row's newest endpoint is a tailnet
/// address, and how the bytes get there when the app knows.
public struct PeerTailnetPath: Decodable, Equatable, Sendable {
    public enum Route: Equatable, Sendable {
        case direct
        case relayed
        case unknown(String)

        public init(token: String) {
            switch token {
            case "direct": self = .direct
            case "relayed": self = .relayed
            default: self = .unknown(token)
            }
        }
    }

    /// `nil` is unknown, which is always the case in node mode.
    public var route: Route?

    public init(route: Route?) {
        self.route = route
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        self.route = try c.decodeIfPresent(String.self, forKey: .route).map(Route.init(token:))
    }

    private enum Keys: String, CodingKey {
        case route
    }
}

/// The pills a row gains from the tailnet keys, in the order they draw.
public enum PeerTailnetPill: String, Equatable, Sendable, CaseIterable {
    case tailnet
    case direct
    case relayed

    public var text: String { rawValue }

    /// The sentence behind the word.
    public var help: String {
        switch self {
        case .tailnet: return "This Mac is reached over your tailnet."
        case .direct: return "Tailscale reaches this Mac directly, not through a relay."
        case .relayed:
            return "Tailscale reaches this Mac through one of its relays, which is slower."
        }
    }

    /// A trusted row: `tailnet` when the key is present, then `direct` or
    /// `relayed` from its route. An unknown or absent route adds nothing.
    public static func trusted(_ path: PeerTailnetPath?) -> [PeerTailnetPill] {
        guard let path else { return [] }
        switch path.route {
        case .direct: return [.tailnet, .direct]
        case .relayed: return [.tailnet, .relayed]
        case .unknown, nil: return [.tailnet]
        }
    }

    /// A found row: `tailnet` when the tailnet was one of the sources that
    /// saw it. No `lan` pill this round (contract section 3).
    public static func found(_ sources: [PeerSource]) -> [PeerTailnetPill] {
        sources.contains(.tailnet) ? [.tailnet] : []
    }

    /// The widest set any row can carry, which is what the reserved slot is
    /// sized from so a pill appearing moves nothing.
    public static let widestTrusted: [PeerTailnetPill] = [.tailnet, .relayed]
}

extension PeerCommand {
    /// `tcr peer tailnet <off|app|node>`: the mode the picker MOVES to.
    public static func tailnet(_ mode: PeerTailnetMode) -> [String] {
        ["peer", "tailnet", mode.rawValue]
    }

    /// `tcr peer tailnet --json`: the readout. Read-only, so it is polled.
    public static let tailnetRead = ["peer", "tailnet", "--json"]
}
