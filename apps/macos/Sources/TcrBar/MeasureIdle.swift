import AppKit
import Combine
import TcrBarCore

/// `TcrBar --measure-idle <seconds>` counts every `tcr` this app spawns while
/// the panel is SHUT, per verb.
///
/// ## The question
///
/// Two reads run on a three second cadence: the poller, which is meant to run
/// whether or not anything is on screen, and the Peers tab's own, which is
/// meant to live and die with the tab. A tab whose poll outlived its view would
/// charge every closed panel two subprocesses every three seconds, forever, and
/// reading the source cannot settle it: a SwiftUI view's teardown is the
/// framework's business, not the call site's.
///
/// ## Why it is a flag and not a test
///
/// The same reason `--measure-open` and `--shell-probe` are flags. What is
/// being measured is whether a real `NSPopover` closing tears a real view tree
/// down, on a real shell, on this version of macOS. A test with a stub view
/// would report the stub.
///
/// ## What it runs
///
/// Nothing. Every spawn goes to ``SpawnLog``'s stub script through the
/// environment override `TcrTool` already honours, so the run reaches no proxy,
/// reads no config and writes nothing outside its own temporary directory. The
/// point is the count, and a child that records its argv and exits is a spawn
/// exactly like any other.
///
/// It is not invisible while it runs: it builds the real shell, so a status
/// item appears for the length of the measurement and a real panel opens and
/// closes once.
enum MeasureIdle {
    static let flag = "--measure-idle"

    /// The window in seconds, or `nil` when the flag is absent. A flag with no
    /// number takes ``defaultSeconds``, which is long enough for several ticks
    /// of a three second cadence to be unmistakable.
    static func requestedSeconds(_ arguments: [String] = CommandLine.arguments) -> Double? {
        guard let at = arguments.firstIndex(of: flag) else { return nil }
        let next = arguments.index(after: at)
        guard next < arguments.endIndex, let seconds = Double(arguments[next]), seconds > 0 else {
            return defaultSeconds
        }
        return seconds
    }

    static let defaultSeconds: Double = 30

    static let openFlag = "--open-seconds"

    /// How long the panel is left open before it is closed. Long enough for
    /// the Peers tab's first read and at least one repeat, so a run that
    /// counted nothing afterwards is known to have been counting something that
    /// was live. `--open-seconds <n>` lengthens it, so the CPU an open panel
    /// costs can be read over a window as long as the closed one.
    static func requestedOpenSeconds(_ arguments: [String] = CommandLine.arguments) -> Double {
        guard let at = arguments.firstIndex(of: openFlag) else { return defaultOpenSeconds }
        let next = arguments.index(after: at)
        guard next < arguments.endIndex, let seconds = Double(arguments[next]), seconds > 0 else {
            return defaultOpenSeconds
        }
        return seconds
    }

    static let defaultOpenSeconds: Double = 8

    @MainActor
    static func run(seconds: Double) -> Never {
        let app = NSApplication.shared
        app.setActivationPolicy(.accessory)

        let openSeconds = requestedOpenSeconds()
        let deadline = seconds + openSeconds + 60
        DispatchQueue.main.asyncAfter(deadline: .now() + deadline) {
            FileHandle.standardError.write(
                Data("measure-idle: deadline after \(Int(deadline))s, nothing concluded\n".utf8))
            exit(2)
        }

        Task { @MainActor in
            await measure(seconds: seconds, openSeconds: openSeconds)  // exits
        }
        app.run()
        exit(2)
    }

