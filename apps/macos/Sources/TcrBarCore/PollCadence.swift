import Foundation

/// A read on a timer whose speed depends on whether anyone is looking.
///
/// With the panel shut, the only reader of the fleet is the menu bar mark, and
/// the mark shows facts that move on a scale of minutes: how full the cup is,
/// whether a pool is near its limit, whether the read failed. Reading those
/// every three seconds charged a shut panel twenty spawns a minute per verb for
/// a picture that could not change faster than the proxy's own quota probe
/// (300 s by default). So the loop runs at ``closed`` until a viewer arrives,
/// and at ``open`` while one is there.
///
/// A viewer arriving reads AT ONCE and then keeps the open pace, so a panel
/// never waits out a slow tick to show fresh numbers. A viewer leaving starts
/// the slow pace from that moment, without a read: the last one is seconds old.
@MainActor
public final class PollCadence {
    public let open: TimeInterval
    public let closed: TimeInterval
    /// Whether something on screen reads what this loop publishes.
    public private(set) var isWatched = false

    private let tick: @MainActor () async -> Void
    private var task: Task<Void, Never>?

    public init(
        open: TimeInterval, closed: TimeInterval, tick: @escaping @MainActor () async -> Void
    ) {
        self.open = open
        self.closed = closed
        self.tick = tick
    }

    deinit { task?.cancel() }

    /// The pace the loop sleeps at right now.
    public var interval: TimeInterval { isWatched ? open : closed }

    public var isRunning: Bool { task != nil }

    /// Reads at once, then every ``interval``. A second call does nothing.
    public func start() {
        guard task == nil else { return }
        run(readingFirst: true)
    }

    public func stop() {
        task?.cancel()
        task = nil
    }

    /// Says whether anyone is looking. A change of answer on a running loop
    /// restarts it: reading at once when a viewer arrives, sleeping first when
    /// the last one leaves. A loop that was never started stays stopped.
    public func setWatched(_ watched: Bool) {
        guard watched != isWatched else { return }
        isWatched = watched
        guard task != nil else { return }
        task?.cancel()
        run(readingFirst: watched)
    }

    private func run(readingFirst: Bool) {
        task = Task { [weak self] in
            var read = readingFirst
            while !Task.isCancelled {
                guard let self else { return }
                if read { await self.tick() }
                read = true
                let interval = self.interval
                try? await Task.sleep(nanoseconds: UInt64(interval * 1_000_000_000))
            }
        }
    }
}
