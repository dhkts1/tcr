import Combine
import Foundation

/// What the last poll established. Every failure mode is a distinct case with its
/// own text: a menu that silently shows an empty list when `tcr` is missing, or
/// when the proxy is down, is a defect — those are different facts and the
/// operator has to be able to tell them apart.
public enum PollState: Equatable {
    /// No poll has completed yet.
    case pending
    /// A fleet was decoded. `source` distinguishes measured counters from
    /// structural zeros.
    case loaded(Fleet)
    /// `tcr` is not on `PATH` and not in any known install directory.
    case toolMissing(searched: [String])
    /// `tcr status --json` exited non-zero — the usual cause is no server.
    case commandFailed(exitCode: Int32, message: String)
    /// The command succeeded but the payload did not match the expected shape.
    case undecodable(message: String)

    public var isHealthyRead: Bool {
        if case .loaded = self { return true }
        return false
    }

    /// One line, always non-empty, safe to put in front of a human.
    public var summary: String {
        switch self {
        case .pending:
            return "Waiting for first poll…"
        case .loaded(let fleet):
            let n = fleet.accounts.count
            let noun = n == 1 ? "account" : "accounts"
            // Every row failed to decode. Saying "0 accounts" here would read as
            // "you have none configured", which is a different and much calmer
            // fact than "tcr answered and this build could not read any of it".
            if n == 0, let unreadable = fleet.unreadableNotice {
                return "no account decoded — \(unreadable)"
            }
            let base =
                fleet.source.countersAreStructural
                ? "\(n) \(noun) — offline read, counters are structurally zero"
                : "\(n) \(noun) — live"
            guard let unreadable = fleet.unreadableNotice else { return base }
            return "\(base) · \(unreadable)"
        case .toolMissing(let searched):
            return "tcr not found on PATH (searched \(searched.count) locations)"
        case .commandFailed(let code, let message):
            let detail = message.isEmpty ? "no output" : message
            return "tcr status failed (exit \(code)): \(detail)"
        case .undecodable:
            // Deliberately drops `message`. It is a Swift `DecodingError`
            // description — key paths and type names — and this property's
            // contract one screen up is "safe to put in front of a human".
            // The raw text is still reachable: `FleetView` hangs it on the
            // banner's tooltip, which is where a reader who wants it looks.
            return "tcr answered, and this build could not read the answer"
        }
    }

    /// The `ready/enabled` label, e.g. `"9/13"` — opt-in, drawn only when the
    /// counts preference is on; the cup's fill level (``capacityFraction``)
    /// carries the same fact on the glyph itself by default.
    ///
    /// `nil` for anything but a healthy read of a fleet with at least one
    /// enabled account: the cup's colour and fill already carry "pending",
    /// "tool missing" and "poll failed" (``MenuBarShell/cupTint(for:awake:cupPool:)``),
    /// and an all-disabled fleet has no numerator/denominator worth showing —
    /// `0/0` would read as a fault, not a fact, the same reasoning
    /// ``Fleet/countsSentence`` gives for returning `nil` in the identical
    /// case.
    ///
    /// A fleet holding Codex accounts counts each pool apart, Claude first:
    /// `"3/4 · 0/2"`. ``countsParts`` has the parts.
    public var countsLabel: String? {
        countsParts?.map(\.text).joined(separator: " · ")
    }

    /// One part of the menu-bar count: one pool's `ready/enabled`, and whether
    /// it is drawn amber.
    public struct CountsPart: Equatable, Sendable {
        public let text: String
        /// The pool is near: nothing ready, and something close to its limit.
        public let isNear: Bool

        public init(text: String, isNear: Bool) {
            self.text = text
            self.isNear = isNear
        }
    }

    /// The parts of ``countsLabel``, or `nil` when it draws nothing.
    ///
    /// A fleet with no Codex account has today's one part, `readyCount` over
    /// `enabledCount`, amber exactly when ``countIsNearCapacity`` says. A fleet
    /// holding Codex accounts has one part per pool that has an enabled
    /// account, Claude first, each amber when its own pool is near, so the
    /// amber says which pool is short. A pool with no enabled account drops
    /// out. `nil` on the cases ``countsLabel`` has always been `nil` on, and
    /// when no pool is left.
    public var countsParts: [CountsPart]? {
        guard case .loaded(let fleet) = self, !fleet.enabledAccounts.isEmpty else { return nil }
        guard let pools = fleet.providerPools() else {
            return [
                CountsPart(
                    text: "\(fleet.readyCount)/\(fleet.enabledCount)",
                    isNear: countIsNearCapacity)
            ]
        }
        let parts = pools.compactMap { pool in
            pool.countsText.map { CountsPart(text: $0, isNear: pool.isNear) }
        }
        return parts.isEmpty ? nil : parts
    }