    @MainActor
    private static func measure(seconds: Double, openSeconds: Double) async -> Never {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("tcrbar-measure-idle-\(ProcessInfo.processInfo.processIdentifier)")
        guard let log = try? SpawnLog(directory: directory) else {
            print("measure-idle: could not write the stub under \(directory.path)")
            fflush(stdout)
            exit(1)
        }
        setenv(TcrTool.overrideEnvKey, log.stub.path, 1)

        // The positive control for the whole run. Everything below counts
        // spawns of this one file, so a run where the app resolved some OTHER
        // `tcr` would count nothing and read exactly like a clean result.
        guard case .success(let resolved) = TcrTool.resolve(), resolved.path == log.stub.path else {
            print("measure-idle: the override did not take, this would have measured nothing")
            fflush(stdout)
            exit(1)
        }
        print("measure-idle: every spawn goes to \(log.stub.path)")

        let shell = MenuBarShell(
            awake: AwakeController.harness(),
            updater: Updater(startingUpdater: false),
            whatsNew: WhatsNewController(
                store: WhatsNewStore(defaults: nil), fetcher: NeverFetches(),
                location: nil, currentVersion: nil))
        // What the app itself starts at launch, and no more: the fleet poll and
        // the knock read. Both are meant to run with the panel shut, and both
        // are in the count below so that the peers read is read against them
        // rather than on its own.
        shell.poller.start()
        shell.knocks.start()

        // The second control. The Peers tab is drawn over a fleet and not over
        // an error banner, so a run that opened the panel before the first read
        // landed would be counting a panel with no tab on it.
        _ = await waitUntil(20) { shell.poller.state.isHealthyRead }
        print("measure-idle: fleet · \(shell.poller.state.summary)")

        // Whether the view tree was evaluated INSIDE the open call, before the
        // run loop could composite a frame: the panel's first frame is then
        // drawn from the state it was told of, not from the last one it saw.
        //
        // `.applicationDefined` for the open window: `.transient` closes the
        // panel the moment anything else on the machine takes focus, and an
        // open window that is mostly shut measures neither state. Measured: two
        // runs on a Mac in use lost the panel after 3 s and 12 s of a 30 s
        // window. The close below still goes through `closePanel()`.
        shell.popover.behavior = .applicationDefined
        let beforeOpen = PanelDrawCount.now.bodies
        shell.openPanel(on: .peers)
        print("measure-idle: bodies inside the open call=\(PanelDrawCount.now.bodies - beforeOpen)")
        _ = await waitUntil(20) { shell.popover.isShown }
        let openedAt = log.mark()
        let openStart = Window.Start.now()
        // Sampled, not assumed. This popover is `.transient`, so anything that
        // takes focus on the machine running the measurement dismisses it, and
        // a window that reports itself as open while the panel had gone would
        // turn a correct result into a puzzling one.
        await tick(openSeconds, label: "open") { "isShown=\(shell.popover.isShown)" }
        let open = openStart.end()
        report("panel open on the Peers tab", seconds: openSeconds, of: log.entries(after: openedAt))
        report("panel open on the Peers tab", open)
        // The positive control for the cost lines. A closed window that drew
        // nothing proves something only if the same counters counted the
        // panel while it was open; a run where they did not is wired to
        // nothing and would report every panel as idle.
        guard open.bodies > 0 else {
            print("measure-idle: the open panel drew no body, the draw counters are wired to nothing")
            fflush(stdout)
            exit(1)
        }

        // `animates = false` before the close, the substitution every harness
        // here makes: an animated dismissal never completes when nothing is
        // being composited, and the panel would then still be open for the half
        // of this run that is the point.
        shell.popover.animates = false
        shell.closePanel()
        let closed = await waitUntil(20) { !shell.popover.isShown }
        print("measure-idle: panel closed, isShown=\(shell.popover.isShown) settled=\(closed)")

        let closedAt = log.mark()
        let closedStart = Window.Start.now()
        await tick(seconds, label: "closed") { "isShown=\(shell.popover.isShown)" }
        let closedCost = closedStart.end()
        report("panel closed", seconds: seconds, of: log.entries(after: closedAt))
        report("panel closed", closedCost)

        // The reopen a click makes, on the view tree that was kept while shut.
        // A body inside the call is the tree catching up on what it was not
        // told while shut, before the run loop composites a frame.
        let beforeReopen = PanelDrawCount.now.bodies
        let reopenSpawns = log.mark()
        shell.openPanel()
        print(
            "measure-idle: reopen · bodies inside the open call=\(PanelDrawCount.now.bodies - beforeReopen)"
        )
        _ = await waitUntil(5) { log.mark() > reopenSpawns }
        print(
            "measure-idle: reopen · isShown=\(shell.popover.isShown) · spawns within 5s=\(log.argvLines(after: reopenSpawns))"
        )
        shell.closePanel()
        _ = await waitUntil(5) { !shell.popover.isShown }

        shell.poller.stop()
        shell.knocks.stop()
        shell.statusItem.isVisible = false
        print("measure-idle: done")
        fflush(stdout)
        exit(0)
    }

