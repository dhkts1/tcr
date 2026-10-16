import Combine
import XCTest

@testable import TcrBarCore

/// With the panel shut the poller runs slowly and tells no view of a poll;
/// opening the panel polls at once and tells the views before they draw.
///
/// Every read here is a stand-in, so no `tcr` runs. The intervals are long
/// enough that no timer tick can land inside a test: every read counted is one
/// the cadence started on purpose.
@MainActor
final class StatusPollerCadenceTests: XCTestCase {
    private let fleetState = PollState.commandFailed(exitCode: 1, message: "stand-in")

    private func poller(reads: @escaping () -> Void) -> StatusPoller {
        StatusPoller(
            interval: 1_000, closedInterval: 2_000,
            read: { [fleetState] in
                reads()
                return fleetState
            })
    }

    private func settle() async {
        for _ in 0..<20 { await Task.yield() }
        try? await Task.sleep(nanoseconds: 50_000_000)
    }

    /// The test the brief names: opening the panel triggers a poll at once,
    /// not one a slow interval later.
    func test__setViewer__panelOpensOnARunningLoop__pollsAtOnce() async {
        var reads = 0
        let poller = poller { reads += 1 }
        poller.start()
        await settle()
        XCTAssertEqual(reads, 1, "start reads once")
        XCTAssertEqual(poller.currentInterval, 2_000, "shut, the loop runs at the slow pace")

        poller.setViewer(.panel, isLooking: true)
        await settle()
        XCTAssertEqual(reads, 2, "opening the panel must read at once")
        XCTAssertEqual(poller.currentInterval, 1_000)

        poller.setViewer(.panel, isLooking: false)
        await settle()
        XCTAssertEqual(reads, 2, "closing reads nothing: the last read is seconds old")
        XCTAssertEqual(poller.currentInterval, 2_000)
        poller.stop()
    }

    /// A second viewer arriving, or one saying it is still there, is not an
    /// opening: no extra read.
    func test__setViewer__repeatedOrSecondViewer__readsNothingMore() async {
        var reads = 0
        let poller = poller { reads += 1 }
        poller.start()
        await settle()
        poller.setViewer(.panel, isLooking: true)
        await settle()
        poller.setViewer(.panel, isLooking: true)
        poller.setViewer(.settings, isLooking: true)
        poller.setViewer(.panel, isLooking: false)
        await settle()
        XCTAssertEqual(reads, 2)
        XCTAssertEqual(poller.currentInterval, 1_000, "Settings is still looking")
        poller.stop()
    }

    /// A poller nobody started (the render harness's, a preview's) stays
    /// stopped when a viewer arrives.
    func test__setViewer__neverStarted__readsNothing() async {
        var reads = 0
        let poller = poller { reads += 1 }
        poller.setViewer(.panel, isLooking: true)
        await settle()
        XCTAssertEqual(reads, 0)
    }

    /// The gate itself: a poll with nobody looking tells no view, a poll with
    /// the panel open does, and the panel opening tells them once so they read
    /// the state that arrived while it was shut. The icon's own subscription,
    /// `$state`, hears every poll either way.
    func test__objectWillChange__onlyWhileAViewerLooks__iconHearsEveryPoll() async {
        let poller = poller {}
        var viewSignals = 0
        var iconSignals = 0
        var bag: Set<AnyCancellable> = []
        poller.objectWillChange.sink { viewSignals += 1 }.store(in: &bag)
        poller.$state.dropFirst().sink { _ in iconSignals += 1 }.store(in: &bag)

        await poller.pollOnce()
        await poller.pollOnce()
        XCTAssertEqual(viewSignals, 0, "a shut panel's views must hear of no poll")
        XCTAssertEqual(iconSignals, 2, "the menu bar mark hears every poll")

        poller.setViewer(.panel, isLooking: true)
        XCTAssertEqual(viewSignals, 1, "opening tells the views once, before the show")

        await poller.pollOnce()
        XCTAssertEqual(viewSignals, 2, "an open panel hears each poll")

        poller.setViewer(.panel, isLooking: false)
        await poller.pollOnce()
        XCTAssertEqual(viewSignals, 2)
        XCTAssertEqual(iconSignals, 4)
    }
}

/// The same pace for the knock read, which has no views to gate.
@MainActor
final class PollCadenceTests: XCTestCase {
    private func settle() async {
        for _ in 0..<20 { await Task.yield() }
        try? await Task.sleep(nanoseconds: 50_000_000)
    }

    func test__setWatched__onARunningLoop__readsAtOnceAndChangesPace() async {
        var reads = 0
        let cadence = PollCadence(open: 1_000, closed: 2_000) { reads += 1 }
        cadence.start()
        await settle()
        XCTAssertEqual(reads, 1)
        XCTAssertEqual(cadence.interval, 2_000)
        cadence.setWatched(true)
        await settle()
        XCTAssertEqual(reads, 2)
        XCTAssertEqual(cadence.interval, 1_000)
        cadence.setWatched(false)
        await settle()
        XCTAssertEqual(reads, 2)
        cadence.stop()
    }

    func test__knockReader__closedPace__isSlowerThanOpenAndFasterThanTheFleet() {
        let reader = KnockReader()
        XCTAssertEqual(reader.currentInterval, KnockReader.defaultClosedInterval)
        reader.setPanelOpen(true)
        XCTAssertEqual(reader.currentInterval, StatusPoller.defaultInterval)
        XCTAssertLessThan(KnockReader.defaultClosedInterval, StatusPoller.defaultClosedInterval)
    }
}
