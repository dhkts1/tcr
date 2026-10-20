import XCTest

@testable import TcrBarCore

/// The Tailscale row's model: the `tcr peer tailnet --json` decode (tailnet
/// contract section 2), the state each answer puts the row in, the words of
/// each state line, and the pills the `peer ls` keys add (section 3).
///
/// Every value here is fake: `100.64.0.7`, `laptop-2`, `tcr-studio`.
final class PeerTailnetTests: XCTestCase {
    /// The instant every state is judged at, and a recorded time a few seconds
    /// before it.
    private let now = Date(timeIntervalSince1970: 1_760_000_100)
    private var freshMs: Int64 { 1_760_000_095_000 }

    // MARK: - Decode

    /// The contract's own example, byte for byte.
    func testTheContractExampleDecodes() throws {
        let json = """
            {
              "mode": "app",
              "nodeSupported": false,
              "held": {
                "mode": "app",
                "state": "ready",
                "address": "100.64.0.7",
                "nodeName": null,
                "found": 2,
                "connected": 0,
                "detail": null,
                "recordedAtMs": 1760000000000
              }
            }
            """
        let reading = try PeerTailnetReading.decode(Data(json.utf8))
        XCTAssertEqual(reading.mode, .app)
        XCTAssertFalse(reading.nodeSupported)
        XCTAssertEqual(
            reading.held,
            .init(
                mode: .app, state: .ready, address: "100.64.0.7", nodeName: nil, found: 2,
                connected: 0, detail: nil, recordedAtMs: 1_760_000_000_000))
    }

