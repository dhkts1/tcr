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
