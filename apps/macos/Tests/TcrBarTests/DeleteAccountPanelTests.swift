import Foundation
import XCTest

@testable import TcrBarCore

/// "Delete Account…" from the panel's side (Gil, 2026-10-14: "remove delete
/// account doesnt work sometimes"): a refused delete is drawn on its row in
/// `tcr`'s words, a delete that succeeded with something to say keeps saying
/// it, and a poll that no longer lists the account draws no row for it.
///
/// No process is spawned: ``RemoveAccountController`` takes a scripted
/// outcome. Account names are invented; this repository is public.
@MainActor
final class DeleteAccountControllerTests: XCTestCase {

    private let alice = AccountRef(name: "alice@example.com")
    private let bob = AccountRef(name: "bob@example.com")

    private func controller(_ outcome: RemoveAccountCommand.Outcome) -> RemoveAccountController {
        RemoveAccountController(perform: { _ in outcome })
    }

    func testNothingRecordedByDefault() {
        let controller = RemoveAccountController()
        XCTAssertNil(controller.failure(for: alice))
        XCTAssertNil(controller.notice(for: alice))
        XCTAssertFalse(controller.isPending(alice))
    }

    /// The query handed to `tcr` is the row's whole name, nothing else.
    func testTheQueryIsTheAccountName() async {
        let seen = QuerySink()
        let controller = RemoveAccountController(perform: { query in
            seen.record(query)
            return .clean
        })
        _ = await controller.remove(account: AccountRef(name: "alice@example.com/org-b"))
        XCTAssertEqual(seen.queries, ["alice@example.com/org-b"])
    }

    func testARefusalIsKeptForTheRowInTcrsWords() async {
        let refusal = RemoveAccountCommand.Failure(
            exitCode: 1, message: "Error: no account matches 'alice@example.com'")
        let controller = controller(.failed(refusal))
        let attempt = await controller.remove(account: alice)
        XCTAssertEqual(attempt, .refused)
        XCTAssertEqual(controller.failure(for: alice), refusal)
        XCTAssertNil(controller.failure(for: bob), "only the row that was clicked")
        XCTAssertFalse(controller.isPending(alice))
    }

    func testASuccessThatSpokeKeepsWhatItSaid() async {
        let said = "[tcr] the proxy running on :1 keeps listing it as disabled until it restarts."
        let controller = controller(.spoke(notice: said))
        let attempt = await controller.remove(account: alice)
        XCTAssertEqual(attempt, .accepted(notice: said))
        XCTAssertEqual(controller.notice(for: alice), said)
        XCTAssertNil(controller.failure(for: alice))
    }

    func testACleanSuccessLeavesNothingOnTheRow() async {
        let controller = controller(.clean)
        let attempt = await controller.remove(account: alice)
        XCTAssertEqual(attempt, .accepted(notice: nil))
        XCTAssertNil(controller.notice(for: alice))
        XCTAssertNil(controller.failure(for: alice))
    }

    /// A retry starts clean: the last attempt's failure does not outlive a
    /// later success.
    func testARetryClearsTheEarlierFailure() async {
        let outcomes = OutcomeQueue([
            .failed(RemoveAccountCommand.Failure(exitCode: 1, message: "busy")), .clean,
        ])
        let controller = RemoveAccountController(perform: { _ in outcomes.next() })
        _ = await controller.remove(account: alice)
        XCTAssertNotNil(controller.failure(for: alice))
        _ = await controller.remove(account: alice)
        XCTAssertNil(controller.failure(for: alice))
    }
}

/// The lines under a row, in one fixed order.
final class AccountRowNoticeTests: XCTestCase {

    func testNothingToSayIsNoLines() {
        XCTAssertEqual(
            AccountRowNotice.collect(
                deleteFailure: nil, deleteNotice: nil, groupFailure: nil, toggleFailure: nil),
            [])
    }

    func testARefusedDeleteIsAFailureLineWithItsReason() {
        let lines = AccountRowNotice.collect(
            deleteFailure: RemoveAccountCommand.Failure(exitCode: 1, message: "no account matches"),
            deleteNotice: nil, groupFailure: nil, toggleFailure: nil)
        XCTAssertEqual(
            lines,
            [AccountRowNotice(text: "delete failed (exit 1): no account matches", tone: .failure)])
    }

