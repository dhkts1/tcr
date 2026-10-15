import XCTest

@testable import TcrBarCore

/// The Peers words that change once the fleet holds a Codex account.
///
/// No Mac lends, serves or carries a Codex request, so with a Codex account in the
/// fleet every promise about "your requests" or "your traffic" names Claude, and one
/// line says where Codex requests go instead. Without one, every sentence is today's,
/// word for word: the first test holds each of them to the literal the panel drew
/// before Codex accounts existed.
final class PeersCodexCopyTests: XCTestCase {

    private let names = ["studio-mac", "attic-nuc"]
    private let cap: Int64 = 52_428_800

    func testEveryPeersStringIsTodaysInAClaudeOnlyFleet() {
        let words = PeersCodexCopy.self
        let off = false
        // The Peers tab's Share card.
        XCTAssertEqual(words.shareCardTitle(holdsCodex: off), "Share accounts with trusted Macs")
        XCTAssertEqual(
            words.shareStateOff(holdsCodex: off),
            "Off. Trusted Macs carry your traffic but never read it.")
        XCTAssertEqual(
            words.shareStateOn(names: names, holdsCodex: off),
            "On. \(PeerFormat.list(names)) may serve your requests.")
        XCTAssertEqual(
            words.yesCannotShare(holdsCodex: off),
            "On means a trusted Mac may serve your requests with its own accounts, and read "
                + "them. There is nothing to share with yet.")
        XCTAssertEqual(
            words.yesOff(firstName: "studio-mac", holdsCodex: off),
            "On means studio-mac may serve your requests with its accounts, and read them. "
                + "Which Mac, how much and for how long: Settings.")
        XCTAssertEqual(
            words.yesOff(firstName: nil, holdsCodex: off),
            "On means a trusted Mac may serve your requests with its accounts, and read them. "
                + "Which Mac, how much and for how long: Settings.")
        // The rows.
        XCTAssertEqual(
            words.untrustedPromise(title: "studio-mac", holdsCodex: off),
            "Trust shows the same six digits here and on studio-mac. Press Trust on both and "
                + "it can carry your traffic without reading it.")
        XCTAssertEqual(
            words.gatewaySentence(title: "studio-mac", capBytesPerHour: cap, holdsCodex: off),
            "studio-mac is carrying your encrypted bytes and can open none of them, out of "
                + "\(PeerFormat.megabytes(cap)) MB an hour.")
        // The section head and the pills.
        XCTAssertEqual(
            words.carrySentence(holdsCodex: off),
            "A trusted Mac carries your traffic when this Mac has no path of its own, and "
                + "reads none of it.")
        XCTAssertEqual(
            words.sharingPillHelp(holdsCodex: off),
            "Trusted Macs may serve your requests on their own accounts, and read them.")
        XCTAssertEqual(
            words.trustedPillHelp(holdsCodex: off),
            "Pinned on both Macs. It can carry your traffic.")
        XCTAssertEqual(
            words.carryPillHelp(holdsCodex: off),
            "May hold your encrypted bytes and can open none of them. The path line under the "
                + "row says whether it is doing so now.")
        XCTAssertEqual(
            words.youLendPillHelp(holdsCodex: off),
            "May serve your requests on its own accounts, and read them.")
        XCTAssertEqual(
            words.theyLendPillHelp(holdsCodex: off),
            "Serving your requests on its own accounts right now, and reading them.")
        XCTAssertEqual(
            words.inFlightPillHelp(peer: "studio-mac", holdsCodex: off),
            "Requests of yours that studio-mac is serving on its own accounts, and reading, "
                + "at this moment.")
        // The trust sheet's last state.
        XCTAssertEqual(
            words.trustDone(peer: "tcr-4b8we1r0zp", holdsCodex: off),
            "Its key is pinned here as tcr-4b8we1r0zp. It can carry your encrypted bytes and "
                + "open none of them; nothing is shared until you turn sharing on. Do the same "
                + "on that Mac, pointed back here, so both sides hold a pin.")
        // Settings > Peers.
        XCTAssertEqual(
            words.settingsShareToggle(holdsCodex: off), "Share accounts with trusted Macs")
        XCTAssertEqual(
            words.carryGrant(title: "studio-mac", holdsCodex: off),
            "studio-mac may carry your traffic without reading it")
        XCTAssertEqual(
            words.serveGrant(title: "studio-mac", holdsCodex: off),
            "studio-mac may serve your requests on its own accounts, and read them")
        XCTAssertEqual(
            words.sharingHead(holdsCodex: off),
            "A trusted Mac serves your requests in plaintext.")
        XCTAssertEqual(
            words.viaRow(holdsCodex: off),
            "auto picks a trusted Mac the moment this one's own route is dead. none never "
                + "routes through a peer; naming one pins it.")
        XCTAssertEqual(
            words.maxInFlight(holdsCodex: off),
            "max in flight. Not a rate limit: how many of your requests one Mac may have open "
                + "at the same moment.")
        XCTAssertEqual(
            words.defaultsNote(holdsCodex: off),
            "These are what Share accounts writes for every trusted Mac. A Mac serving your "
                + "request reads it in plaintext; your own sign-in never leaves this Mac. 7d_oi "
                + "is Fable's own allowance, which is why it is offered separately and defaults "
                + "to all of it.")
        XCTAssertEqual(
            words.carryGrantDetail(holdsCodex: off),
            "Used when this Mac has no route of its own. It holds your encrypted bytes and "
                + "cannot open them.")
    }