    /// Which part of the count is amber, in order; empty when nothing is drawn.
    public var countsNearHalves: [Bool] { countsParts?.map(\.isNear) ?? [] }

    /// The fleet of a healthy read, and `nil` for every other state.
    public var loadedFleet: Fleet? {
        guard case .loaded(let fleet) = self else { return nil }
        return fleet
    }

    /// The cup's fill level: `readyCount / enabledCount`, `0...1`.
    ///
    /// `nil` when there is nothing to divide by — a poll that has not loaded a
    /// fleet yet, a read failure, or a fleet with no enabled accounts — the
    /// same condition ``countsLabel`` already treats as "nothing to show", so
    /// the two never disagree about which fleets have a real ratio. The mark
    /// draws `nil` as an empty cup (`MenuBarMark.image(fraction:tint:)`), never
    /// a `0` a reader could mistake for a measured "all spent".
    public var capacityFraction: Double? { capacityFraction(cupPool: .emptier) }

    /// ``capacityFraction`` under the cup's pin. A fleet with no Codex account
    /// has one pool and today's fill, whatever the pin says. A fleet holding
    /// Codex accounts fills to its governing pool
    /// (``Fleet/governingPool(_:now:)``): the emptier one, or the pinned one
    /// when the fleet holds both providers and that pool has an enabled
    /// account.
    public func capacityFraction(cupPool: MenuBarCupPool) -> Double? {
        guard case .loaded(let fleet) = self else { return nil }
        guard fleet.holdsCodex else {
            guard fleet.enabledCount > 0 else { return nil }
            return Double(fleet.readyCount) / Double(fleet.enabledCount)
        }
        return fleet.governingPool(cupPool)?.readyFraction
    }

    /// The menu-bar tooltip's own line: ``Fleet/countsSentence`` when there is
    /// one to give, else ``summary`` unchanged.
    ///
    /// `countsSentence` is `nil` for every case but a healthy read with at
    /// least one enabled account — a pending poll, a missing `tcr`, a failed
    /// command, an undecodable payload, and a healthy read of an all-disabled
    /// fleet all fall through to the same `summary` a reader already knows,
    /// rather than growing a second empty-fleet sentence to keep in sync with
    /// the first.
    public var tooltipSentence: String { tooltipSentence(cupPool: .emptier) }

    /// ``tooltipSentence`` under the cup's pin, with the pools' returns counted
    /// down from `now`.
    ///
    /// - A fleet with no Codex account: today's sentence, whatever the pin.
    /// - A Codex-only fleet: today's sentence form over its one pool, with
    ///   "Codex" before "accounts" (`2 of 4 Codex accounts ready`), or
    ///   ``summary`` when nothing is enabled, as today.
    /// - A fleet holding both providers: one line per pool,
    ///   `Claude · 3 of 4 accounts ready · 1 parked`, or `Codex · 2 parked`
    ///   for a pool with nothing enabled, and ` · shown on the cup` on the
    ///   line of the pool the cup follows.
    ///
    /// An unreadable row's notice closes the last line, as it always has.
    public func tooltipSentence(cupPool: MenuBarCupPool, now: Date = Date()) -> String {
        guard case .loaded(let fleet) = self, let pools = fleet.providerPools(now: now) else {
            return claudeOnlyTooltipSentence
        }
        let sentence: String
        if fleet.holdsBothProviders {
            let governing = fleet.governingPool(cupPool, now: now)?.provider
            sentence = pools.map { pool in
                let line =
                    "\(pool.name) · " + (pool.countsSentence() ?? "\(pool.parkedCount) parked")
                return pool.provider == governing ? line + " · shown on the cup" : line
            }.joined(separator: "\n")
        } else {
            guard let only = pools.first?.countsSentence(nounPrefix: "Codex ") else {
                return summary
            }
            sentence = only
        }
        guard let unreadable = fleet.unreadableNotice else { return sentence }
        return "\(sentence) · \(unreadable)"
    }

    /// Today's tooltip sentence, for a fleet with no Codex account and for
    /// every state that is not a healthy read.
    private var claudeOnlyTooltipSentence: String {
        guard case .loaded(let fleet) = self, let sentence = fleet.countsSentence else {
            return summary
        }
        guard let unreadable = fleet.unreadableNotice else { return sentence }
        return "\(sentence) · \(unreadable)"
    }