    /// One line per verb plus a total, in the shape the other harnesses print:
    /// a prefix to grep for, the window, the verb, the count, and the rate it
    /// works out at so a three second cadence is recognisable as one.
    /// Plus the spawns themselves, one line each, seconds from the start of the
    /// window: an average of one every three seconds and three in the first
    /// second are the same figure, and only one of them is a cadence.
    private static func report(_ window: String, seconds: Double, of entries: [SpawnLog.Entry]) {
        let counts = SpawnLog.counts(ofArgv: entries.map(\.argv))
        print(
            String(
                format: "measure-idle: %@ · %.0fs · spawns=%d · verbs=%d", window, seconds,
                entries.count, counts.count))
        for row in counts {
            print(
                String(
                    format: "measure-idle: %@ · %@ · spawns=%d · every %.1fs", window, row.verb,
                    row.count, seconds / Double(row.count)))
        }
        if counts.isEmpty {
            print("measure-idle: \(window) · nothing spawned")
        }
        let first = entries.first?.at ?? 0
        for entry in entries {
            print("measure-idle: \(window) · at=\(entry.at - first)s · \(entry.argv)")
        }
        fflush(stdout)
    }

    /// What one window cost this process: CPU, memory at its end, and how many
    /// times the panel's view tree was evaluated and laid out inside it.
    struct Window {
        let seconds: Double
        let cpuSeconds: Double
        let residentBytes: UInt64?
        let bodies: Int
        let layouts: Int

        struct Start {
            let at: Date
            let cost: SelfCost
            let draws: (bodies: Int, layouts: Int)

            static func now() -> Start {
                Start(at: Date(), cost: SelfCost.read(), draws: PanelDrawCount.now)
            }

            func end() -> Window {
                let cost = SelfCost.read()
                let draws = PanelDrawCount.now
                return Window(
                    seconds: Date().timeIntervalSince(at),
                    cpuSeconds: cost.cpuSeconds - self.cost.cpuSeconds,
                    residentBytes: cost.residentBytes,
                    bodies: draws.bodies - self.draws.bodies,
                    layouts: draws.layouts - self.draws.layouts)
            }
        }
    }

    /// One line per window: CPU seconds and the share of one core they work
    /// out at, resident memory at the window's end, and the panel's body and
    /// layout counts. The same counter `ps -o time=` reads, taken in-process so
    /// the window is exactly the one the spawn count covers.
    private static func report(_ window: String, _ cost: Window) {
        let rss = cost.residentBytes.map { String($0 / 1024) } ?? "unknown"
        print(
            String(
                format:
                    "measure-idle: %@ · cost · cpu=%.3fs over %.1fs · %.2f%% of one core · rss=%@KB · bodies=%d · layouts=%d",
                window, cost.cpuSeconds, cost.seconds, 100 * cost.cpuSeconds / cost.seconds, rss,
                cost.bodies, cost.layouts))
        fflush(stdout)
    }

    /// Waits, printing where it has got to, and what the panel was, every two
    /// seconds.
    ///
    /// A silent half-minute and a run that died during it read the same on a
    /// terminal, and the half of this run whose result is an ABSENCE of spawns
    /// is exactly the half where that matters. The sample is what tells them
    /// apart, and it carries the panel's own state so a window is read against
    /// what the panel was doing rather than what it was asked to do.
    private static func tick(_ seconds: Double, label: String, sample: () -> String) async {
        var waited = 0.0
        while waited < seconds {
            let step = min(2.0, seconds - waited)
            await sleep(step)
            waited += step
            print(
                String(
                    format: "measure-idle: %@ · %.0fs of %.0fs · %@", label, waited, seconds,
                    sample()))
            fflush(stdout)
        }
    }

    private static func sleep(_ seconds: Double) async {
        try? await Task.sleep(nanoseconds: UInt64(seconds * 1_000_000_000))
    }

    /// Poll a condition until it holds. Not a sleep until green: the answer is
    /// returned and printed, so a window that opened over a panel which never
    /// closed says so instead of being reported as an idle count.
    private static func waitUntil(_ timeout: Double, _ condition: () -> Bool) async -> Bool {
        var waited = 0.0
        while waited < timeout {
            if condition() { return true }
            await sleep(0.02)
            waited += 0.02
        }
        return condition()
    }
}
