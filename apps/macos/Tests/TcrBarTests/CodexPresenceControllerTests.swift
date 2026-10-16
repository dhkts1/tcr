import Foundation
import XCTest

@testable import TcrBarCore

final class CodexPresenceControllerTests: XCTestCase {
    @MainActor
    func testEarlyAddAccountWaitsForPresenceBeforeStartingClaudeOrShowingTheChooser() async {
        for present in [true, false] {
            let gate = PresenceResolutionGate(present: present)
            let controller = CodexPresenceController(resolver: { gate.resolve() })
            let prewarm = Task { await controller.refresh(fleetHoldsCodex: false) }
            await waitForResolver(gate)
            var phase: LoginPhase?
            var claudeStarts = 0
            let click = Task {
                let presence = await controller.resolvedForLogin(fleetHoldsCodex: false)
                if presence.isPresent { phase = .choosing } else { claudeStarts += 1 }
            }
            try? await Task.sleep(nanoseconds: 20_000_000)
            XCTAssertNil(phase)
            XCTAssertEqual(claudeStarts, 0, "unresolved presence must not start a Claude login")
            gate.release()
            await prewarm.value
            await click.value
            XCTAssertTrue(controller.isResolved)
            XCTAssertEqual(phase, present ? .choosing : nil)
            XCTAssertEqual(claudeStarts, present ? 0 : 1)
            XCTAssertEqual(gate.calls, 1, "launch and click must share the same resolution")
        }
    }

    @MainActor
    func testConcurrentOpenAndLaunchRefreshesShareOneWorker() async {
        let gate = PresenceResolutionGate(present: true)
        let controller = CodexPresenceController(resolver: { gate.resolve() })
        let first = Task { await controller.refresh(fleetHoldsCodex: false) }
        await waitForResolver(gate)
        let second = Task { await controller.refresh(fleetHoldsCodex: true) }
        try? await Task.sleep(nanoseconds: 20_000_000)
        let calls = gate.calls
        gate.release()
        await first.value
        await second.value
        XCTAssertEqual(calls, 1)
        XCTAssertTrue(controller.presence.isPresent)
    }

    /// Only an empty fleet's first frame reads presence, so only that opening waits for the
    /// scan. A fleet with accounts opens at once, however long the scan takes.
    @MainActor
    func testOnlyAnEmptyFleetsOpeningWaitsForPresence() throws {
        let unresolved = CodexPresenceController(resolver: { (.absent, nil) })
        XCTAssertFalse(unresolved.openingWaits(for: try claudeOnlyFleet()))
        XCTAssertFalse(unresolved.openingWaits(for: nil), "no fleet yet: the panel draws its wait")
        XCTAssertTrue(unresolved.openingWaits(for: Fleet(accounts: [])), "positive control")
        let resolved = CodexPresenceController(initialPresence: .absent, resolver: { (.absent, nil) })
        XCTAssertFalse(resolved.openingWaits(for: Fleet(accounts: [])))
    }

    /// An opening re-reads this Mac's codex only when it may have something to say: a
    /// Claude-only fleet on a Mac where the last read found no codex pays for no scan.
    @MainActor
    func testAClaudeOnlyOpeningPaysForNoCodexScan() {
        let absent = CodexPresenceController(initialPresence: .absent, resolver: { (.absent, nil) })
        XCTAssertFalse(absent.rescansAtOpen(fleetHoldsCodex: false))
        XCTAssertTrue(absent.rescansAtOpen(fleetHoldsCodex: true))
        let present = CodexPresenceController(
            initialPresence: CodexPresence(codexExecutable: "/opt/example/codex"),
            resolver: { (.absent, nil) })
        XCTAssertTrue(present.rescansAtOpen(fleetHoldsCodex: false))
        XCTAssertTrue(
            CodexPresenceController(resolver: { (.absent, nil) }).rescansAtOpen(fleetHoldsCodex: false))
    }

    /// Codex installed after the last read is found by an Add account click, which reads an
    /// absent answer again without waiting on it: the click goes on with the answer it had.
    @MainActor
    func testAnAddAccountClickReadsAnAbsentAnswerAgainWithoutWaiting() async {
        let gate = PresenceResolutionGate(present: true)
        let controller = CodexPresenceController(initialPresence: .absent, resolver: { gate.resolve() })
        let presence = await controller.resolvedForLogin(fleetHoldsCodex: false)
        XCTAssertFalse(presence.isPresent, "this click keeps the answer it had")
        await waitForResolver(gate)
        gate.release()
        for _ in 0..<100 where !controller.presence.isPresent {
            try? await Task.sleep(nanoseconds: 1_000_000)
        }
        XCTAssertTrue(controller.presence.isPresent, "the next click is offered codex")
    }

    @MainActor
    private func waitForResolver(_ gate: PresenceResolutionGate) async {
        for _ in 0..<100 where gate.calls == 0 {
            try? await Task.sleep(nanoseconds: 1_000_000)
        }
        XCTAssertGreaterThan(gate.calls, 0)
    }
}

private final class PresenceResolutionGate: @unchecked Sendable {
    private let lock = NSLock()
    private let ready = DispatchSemaphore(value: 0)
    private var count = 0
    private let present: Bool

    init(present: Bool) { self.present = present }
    var calls: Int { lock.withLock { count } }

    func resolve() -> (CodexPresence, String?) {
        lock.withLock { count += 1 }
        ready.wait()
        return (present ? CodexPresence(codexExecutable: "/opt/example/codex") : .absent, nil)
    }

    func release() {
        for _ in 0..<4 { ready.signal() }
    }
}

/// alice, the one Claude row of apps/macos/Tests/Fixtures/codex-status.json, alone.
private func claudeOnlyFleet() throws -> Fleet {
    let url = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent()  // TcrBarTests
        .deletingLastPathComponent()  // Tests
        .appendingPathComponent("Fixtures/codex-status.json")
    let rows = try XCTUnwrap(
        JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [[String: Any]])
    let alice = try XCTUnwrap(rows.first { $0["name"] as? String == "alice@example.com" })
    return try Fleet.decode(JSONSerialization.data(withJSONObject: [alice]))
}