    /// The whole menu-bar tooltip.
    ///
    /// The menu bar has room for a glyph and nothing else, so this is where the
    /// poll's own summary is reachable by a human who has not opened the panel.
    ///
    /// A one-line sentence (a fleet with one provider, or a read that failed)
    /// is composed as the menu bar always has: the sentence, then the
    /// running-tools clause when the segment is shown, then the keep-awake
    /// clause, with a waiting Mac's sentence and one space in front of it all.
    /// That order matches the mockup's own (`docs/design/menubar-mark-mockup.html`,
    /// "… · 2 parked · 3 tools running"), and the sentence already says "near
    /// their limit" in words, so the amber count is never colour alone.
    ///
    /// A fleet holding both providers has one line per pool, so the rest moves
    /// onto lines of its own: the waiting Mac first, then the pool lines, then
    /// what the Mac is doing (`3 tools running · Keeping this Mac awake`),
    /// which is left out when it would be empty.
    public func menuBarToolTip(
        awake: Bool, showRunningTools: Bool, knocks: Int, cupPool: MenuBarCupPool,
        now: Date = Date()
    ) -> String {
        let sentence = tooltipSentence(cupPool: cupPool, now: now)
        let running = runningToolsCount(showRunningTools: showRunningTools).map { count in
            "\(count) \(count == 1 ? "tool" : "tools") running"
        }
        let asking = PeerAdmission.knockBarSentence(count: knocks)
        guard loadedFleet?.holdsBothProviders == true else {
            var line = sentence
            if let running { line += " · \(running)" }
            if awake { line = "\(line) · \(KeepAwakeGlyph.accessibilityDescription)" }
            guard let asking else { return line }
            return "\(asking) \(line)"
        }
        let activity = [running, awake ? KeepAwakeGlyph.accessibilityDescription : nil]
            .compactMap { $0 }
        var lines = [sentence]
        if let asking { lines.insert(asking, at: 0) }
        if !activity.isEmpty { lines.append(activity.joined(separator: " · ")) }
        return lines.joined(separator: "\n")
    }

    /// The running-tools segment count, or `nil` when it must not be shown.
    ///
    /// Two independent gates, both required (`docs/design/menubar-mark-mockup.html`
    /// Rules: "appears only when the preference is on AND the wire carries
    /// `sessions`"): `showRunningTools` is the Settings toggle, and
    /// ``Fleet/sessionsSupported`` is whether THIS read actually populated
    /// ``Fleet/sessions`` — distinct from an empty ``Fleet/toolsRunning``, which
    /// is also true of a fleet with nothing running right now. A preference left
    /// on for months must never draw a false `0` when the `tcr sessions --json`
    /// half of the poll did not answer — an old bundled `tcr`, an old proxy, a
    /// dead one (see ``Fleet/SessionsChannel``).
    public func runningToolsCount(showRunningTools: Bool) -> Int? {
        guard showRunningTools, case .loaded(let fleet) = self, fleet.sessionsSupported else {
            return nil
        }
        return fleet.toolsRunning.count
    }

    /// Whether the `ready/enabled` count should draw amber: zero accounts ready
    /// and at least one near its limit. Reuses ``Fleet/capacityGlyphState``'s own
    /// `.near` case rather than a second predicate that could drift from the
    /// glyph's shape — the glyph and the count's colour must always agree on
    /// which state they are both describing.
    public var countIsNearCapacity: Bool {
        guard case .loaded(let fleet) = self else { return false }
        return fleet.capacityGlyphState == .near
    }

    /// Which colour REGIME the one coffee-cup glyph is in — the part of
    /// ``MenuBarMark/Tint`` that does not need an actual `NSColor` to decide,
    /// so it can live (and be tested) here in `TcrBarCore` rather than in
    /// `MenuBarShell`, which the test target does not link (see
    /// `RunningToolsMarkTests`'s own doc-comment on that boundary). The
    /// caller (`MenuBarShell.cupTint(for:awake:cupPool:)`) turns this into a real
    /// ``MenuBarMark/Tint`` by attaching `Tok`'s colours.
    ///
    /// Precedence, highest first:
    ///
    ///  1. A read failure (`toolMissing`, `commandFailed`, `undecodable`) is
    ///     `.failed` — there is no fleet to ask about capacity or keep-awake
    ///     at all, so this outranks everything else.
    ///  2. `Fleet.capacityGlyphState == .near` is `.near` — fleet *capacity*,
    ///     not the worst account: in a rotating pool spent accounts are the
    ///     mechanism working, so a worst-wins colour would sit at its most
    ///     alarming setting whenever any one of thirteen accounts was spent,
    ///     which is nearly always. Reuses the identical predicate
    ///     ``countIsNearCapacity`` does, so the two can never disagree about
    ///     which state they are both describing.
    ///  3. Otherwise: `.awake` while keep-awake holds the Mac up, else
    ///     `.template` — the plain, system-tinted cup.
    ///
    /// `.pending` and an all-disabled fleet both fall through to step 3:
    /// neither is a failure, so the cup stays whatever keep-awake says while
    /// its fill level (``capacityFraction``) draws empty.
    public enum CupTintKind: Equatable {
        case template
        case awake
        case near
        case failed
    }

