import Foundation
import XCTest

@testable import TcrBarCore

/// What the panel keeps, for this app run only, under a Codex account's line: the reason a
/// re-login was refused, and the warning a group add printed. Each clears once the row it
/// was about has moved on.
///
/// The rows are the canonical fixture's, decoded the way the panel decodes a poll.
final class CodexNoteStoreTests: XCTestCase {

    private let routedReason =
        "this Codex login's ChatGPT workspace is not served at https://chatgpt.com with no "
        + "routing of its own (workspace_backend_origin is \"https://gov.chatgpt.com\"): "
        + "codex sends that workspace's traffic to its own backend, or routes it by residency, "
        + "and tcr pools a Codex login only at https://chatgpt.com. Nothing was written."

    private let warning =
        "'ivy@example.com' is a Codex account. A Codex session cannot ask for a group, so "
        + "'orchard' labels it and can park it, but never routes to it; while 'orchard' is "
        + "reserved, it serves nothing."

    /// A refused re-login is kept for the row it was about, and for no other.
    func testARefusedReloginIsKeptForTheRow() throws {
        let erin = try fixtureAccount(named: "erin@example.com/codex")
        let ivy = try fixtureAccount(named: "ivy@example.com")
        var notes = CodexNotes()
        notes.recordReloginRefusal(routedReason, for: erin)
        XCTAssertEqual(notes.reloginRefusal(for: erin.ref), routedReason)
        XCTAssertNil(notes.reloginRefusal(for: ivy.ref))
    }

    /// The refusal stays while the row is as it was when the refusal came back, and goes
    /// once its state or its sign-out reason changes, or once the row leaves the fleet:
    /// either way, the refusal no longer describes it.
    func testTheNoteClearsWhenTheRowsStateChanges() throws {
        let erin = try fixtureAccount(named: "erin@example.com/codex")
        let ivy = try fixtureAccount(named: "ivy@example.com")
        var notes = CodexNotes()
        notes.recordReloginRefusal(routedReason, for: erin)
        notes.reconcile(with: [erin, ivy])
        XCTAssertEqual(notes.reloginRefusal(for: erin.ref), routedReason)

        let signedIn = try fixtureAccount(named: "erin@example.com/codex") { row in
            row["status"] = "active"
            row.removeValue(forKey: "loginError")
        }
        var cleared = notes
        cleared.reconcile(with: [signedIn, ivy])
        XCTAssertNil(cleared.reloginRefusal(for: erin.ref), "kept after erin signed in")

        let newReason = try fixtureAccount(named: "erin@example.com/codex") { row in
            row["loginError"] = "refresh_token_invalidated"
        }
        var moved = notes
        moved.reconcile(with: [newReason, ivy])
        XCTAssertNil(moved.reloginRefusal(for: erin.ref), "kept after erin's sign-out reason changed")

        var gone = notes
        gone.reconcile(with: [ivy])
        XCTAssertNil(gone.reloginRefusal(for: erin.ref), "kept after erin left the fleet")
    }

    /// A group add that warned keeps the warning without its prefix; one that warned of
    /// nothing keeps nothing. The warning goes once the row is no longer in that group.
    func testAGroupAddWarningIsKeptWithoutItsPrefix() throws {
        let ivy = try fixtureAccount(named: "ivy@example.com")
        var notes = CodexNotes()
        notes.recordGroupAddNotice("warning: " + warning, group: "orchard", for: ivy.ref)
        XCTAssertEqual(notes.groupAddWarning(for: ivy.ref), warning)

        var quiet = CodexNotes()
        quiet.recordGroupAddNotice(
            "Added 'ivy@example.com' to group 'orchard'.", group: "orchard", for: ivy.ref)
        XCTAssertNil(quiet.groupAddWarning(for: ivy.ref))
        XCTAssertEqual(quiet, CodexNotes(), "a notice with no warning recorded something")

        notes.reconcile(with: [ivy])
        XCTAssertEqual(notes.groupAddWarning(for: ivy.ref), warning, "dropped while ivy is in orchard")
        let left = try fixtureAccount(named: "ivy@example.com") { row in row["groups"] = [String]() }
        notes.reconcile(with: [left])
        XCTAssertNil(notes.groupAddWarning(for: ivy.ref), "kept after ivy left orchard")
    }

    /// The store the panel observes forwards to the same value, and starts from the notes it
    /// is given, so a render can hand one its scene's notes.
    @MainActor
    func testTheStoreForwardsToItsNotes() throws {
        let erin = try fixtureAccount(named: "erin@example.com/codex")
        let ivy = try fixtureAccount(named: "ivy@example.com")
        let store = CodexNoteStore()
        XCTAssertEqual(store.notes, CodexNotes())
        store.recordReloginRefusal(routedReason, for: erin)
        store.recordGroupAddNotice("warning: " + warning, group: "orchard", for: ivy.ref)
        XCTAssertEqual(store.notes.reloginRefusal(for: erin.ref), routedReason)
        XCTAssertEqual(store.notes.groupAddWarning(for: ivy.ref), warning)
        XCTAssertEqual(CodexNoteStore(notes: store.notes).notes, store.notes)
        store.reconcile(with: [])
        XCTAssertEqual(store.notes, CodexNotes())
    }

    // MARK: - Helpers

    /// One row of the canonical Codex fixture, decoded the way a poll is, after `edit`.
    func fixtureAccount(
        named name: String, editing edit: (inout [String: Any]) -> Void = { _ in }
    ) throws -> Account {
        var rows = try XCTUnwrap(
            JSONSerialization.jsonObject(with: CanonicalFixtures.data("codex-status.json"))
                as? [[String: Any]])
        let index = try XCTUnwrap(rows.firstIndex { $0["name"] as? String == name }, "no \(name)")
        edit(&rows[index])
        let fleet = try Fleet.decode(JSONSerialization.data(withJSONObject: rows))
        return try XCTUnwrap(fleet.accounts.first { $0.name == name })
    }
}