    func testEveryPeersStringNamesClaudeWithACodexAccount() {
        let words = PeersCodexCopy.self
        let on = true
        XCTAssertEqual(
            words.shareCardTitle(holdsCodex: on), "Share Claude accounts with trusted Macs")
        XCTAssertEqual(
            words.shareStateOff(holdsCodex: on),
            "Off. Trusted Macs carry your Claude traffic but never read it.")
        XCTAssertEqual(
            words.shareStateOn(names: names, holdsCodex: on),
            "On. \(PeerFormat.list(names)) may serve your Claude requests.")
        XCTAssertEqual(
            words.yesCannotShare(holdsCodex: on),
            "On means a trusted Mac may serve your Claude requests with its own accounts, and "
                + "read them. There is nothing to share with yet.")
        XCTAssertEqual(
            words.yesOff(firstName: "studio-mac", holdsCodex: on),
            "On means studio-mac may serve your Claude requests with its accounts, and read "
                + "them. Which Mac, how much and for how long: Settings.")
        XCTAssertEqual(
            words.yesOff(firstName: nil, holdsCodex: on),
            "On means a trusted Mac may serve your Claude requests with its accounts, and read "
                + "them. Which Mac, how much and for how long: Settings.")
        XCTAssertEqual(
            words.untrustedPromise(title: "studio-mac", holdsCodex: on),
            "Trust shows the same six digits here and on studio-mac. Press Trust on both and "
                + "it can carry your Claude traffic without reading it.")
        XCTAssertEqual(
            words.gatewaySentence(title: "studio-mac", capBytesPerHour: cap, holdsCodex: on),
            "studio-mac is carrying your encrypted Claude bytes and can open none of them, out "
                + "of \(PeerFormat.megabytes(cap)) MB an hour.")
        XCTAssertEqual(
            words.carrySentence(holdsCodex: on),
            "A trusted Mac carries your Claude traffic when this Mac has no path of its own, "
                + "and reads none of it.")
        XCTAssertEqual(
            words.sharingPillHelp(holdsCodex: on),
            "Trusted Macs may serve your Claude requests on their own accounts, and read them.")
        XCTAssertEqual(
            words.trustedPillHelp(holdsCodex: on),
            "Pinned on both Macs. It can carry your Claude traffic.")
        XCTAssertEqual(
            words.carryPillHelp(holdsCodex: on),
            "May hold your encrypted Claude bytes and can open none of them. The path line "
                + "under the row says whether it is doing so now.")
        XCTAssertEqual(
            words.youLendPillHelp(holdsCodex: on),
            "May serve your Claude requests on its own accounts, and read them.")
        XCTAssertEqual(
            words.theyLendPillHelp(holdsCodex: on),
            "Serving your Claude requests on its own accounts right now, and reading them.")
        XCTAssertEqual(
            words.inFlightPillHelp(peer: "studio-mac", holdsCodex: on),
            "Claude requests of yours that studio-mac is serving on its own accounts, and "
                + "reading, at this moment.")
        XCTAssertEqual(
            words.trustDone(peer: "tcr-4b8we1r0zp", holdsCodex: on),
            "Its key is pinned here as tcr-4b8we1r0zp. It can carry your encrypted Claude "
                + "bytes and open none of them; nothing is shared until you turn sharing on. "
                + "Do the same on that Mac, pointed back here, so both sides hold a pin.")
        XCTAssertEqual(
            words.settingsShareToggle(holdsCodex: on), "Share Claude accounts with trusted Macs")
        XCTAssertEqual(
            words.carryGrant(title: "studio-mac", holdsCodex: on),
            "studio-mac may carry your Claude traffic without reading it")
        XCTAssertEqual(
            words.serveGrant(title: "studio-mac", holdsCodex: on),
            "studio-mac may serve your Claude requests on its own accounts, and read them")
        XCTAssertEqual(
            words.sharingHead(holdsCodex: on),
            "A trusted Mac serves your Claude requests in plaintext.")
        XCTAssertEqual(
            words.viaRow(holdsCodex: on),
            "auto picks a trusted Mac for your Claude traffic the moment this one's own route "
                + "is dead. none never routes through a peer; naming one pins it.")
        XCTAssertEqual(
            words.maxInFlight(holdsCodex: on),
            "max in flight. Not a rate limit: how many of your Claude requests one Mac may have "
                + "open at the same moment.")
        XCTAssertEqual(
            words.defaultsNote(holdsCodex: on),
            "These are what Share Claude accounts writes for every trusted Mac. A Mac serving "
                + "your Claude request reads it in plaintext; your own sign-in never leaves "
                + "this Mac. 7d_oi is Fable's own allowance, which is why it is offered "
                + "separately and defaults to all of it.")
        XCTAssertEqual(
            words.carryGrantDetail(holdsCodex: on),
            "Used when this Mac has no route of its own. It holds your encrypted Claude bytes "
                + "and cannot open them.")
    }