    public func capacityTintKind(awake: Bool) -> CupTintKind {
        capacityTintKind(awake: awake, cupPool: .emptier)
    }

    /// ``capacityTintKind(awake:)`` under the cup's pin. A fleet with no Codex
    /// account is today's, whatever the pin says. A fleet holding Codex
    /// accounts is amber when the pool the cup follows is near
    /// (``PoolCapacity/isNear``), so the cup's colour and its fill describe one
    /// pool. A failed read, and keep-awake, are as above.
    public func capacityTintKind(awake: Bool, cupPool: MenuBarCupPool) -> CupTintKind {
        switch self {
        case .toolMissing, .commandFailed, .undecodable:
            return .failed
        case .loaded(let fleet) where fleet.holdsCodex:
            if fleet.governingPool(cupPool)?.isNear == true { return .near }
            return awake ? .awake : .template
        case .loaded(let fleet) where fleet.capacityGlyphState == .near:
            return .near
        case .pending, .loaded:
            return awake ? .awake : .template
        }
    }
}

/// Runs `tcr status --json` on a timer and publishes the result.
@MainActor
public final class StatusPoller: ObservableObject {
    /// 3s: fast enough that a quota flip is visible while watching, slow enough
    /// that the CLI's own work stays in the noise.
    public nonisolated static let defaultInterval: TimeInterval = 3

    @Published public private(set) var state: PollState = .pending
    @Published public private(set) var lastPollAt: Date?

    public let interval: TimeInterval
    private var task: Task<Void, Never>?

    public init(interval: TimeInterval = StatusPoller.defaultInterval) {
        self.interval = interval
    }

    /// A poller pinned to one state, for deterministic rendering.
    ///
    /// The state-rendering harness and SwiftUI previews both need a panel that
    /// shows a chosen state without running `tcr` or starting a timer. Without
    /// this seam the only way to see a state is to make the real fleet enter it,
    /// which for "unreadable row" or "zero capacity" means waiting for a bad day.
    ///
    /// It never calls `start()`, so no timer exists and nothing is executed.
    public init(pinnedState: PollState, lastPollAt: Date? = nil) {
        self.interval = Self.defaultInterval
        self.state = pinnedState
        self.lastPollAt = lastPollAt
    }

    deinit { task?.cancel() }

    public func start() {
        guard task == nil else { return }
        task = Task { [weak self] in
            guard let self else { return }
            while !Task.isCancelled {
                await self.pollOnce()
                try? await Task.sleep(nanoseconds: UInt64(self.interval * 1_000_000_000))
            }
        }
    }

    public func stop() {
        task?.cancel()
        task = nil
    }

    /// Returns the state it just published, so a caller that polls *in order to
    /// check something* can compare against the exact read it triggered rather
    /// than against whatever `state` holds by the time it looks — a later timer
    /// tick can land in between. The toggle read-back
    /// (``AccountController/record(readback:requestedEnabled:account:now:)``)
    /// depends on that.
    @discardableResult
    public func pollOnce() async -> PollState {
        let next = await Self.fetch()
        state = next
        lastPollAt = Date()
        return next
    }

    /// One poll: both reads, at once, folded into one state.
    ///
    /// The two halves are independent: `tcr status --json` reports accounts
    /// and `tcr sessions --json` reports sessions, and neither is an input to
    /// the other, so a poll that ran them one after another charged the panel
    /// the sum of two subprocesses for work that fits in the longer of them.
    /// Measured on one machine before this: the pair took 229ms, 288ms and
    /// 249ms while the slower half alone was 147ms, 146ms and 84ms.
    ///
    /// Each half is its own detached task, so both wait in parallel rather
    /// than one thread waiting twice. `await` on both before anything is
    /// decided, so nothing here can return while a child it started is still
    /// running.
    ///
    /// Always called off the main actor: `nonisolated` and `async`, so the
    /// blocking waits inside each detached task never land on the main thread
    /// (where `Process`'s own wait spins the run loop and costs a multiple of
    /// what the child costs).
    nonisolated static func fetch() async -> PollState {
        switch TcrTool.resolve() {
        case .failure(let notFound):
            return .toolMissing(searched: notFound.searched)
        case .success(let executable):
            // The sessions child is started even on a poll whose accounts half
            // will fail, and its answer is then dropped by ``combine``. That is
            // one cheap read on a proxy that is already not answering, against
            // every healthy poll no longer waiting twice.
            async let accounts = Task.detached(priority: .utility) {
                fetchAccounts(executable: executable)
            }.value
            async let sessions = Task.detached(priority: .utility) {
                fetchSessions(executable: executable)
            }.value
            return combine(accounts: await accounts, sessions: await sessions)
        }
    }