    func testARefusedGroupEditNamesTheGroup() {
        let lines = AccountRowNotice.collect(
            deleteFailure: nil, deleteNotice: nil,
            groupFailure: (group: "red", failure: GroupCommand.Failure(exitCode: 1, message: "nope")),
            toggleFailure: nil)
        XCTAssertEqual(lines.count, 1)
        XCTAssertEqual(lines.first?.tone, .failure)
        XCTAssertTrue(lines.first?.text.contains("“red”") ?? false, "\(lines)")
        XCTAssertTrue(lines.first?.text.contains("nope") ?? false, "\(lines)")
    }

    func testFailuresComeBeforeANoteAndTheNoteIsNotAFailure() {
        let lines = AccountRowNotice.collect(
            deleteFailure: nil, deleteNotice: "keeps listing it until it restarts",
            groupFailure: (group: "red", failure: GroupCommand.Failure(exitCode: 2, message: "x")),
            toggleFailure: nil)
        XCTAssertEqual(lines.map(\.tone), [.failure, .note])
    }
}

/// The panel model: a poll that no longer lists an account draws no row, no
/// strip segment and no kept-open card for it.
final class DeletedAccountLeavesThePanelTests: XCTestCase {

    private let now = Date(timeIntervalSince1970: 1_800_000_000)

    func testAPollWithoutTheAccountDrawsNothingForIt() {
        let before = Fleet(accounts: [
            row("alice@example.com", disabled: true, groups: ["red", "blue"]),
            row("bob@example.com", groups: ["red"]),
        ])
        let after = Fleet(accounts: [row("bob@example.com", groups: ["red"])])

        let drawnBefore = drawnIDs(before)
        XCTAssertTrue(
            drawnBefore.contains("alice@example.com"),
            "positive control: the parked account in two groups is drawn before the delete")
        XCTAssertEqual(Set(drawnIDs(after)), ["bob@example.com"])
        XCTAssertFalse(
            FleetStrip(drawnRows: rows(after), now: now).segments.contains {
                $0.id == "alice@example.com"
            })

        let kept = KeptOpenCards(["alice@example.com", "bob@example.com"])
        XCTAssertEqual(kept.pruned(to: after.accounts).ids, ["bob@example.com"])
    }

    private func rows(_ fleet: Fleet) -> [FleetSectionRow] {
        fleet.accountsTabSections(expandedGroups: ["red", "blue"])
            .rowsInDrawOrder(expandedGroups: ["red", "blue"], parkedCap: 10) { _ in false }
    }

    private func drawnIDs(_ fleet: Fleet) -> [String] {
        rows(fleet).map(\.account.id)
    }

    private func row(_ name: String, disabled: Bool = false, groups: [String]? = nil) -> Account {
        Account(
            name: name,
            priority: 1,
            status: "active",
            disabled: disabled,
            quota: 0.5,
            quotaState: .ok,
            fiveHour: 0.1,
            sevenDay: 0.3,
            sevenDayOi: nil,
            fiveHourState: nil,
            sevenDayState: nil,
            fiveHourResetAtMs: nil,
            sevenDayResetAtMs: nil,
            held: [],
            requests: 0,
            inputTokens: 0,
            outputTokens: 0,
            cacheReadTokens: 0,
            cacheHitRatio: nil,
            probeStatus: .ok,
            probeError: nil,
            lastStreamError: nil,
            streamErrorCount: 0,
            source: .live,
            serverSha: "abc1234",
            serverDirty: false,
            groups: groups,
            reservedGroups: nil,
            parkedGroups: nil,
            groupColors: nil
        )
    }
}

/// Collects the queries a scripted `tcr remove` was handed.
private final class QuerySink: @unchecked Sendable {
    private let lock = NSLock()
    private var seen: [String] = []
    func record(_ query: String) {
        lock.lock()
        seen.append(query)
        lock.unlock()
    }
    var queries: [String] {
        lock.lock()
        defer { lock.unlock() }
        return seen
    }
}

/// Hands out scripted outcomes in order.
private final class OutcomeQueue: @unchecked Sendable {
    private let lock = NSLock()
    private var outcomes: [RemoveAccountCommand.Outcome]
    init(_ outcomes: [RemoveAccountCommand.Outcome]) { self.outcomes = outcomes }
    func next() -> RemoveAccountCommand.Outcome {
        lock.lock()
        defer { lock.unlock() }
        return outcomes.removeFirst()
    }
}