    /// The one line that says where Codex requests go instead.
    func testTheCodexStaysOnThisMacLine() {
        XCTAssertEqual(
            PeersCodexCopy.codexStaysOnThisMac,
            "Codex requests always leave from this Mac: no Mac lends, serves or carries them "
                + "in this version.")
    }

    /// The lease sheet's scope says whose accounts "all" is, and a caption says
    /// where the Codex ones are.
    func testTheLeaseScopeLabelAndCaption() {
        XCTAssertEqual(PeersCodexCopy.leaseCaption, "Codex accounts stay on this Mac.")
        XCTAssertEqual(LendScope.all.label(holdsCodex: true), "All Claude accounts")
    }

    /// Every surface that draws one of these words asks this type for it, with the
    /// fleet's flag, and the anchors the Peers wiring tests hold are still there.
    /// Read from the source, because the views are in the app target, which this
    /// test target does not link.
    func testThePeersAndSettingsSurfacesTakeTheirWordsFromCore() throws {
        let tab = try repoFile("apps/macos/Sources/TcrBar/PanelV4/PeersTabV4.swift")
        for call in [
            "PeersCodexCopy.shareCardTitle(holdsCodex: controller.holdsCodex)",
            "PeersCodexCopy.codexStaysOnThisMac",
            "PeersCodexCopy.untrustedPromise(",
            "PeersCodexCopy.gatewaySentence(",
            "Self.read(holdsCodex: holdsCodex)",
        ] {
            XCTAssertTrue(tab.contains(call), "PeersTabV4.swift no longer holds \(call)")
        }
        XCTAssertFalse(
            tab.contains("\"Off. Trusted Macs carry your traffic but never read it.\""),
            "the Peers tab words its share state itself again, so a mixed fleet reads it")
        for anchor in ["static let carrySentence =", "PeersSnapshotBuilder.carrySentence"] {
            XCTAssertTrue(tab.contains(anchor), "PeersTabV4.swift lost \(anchor)")
        }

        let pane = try repoFile("apps/macos/Sources/TcrBar/PeersSettingsView.swift")
        for call in [
            "PeersCodexCopy.settingsShareToggle(holdsCodex: holdsCodex)",
            "PeersCodexCopy.carryGrant(title: row.title, holdsCodex: holdsCodex)",
            "PeersCodexCopy.serveGrant(title: row.title, holdsCodex: holdsCodex)",
            "PeerLease.eligibleLendAccounts(fleet.accounts)",
            "PeersCodexCopy.leaseCaption",
            "label(holdsCodex: holdsCodex)",
        ] {
            XCTAssertTrue(pane.contains(call), "PeersSettingsView.swift no longer holds \(call)")
        }
        for anchor in [
            "scopeMenuItems { write($0, grant.terms, end) }",
            "plaintext ? Tok.unknown : Tok.inkFaint",
            "PeerCommand.shareDefaults(scope: defaultScope, terms: terms)",
        ] {
            XCTAssertTrue(pane.contains(anchor), "PeersSettingsView.swift lost \(anchor)")
        }

        let panes = try repoFile("apps/macos/Sources/TcrBar/SettingsPanes.swift")
        XCTAssertTrue(panes.contains("RestartAlertCopy.restartInformative(holdsCodex:"))
        XCTAssertFalse(
            panes.contains("Every live session loses its prompt cache."),
            "the restart alert words itself again, so a mixed fleet reads today's sentence")
    }
}

/// A file of this repository, as text.
private func repoFile(_ path: String) throws -> String {
    let root = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent()  // TcrBarTests
        .deletingLastPathComponent()  // Tests
        .deletingLastPathComponent()  // apps/macos
        .deletingLastPathComponent()  // apps
        .deletingLastPathComponent()  // repo root
    return try String(contentsOf: root.appendingPathComponent(path), encoding: .utf8)
}