    /// `held: null` is "no server recorded one", which every read answers
    /// until the serving process records its tailnet tick.
    func testANullHeldIsNoRecord() throws {
        let reading = try PeerTailnetReading.decode(
            Data(#"{"mode":"off","nodeSupported":true,"held":null}"#.utf8))
        XCTAssertEqual(reading, PeerTailnetReading(mode: .off, nodeSupported: true, held: nil))
    }

    /// A record without the three keys every state needs is not a record.
    func testAPartialHeldIsNoRecord() throws {
        let reading = try PeerTailnetReading.decode(
            Data(#"{"mode":"app","held":{"mode":"app","state":"ready"}}"#.utf8))
        XCTAssertNil(reading.held)
        XCTAssertFalse(reading.nodeSupported, "an absent nodeSupported is not a supported node")
    }

    /// A mode this build cannot name is not a picker selection: it throws, and
    /// the read becomes `unreadable`, never a guessed mode.
    func testAnUnknownModeIsUnreadableNotGuessed() {
        XCTAssertThrowsError(try PeerTailnetReading.decode(Data(#"{"mode":"funnel"}"#.utf8)))
        guard case .unreadable = PeerTailnetRead.output(#"{"mode":"funnel"}"#) else {
            return XCTFail("an unknown mode must read as unreadable")
        }
    }

    /// A state token added on the Rust side draws as itself.
    func testAnUnknownStateTokenIsKeptAndSaid() throws {
        let read = PeerTailnetRead.output(
            #"{"mode":"app","held":{"mode":"app","state":"exitNode","recordedAtMs":1760000095000}}"#
        )
        let state = PeerTailnetState.state(read, now: now)
        XCTAssertEqual(state, .unknownState("exitNode"))
        XCTAssertTrue(state.line.contains("exitNode"))
        XCTAssertEqual(state.tone, .warn)
    }

    /// An older `tcr` has no verb: clap says "unrecognized subcommand". That
    /// is `unsupported`, a different row from a verb that failed.
    func testAnOlderTcrIsUnsupportedAndAFailureIsUnreadable() {
        XCTAssertEqual(
            PeerTailnetRead.failure(
                "tcr peer tailnet --json failed (exit 2): error: unrecognized subcommand 'tailnet'"),
            .unsupported)
        XCTAssertEqual(
            PeerTailnetRead.failure("tcr peer tailnet --json failed (exit 1): peers file locked"),
            .unreadable("tcr peer tailnet --json failed (exit 1): peers file locked"))
        XCTAssertFalse(PeerTailnetRead.unsupported.canPick)
        XCTAssertFalse(PeerTailnetRead.notRead.canPick)
        XCTAssertEqual(PeerTailnetState.state(.unsupported, now: now), .unsupportedTcr)
    }

    // MARK: - State

    private func read(
        _ mode: PeerTailnetMode, _ state: PeerTailnetReading.State, heldMode: PeerTailnetMode? = nil,
        address: String? = nil, nodeName: String? = nil, found: Int? = nil,
        connected: Int? = nil, detail: String? = nil, recordedAtMs: Int64? = nil
    ) -> PeerTailnetRead {
        .read(
            PeerTailnetReading(
                mode: mode, nodeSupported: true,
                held: .init(
                    mode: heldMode ?? mode, state: state, address: address, nodeName: nodeName,
                    found: found, connected: connected, detail: detail,
                    recordedAtMs: recordedAtMs ?? freshMs)))
    }

    /// The six states of the mockup's scene 3, in its exact words.
    func testTheMockupsSixStatesSayTheMockupsWords() {
        let off = PeerTailnetState.state(
            .read(PeerTailnetReading(mode: .off, nodeSupported: true, held: nil)), now: now)
        XCTAssertEqual(off.line, "Off. A pasted key still works over a tailnet.")
        XCTAssertEqual(off.tone, .plain)

        let app = PeerTailnetState.state(
            read(.app, .ready, address: "100.64.0.7", found: 2), now: now)
        XCTAssertEqual(app.line, "Through the Tailscale app · 100.64.0.7 · 2 of your Macs found")
        XCTAssertEqual(app.tone, .ok)

        let notRunning = PeerTailnetState.state(read(.app, .appNotRunning), now: now)
        XCTAssertEqual(notRunning.line, "The Tailscale app is not running on this Mac.")
        XCTAssertEqual(notRunning.link?.title, "Open Tailscale")
        XCTAssertEqual(notRunning.tone, .warn)

        let notJoined = PeerTailnetState.state(read(.node, .nodeNotJoined), now: now)
        XCTAssertEqual(notJoined.line, "Not on a tailnet yet.")
        XCTAssertEqual(notJoined.link?.title, "Join…")

        let connecting = PeerTailnetState.state(read(.node, .nodeConnecting), now: now)
        XCTAssertEqual(connecting.line, "Connecting to Tailscale…")
        XCTAssertEqual(connecting.tone, .plain)

        let node = PeerTailnetState.state(
            read(.node, .ready, address: "100.64.0.9", nodeName: "tcr-studio", connected: 1),
            now: now)
        XCTAssertEqual(node.line, "tcr's own node · tcr-studio · 100.64.0.9 · 1 paired Mac connected")
        XCTAssertEqual(node.link?.title, "Details…")
        XCTAssertEqual(node.tone, .ok)
    }

    /// Stale is OLDER than 60 s, judged against the reader's clock.
    func testARecordOlderThanSixtySecondsIsStale() {
        let exactly = PeerTailnetState.state(
            read(.app, .ready, recordedAtMs: 1_760_000_040_000), now: now)
        XCTAssertNotEqual(exactly, .stale, "60 s old is the limit, not past it")
        let past = PeerTailnetState.state(
            read(.app, .ready, recordedAtMs: 1_760_000_039_999), now: now)
        XCTAssertEqual(past, .stale)
        XCTAssertEqual(past.tone, .warn)
    }

    /// The server lags the file by up to 20 s; a record for the old mode is
    /// not the new mode working.
    func testARecordForAnotherModeIsApplyingNotReady() {
        let state = PeerTailnetState.state(read(.node, .ready, heldMode: .app), now: now)
        XCTAssertEqual(state, .applying)
        XCTAssertNotEqual(state.tone, .ok)
    }

    /// No record at all with a mode chosen is said as that, never as working.
    func testNoRecordWithAModeChosenIsNotReported() {
        let state = PeerTailnetState.state(
            .read(PeerTailnetReading(mode: .app, nodeSupported: false, held: nil)), now: now)
        XCTAssertEqual(state, .notReported)
    }

    /// Off needs no server to agree.
    func testOffIsOffWhateverTheServerLastRecorded() {
        let state = PeerTailnetState.state(read(.off, .ready, heldMode: .app), now: now)
        XCTAssertEqual(state, .off)
    }

    /// A failed press is said on the row, over whatever the read says.
    func testAPressFailureIsSaidOverTheReading() {
        let state = PeerTailnetState.state(
            read(.app, .ready, address: "100.64.0.7", found: 2),
            pressFailure: .pickRefused("tcr peer tailnet node failed (exit 1): no"), now: now)
        XCTAssertEqual(state.line, "Not changed: tcr peer tailnet node failed (exit 1): no")
        XCTAssertEqual(state.tone, .warn)
        XCTAssertEqual(
            PeerTailnetPressFailure.appNotOpened(nil).line,
            "The Tailscale app was not found on this Mac.")
    }

    /// The line's height is reserved as ONE line, so no state may say nothing
    /// or say two lines.
    func testEveryStateSaysExactlyOneNonEmptyLine() {
        let states: [PeerTailnetState] = [
            .notRead, .unsupportedTcr, .unreadable("x"), .pressFailed("x"), .off, .notReported,
            .stale, .applying, .appReady(address: nil, found: nil),
            .appReady(address: "100.64.0.7", found: 0), .appMissing, .appNotRunning,
            .readFailed(nil), .readFailed("timed out"),
            .nodeReady(name: nil, address: nil, connected: nil),
            .nodeReady(name: "tcr-studio", address: "100.64.0.9", connected: 2), .nodeNotJoined,
            .nodeConnecting, .nodeUnsupported, .unknownState("x"),
        ]
        for state in states {
            XCTAssertFalse(state.line.isEmpty, "\(state) says nothing")
            XCTAssertFalse(state.line.contains("\n"), "\(state) says two lines")
        }
        XCTAssertEqual(
            PeerTailnetState.appReady(address: "100.64.0.7", found: 0).line,
            "Through the Tailscale app · 100.64.0.7 · none of your Macs found yet")
        XCTAssertEqual(
            PeerTailnetState.nodeReady(name: nil, address: "100.64.0.9", connected: 2).line,
            "tcr's own node · 100.64.0.9 · 2 paired Macs connected")
    }

    // MARK: - The picker

    /// The own node is offered only by a `tcr` built with it, and says why
    /// when it is not.
    func testTheOwnNodeIsOfferedOnlyWhenSupported() {
        XCTAssertFalse(PeerTailnetMode.node.isOffered(nodeSupported: false))
        XCTAssertEqual(PeerTailnetMode.node.menuDetail(nodeSupported: false), "Not in this build")
        XCTAssertTrue(PeerTailnetMode.node.isOffered(nodeSupported: true))
        XCTAssertEqual(
            PeerTailnetMode.node.menuDetail(nodeSupported: true),
            "Experimental. Joins by itself, no app.")
        XCTAssertTrue(PeerTailnetMode.off.isOffered(nodeSupported: false))
        XCTAssertTrue(PeerTailnetMode.app.isOffered(nodeSupported: false))
        XCTAssertEqual(
            PeerTailnetMode.allCases.map(\.title), ["Off", "Tailscale app", "tcr's own node"])
    }

    /// The argv: the mode the picker moves TO, and the read.
    func testTheArgv() {
        XCTAssertEqual(PeerCommand.tailnet(.off), ["peer", "tailnet", "off"])
        XCTAssertEqual(PeerCommand.tailnet(.app), ["peer", "tailnet", "app"])
        XCTAssertEqual(PeerCommand.tailnet(.node), ["peer", "tailnet", "node"])
        XCTAssertEqual(PeerCommand.tailnetRead, ["peer", "tailnet", "--json"])
    }

    // MARK: - Pills (contract section 3)

    func testTrustedRowPillsFollowTheRoute() {
        XCTAssertEqual(PeerTailnetPill.trusted(nil), [])
        XCTAssertEqual(PeerTailnetPill.trusted(.init(route: nil)), [.tailnet])
        XCTAssertEqual(PeerTailnetPill.trusted(.init(route: .direct)), [.tailnet, .direct])
        XCTAssertEqual(PeerTailnetPill.trusted(.init(route: .relayed)), [.tailnet, .relayed])
        XCTAssertEqual(PeerTailnetPill.trusted(.init(route: .unknown("peer"))), [.tailnet])
    }

    func testFoundRowPillIsTailnetOnlyNeverLan() {
        XCTAssertEqual(PeerTailnetPill.found([]), [])
        XCTAssertEqual(PeerTailnetPill.found([.lan]), [], "no lan pill this round")
        XCTAssertEqual(PeerTailnetPill.found([.lan, .tailnet]), [.tailnet])
    }

    /// The reserved slot is sized from the widest set, which has to BE a set
    /// a row can carry.
    func testTheWidestSetIsOneARowCanCarry() {
        XCTAssertEqual(PeerTailnetPill.widestTrusted, PeerTailnetPill.trusted(.init(route: .relayed)))
    }

    /// The `peer ls` keys decode, and a row from an older `tcr` without them
    /// decodes exactly as before.
    func testThePeerListKeysDecodeAndAreOptional() throws {
        let json = """
            {"finding": true, "peers": [
              {"name": "laptop-2", "address": "192.168.1.20:7755", "trusted": false,
               "lastSeenMs": 1, "sources": ["lan", "tailnet"]},
              {"id": "tcr-aaaa", "name": "office-mini", "address": "100.64.0.8:7755",
               "trusted": true, "tailnet": {"route": "relayed"}},
              {"id": "tcr-bbbb", "name": "desk", "address": "192.168.1.30:7755",
               "trusted": true, "tailnet": null},
              {"name": "old", "address": "192.168.1.40:7755", "trusted": false}
            ]}
            """
        let document = try JSONDecoder().decode(PeerListDocument.self, from: Data(json.utf8))
        XCTAssertEqual(document.peers[0].sources, [.lan, .tailnet])
        XCTAssertEqual(document.peers[1].tailnet, PeerTailnetPath(route: .relayed))
        XCTAssertNil(document.peers[2].tailnet)
        XCTAssertEqual(document.peers[3].sources, [])
        XCTAssertNil(document.peers[3].tailnet)
    }

    /// The live half is where the tailnet keys are measured, so it wins, and
    /// a live row without them keeps what the file said.
    func testTheLiveReadCarriesTheTailnetKeys() {
        let file = PeerListDocument(
            peers: [.init(id: "tcr-aaaa", name: "office-mini", trusted: true)])
        let measured = PeerListDocument.LivePeersRead(
            supported: true,
            peers: [.init(id: "tcr-aaaa", trusted: true, tailnet: .init(route: .direct))])
        XCTAssertEqual(
            file.mergingLive(measured).peers.first?.tailnet, PeerTailnetPath(route: .direct))

        let kept = PeerListDocument(
            peers: [
                .init(
                    id: "tcr-aaaa", name: "office-mini", trusted: true,
                    tailnet: .init(route: .relayed))
            ])
        let silent = PeerListDocument.LivePeersRead(
            supported: true, peers: [.init(id: "tcr-aaaa", trusted: true)])
        XCTAssertEqual(
            kept.mergingLive(silent).peers.first?.tailnet, PeerTailnetPath(route: .relayed))
    }
}