    /// The containment rule, as one function rather than as the shape of a
    /// control flow.
    ///
    /// The sessions read may only ever ADD. A fleet that decoded is published
    /// whatever the sessions half did, so an old bundled `tcr`, a dead proxy or
    /// unreadable output costs the Sessions and Tools tabs and nothing else;
    /// and an accounts half that did not decode publishes its own failure
    /// untouched, so a sessions read that happened to succeed can never dress
    /// up a poll that failed.
    ///
    /// It became a function when the two reads started at once. While the
    /// sessions call was reached by falling through the accounts branch, "only
    /// ever adds" was enforced by an early return that could be deleted
    /// without anything noticing; now the rule has a name, and a test.
    nonisolated static func combine(
        accounts: PollState, sessions: Fleet.SessionsRead
    ) -> PollState {
        guard case .loaded(let fleet) = accounts else { return accounts }
        return .loaded(fleet.withSessions(sessions))
    }

    /// Run `tcr status --json` and classify it. Blocking, always called off
    /// the main actor.
    nonisolated static func fetchAccounts(executable: URL) -> PollState {
        do {
            return classify(
                try TcrTool.run(executable: executable, arguments: ["status", "--json"]))
        } catch {
            return .commandFailed(exitCode: -1, message: error.localizedDescription)
        }
    }

    /// Run `tcr sessions --json` and classify it. Never throws: a spawn that
    /// fails is a channel state, not a poll failure.
    nonisolated static func fetchSessions(executable: URL) -> Fleet.SessionsRead {
        do {
            let output = try TcrTool.run(
                executable: executable, arguments: ["sessions", "--json"])
            return classifySessions(output)
        } catch {
            return Fleet.SessionsRead(channel: .commandFailed(error.localizedDescription))
        }
    }

    /// Pure classification of a finished `tcr sessions --json` invocation — the
    /// part worth testing, mirroring ``classify(_:)`` for the accounts half.
    ///
    /// The non-zero branch splits in two on purpose. A `tcr` built before this
    /// subcommand existed does not fail, it REFUSES TO PARSE: clap exits 2 with
    /// "unrecognized subcommand" on stderr. Reporting that as a generic command
    /// failure — or, as the tab did before this channel, as "this server
    /// predates sessions" — blames the proxy for the app's own staleness, and
    /// sends an operator to restart a process that was never the problem.
    public nonisolated static func classifySessions(_ output: TcrTool.Output) -> Fleet.SessionsRead {
        guard output.exitCode == 0 else {
            let stderr = output.stderr.trimmingCharacters(in: .whitespacesAndNewlines)
            let lowered = stderr.lowercased()
            // Both clap spellings: current ("unrecognized subcommand") and the
            // pre-4.x one ("wasn't expected"), so an older bundled binary is
            // still identified as the old half rather than as a dead proxy.
            if lowered.contains("unrecognized subcommand")
                || lowered.contains("wasn't expected")
                || lowered.contains("unexpected argument")
            {
                return Fleet.SessionsRead(channel: .toolPredatesSessions)
            }
            return Fleet.SessionsRead(
                channel: .commandFailed(stderr.isEmpty ? "exit \(output.exitCode)" : stderr))
        }
        do {
            return try Fleet.decodeSessions(output.stdout)
        } catch {
            return Fleet.SessionsRead(channel: .unreadable("\(error)"))
        }
    }

    /// Pure classification of a finished invocation — the part worth testing.
    public nonisolated static func classify(_ output: TcrTool.Output) -> PollState {
        guard output.exitCode == 0 else {
            return .commandFailed(
                exitCode: output.exitCode,
                message: output.stderr.trimmingCharacters(in: .whitespacesAndNewlines)
            )
        }
        do {
            return .loaded(try Fleet.decode(output.stdout))
        } catch {
            return .undecodable(message: "\(error)")
        }
    }
}
