import AppKit
import SwiftUI
import TcrBarCore

/// Rasterise every panel state to PNG, in-process, then exit.
///
/// ## Why this exists
///
/// Every genuine bug in this app shipped past a green build: a `ScrollView` that
/// collapsed thirteen rows to one, a null quota that blanked the whole panel, and
/// a takeover that reported failure as success. None of them were visible to
/// `swift test`, because none of them are facts about types — they are facts about
/// what AppKit draws once it proposes a size.
///
/// Screen capture is not always available (`screencapture` needs Screen Recording
/// and `osascript` needs assistive access, both of which a build machine or a
/// headless agent may lack), and a screenshot only ever shows the state the live
/// fleet happens to be in. `ImageRenderer` needs neither permission: it draws the
/// real view, with the real tokens, into a bitmap this process owns.
///
/// The states below are chosen because they are the ones that are HARD to observe
/// live — you cannot wait for "every account exhausted" or "a row failed to
/// decode" on demand.
///
/// ## Usage
///
///     TcrBar.app/Contents/MacOS/TcrBar --render-states <output-directory>
///
/// Writes one PNG per state and exits without ever showing a menu-bar item,
/// polling `tcr`, or touching a server.
///
///     TcrBar.app/Contents/MacOS/TcrBar --render-states <output-directory> --only <substring>
///
/// Renders only the scenes whose name contains `<substring>`, in every scene
/// list below, and prints how many matched. The whole set is several hundred
/// PNGs, too slow for a check that needs four of them. A substring nothing
/// matches exits 1, so a typo cannot pass as an empty, successful render.
enum RenderStates {
    static let flag = "--render-states"
    static let onlyFlag = "--only"

    /// Returns the output directory when the process was launched to render.
    static func requestedDirectory(_ arguments: [String] = CommandLine.arguments) -> URL? {
        guard let i = arguments.firstIndex(of: flag), i + 1 < arguments.count else { return nil }
        return URL(fileURLWithPath: arguments[i + 1])
    }

    /// What `--only` asked for: `.all` without the flag, `.matching` with a
    /// substring, and `.missingValue` for a flag with nothing after it, which
    /// is refused rather than read as "everything".
    enum SceneFilter: Equatable {
        case all
        case matching(String)
        case missingValue

        func admits(_ name: String) -> Bool {
            switch self {
            case .all: return true
            case .matching(let part): return name.contains(part)
            case .missingValue: return false
            }
        }
    }

    static func requestedFilter(_ arguments: [String] = CommandLine.arguments) -> SceneFilter {
        guard let i = arguments.firstIndex(of: onlyFlag) else { return .all }
        guard i + 1 < arguments.count, !arguments[i + 1].isEmpty,
            !arguments[i + 1].hasPrefix("--")
        else { return .missingValue }
        return .matching(arguments[i + 1])
    }

    /// Every state worth looking at, with the name its PNG gets.
    ///
    /// `awake` is a per-scene flag rather than a twelfth state because it is
    /// orthogonal to the poll: the mode can be on under any fleet at all. Two
    /// scenes carry it ON — one on the Accounts tab, one on Sessions, since
    /// `FleetView.v4FooterAwakeQuit` draws on every tab and a single scene
    /// would only prove the Accounts case.
    ///
    ///  - The tinted mark is drawn on the status item (``MenuBarShell``), which
    ///    is not part of this view, so no scene here renders it.
    ///  - The switch itself is drawn by ``SnapshotSwitch`` in `snapshotMode`,
    ///    not by the live `.switch` Toggle: `ImageRenderer` draws a `.switch`
    ///    toggle as the same yellow "prohibited" placeholder regardless of
    ///    `isOn` (the limitation this file records for a `.checkbox` toggle
    ///    too), and these renders are the README's screenshots. So the ON scenes
    ///    now show the thumb on the right; what they still do not prove is how
    ///    the live AppKit switch looks.
    private static var scenes: [(name: String, state: PollState, awake: Bool, control: String?)] {
        [
            ("01-healthy", .loaded(fleet(healthyJSON)), false, nil),
            // The README's top image: `01-healthy`'s two accounts with the
            // proxy running, as a reader's own panel would be. `01-healthy`
            // itself stays unsupervised; see `supervisedScenes`.
            ("01b-healthy-running", .loaded(fleet(healthyJSON)), false, nil),
            ("01c-divergent-windows", .loaded(fleet(divergentWindowsJSON)), false, nil),
            ("01d-unmeasured-window-proof", .loaded(fleet(unmeasuredWindowJSON)), false, nil),
            ("01e-plan-labels", .loaded(fleet(planLabelsJSON)), false, nil),
            ("01f-duplicate-email", .loaded(fleet(duplicateEmailJSON)), false, nil),
            // The control account is pinned to the worst row deliberately: the
            // CONTROL pill is one of the six things competing for its width,
            // and a fixture that left it off would not be the row that
            // overflowed.
            (
                "01g-widest-row", .loaded(fleet(widestRowJSON)), false,
                "henry.fitzgerald@example.com"
            ),
            ("02-mixed-thirteen", .loaded(fleet(mixedJSON)), false, nil),
            // Option H's hover card (unit H2), pinned by ``hoverCardFixture(for:)``
            // because this harness has no pointer. 02b: a line in the upper
            // half, so the card opens below it, on the control account so the
            // card's CONTROL pill is in frame. 02c: an out-of-tokens line in
            // the lower half, so the card opens above it. Each is also drawn
            // without its card and must come out the same size
            // (``hoverCardKeepsPanelSize(without:with:named:)``).
            (
                "02b-mixed-thirteen-hover", .loaded(fleet(mixedJSON)), false,
                "near@example.com"
            ),
            ("02c-mixed-thirteen-hover-lower", .loaded(fleet(mixedJSON)), false, nil),
            // The strip over the Accounts tab (unit H3) with the pointer on
            // its 8th segment, spent-3, pinned by ``litAccountFixture(for:)``:
            // the segment ringed and spent-3's line lit, and no hover card.
            // Held to the same with-and-without size check as 02b.
            ("02d-mixed-thirteen-segment-hover", .loaded(fleet(mixedJSON)), false, nil),
            // "Keep Card Open" (Gil, 2026-10-14) on one live account, ok-2, and
            // one out-of-tokens account, spent-2, pinned by
            // ``keptOpenFixture(for:)``: each draws its hover card's content in
            // its own place in the list, splitting its box around it.
            ("02e-mixed-thirteen-kept-open", .loaded(fleet(mixedJSON)), false, nil),
            ("03-zero-capacity", .loaded(fleet(exhaustedJSON)), false, nil),
            ("04-unmeasured-row", .loaded(fleet(unmeasuredJSON)), false, nil),
            ("04b-needs-relogin-row", .loaded(fleet(needsReloginJSON)), false, nil),
            ("04c-probed-then-broken-row", .loaded(fleet(probedThenBrokenJSON)), false, nil),
            ("05-unreadable-row", .loaded(partiallyUnreadableFleet()), false, nil),
            ("06-offline-source", .loaded(fleet(offlineJSON)), false, nil),
            ("07-empty-fleet", .loaded(fleet("[]")), false, nil),
            (
                "08-tool-missing", .toolMissing(searched: ["/usr/local/bin/tcr", "/opt/homebrew/bin/tcr"]),
                false, nil
            ),
            ("09-command-failed", .commandFailed(exitCode: 1, message: "connection refused"), false, nil),
            ("10-undecodable", .undecodable(message: "DecodingError.valueNotFound: quota"), false, nil),
            ("11-pending", .pending, false, nil),
            ("12-keeping-awake", .loaded(fleet(healthyJSON)), true, nil),
            // The same switch, on a tab other than Accounts — the footer row
            // is app state, not tab state (`FleetView.v4FooterAwakeQuit`), so
            // one ON scene away from Accounts is what proves that rather than
            // assumes it.
            ("12b-keeping-awake-sessions-tab", .loaded(sessionsTabFleet), true, nil),
            // The control-account row indicator (`FleetView.controlIndicator`) —
            // the ONE piece of this feature `ImageRenderer` can actually draw.
            // `Menu` contents (the gear's "Use as control account" item, its
            // checkmark) never rasterise regardless of state; see this file's
            // own header and `AccountRow.accountActionsMenu`'s doc-comment.
            //
            // `controlAccountJSON`, not `healthyJSON`: this is the one scene
            // rendered at both `.auto` and `.comfortable` (`densityVariantScenes`
            // below), and `.auto` only resolves `.compact` above four accounts
            // (`PanelDensityPreference.comfortableCeiling`). `healthyJSON`'s two
            // rows sit under that line, so `.auto` and a forced `.comfortable`
            // drew the identical picture and the density variant proved
            // nothing.
            ("13-control-account", .loaded(fleet(controlAccountJSON)), false, "alice@example.com"),
            // Every spend branch at once — see `usageStatsJSON`.
            ("14-usage-stats", .loaded(fleet(usageStatsJSON)), false, nil),
            // A parked group beside a live one — see `parkedGroupJSON`.
            ("15-parked-group", .loaded(fleet(parkedGroupJSON)), false, nil),
            // The SAME fleet, expanded — see `render(_:appearance:into:)`'s
            // own seeding of `FleetView.expandedGroupsKey` for
            // `henry-team`, the wholly-parked group `parkedGroupJSON` builds.
            ("15b-parked-group-expanded", .loaded(fleet(parkedGroupJSON)), false, nil),
            // A line INSIDE a group box with its hover card (unit H1b): carol,
            // DEV's one live account, pinned by ``hoverCardFixture(for:)`` and
            // held to the same with-and-without size check as 02b.
            ("15c-parked-group-hover", .loaded(fleet(parkedGroupJSON)), false, nil),
            // F2 — the Sessions tab, grouped by account, one row with a
            // tool running and one waiting, plus an unassigned session with
            // no matching Claude Code session file (`sessionsFixture`'s
            // third entry) to review the id-head fallback.
            ("16-sessions-tab", .loaded(sessionsTabFleet), false, nil),
            // F3 — the Tools tab: running now, slowest today (one at the
            // Bash timeout), and the fleet-wide totals line.
            ("17-tools-tab", .loaded(sessionsTabFleet), false, nil),
            // The same fleet with one TIMED OUT TODAY class OPEN — see
            // `expandedTimeoutClassesFixture`. Its own scene because the
            // disclosure is that card's whole point (a count that opens to
            // the commands behind it) and a harness that can only draw it
            // closed reviews half the section.
            ("17b-tools-tab-timeout-class-open", .loaded(sessionsTabFleet), false, nil),
            // Heat and fans on the machine line, `docs/design/fan-readout-mockup.html`
            // panels 2, 4, 5 and 6. Each is scene 17 with only the machine
            // reading changed (``heatScenes``), so the first card's top must
            // sit at the same pixel row as in 17: the numbers line reserves its
            // lines whatever it holds (`FleetView.toolsNumbersReservedLines`).
            ("17d-tools-tab-heat-manual", .loaded(sessionsTabFleet), false, nil),
            ("17e-tools-tab-heat-fans-off", .loaded(sessionsTabFleet), false, nil),
            ("17f-tools-tab-heat-no-fans", .loaded(sessionsTabFleet), false, nil),
            ("17g-tools-tab-heat-worst-case", .loaded(heatWorstCaseFleet), false, nil),
            // Fan control, mockup panels 7 to 11: the numbers line under each
            // popover, `fans off` (7, 8), ` fixed` (9), no word in Follow CPU
            // (10), ` manual` while Macs Fan Control holds the fans (11). Scene
            // 17 but the machine reading, like 17d to 17g. The popover itself
            // is ``fanPopoverScenes``: `ImageRenderer` draws a view, never a
            // presentation.
            ("17h-tools-tab-fan-install", .loaded(sessionsTabFleet), false, nil),
            ("17i-tools-tab-fan-approval", .loaded(sessionsTabFleet), false, nil),
            ("17j-tools-tab-fan-fixed", .loaded(sessionsTabFleet), false, nil),
            ("17k-tools-tab-fan-follow-cpu", .loaded(sessionsTabFleet), false, nil),
            ("17l-tools-tab-fan-another-app", .loaded(sessionsTabFleet), false, nil),
            // The same two tabs at the LIVE fleet's measured dimensions
            // instead of the mockup's — see `realShapeFleet`. These are the
            // only scenes where TIMED OUT TODAY is absent (the real fleet
            // reports no timeout class, so the card does not draw) and the
            // only ones where the Sessions tab's warm-up fold has anything
            // to fold.
            ("22-tools-tab-real-fleet", .loaded(realShapeFleet), false, nil),
            ("22b-sessions-tab-real-fleet", .loaded(realShapeFleet), false, nil),
            // The forward-compat case both tabs must show as one sentence,
            // never an empty list: `healthyJSON` carries no `sessions` key at
            // all, the shape every server shipped before F1.
            ("18-sessions-tab-old-server", .loaded(fleet(healthyJSON)), false, nil),
            // The same server, on the Tools tab. Its own scene because the two
            // tabs draw two different summary lines above that one sentence,
            // and the Tools half is the one the 2026-09-13 interface review
            // caught printing "0 tool calls today" directly over "This server
            // predates sessions — update tcr." (finding 9). A scene nobody
            // renders is a claim nobody can check.
            ("18b-tools-tab-old-server", .loaded(fleet(healthyJSON)), false, nil),
            // The case the single old sentence got WRONG: the proxy is fine
            // and the BUNDLED `tcr` is the stale half, so it has no `sessions`
            // subcommand to ask with. Telling an operator to restart a healthy
            // proxy here is worse than saying nothing.
            ("18c-sessions-tab-old-tcr", .loaded(oldToolFleet), false, nil),
            ("18d-tools-tab-old-tcr", .loaded(oldToolFleet), false, nil),
            // The longest sentence either banner can draw — the CLI's own
            // stderr, inlined. Its own scene because a wrapping failure is
            // only ever visible in pixels.
            ("18e-sessions-tab-command-failed", .loaded(sessionsFailedFleet), false, nil),
            // The Accounts tab's structure, matching
            // the mockup's Accounts panel —
            // 2 solo cards, a 3-member parked group, a 6-member active
            // group — so the pixelmatch gate compares two panels with the
            // same SHAPE, not a 2-card fixture against a 4-section mockup.
            // `01-healthy` is left alone: other scenes and tests key off its
            // exact 2-account shape, and this is a dedicated fixture for the
            // parity gate rather than a rewrite of a scene with other jobs.
            // Pinned so the parity render also proves the `CONTROL` pill in a
            // full panel: `henry10@example.com` is the first loose card, so
            // it draws the pill beside its plan without a group's legend
            // competing for the same row.
            (
                "19-accounts-tab-parity", .loaded(accountsParityFleet), false,
                "henry10@example.com"
            ),
            // The row `v4UpdateRow` restored (FleetView.swift): an available
            // update, drawn under the summary with the "Update…" button —
            // see ``updateState(for:)`` for how the `Updater` gets there.
            ("21-update-available", .loaded(fleet(healthyJSON)), false, nil),
            // Same row, the failure branch — the reason in place of the
            // button, and no button at all.
            ("21b-update-failed", .loaded(fleet(healthyJSON)), false, nil),
            // The Accounts tab's no-requests banner
            // (`FleetView.noRequestsBanner`, `NoRequestsBanner`). A live
            // fleet, every account at zero requests, five minutes in. See
            // ``noRequestsBannerZeroSince`` and ``noRequestsBannerRoute`` for
            // the seeded clock and route.
            ("22-no-requests-banner", .loaded(fleet(zeroRequestsJSON)), false, nil),
            // The tab strip at the widest counts this panel can reach, which
            // no other scene draws: every existing fixture badges one or two
            // digits by accident of its own subject, and the strip's failure
            // is a width failure. See ``widestTabBadgesFleet``.
            ("23-widest-tab-badges", .loaded(widestTabBadgesFleet), false, nil),
            // A delete `tcr` refused, and one that landed on a proxy too old
            // to drop the row: each line under its own account, in `tcr`'s
            // words. Option H's rows drew neither until 2026-10-14, so a
            // refused "Delete Account…" looked like a click that did nothing.
            // See ``removeControllerFixture(for:)``.
            ("24-delete-outcomes", .loaded(fleet(healthyJSON)), false, nil),
            // The one-time "Send claude through tcr?" prompt on the Accounts
            // tab, then the same opening after "In my shell" was answered:
            // the outcome line in the buttons' place. The second scene also
            // checks that answering changed no size and did draw
            // (``claudeRoutePromptKeepsPanelSize(before:after:named:)``).
            ("25-claude-route-prompt", .loaded(fleet(healthyJSON)), false, nil),
            ("25b-claude-route-answered", .loaded(fleet(healthyJSON)), false, nil),
            // A fleet holding both providers: both Claude accounts out of
            // tokens, one Codex account ready and one held on its week. The
            // strip counts each pool on its own row, and the money line adds
            // the Codex tokens. See ``mixedFleetJSON``.
            ("24-mixed-fleet", .loaded(fleet(mixedFleetJSON)), false, nil),
            // The same fleet once a Claude request on an unpriced model lands: the money
            // line's every clause at once, which must not wrap to a second line. See
            // ``mixedFleetUnpricedJSON``.
            ("24e-mixed-fleet-unpriced", .loaded(fleet(mixedFleetUnpricedJSON)), false, nil),
            // A Codex account's card in its states, kept open so each draws as a
            // card: a Pro plan's one weekly row (bob), spent on its week with
            // credits (carol), both windows (dana), and refused (gil). See
            // ``codexScene(_:)``.
            ("25-codex-cards", .loaded(fleet(codexScene([.bob, .carol, .dana, .gil]))), false, nil),
            // The other four: refused with a known hold (hal), signed out with its
            // reason under its card (erin), a plan that reports no window (jo), and
            // a reserved Codex account no request can reach (kim, in RESEARCH).
            ("25b-codex-cards-b", .loaded(fleet(codexScene([.hal, .erin, .jo, .kim]))), false, nil),
            // Every canonical row kept open, at the compact density ten rows take.
            (
                "25c-codex-cards-compact", .loaded(fleet(codexScene(CanonicalCodexRow.allCases))),
                false, nil
            ),
            // A Codex account before its first probe: unmeasured, both rows empty.
            ("25e-codex-unmeasured", .loaded(fleet(unmeasuredCodexJSON)), false, nil),
            // Every canonical row as the tab draws it, one line each, nothing pinned.
            ("25g-codex-lines", .loaded(fleet(codexScene(CanonicalCodexRow.allCases))), false, nil),
            // The same lines, with the signed-out Codex account's hover card showing.
            (
                "25h-codex-hover-card", .loaded(fleet(codexScene(CanonicalCodexRow.allCases))),
                false, nil
            ),
            // Both providers at compact density: ORCHARD is drawn once under each head, each
            // box holding its own provider's members. See ``mixedFleetCompactJSON``.
            ("24b-mixed-fleet-compact", .loaded(fleet(mixedFleetCompactJSON)), false, nil),
            // A fleet of Codex accounts alone: one list and no head.
            ("24c-codex-only", .loaded(fleet(codexScene([.bob, .dana, .carol, .erin]))), false, nil),
            // Both providers, every Codex account switched off: the head still counts them.
            ("24d-mixed-codex-all-parked", .loaded(fleet(mixedCodexAllParkedJSON)), false, nil),
            // The empty fleet on a Mac holding a codex login: one more line under its detail.
            // See ``codexSeeds(for:)``.
            ("07b-empty-fleet-codex-login", .loaded(fleet("[]")), false, nil),
            // A proxy that cannot pool codex: the Codex line says why.
            (
                "26-codex-not-pooled-banner",
                .loaded(
                    fleet(
                        codexRows(
                            [.bob, .dana],
                            extra: ["codexRoute": unpooledRoute, "codexTraffic": quietCodexTraffic]))),
                false, nil
            ),
            // The canonical refusals, while this Mac shares its accounts with trusted Macs.
            ("26b-codex-refusals-banner", .loaded(fleet(codexRows([.bob, .carol]))), false, nil),
            // Requests that ran on their own login, the one Codex line drawn dim.
            (
                "26c-codex-unpooled-banner",
                .loaded(
                    fleet(codexRows([.bob], extra: ["codexTraffic": ["refused10m": 0, "unpooled10m": 3]]))
                ),
                false, nil
            ),
            // Two codex sessions running and no Codex request reaching tcr for ten minutes.
            ("26d-codex-no-requests-banner", .loaded(fleet(idleCodexJSON)), false, nil),
            // The same, on a Mac running Claude Code too: no Claude account, so no Claude line.
            ("26e-codex-only-no-claude-nag", .loaded(fleet(idleCodexJSON)), false, nil),
            // A route that is not pooled and refusals at once: only the route's line shows.
            (
                "26f-codex-banner-precedence",
                .loaded(fleet(codexRows([.bob, .dana], extra: ["codexRoute": unpooledRoute]))),
                false, nil
            ),
            // 26d with no command-line tcr on this Mac: the line names the tcr inside TcrBar.
            ("26g-codex-no-requests-banner-no-path-tcr", .loaded(fleet(idleCodexJSON)), false, nil),
            // Scene 22's Claude account beside a Codex account that is serving: the Claude
            // line names Claude, and no Codex line shows.
            ("26h-mixed-claude-no-requests-banner", .loaded(fleet(mixedClaudeQuietJSON)), false, nil),
            // The tcr inside TcrBar is too old for a config holding a Codex account: the
            // poll's banner names that tcr and the fix, where tcr's words name a version.
            (
                "09b-bundled-tcr-predates-codex",
                .commandFailed(
                    exitCode: 1,
                    message:
                        "error: could not read /Users/example/.config/tcr.json: account "
                        + "\"bob@example.com/codex\" carries credentialVersion 2, which this build "
                        + "only understands up to 1; refusing to load rather than silently "
                        + "reinterpret its credential shape"),
                false, nil
            ),
            // A Codex re-login tcr refused, its reason kept under erin's line for this app
            // run. See ``codexNotesFixture(for:)``.
            ("25d-codex-relogin-refused", .loaded(fleet(codexReloginRefusedJSON)), false, nil),
            // ivy added to ORCHARD: tcr's warning that no Codex request can ask for a
            // group, kept under her line.
            ("25f-codex-group-add-warning", .loaded(fleet(codexGroupAddWarningJSON)), false, nil),
            // The Sessions tab of a fleet holding both providers: alice's Claude
            // session and bob's Codex one, both in tcr, bob's joined by
            // the first line of his rollout file. It shows the mixed fleet's
            // footer, the `Codex` lead on bob's subtitle and the summary's two
            // clause groups.
            // See ``sessionsTabMixedFleet``.
            ("16b-sessions-tab-mixed", .loaded(sessionsTabMixedFleet), false, nil),
            // A Codex-only fleet with no session and no Codex request in the last
            // hour: the Codex-only empty line and footer.
            ("16c-sessions-tab-codex-only-empty", .loaded(codexOnlyQuietSessionsFleet), false, nil),
            // A proxy too old to record Codex sessions: bob served seven requests
            // in the last hour and the list holds alice's session alone, so the
            // tab says why bob's is missing above it.
            ("16d-sessions-tab-old-server-codex", .loaded(oldServerCodexSessionsFleet), false, nil),
            // The Tools tab with a running Codex call: bob's one-shot
            // `exec_command` filling its own 120s ring, red with 20s left, beside
            // alice's calm Bash ring and its ✕, `no Kill` in bob's slot, and the
            // Codex categories in BY TOOL. See ``toolsTabCodexFleet``.
            ("17c-tools-tab-codex", .loaded(toolsTabCodexFleet), false, nil),
            // One fleet on every tab, so `scripts/check-tabs-one-height.swift`
            // can hold the four to one height and a still strip (Gil,
            // 2026-10-16: "can you make also it to not change the height?").
            // `sessionsTabFleet` because it fills all four: accounts, live
            // sessions, running tools.
            ("27-one-height-accounts", .loaded(sessionsTabFleet), false, nil),
            ("27-one-height-sessions", .loaded(sessionsTabFleet), false, nil),
            ("27-one-height-tools", .loaded(sessionsTabFleet), false, nil),
            ("27-one-height-peers", .loaded(sessionsTabFleet), false, nil),
        ]
    }

    /// The claude-route controller a scene draws with: scenes 25 and 25b get
    /// one pinned to the prompt (asked, then answered); every other scene a
    /// pinned controller that shows nothing. Pinned controllers never run
    /// `tcr` and never write the operator's defaults.
    @MainActor
    private static func claudeRouteFixture(for sceneName: String) -> ClaudeRouteController {
        let off = ClaudeRouteCommand.Reading.status(ClaudeRouteStatus(route: .off))
        switch sceneName {
        case "25-claude-route-prompt":
            return ClaudeRouteController(pinned: off, promptShown: true)
        case "25b-claude-route-answered":
            return ClaudeRouteController(
                pinned: off, promptShown: true,
                promptOutcome: ClaudeRoutePrompt.outcome(for: .shell, failure: nil))
        default:
            return ClaudeRouteController(pinned: off)
        }
    }

    /// Answering the prompt must move nothing (Gil, 2026-10-14): the panel
    /// with the question and the panel with its outcome line are the same
    /// size, and they are different pictures (a swap that drew nothing would
    /// pass the size half alone). Prints what failed and answers `false`,
    /// which fails the run, the same contract as
    /// ``hoverCardKeepsPanelSize(without:with:named:)``.
    @MainActor
    private static func claudeRoutePromptKeepsPanelSize<A: View, B: View>(
        before: A, after: B, named name: String
    ) -> Bool {
        let asked = ImageRenderer(content: before)
        let answered = ImageRenderer(content: after)
        asked.scale = 2
        answered.scale = 2
        guard let a = asked.nsImage, let b = answered.nsImage else {
            FileHandle.standardError.write(Data("prompt check: \(name) did not render\n".utf8))
            return false
        }
        if a.size != b.size {
            FileHandle.standardError.write(
                Data(
                    ("prompt check: \(name) is \(Int(b.size.width))x\(Int(b.size.height))pt "
                        + "answered and \(Int(a.size.width))x\(Int(a.size.height))pt asked: "
                        + "answering changed the panel's size\n").utf8))
            return false
        }
        if a.tiffRepresentation == b.tiffRepresentation {
            FileHandle.standardError.write(
                Data("prompt check: \(name) drew the same picture asked and answered\n".utf8))
            return false
        }
        print("  prompt check: \(name) \(Int(b.size.width))x\(Int(b.size.height))pt either way")
        return true
    }

    /// The delete outcomes scene 24 draws; every other scene gets a controller
    /// holding nothing, which draws no line, the panel's own default.
    @MainActor
    private static func removeControllerFixture(for sceneName: String) -> RemoveAccountController {
        guard sceneName == "24-delete-outcomes" else { return RemoveAccountController() }
        return .harness(
            failures: [
                AccountRef(name: "alice@example.com"): RemoveAccountCommand.Failure(
                    exitCode: 1,
                    message:
                        "Error: the proxy running on :3456 refused this: no account matches 'alice@example.com' Nothing was changed."
                )
            ],
            notices: [
                AccountRef(name: "bob@example.com"):
                    "[tcr] the proxy running on :3456 stopped routing to 'bob@example.com', but it predates live removal and keeps listing it as disabled until it restarts."
            ])
    }

    /// The Codex notes scenes 25d and 25f draw under a line, recorded against the rows
    /// those scenes draw, then opened on `fleet` as the shell opens the panel, which reads
    /// each signed-out row's reason. Every other scene records none, so a note an
    /// operator's own run recorded never reaches a render.
    private static func codexNotesFixture(for sceneName: String, opening fleet: Fleet?)
        -> CodexNotes
    {
        var notes = CodexNotes()
        switch sceneName {
        case "25d-codex-relogin-refused":
            notes.recordReloginRefusal(routedRefusal, for: sceneRow(.erin))
        case "25f-codex-group-add-warning":
            notes.recordGroupAddNotice(
                orchardWarning, group: "orchard", for: sceneRow(.ivy).ref)
        default:
            break
        }
        // Read as begun before every note was recorded, so the opening keeps them all.
        if let fleet { notes.openedOn(fleet.accounts, pollBegan: .distantPast) }
        return notes
    }

    /// Seeds for scene 22, the one scene that needs
    /// ``FleetView``'s `initialZeroRequestsSince`/`initialClaudeCount`/
    /// `initialClaudeRoute`. Every other scene leaves all three `nil`, which
    /// draws no banner at all, exactly the panel's own default.
    private static let noRequestsBannerZeroSince = Date().addingTimeInterval(-600)
    private static let noRequestsBannerRoute = ClaudeRouteRead.Route(
        url: "http://127.0.0.1:9443", source: "settings.json")

    /// The scenes that seed the Claude line's clock, count and route: scene 22,
    /// and the two Codex scenes that show what that line does beside a Codex
    /// account.
    private static let claudeBannerScenes: Set<String> = [
        "22-no-requests-banner", "26e-codex-only-no-claude-nag",
        "26h-mixed-claude-no-requests-banner",
    ]

    /// What a scene tells the panel about codex on this Mac and about its Codex
    /// line's inputs. The harness never reads the process table, the clock, the
    /// peers file, the bundle or the disk for these, so a scene that needs one
    /// names it here.
    private struct CodexSeeds {
        var codexCount: Int?
        var codexZeroSince: Date?
        var peerSharing: Bool?
        var presence: CodexPresence = .absent
        var home: String?
        var bundledTcr: String?
    }

    /// The seeds for one scene: none, the panel's own default, for every scene
    /// but these.
    private static func codexSeeds(for sceneName: String) -> CodexSeeds {
        switch sceneName {
        case "07b-empty-fleet-codex-login":
            return CodexSeeds(
                presence: CodexPresence(codexHome: "/Users/example/.codex", hasLoginFile: true),
                home: "/Users/example")
        case "26b-codex-refusals-banner":
            return CodexSeeds(peerSharing: true)
        case "26d-codex-no-requests-banner":
            return CodexSeeds(codexCount: 2, codexZeroSince: noRequestsBannerZeroSince)
        case "26e-codex-only-no-claude-nag":
            return CodexSeeds(codexCount: 1, codexZeroSince: noRequestsBannerZeroSince)
        case "26g-codex-no-requests-banner-no-path-tcr":
            return CodexSeeds(
                codexCount: 2, codexZeroSince: noRequestsBannerZeroSince,
                bundledTcr: "/Applications/TcrBar.app/Contents/MacOS/tcr")
        default:
            return CodexSeeds()
        }
    }

    /// The ``UpdateState`` a scene's `Updater` should report, or `nil` for
    /// every scene not about the update row — the same by-name lookup
    /// ``initialTab(for:)`` already uses rather than a tuple element every
    /// other scene would carry for nothing.
    private static func updateState(for sceneName: String) -> UpdateState? {
        switch sceneName {
        case "21-update-available": return .available(version: "0.2.50")
        case "21b-update-failed":
            return .failed(
                "the proxy on :3456 rejected the api-key in "
                    + "~/.config/tcr.json while checking the feed")
        default: return nil
        }
    }

    /// The sign-in sheet (``LoginSheet``), one PNG per state, drawn WHERE IT
    /// APPEARS: over the Accounts tab, on the dimming scrim a real sheet puts
    /// there.
    ///
    /// A separate list because a sheet is not a `PollState`: it is presented
    /// over the panel, and `ImageRenderer` draws a view, never a presentation —
    /// a `.sheet` modifier on the panel rasterises as the panel alone. So the
    /// harness composes the two by hand, which is exactly why ``LoginSheet``
    /// takes a phase rather than a live ``LoginSession``. The composition is
    /// an approximation of AppKit's presentation (which insets and shadows the
    /// sheet itself), and it answers the question a bare sheet could not: how
    /// much of the panel is still legible behind it, and where the eye lands.
    ///
    /// **The spinner is drawn as a still `clock`** (`LoginSheet.snapshotMode`).
    /// `ImageRenderer` rasterises a `ProgressView` as the macOS "prohibited"
    /// placeholder — a red crossed-out circle, the same class of limitation
    /// this file already records for a `.checkbox` toggle — so the fixture
    /// showed a state the app never draws. The still glyph is a stand-in for
    /// motion, and is not evidence about the spinner either way.
    ///
    /// `.failed` is the state this list exists for. It is the one nobody sees
    /// until it happens to them, it carries the longest string on the sheet
    /// (`tcr`'s own refusal, unparaphrased), and a wrapping failure line is the
    /// kind of thing a green build says nothing about.
    private static var sheetScenes: [(name: String, phase: LoginPhase, url: URL?)] {
        let authorize = URL(
            string: "https://claude.ai/oauth/authorize?code=true&state=RENDER-FIXTURE")
        return [
            ("20-login-opening", .opening, nil),
            (
                "20b-login-waiting", .waitingForBrowser(email: "alice@example.com"),
                authorize
            ),
            ("20c-login-saved", .saved(account: "alice@example.com"), nil),
            (
                "20d-login-failed",
                .failed(
                    reason:
                        "the proxy on :3456 rejected the api-key in "
                        + "~/.config/tcr.json while checking whether it could take a "
                        + "live login — no browser was opened and nothing was changed."),
                nil
            ),
        ]
    }

    /// The sheet for a Codex account: the chooser, the sign-in in each of its phases,
    /// the import, and what the sheet says when tcr cannot find codex. Every reason and
    /// notice is inlined as tcr sends it, because a render never reads the test bundle.
    private static var codexSheetScenes:
        [(name: String, phase: LoginPhase, url: URL?, facts: LoginSheetFacts)]
    {
        let authorize = URL(
            string: "https://auth.openai.com/oauth/authorize?client_id=app_EXAMPLE&state=RENDER-FIXTURE")
        let signIn = LoginSheetFacts(request: .codex(account: nil))
        let home = "/Users/example/.codex"
        let saved = LoginSheetFacts(
            request: .codex(account: nil),
            notices: [
                "tcr pools marked Codex connections to chatgpt.com: use `tcr run --codex`; keep "
                    + "the marker scoped to that launch, since unmarked connections keep their own login",
                "codex needs a login of its own to start, even when pooled, and none is at "
                    + "/Users/example/.codex/auth.json: unless codex keeps its login in the "
                    + "keyring, run `codex login` once",
                "the tcr at /usr/local/bin/tcr (0.2.44) predates Codex accounts: update it before "
                    + "it next starts — it would read this login as a Claude account and send its "
                    + "token to Anthropic",
            ],
            savedPlan: "ChatGPT Pro")
        var savedWithoutPathTcr = saved
        savedWithoutPathTcr.notices = Array(saved.notices.prefix(2))
        savedWithoutPathTcr.bundledTcrWithoutCLI = "/Applications/TcrBar.app/Contents/MacOS/tcr"
        let missing = LoginSheetFacts(request: .codex(account: nil), failureCode: "codex-missing")
        var refusedChoice = missing
        refusedChoice.refusedCodexChoice = "~/Downloads/codex is not an executable file."
        let missingReason =
            "could not find the codex CLI (searched TCR_CODEX_BIN, CODEX_BIN, PATH, "
            + "/Users/example/.local/bin, /Users/example/.cargo/bin, /opt/homebrew/bin, "
            + "/usr/local/bin, /Users/example/.npm-global/bin). Install it, or set TCR_CODEX_BIN "
            + "to its path. Nothing was written."
        return [
            ("20e-login-choose", .choosing, nil, LoginSheetFacts()),
            ("20f-login-codex-opening", .opening, nil, signIn),
            ("20g-login-codex-waiting", .waitingForBrowser(email: nil), authorize, signIn),
            ("20h-login-codex-waiting-no-url", .waitingForBrowser(email: nil), nil, signIn),
            ("20i-login-codex-saved", .saved(account: "bob@example.com"), nil, saved),
            (
                "20j-login-codex-refused", .failed(reason: routedRefusal), nil,
                LoginSheetFacts(request: .codex(account: nil), failureCode: "codex-routed")
            ),
            ("20k-login-codex-missing", .failed(reason: missingReason), nil, missing),
            (
                "20l-login-codex-identity",
                .failed(
                    reason:
                        "the ChatGPT login that came back is carol@example.com, not the one "
                        + "'erin@example.com/codex' holds. Sign out of chatgpt.com in the browser "
                        + "first, or use a private window, then retry "
                        + "`tcr login --codex --account erin@example.com/codex`. Nothing was written."),
                nil,
                LoginSheetFacts(
                    request: .codex(account: "erin@example.com/codex"), failureCode: "codex-identity")
            ),
            (
                "20m-codex-import-confirm", .confirmingImport(home: home, account: nil), nil,
                LoginSheetFacts(request: .codexImport(home: home, account: nil))
            ),
            (
                "20n-codex-import-saved", .saved(account: "bob@example.com"), nil,
                LoginSheetFacts(
                    request: .codexImport(home: home, account: nil), savedPlan: "ChatGPT Pro")
            ),
            (
                "20i-login-codex-saved-no-path-tcr", .saved(account: "bob@example.com"), nil,
                savedWithoutPathTcr
            ),
            ("20o-login-codex-choice-refused", .failed(reason: missingReason), nil, refusedChoice),
        ]
    }

    /// A panel drawn in a real window with its lists in their scroll views
    /// (``FleetView/snapshotKeepsScrollViews``), on one tab.
    ///
    /// Every other panel scene lays its lists out unclipped, so none of them
    /// can show where a list that overflows stops against the footer: the
    /// band between the last row and the footer existed only in the live
    /// popover. These scenes draw what the popover draws.
    ///
    /// `fixtures` names the scene whose per-scene fixtures (session files,
    /// process readings) it borrows. Each fleet's three tabs share one fleet
    /// and one `fixtures`, so the three PNGs differ by the selected tab alone
    /// and must come out the same height.
    struct LiveListScene {
        let name: String
        let fixtures: String
        let fleet: Fleet
        let tab: PanelTab
    }

    private static var liveListScenes: [LiveListScene] {
        let fleets: [(name: String, fixtures: String, fleet: Fleet)] = [
            // Thirteen accounts and lists longer than the space: the Accounts
            // tab is the tallest, and Sessions and Tools both overflow.
            ("long-lists", "28-live-list-long-lists", longListsFleet),
            // The live fleet's shape: two accounts, a short Sessions list (the
            // warm-ups fold), a Tools list that fits.
            ("real-fleet", "22-tools-tab-real-fleet", realShapeFleet),
        ]
        return fleets.flatMap { entry in
            [(PanelTab.accounts, "accounts"), (.sessions, "sessions"), (.tools, "tools")].map {
                LiveListScene(
                    name: "28-live-list-\(entry.name)-\($0.1)", fixtures: entry.fixtures,
                    fleet: entry.fleet, tab: $0.0)
            }
        }
    }

    /// Which tab a scene opens on — `.accounts` for every scene above scene
    /// 16, so this stays a lookup by name rather than a fifth tuple element
    /// every existing scene would have to grow.
    private static func initialTab(for sceneName: String) -> PanelTab {
        switch sceneName {
        case "12b-keeping-awake-sessions-tab", "16-sessions-tab", "18-sessions-tab-old-server",
            "18c-sessions-tab-old-tcr", "18e-sessions-tab-command-failed",
            "22b-sessions-tab-real-fleet", "16b-sessions-tab-mixed",
            "16c-sessions-tab-codex-only-empty", "16d-sessions-tab-old-server-codex",
            "27-one-height-sessions":
            return .sessions
        case "17-tools-tab", "17b-tools-tab-timeout-class-open", "18b-tools-tab-old-server",
            "18d-tools-tab-old-tcr", "22-tools-tab-real-fleet", "17c-tools-tab-codex",
            "27-one-height-tools", "17d-tools-tab-heat-manual", "17e-tools-tab-heat-fans-off",
            "17f-tools-tab-heat-no-fans", "17g-tools-tab-heat-worst-case",
            "17h-tools-tab-fan-install", "17i-tools-tab-fan-approval", "17j-tools-tab-fan-fixed",
            "17k-tools-tab-fan-follow-cpu", "17l-tools-tab-fan-another-app":
            return .tools
        case "27-one-height-peers":
            return .peers
        default: return .accounts
        }
    }

    /// The row whose hover card a scene shows (``FleetSectionRow/id``), or
    /// `nil` for none: a lookup by name for the reason ``initialTab(for:)`` is
    /// one.
    private static func hoverCardFixture(for sceneName: String) -> String? {
        switch sceneName {
        case "02b-mixed-thirteen-hover":
            return FleetSectionRow.id(group: .ungrouped, accountID: "near@example.com")
        case "02c-mixed-thirteen-hover-lower":
            return FleetSectionRow.id(group: .ungrouped, accountID: "spent-5@example.com")
        case "15c-parked-group-hover":
            return FleetSectionRow.id(group: .named("dev"), accountID: "carol@example.com")
        case "25h-codex-hover-card":
            return FleetSectionRow.id(
                group: .ungrouped, accountID: CanonicalCodexRow.erin.rawValue)
        default: return nil
        }
    }

    /// The accounts (``Account/id``) a scene keeps open. Every scene gets a
    /// set, empty for all but one, so the operator's own choice in
    /// `UserDefaults` never reaches a render.
    private static func keptOpenFixture(for sceneName: String) -> KeptOpenCards {
        switch sceneName {
        case "02e-mixed-thirteen-kept-open":
            return KeptOpenCards(["ok-2@example.com", "spent-2@example.com"])
        // The Codex card scenes keep their accounts' cards open, so each state
        // draws as the card it is.
        case "25-codex-cards":
            return KeptOpenCards([CanonicalCodexRow.bob, .carol, .dana, .gil].map(\.rawValue))
        case "25b-codex-cards-b":
            return KeptOpenCards([CanonicalCodexRow.hal, .erin, .jo, .kim].map(\.rawValue))
        case "25c-codex-cards-compact":
            return KeptOpenCards(CanonicalCodexRow.allCases.map(\.rawValue))
        case "25e-codex-unmeasured":
            return KeptOpenCards(["lee@example.com/codex"])
        default: return KeptOpenCards()
        }
    }

    /// The account (``Account/id``) whose strip segment a scene points at, or
    /// `nil` for none: a lookup by name for the reason ``initialTab(for:)`` is
    /// one.
    private static func litAccountFixture(for sceneName: String) -> String? {
        switch sceneName {
        case "02d-mixed-thirteen-segment-hover": return "spent-3@example.com"
        default: return nil
        }
    }

    /// The hover card's two promises, measured on the scene that pins one:
    /// the panel is the SAME size with the card as without it (the card is an
    /// overlay and must never grow the list, the `ffe8a86` rule), and the two
    /// pictures DIFFER (the card really drew). The second half is the positive
    /// control: a card that silently failed to draw would pass the first half
    /// on its own. Prints what failed and answers `false`, which fails the run.
    ///
    /// A pointed-at strip segment (unit H3) makes the same two promises: the
    /// ring and the lit line change no size, and they do draw.
    @MainActor
    private static func hoverCardKeepsPanelSize<A: View, B: View>(
        without: A, with: B, named name: String
    ) -> Bool {
        let plain = ImageRenderer(content: without)
        let hovered = ImageRenderer(content: with)
        plain.scale = 2
        hovered.scale = 2
        guard let a = plain.nsImage, let b = hovered.nsImage else {
            FileHandle.standardError.write(Data("hover check: \(name) did not render\n".utf8))
            return false
        }
        if a.size != b.size {
            FileHandle.standardError.write(
                Data(
                    ("hover check: \(name) is \(Int(b.size.width))x\(Int(b.size.height))pt "
                        + "with its card and \(Int(a.size.width))x\(Int(a.size.height))pt "
                        + "without: the card changed the panel's size\n").utf8))
            return false
        }
        if a.tiffRepresentation == b.tiffRepresentation {
            FileHandle.standardError.write(
                Data("hover check: \(name) drew the same picture with and without its card\n".utf8))
            return false
        }
        print("  hover check: \(name) \(Int(b.size.width))x\(Int(b.size.height))pt either way")
        return true
    }

    /// `SessionFile`s for ``sessionsFixture``'s three sessions, on the two
    /// scenes that render it — found in review (2026-09-12): without
    /// these, `FleetView`'s `snapshotMode` never reads a file for any
    /// session (its own doc-comment: "the harness's session ids are fixture
    /// strings that join to nothing real"), so every session read as
    /// `.unknown` → "idle" regardless of what `sessionsFixture`'s own
    /// comments say each one is doing. The three statuses here match that
    /// fixture's own narrative exactly: `aaaaaaaa` has a Bash call running
    /// now (busy), `bbbbbbbb`'s row comment says "waiting 12m" on the
    /// mockup this fixture is modelled on, `cccccccc` is the idle,
    /// unassigned control case.
    private static func sessionFilesFixture(for sceneName: String) -> [String: SessionFile] {
        // `realShapeFleet`'s two working sessions need files for the same
        // reason the mockup fixture's do: without one a session reads
        // `.unknown` and renders "idle", which on the real-shape scenes would
        // draw a tab where NOTHING is busy and quietly libel the fold. Its 18
        // warm-ups are deliberately given no file — a cache-warm stub really
        // does have no Claude Code session behind it, so `.unknown` is the
        // honest read there, not a gap in the fixture.
        if sceneName == "22-tools-tab-real-fleet" || sceneName == "22b-sessions-tab-real-fleet" {
            return [
                "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa": SessionFile(
                    sessionId: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                    cwd: "/Users/alice/git/demo", name: "demo-a1", status: "busy"),
                "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb": SessionFile(
                    sessionId: "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
                    cwd: "/Users/bob/git/demo-ui", name: "demo-ui-b2", status: "busy"),
            ]
        }
        // The canonical sessions' files: alice's Claude Code session file, and
        // the file the join makes for bob from line 1 of his codex rollout,
        // which carries his folder and nothing else. The old-server scene has
        // no Codex session to join.
        let alice = SessionFile(
            sessionId: "a1f3c2d4-0000-4000-8000-000000000001",
            cwd: "/Users/example/git/tcr", name: "tcr-c7", status: "busy")
        let bob = SessionFile(
            sessionId: "019a3c5e-0000-7000-8000-000000000002",
            cwd: "/Users/example/git/tcr")
        switch sceneName {
        case "16b-sessions-tab-mixed", "17c-tools-tab-codex":
            return [alice.sessionId: alice, bob.sessionId: bob]
        case "16d-sessions-tab-old-server-codex":
            return [alice.sessionId: alice]
        default: break
        }
        guard
            sceneName == "16-sessions-tab" || sceneName == "17-tools-tab"
                || sceneName == "17b-tools-tab-timeout-class-open"
        else { return [:] }
        return [
            "aaaaaaaa-1111-2222-3333-444444444444": SessionFile(
                sessionId: "aaaaaaaa-1111-2222-3333-444444444444",
                cwd: "/Users/alice/git/tcr", name: "tcr-c7",
                status: "busy"),
            "bbbbbbbb-1111-2222-3333-444444444444": SessionFile(
                sessionId: "bbbbbbbb-1111-2222-3333-444444444444",
                cwd: "/Users/alice/git/orchard", name: "orchard-c2",
                status: "waiting"),
            "cccccccc-1111-2222-3333-444444444444": SessionFile(
                sessionId: "cccccccc-1111-2222-3333-444444444444",
                cwd: "/Users/alice/git/orchard coder", name: "m-075377",
                status: "idle"),
            "dddddddd-1111-2222-3333-444444444444": SessionFile(
                sessionId: "dddddddd-1111-2222-3333-444444444444",
                cwd: "/Users/bob/git/toolkit", name: "toolkit-c1",
                status: "busy"),
            "eeeeeeee-1111-2222-3333-444444444444": SessionFile(
                sessionId: "eeeeeeee-1111-2222-3333-444444444444",
                cwd: "/Users/bob/git/token", name: "token-b4",
                status: "idle"),
        ]
    }

    @MainActor
    static func run(into directory: URL, only filter: SceneFilter = .all) -> Never {
        if filter == .missingValue {
            FileHandle.standardError.write(
                Data("\(onlyFlag) needs a substring of a scene name\n".utf8))
            exit(1)
        }
        // A Codex row's note shortens its codex home against the home folder, and
        // the fixtures' homes are `/Users/example`: so every scene reads the same
        // on every Mac, and none names this one's user.
        HomePath.current = "/Users/example"
        do {
            try FileManager.default.createDirectory(
                at: directory, withIntermediateDirectories: true)
        } catch {
            FileHandle.standardError.write(
                Data("cannot create \(directory.path): \(error)\n".utf8))
            exit(1)
        }

        var written = 0
        var attempted = 0
        var matched = 0
        for scene in scenes where filter.admits(scene.name) {
            matched += 1
            for appearance in Appearance.allCases {
                attempted += 1
                if render(scene, appearance: appearance, density: .auto, into: directory) {
                    written += 1
                }
            }
            // The parity fixture also renders at `.comfortable`, suffixed —
            // see `densityVariantScenes` — so the mockup comparison stays
            // possible at both densities `PanelDensityPreference` offers,
            // not only the shipped default.
            //
            // The main pass above is `.auto`, the shipped default since Gil's
            // "make compact the default please above 4 accounts": forcing
            // `.compact` there would have made every PNG in this set a
            // picture of a setting nobody has, which is the one thing a
            // review harness may not be.
            if densityVariantScenes.contains(scene.name) {
                for appearance in Appearance.allCases {
                    attempted += 1
                    if render(
                        scene, appearance: appearance, density: .comfortable, into: directory)
                    {
                        written += 1
                    }
                }
            }
        }
        for live in liveListScenes where filter.admits(live.name) {
            matched += 1
            let scene = (
                name: live.fixtures, state: PollState.loaded(live.fleet), awake: false,
                control: String?.none
            )
            for appearance in Appearance.allCases {
                attempted += 1
                if render(scene, appearance: appearance, density: .auto, live: live, into: directory) {
                    written += 1
                }
            }
        }
        for scene in sheetScenes where filter.admits(scene.name) {
            matched += 1
            for appearance in Appearance.allCases {
                attempted += 1
                if renderSheet(scene, appearance: appearance, into: directory) { written += 1 }
            }
        }
        // The Codex sheets, the same way, each with what it says beyond its phase.
        for scene in codexSheetScenes where filter.admits(scene.name) {
            matched += 1
            for appearance in Appearance.allCases {
                attempted += 1
                if renderSheet(
                    (scene.name, scene.phase, scene.url), appearance: appearance, into: directory,
                    facts: scene.facts)
                {
                    written += 1
                }
            }
        }
        for scene in peerSceneList where filter.admits(scene.name) {
            matched += 1
            for appearance in Appearance.allCases {
                attempted += 1
                if renderPeer(scene, appearance: appearance, into: directory) { written += 1 }
            }
        }
        // The same peers, with a Codex account in the fleet.
        for scene in mixedPeerScenes where filter.admits(scene.name) {
            matched += 1
            for appearance in Appearance.allCases {
                attempted += 1
                if renderPeer(scene, appearance: appearance, holdsCodex: true, into: directory) {
                    written += 1
                }
            }
        }
        for scene in peerSheetScenes where filter.admits(scene.name) {
            matched += 1
            for appearance in Appearance.allCases {
                attempted += 1
                if renderPeerSheet(scene, appearance: appearance, into: directory) { written += 1 }
            }
        }
        // Two of those sheets again, with a Codex account in the fleet.
        for scene in mixedPeerSheetScenes where filter.admits(scene.name) {
            matched += 1
            for appearance in Appearance.allCases {
                attempted += 1
                if renderPeerSheet(
                    scene, appearance: appearance, holdsCodex: true, into: directory)
                {
                    written += 1
                }
            }
        }
        for scene in peerMovedSheetScenes where filter.admits(scene.name) {
            matched += 1
            for appearance in Appearance.allCases {
                attempted += 1
                if renderPeerMovedSheet(scene, appearance: appearance, into: directory) {
                    written += 1
                }
            }
        }
        for scene in peerInviteSheetScenes where filter.admits(scene.name) {
            matched += 1
            for appearance in Appearance.allCases {
                attempted += 1
                if renderPeerInviteSheet(scene, appearance: appearance, into: directory) {
                    written += 1
                }
            }
        }
        for scene in peerSealedSheetScenes where filter.admits(scene.name) {
            matched += 1
            for appearance in Appearance.allCases {
                attempted += 1
                if renderPeerSealedSheet(scene, appearance: appearance, into: directory) {
                    written += 1
                }
            }
        }
        for scene in controlScenes where filter.admits(scene.name) {
            matched += 1
            for appearance in Appearance.allCases {
                attempted += 1
                if renderControl(scene, appearance: appearance, into: directory) { written += 1 }
            }
        }
        for scene in fanPopoverScenes where filter.admits(scene.name) {
            matched += 1
            for appearance in Appearance.allCases {
                attempted += 1
                if renderFanPopover(scene, appearance: appearance, into: directory) { written += 1 }
            }
        }
        for scene in tabStripScenes where filter.admits(scene.name) {
            matched += 1
            for appearance in Appearance.allCases {
                attempted += 1
                if renderTabStrip(scene, appearance: appearance, into: directory) { written += 1 }
            }
        }

        if case .matching(let part) = filter {
            print("\n\(onlyFlag) \(part): \(matched) scenes matched")
            if matched == 0 {
                FileHandle.standardError.write(
                    Data("\(onlyFlag) \(part) matched no scene: nothing rendered\n".utf8))
                exit(1)
            }
        }
        print("\nrendered \(written)/\(attempted) images into \(directory.path)")
        exit(written == attempted ? 0 : 1)
    }

    /// Both appearances, because a token that only exists in one of them is how a
    /// light-mode palette ships broken. The first run of this harness rendered
    /// light-mode by default and I would not otherwise have seen it.
    enum Appearance: String, CaseIterable {
        case dark, light

        var nsAppearance: NSAppearance? {
            NSAppearance(named: self == .dark ? .darkAqua : .aqua)
        }
    }

    /// Scenes rendered twice — once at `V4`'s shipped `.auto` default, once
    /// forced to `.comfortable` — so the one fixture compared against
    /// the mockup crop stays comparable at both densities, not only the one
    /// now shipping. Every
    /// other scene renders `.compact` alone: this harness is a review
    /// artifact, and doubling all 24 scenes would be 24 extra PNGs nobody
    /// asked to review.
    private static let densityVariantScenes: Set<String> = [
        "19-accounts-tab-parity",
        // The `CONTROL` pill is new chrome on the card header. `.comfortable`
        // is the density the shipped default and `.auto` can both resolve
        // to, so rendering both against this one card shows whether the
        // pill still fits beside `ROTATING` and the state pill rather than
        // taking the fit on faith from a single default render.
        "13-control-account",
    ]

    @MainActor
    private static func render(
        _ scene: (name: String, state: PollState, awake: Bool, control: String?),
        appearance: Appearance,
        density: PanelDensity,
        live: LiveListScene? = nil,
        into directory: URL
    ) -> Bool {
        // The appearance has to be current for the duration of the rasterisation:
        // every token resolves through NSColor's dynamic provider, which reads the
        // CURRENT appearance, not one baked into the view.
        return withDrawingAppearance(appearance.nsAppearance) {
            renderUnderCurrentAppearance(
                scene, appearance: appearance, density: density, live: live, into: directory)
        }
    }

    /// The body of ``render(_:appearance:density:into:)``, run with the drawing
    /// appearance already installed.
    ///
    /// A separate function only because the macOS 12 replacement for assigning
    /// `NSAppearance.current` takes a block (``withDrawingAppearance(_:perform:)``):
    /// wrapping ninety lines in a closure would have re-indented the whole
    /// function to change nothing.
    @MainActor
    private static func renderUnderCurrentAppearance(
        _ scene: (name: String, state: PollState, awake: Bool, control: String?),
        appearance: Appearance,
        density: PanelDensity,
        live: LiveListScene? = nil,
        into directory: URL
    ) -> Bool {
        // `V4.compact` reads this key straight out of `UserDefaults`
        // (`PanelDensityPreference.current()`), so forcing a density for one
        // render is a write-then-restore around this call, the same pattern
        // `expandedGroupsKey` already uses below for scene 15b. Removed
        // rather than restored to a prior value: nothing in this process
        // should be running under a real density preference already set, and
        // "absent" is `PanelDensityPreference`'s own definition of the
        // shipped default.
        UserDefaults.standard.set(density.rawValue, forKey: PanelDensityPreference.key)
        defer { UserDefaults.standard.removeObject(forKey: PanelDensityPreference.key) }

        // `.harness()`, never a real controller: drawing a checkbox in its ON
        // state must not actually stop this machine sleeping. A harness with a
        // side effect on the operator's power settings would be a worse bug than
        // anything it could catch.
        //
        // That sentence used to be satisfied by `activity: .inert` alone, and
        // stopped being once the control started REMEMBERING its state: the
        // `setOn` below would have written `keepThisMacAwake` into the
        // operator's own defaults — true on scene 12, false on the next one —
        // so a render would silently disarm a setting they had turned on.
        // `.harness()` is inert on both halves.
        let awake = AwakeController.harness()
        awake.setOn(scene.awake)

        let updater = Updater(startingUpdater: false)
        if let state = updateState(for: scene.name) {
            updater.setUpdateStateForPreview(state)
        }

        // `expandedGroups` reads `UserDefaults.standard` at construction
        // — real for the shipping app, but this harness only ever runs under
        // `TCRBAR_DEV_BUILD=1`'s OWN bundle id (`build-tcrbar.sh`'s own
        // comment: "gives a non-shipping build its own identity"), a
        // `UserDefaults` domain the shipping app never reads or writes.
        // Seeded here, for exactly one scene, rather than threading a new
        // constructor parameter through `FleetView` for a harness-only need:
        // every OTHER scene explicitly clears the key first, so construction
        // order across scenes cannot leak one render's expansion into the
        // next.
        if scene.name == "15b-parked-group-expanded" {
            UserDefaults.standard.set(
                ["g:henry-team"], forKey: FleetView.expandedGroupsKey)
        } else {
            UserDefaults.standard.removeObject(forKey: FleetView.expandedGroupsKey)
        }

        // Computed once, so the panel drawn with a pin and without it carry the
        // same seeds.
        let seeds = codexSeeds(for: scene.name)

        // A closure over the hover pin, so a scene that pins a hover card can
        // also be drawn without it and the two compared
        // (``hoverCardKeepsPanelSize(without:with:named:)``).
        let panel = {
            (
                hoverCard: String?, litAccount: String?, routeController: ClaudeRouteController,
                bannerSnapshot: CodexBannerSnapshot?
            ) in
            FleetView(
                poller: StatusPoller(pinnedState: scene.state, lastPollAt: recentPoll),
                // A scene in `supervisedScenes` has its server pinned to the
                // supervised state. Otherwise the app draws "Start server",
                // "Take over port…" and "Not supervised by TcrBar", which
                // depend on what is running on the machine doing the render,
                // not on the layout. Pinned, never spawned:
                // `ServerController.harness(pinned:)` signals nothing.
                server: supervisedScenes.contains(scene.name)
                    ? ServerController.harness(pinned: .supervising(pid: 4242))
                    : ServerController(),
                loginItem: LoginItem(),
                accounts: AccountController(),
                control: ControlAccountController(pinned: scene.control),
                awake: awake,
                // `startingUpdater: false`: this process was asked for PNGs. A
                // started updater schedules background checks and can put a
                // window on screen, neither of which belongs in a render run.
                // `updateState(for:)` above sets ``Updater/updateState``
                // directly for the two scenes that need it.
                updater: updater,
                groupController: GroupController(),
                removeController: removeControllerFixture(for: scene.name),
                routeController: routeController,
                startServerAtLaunch: .constant(false),
                // Render mode never looks at this Mac's codex: `.absent`, except in
                // the one scene that names a presence of its own.
                codexPresence: seeds.presence,
                // Empty, except in the two scenes that draw a note under a line,
                // and opened on the scene's fleet, as the shell opens the panel,
                // so a signed-out row's reason is read.
                codexNotes: CodexNoteStore(
                    notes: codexNotesFixture(for: scene.name, opening: scene.state.loadedFleet)),
                codexBannerSnapshot: bannerSnapshot,
                snapshotMode: true,
                snapshotKeepsScrollViews: live != nil,
                initialTab: live?.tab ?? initialTab(for: scene.name),
                // A heat scene is scene 17 in every fixture but the machine
                // reading, so the two differ by the numbers line alone.
                initialSessionFiles: sessionFilesFixture(
                    for: heatScenes[scene.name] == nil ? scene.name : "17-tools-tab"),
                initialMachineStats: heatScenes[scene.name] ?? machineFixture,
                initialRunningProcesses: runningProcessesFixture(
                    for: heatScenes[scene.name] == nil ? scene.name : "17-tools-tab",
                    state: scene.state),
                initialExpandedTimeoutClasses: expandedTimeoutClassesFixture(for: scene.name),
                initialZeroRequestsSince: claudeBannerScenes.contains(scene.name)
                    ? noRequestsBannerZeroSince : nil,
                initialClaudeCount: claudeBannerScenes.contains(scene.name) ? 1 : nil,
                initialClaudeRoute: claudeBannerScenes.contains(scene.name)
                    ? noRequestsBannerRoute : nil,
                initialCodexCount: seeds.codexCount,
                initialCodexZeroRequestsSince: seeds.codexZeroSince,
                initialPeerSharing: seeds.peerSharing,
                initialHomeDirectory: seeds.home,
                initialBundledTcrWithoutCLI: seeds.bundledTcr,
                initialHoverCard: hoverCard,
                initialLitAccount: litAccount,
                initialKeptOpen: keptOpenFixture(for: scene.name)
            )
            .environment(\.colorScheme, appearance == .dark ? .dark : .light)
            // A FIXED height, not the measured one.
            //
            // The panel sizes itself from a GeometryReader preference, which needs
            // a second layout pass that ImageRenderer does not perform — the first
            // version of this harness produced eleven blank images because
            // `rowsHeight` was still 0 when the bitmap was taken. Proposing a
            // concrete size renders the real content; the scroll area is simply
            // shown at full height instead of clipped.
            .fixedSize()
        }
        let hoverCard = hoverCardFixture(for: scene.name)
        let litAccount = litAccountFixture(for: scene.name)
        let view = panel(hoverCard, litAccount, claudeRouteFixture(for: scene.name), nil)

        if let live {
            return rasteriseInWindow(
                view, appearance: appearance,
                named: "\(live.name)-\(density.rawValue)-\(appearance.rawValue).png",
                into: directory)
        }

        // The main pass is `.auto`, the shipped default, so its files are
        // `<scene>-auto-<appearance>.png`; the forced `.comfortable` variant's
        // are `<scene>-comfortable-<appearance>.png`, per
        // `densityVariantScenes`. Only a `.compact` render, which no pass asks
        // for, would be unsuffixed. Computed here rather than in `rasterise`,
        // which is shared with the sheet scenes and has no density of its own.
        let name =
            density == .compact
            ? "\(scene.name)-\(appearance.rawValue).png"
            : "\(scene.name)-\(density.rawValue)-\(appearance.rawValue).png"
        if hoverCard != nil || litAccount != nil,
            !hoverCardKeepsPanelSize(
                without: panel(nil, nil, claudeRouteFixture(for: scene.name), nil), with: view,
                named: name)
        {
            return false
        }
        if scene.name == "25b-claude-route-answered",
            !claudeRoutePromptKeepsPanelSize(
                before: panel(nil, nil, claudeRouteFixture(for: "25-claude-route-prompt"), nil),
                after: view, named: name)
        {
            return false
        }
        if scene.name == "26b-codex-refusals-banner" || scene.name == "16d-sessions-tab-old-server-codex" {
            let snapshot = CodexBannerSnapshot()
            guard
                codexBannerKeepsPanelSize(
                    snapshot: snapshot,
                    panel: panel(nil, nil, claudeRouteFixture(for: scene.name), snapshot),
                    sessions: scene.name == "16d-sessions-tab-old-server-codex")
            else { return false }
        }
        return rasterise(view, named: name, into: directory)
    }

    /// Reuse one hosting view through polling and reopening, as the real popover does.
    /// A new opening must change its height: the positive control rules out a host
    /// that stopped observing the banner altogether.
    @MainActor
    private static func codexBannerKeepsPanelSize<Panel: View>(
        snapshot: CodexBannerSnapshot, panel: Panel, sessions: Bool = false
    ) -> Bool {
        snapshot.observe(nil)
        snapshot.observeSessionsLine(nil)
        snapshot.decideAtOpen()
        let host = NSHostingView(rootView: panel)
        func size() -> NSSize {
            CFRunLoopRunInMode(.defaultMode, 0.02, false)
            host.layoutSubtreeIfNeeded()
            return host.fittingSize
        }
        let closed = size()
        let longNotice = String(repeating: "An example route is unavailable. ", count: 8)
        if sessions {
            snapshot.observeSessionsLine(longNotice)
        } else {
            snapshot.observe(.notPooled(reason: longNotice))
        }
        let polled = size()
        guard closed == polled else {
            FileHandle.standardError.write(
                Data("Codex banner check: polling changed the open panel's size\n".utf8))
            return false
        }
        snapshot.decideAtOpen()
        let reopened = size()
        guard reopened.height > closed.height else {
            FileHandle.standardError.write(
                Data("Codex banner check: reopening did not show the new banner\n".utf8))
            return false
        }
        if sessions { snapshot.observeSessionsLine(nil) } else { snapshot.observe(nil) }
        guard size() == reopened else {
            FileHandle.standardError.write(Data("Codex banner check: polling removed the open banner\n".utf8))
            return false
        }
        print("  Codex banner check: one persistent host stays fixed until reopened")
        return true
    }

    /// One state of the sign-in sheet, rendered on its own.
    ///
    /// Same appearance dance as ``render(_:appearance:into:)`` and the same
    /// writer, and deliberately NO controller of any kind: a ``LoginSession``
    /// would spawn `tcr login`. The harness draws a phase, never a login.
    ///
    /// `facts` is everything a Codex sheet says beyond its phase, empty for the Claude
    /// sheets, and `displayHome` the home a codex folder is shortened against: always
    /// `/Users/example`, never the rendering Mac's.
    @MainActor
    private static func renderSheet(
        _ scene: (name: String, phase: LoginPhase, url: URL?),
        appearance: Appearance,
        into directory: URL,
        facts: LoginSheetFacts = LoginSheetFacts(),
        displayHome: String = "/Users/example"
    ) -> Bool {
        return withDrawingAppearance(appearance.nsAppearance) {
            renderSheetUnderCurrentAppearance(
                scene, appearance: appearance, into: directory, facts: facts,
                displayHome: displayHome)
        }
    }

    /// The body of ``renderSheet(_:appearance:into:)``, run with the drawing
    /// appearance already installed — the same split, for the same reason, as
    /// ``renderUnderCurrentAppearance(_:appearance:density:into:)``.
    @MainActor
    private static func renderSheetUnderCurrentAppearance(
        _ scene: (name: String, phase: LoginPhase, url: URL?),
        appearance: Appearance,
        into directory: URL,
        facts: LoginSheetFacts,
        displayHome: String
    ) -> Bool {
        // The panel underneath is the ordinary healthy Accounts tab, built the
        // same way every other scene builds one — pinned state, harness
        // controllers, nothing that can spawn or signal anything.
        UserDefaults.standard.removeObject(forKey: FleetView.expandedGroupsKey)
        let panel =
            FleetView(
                poller: StatusPoller(
                    pinnedState: .loaded(fleet(healthyJSON)), lastPollAt: recentPoll),
                server: ServerController.harness(pinned: .supervising(pid: 4242)),
                loginItem: LoginItem(),
                accounts: AccountController(),
                control: ControlAccountController(pinned: nil),
                awake: AwakeController.harness(),
                updater: Updater(startingUpdater: false),
                groupController: GroupController(),
                removeController: RemoveAccountController(),
                routeController: ClaudeRouteController(pinned: nil),
                startServerAtLaunch: .constant(false),
                codexPresence: .absent,
                snapshotMode: true,
                initialTab: .accounts,
                initialSessionFiles: [:],
                initialKeptOpen: KeptOpenCards()
            )
            .environment(\.colorScheme, appearance == .dark ? .dark : .light)
            .fixedSize()

        let view =
            panel
            .overlay {
                ZStack {
                    Color.black.opacity(sheetScrimAlpha)
                    // The rounded fill goes UNDER the sheet rather than
                    // clipping it: `.clipShape` + `.shadow` rasterises through
                    // an offscreen layer whose backing showed as white corners
                    // around the light-mode sheet, which is not a surface this
                    // app has.
                    LoginSheet(
                        phase: scene.phase, authorizeURL: scene.url,
                        snapshotMode: true, facts: facts, displayHome: displayHome
                    )
                    .background(RoundedRectangle(cornerRadius: V4.cardRadius).fill(Tok.panel))
                    .shadow(radius: sheetShadowRadius)
                }
            }
            .environment(\.colorScheme, appearance == .dark ? .dark : .light)
        return rasterise(view, named: "\(scene.name)-\(appearance.rawValue).png", into: directory)
    }

    /// Harness-only geometry for the sheet composition above: how far the
    /// panel behind a sheet is dimmed, and the sheet's drop shadow. Not design
    /// tokens and not in `V4.swift` — AppKit owns both for a real
    /// presentation, and `V4.swift` holds transcribed mockup values only.
    /// These exist so the fixture reads like the thing it is picturing.
    private static let sheetScrimAlpha: Double = 0.45
    private static let sheetShadowRadius: CGFloat = 12

    // MARK: - The Peers tab

    /// The Peers tab's states, scenes 45 to 51 plus the unsupported collapse
    ///, the Peers tab mockup (kept outside the tree), one scene per state, in
    /// its order.
    ///
    /// A THIRD scene array, because peers are a sibling document
    /// (`tcr peer ls --json`), not a field of
    /// ``PollState``, so a peer state cannot be expressed as one of `scenes`'s
    /// tuples at all. Same shape as ``sheetScenes`` and for the same class of
    /// reason.
    ///
    /// Every fixture below is a ``PeersSnapshot`` built through
    /// ``PeersSnapshotBuilder`` from a ``PeerListDocument``, the real decode
    /// path, with a pinned `now`, so a scene cannot show a sentence the
    /// running panel would not produce from the same JSON. And the controller
    /// is ``PeerController/pinned(_:)``: no subprocess, no listener, no proxy,
    /// nothing that could reach `127.0.0.1:3456`.
    /// Every scene, plus the refusal banner each one draws (almost always
    /// none).
    private static var peerSceneList:
        [(
            name: String, snapshot: PeersSnapshot, dry: Bool, refusal: PeerRefusal,
            detailsOpen: Bool
        )]
    {
        peerStates.map { ($0.name, $0.snapshot, $0.dry, PeerRefusal(), false) } + [
            // A verb this panel ran was refused, and the tab is STILL THERE.
            //
            // It had no fixture, which is how the opposite shipped: a refusal
            // used to be drawn instead of the whole tab and then wiped by the
            // next poll about three seconds later. The message is the one a
            // mismatched pairing produces, because that is the longest
            // refusal on this path and the one most likely to be cut.
            (
                "63-peers-refused",
                peersSnapshot(
                    PeerListDocument(
                        finding: true,
                        peers: [
                            .init(
                                name: "studio-mac", address: "studio-mac.local:7749",
                                lastSeenMs: peerMsAgo(2))
                        ])),
                false,
                PeerRefusal(
                    message: "tcr peer share on failed (exit 1): peer share: refused, no Mac "
                        + "is trusted yet, so there is nobody to share with"),
                false
            ),
            // The same refusal with the VERB carried alongside it, which is
            // what the banner needs to lead with the act: `Sharing was
            // refused` rather than `That was refused`, which names none of the
            // dozen controls on this tab.
            //
            // The scene above keeps the verb off deliberately: it is the older
            // shape, a refusal this build has no word for, and the two
            // headlines have to be comparable side by side.
            (
                "65-peers-refused-verb",
                peersSnapshot(
                    PeerListDocument(
                        finding: true,
                        peers: [
                            .init(
                                name: "studio-mac", address: "studio-mac.local:7749",
                                lastSeenMs: peerMsAgo(2))
                        ])),
                false,
                PeerRefusal(
                    message: "tcr peer share on failed (exit 1): peer share: refused, no Mac "
                        + "is trusted yet, so there is nobody to share with",
                    verb: ["peer", "share", "on"]),
                false
            ),
            // And with Details pressed, which is the whole point of putting
            // the raw line behind a button: the sentence leads, the command
            // line and the exit code are one press away, and both halves need
            // a picture or the press is a claim about a state nobody has seen.
            (
                "66-peers-refused-verb-details",
                peersSnapshot(
                    PeerListDocument(
                        finding: true,
                        peers: [
                            .init(
                                name: "studio-mac", address: "studio-mac.local:7749",
                                lastSeenMs: peerMsAgo(2))
                        ])),
                false,
                PeerRefusal(
                    message: "tcr peer share on failed (exit 1): peer share: refused, no Mac "
                        + "is trusted yet, so there is nobody to share with",
                    verb: ["peer", "share", "on"]),
                true
            ),
        ]
    }

    private static var peerStates: [(name: String, snapshot: PeersSnapshot, dry: Bool)] {
        [
            // 1. A fresh install: finding off, and the only state a one-Mac
            //    network ever has.
            ("45-peers-off", peersSnapshot(PeerListDocument()), false),
            // 2. Looking, and nothing found. A named state rather than a
            //    spinner over an empty list, because "there may be nothing to
            //    find" is the honest answer and `finding` is its own field.
            ("46-peers-finding", peersSnapshot(PeerListDocument(finding: true)), false),
            // 3. Two found, neither trusted, the second with its name NOT
            //    announced (decision row 9), so this side has an address and
            //    no name and the row is drawn rather than hidden.
            ("47-peers-found-untrusted", peersSnapshot(foundUntrustedPeers), false),
            // 3c. The same two, seen over the tailnet: a `tailnet` pill in the
            //     slot every found row reserves.
            ("47c-peers-found-tailnet", peersSnapshot(foundOverTailnetPeers), false),
            // 4. The safe steady state, and the one most operators stay in:
            //    one Mac trusted and carrying, sharing still off.
            ("48-peers-trusted-share-off", peersSnapshot(trustedShareOffPeers), false),
            // 5. The one screen where plaintext crosses a machine boundary, so
            //    the one screen that uses the reserved hue: sharing on, with
            //    the meter that says how much. The second row reads `nothing
            //    yet` rather than a blank, zero spent is a measurement.
            ("49-peers-share-on", peersSnapshot(shareOnPeers), false),
            // 6. The other direction, and the reason the mesh exists: this
            //    Mac's own accounts are dry (`dry: true` puts the header's
            //    summary line in its own honest state) and studio-mac is
            //    answering. The egress line is the first thing an operator
            //    reads, because it explains why work is still moving.
            (
                "50-peers-borrowing",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-4b8we1r0zp", name: "studio-mac",
                                address: "studio-mac.local:7749", trusted: true,
                                lastSeenMs: peerMsAgo(1), carries: true, serves: true,
                                inFlight: 2, leaseSpent: 0.62, leaseTtlSeconds: 180),
                            .init(
                                id: "tcr-92hbq5t7yv", name: "attic-nuc",
                                address: "attic-nuc.local:7749", trusted: true,
                                lastSeenMs: peerMsAgo(4), carries: true, serves: true,
                                noHeadroom: true),
                        ],
                        answeringOn: .init(peer: "studio-mac", inFlight: 2))),
                true
            ),
            // 7. Sharing on and the peer asleep. The meter reads zero rather
            //    than the 34% it read six minutes ago (the mockup's rule 5),
            //    and the row still says the offer stands, two fields, because
            //    one could not do both.
            (
                "51-peers-asleep",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-4b8we1r0zp", name: "studio-mac",
                                address: "studio-mac.local:7749", trusted: true,
                                lastSeenMs: peerMsAgo(360), carries: true, serves: true,
                                leaseSpent: 0.34, leaseTtlSeconds: 0)
                        ])),
                false
            ),
            // 8. A Mac asking to pair, which is decision row 10's own first
            //    screen and had no fixture at all: the knock card is drawn by
            //    `PeersTabV4`, the pane fixture carries a `pending` row, and
            //    the TAB never rendered one. A card nobody renders is a card
            //    nobody reviews, and this one carries three controls and the
            //    only sentence on the tab about what Accept does.
            //
            //    The address is private-range and the instance id is
            //    obviously fake: this repository is public and these PNGs are
            //    review artifacts.
            (
                "52-peers-knock",
                peersSnapshot(
                    PeerListDocument(
                        finding: true,
                        peers: [
                            .init(
                                name: "studio-mac", address: "studio-mac.local:7749",
                                lastSeenMs: peerMsAgo(2))
                        ],
                        pending: [
                            .init(
                                addr: "10.0.1.24", instanceId: "8f2c1ad63b0e4471",
                                proposedName: "loft-mini", wireVersion: 1,
                                firstSeenMs: peerMsAgo(30), lastSeenMs: peerMsAgo(4))
                        ])),
                false
            ),
            // 8a. The same tab, same found Mac, with NOBODY asking. The twin
            //     of the scene above and the only way to read what a knock
            //     costs the panel: the two PNGs differ by one card, so the
            //     difference in their heights is that card, measured rather
            //     than asserted off the arithmetic that charges it.
            (
                "67-peers-knock-none",
                peersSnapshot(
                    PeerListDocument(
                        finding: true,
                        peers: [
                            .init(
                                name: "studio-mac", address: "studio-mac.local:7749",
                                lastSeenMs: peerMsAgo(2))
                        ])),
                false
            ),
            // 8b. Two Macs asking at once, which is where a colour can stop
            //     meaning anything: two amber cards in a column, each with its
            //     own address, its own deadline and its own pair of answers.
            (
                "68-peers-knock-two",
                peersSnapshot(
                    PeerListDocument(
                        finding: true,
                        peers: [
                            .init(
                                name: "studio-mac", address: "studio-mac.local:7749",
                                lastSeenMs: peerMsAgo(2))
                        ],
                        pending: [
                            .init(
                                addr: "10.0.1.24", instanceId: "8f2c1ad63b0e4471",
                                proposedName: "loft-mini", wireVersion: 1,
                                firstSeenMs: peerMsAgo(30), lastSeenMs: peerMsAgo(4)),
                            .init(
                                addr: "10.0.1.31", instanceId: "3b1d90c47ae25f68",
                                proposedName: "attic-nuc", wireVersion: 1,
                                firstSeenMs: peerMsAgo(340), lastSeenMs: peerMsAgo(8)),
                        ])),
                false
            ),
            // 8c. A knock that proposed no name at all, which is what a Mac
            //     with `announceName` off looks like from this side: the
            //     headline is already the address, so the line under it says
            //     what is missing rather than saying the address twice.
            (
                "69-peers-knock-no-name",
                peersSnapshot(
                    PeerListDocument(
                        finding: true,
                        peers: [
                            .init(
                                name: "studio-mac", address: "studio-mac.local:7749",
                                lastSeenMs: peerMsAgo(2))
                        ],
                        pending: [
                            .init(
                                addr: "10.0.1.31", instanceId: "3b1d90c47ae25f68",
                                wireVersion: 1, firstSeenMs: peerMsAgo(45),
                                lastSeenMs: peerMsAgo(5))
                        ])),
                false
            ),
            // 8d. A knock whose sender stamped no first-seen time: NO pill at
            //     all, never a zero and never an invented ten minutes. The
            //     nil arm of the pill helper is the one a reader is most
            //     likely to assume was filled with a number.
            (
                "70-peers-knock-no-expiry",
                peersSnapshot(
                    PeerListDocument(
                        finding: true,
                        peers: [
                            .init(
                                name: "studio-mac", address: "studio-mac.local:7749",
                                lastSeenMs: peerMsAgo(2))
                        ],
                        pending: [
                            .init(
                                addr: "10.0.1.24", instanceId: "8f2c1ad63b0e4471",
                                proposedName: "loft-mini", wireVersion: 1,
                                firstSeenMs: 0, lastSeenMs: peerMsAgo(4))
                        ])),
                false
            ),
            // 9. Trust pressed, and the other operator has not answered. The
            //    row state, built through the same overlay the
            //    live controller applies, so the fixture cannot show a row
            //    the panel would not draw.
            (
                "53-peers-waiting",
                PeersSnapshotBuilder.waiting(
                    peersSnapshot(
                        PeerListDocument(
                            finding: true,
                            peers: [
                                .init(
                                    name: "studio-mac", address: "studio-mac.local:7749",
                                    lastSeenMs: peerMsAgo(2)),
                                .init(
                                    name: "attic-nuc", address: "attic-nuc.local:7749",
                                    lastSeenMs: peerMsAgo(9)),
                            ])),
                    knocked: ["studio-mac.local:7749"]),
                false
            ),
            // 10. A lease that ENDED, on both sides of it: the Mac this one
            //     lends to (Re-lend, and the row greys) and the Mac that was
            //     serving this one (no button, because re-lending is its
            //     operator's act). Decision row 13 keeps both rows rather
            //     than deleting them.
            (
                "54-peers-lease-ended",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-92hbq5t7yv", name: "attic-nuc",
                                address: "attic-nuc.local:7749", trusted: true,
                                lastSeenMs: peerMsAgo(6),
                                lend: [
                                    .init(
                                        leaseId: "ls-2e77", scope: .group("work"),
                                        window: .week, fraction: 0.20,
                                        until: Int64(peerNow.timeIntervalSince1970) - 1800,
                                        ended: true)
                                ]),
                            .init(
                                id: "tcr-4b8we1r0zp", name: "studio-mac",
                                address: "studio-mac.local:7749", trusted: true,
                                lastSeenMs: peerMsAgo(3), carries: true, serves: true,
                                leaseSpent: 0.34, leaseTtlSeconds: 240,
                                until: Int64(peerNow.timeIntervalSince1970) - 600,
                                ended: true),
                        ])),
                false
            ),
            // 11. The same lease still RUNNING, which is the row decision
            //     row 13's "ends in 1 h" is about: without a fixture the
            //     clause could ship saying nothing and every gate would pass.
            (
                "55-peers-lease-ends-soon",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-4b8we1r0zp", name: "studio-mac",
                                address: "studio-mac.local:7749", trusted: true,
                                lastSeenMs: peerMsAgo(1), carries: true, serves: true,
                                inFlight: 2, leaseSpent: 0.62, leaseTtlSeconds: 180,
                                until: Int64(peerNow.timeIntervalSince1970) + 3600)
                        ],
                        answeringOn: .init(peer: "studio-mac", inFlight: 2))),
                true
            ),
            // 9. The live half, which is what the paths block is for: two
            //    Macs, two ways to reach each, and the three states of a
            //    measurement side by side. studio-mac has a measured direct
            //    path and a second endpoint nothing has probed; attic-nuc is
            //    reachable only THROUGH studio-mac, which is the row an
            //    operator has to be able to read at a glance, and its direct
            //    path has measured loss. Rendered so the sub-line stack can be
            //    looked at: the arithmetic that reserves room for it
            //    (`PeerRowModel.rowShape`) is a number, and a number is not a
            //    picture of a row.
            (
                "56-peers-paths",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-4b8we1r0zp", name: "studio-mac",
                                address: "192.168.1.24:7749", trusted: true,
                                lastSeenMs: peerMsAgo(2), carries: true, serves: true,
                                leaseSpent: 0.34, leaseTtlSeconds: 240,
                                tokensPerHour: 90_000,
                                paths: [
                                    .init(
                                        endpoint: "192.168.1.24:7749", kind: .direct,
                                        rttMs: 18.5, lossPct: 0,
                                        lastOkMs: peerMsAgo(2)),
                                    .init(endpoint: "10.0.1.24:7749", kind: .direct),
                                ]),
                            .init(
                                id: "tcr-92hbq5t7yv", name: "attic-nuc",
                                address: "10.0.1.31:7749", trusted: true,
                                lastSeenMs: peerMsAgo(9), carries: true,
                                paths: [
                                    .init(
                                        endpoint: "10.0.1.31:7749", kind: .direct,
                                        rttMs: 96, lossPct: 0.02),
                                    .init(
                                        endpoint: "tcr-4b8we1r0zp", kind: .via,
                                        rttMs: 128, lossPct: 0,
                                        lastOkMs: peerMsAgo(30)),
                                ]),
                        ])),
                false
            ),
            // One scene per path state. Two states already had a shape on
            // screen; the third had none at all until now, because a
            // trusted Mac with no paths drew no line and the `asleep` pill
            // carried two facts.
            (
                "w12-path-direct",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-4b8we1r0zp", name: "studio-mac",
                                address: "192.168.1.24:7749", trusted: true,
                                lastSeenMs: peerMsAgo(2), carries: true, serves: true,
                                leaseSpent: 0.34, leaseTtlSeconds: 240,
                                paths: [
                                    .init(
                                        endpoint: "192.168.1.24:7749", kind: .direct,
                                        rttMs: 14, lossPct: 0, lastOkMs: peerMsAgo(2))
                                ])
                        ])),
                false
            ),
            // Forwarded and lossy: amber, and the forwarder is named rather
            // than shown as the peer id the wire carries. 6 per cent, which is
            // over the 3 per cent line this scene sets, so the scene checks the
            // rule instead of restating it.
            (
                "w12-path-via",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-4b8we1r0zp", name: "studio-mac",
                                address: "192.168.1.24:7749", trusted: true,
                                lastSeenMs: peerMsAgo(4), carries: true, serves: true,
                                leaseSpent: 0.34, leaseTtlSeconds: 240,
                                paths: [
                                    .init(
                                        endpoint: "tcr-92hbq5t7yv", kind: .via,
                                        rttMs: 96, lossPct: 0.06, lastOkMs: peerMsAgo(4))
                                ]),
                            .init(
                                id: "tcr-92hbq5t7yv", name: "loft-mini",
                                address: "10.0.1.31:7749", trusted: true,
                                lastSeenMs: peerMsAgo(3), carries: true,
                                paths: [
                                    .init(
                                        endpoint: "10.0.1.31:7749", kind: .direct,
                                        rttMs: 18, lossPct: 0)
                                ]),
                        ])),
                false
            ),
            // Trusted and awake, but nothing has ever found a way to reach
            // it: the case a firewall eating the port produces. A recent
            // lastSeenMs keeps the freshness read as awake rather than
            // asleep, and an empty paths array is the honest absence rather
            // than a stale reading. This used to share `lastSeenMs` and a
            // spent, ended lease with the asleep row below, which drew the
            // same picture for two different facts.
            (
                "w12-path-none",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-4b8we1r0zp", name: "studio-mac",
                                address: "studio-mac.local:7749", trusted: true,
                                lastSeenMs: peerMsAgo(2), carries: true)
                        ])),
                false
            ),
            // The absent line as a CONTROL, beside a row that has a path, so
            // the difference between the two sub-lines is in one picture: one
            // states a route, the other states the absence AND offers the act
            // that answers it. The scene above is the same line on a Mac with
            // nothing else around it; this one is what the tab really looks
            // like when one Mac of two cannot be reached, which is the state
            // the act exists for.
            (
                "w13-path-none-control",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-92hbq5t7yv", name: "loft-mini",
                                address: "192.168.4.31:41234", trusted: true,
                                lastSeenMs: peerMsAgo(2), carries: true, serves: true,
                                leaseSpent: 0.22, leaseTtlSeconds: 240,
                                paths: [
                                    .init(
                                        endpoint: "192.168.4.31:41234", kind: .direct,
                                        rttMs: 11, lossPct: 0, lastOkMs: peerMsAgo(2))
                                ]),
                            .init(
                                id: "tcr-4b8we1r0zp", name: "studio-mac",
                                address: "studio-mac.local:7749", trusted: true,
                                lastSeenMs: peerMsAgo(172_800), carries: true),
                        ],
                        name: "desk-mac")),
                false
            ),
            // Every scene of the mini mesh card is the REAL tab with the
            // real card in it, so the mesh cannot show a shape the panel
            // would not draw from the same document.
            //
            // 5a: one Mac direct and healthy, one carried and lossy.
            (
                "w12-mesh-two-macs",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-92hbq5t7yv", name: "attic-nuc",
                                address: "10.0.1.31:7749", trusted: true,
                                lastSeenMs: peerMsAgo(2), carries: true,
                                paths: [
                                    .init(
                                        endpoint: "10.0.1.31:7749", kind: .direct,
                                        rttMs: 16, lossPct: 0.01)
                                ]),
                            .init(
                                id: "tcr-4b8we1r0zp", name: "loft-mini",
                                address: "10.0.1.24:7749", trusted: true,
                                lastSeenMs: peerMsAgo(3), carries: true,
                                paths: [
                                    .init(
                                        endpoint: "tcr-92hbq5t7yv", kind: .via,
                                        rttMs: 58, lossPct: 0.05)
                                ]),
                        ],
                        name: "desk-mac")),
                false
            ),
            // 5b: the direct line to loft-mini is gone and the only way left
            // is through attic-nuc, which is still reached directly itself.
            (
                "w12-mesh-via-only",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-92hbq5t7yv", name: "attic-nuc",
                                address: "10.0.1.31:7749", trusted: true,
                                lastSeenMs: peerMsAgo(2), carries: true,
                                paths: [
                                    .init(
                                        endpoint: "10.0.1.31:7749", kind: .direct,
                                        rttMs: 18, lossPct: 0)
                                ]),
                            .init(
                                id: "tcr-4b8we1r0zp", name: "loft-mini",
                                address: "10.0.1.24:7749", trusted: true,
                                lastSeenMs: peerMsAgo(8), carries: true,
                                paths: [
                                    .init(
                                        endpoint: "tcr-92hbq5t7yv", kind: .via,
                                        rttMs: 140, lossPct: 0.04)
                                ]),
                        ],
                        name: "desk-mac")),
                false
            ),
            // 5c: no trusted Macs at all, but one found and not yet trusted,
            // so the empty mesh card is pictured beside a real found row
            // rather than beside nothing at all. One sentence, no ring, and
            // no link to a page with nothing on it. A found peer keeps this
            // scene apart from the plain looking state below, which has no
            // peers of any kind.
            (
                "w12-mesh-empty",
                peersSnapshot(
                    PeerListDocument(
                        finding: true,
                        peers: [
                            .init(
                                name: "studio-mac", address: "studio-mac.local:7749",
                                lastSeenMs: peerMsAgo(2))
                        ],
                        name: "desk-mac")),
                false
            ),
            // 5d: seven trusted, five drawn, two collapsed. Chosen because it
            // needs all four loss colours and both line styles at once, and
            // because it is the fixture `PeerMeshLayoutTests` runs its
            // no-collision gate over.
            (
                "w12-mesh-collapse",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-92hbq5t7yv", name: "attic-nuc",
                                address: "10.0.1.31:7749", trusted: true,
                                lastSeenMs: peerMsAgo(2), carries: true,
                                paths: [
                                    .init(
                                        endpoint: "10.0.1.31:7749", kind: .direct,
                                        rttMs: 16, lossPct: 0.01)
                                ]),
                            .init(
                                id: "tcr-4b8we1r0zp", name: "loft-mini",
                                address: "10.0.1.24:7749", trusted: true,
                                lastSeenMs: peerMsAgo(4), carries: true,
                                paths: [
                                    .init(
                                        endpoint: "tcr-92hbq5t7yv", kind: .via,
                                        rttMs: 72, lossPct: 0.06)
                                ]),
                            .init(
                                id: "tcr-7a1c5m9x2k", name: "office-mini",
                                address: "10.0.1.41:7749", trusted: true,
                                lastSeenMs: peerMsAgo(6), carries: true,
                                paths: [
                                    .init(
                                        endpoint: "10.0.1.41:7749", kind: .direct,
                                        rttMs: 210, lossPct: 0.14)
                                ]),
                            .init(
                                id: "tcr-3f8d2v6b1n", name: "lab-mac",
                                address: "10.0.1.52:7749", trusted: true,
                                lastSeenMs: peerMsAgo(600), carries: true),
                            .init(
                                id: "tcr-5k2p8w4r7t", name: "gil-laptop",
                                address: "10.0.1.63:7749", trusted: true,
                                lastSeenMs: peerMsAgo(5), carries: true,
                                paths: [
                                    .init(endpoint: "tcr-92hbq5t7yv", kind: .via, rttMs: 88)
                                ]),
                            .init(
                                id: "tcr-6m3q9z1h5j", name: "shed-mac",
                                address: "10.0.1.74:7749", trusted: true,
                                lastSeenMs: peerMsAgo(7), carries: true,
                                paths: [
                                    .init(
                                        endpoint: "10.0.1.74:7749", kind: .direct,
                                        rttMs: 24, lossPct: 0)
                                ]),
                            .init(
                                id: "tcr-8n4t2y6u0i", name: "van-mac",
                                address: "10.0.1.85:7749", trusted: true,
                                lastSeenMs: peerMsAgo(9), carries: true,
                                paths: [
                                    .init(
                                        endpoint: "10.0.1.85:7749", kind: .direct,
                                        rttMs: 31, lossPct: 0)
                                ]),
                        ],
                        name: "desk-mac")),
                false
            ),
            // The forward-compat state: an older `tcr` answering
            // `{"supported": false}` collapses the tab to ONE honest line and
            // must never draw
            // `Unreadable status output` (`FleetView.swift:1008`). A scene
            // nobody renders is a claim nobody can check.
            (
                "44-peers-unsupported",
                peersSnapshot(PeerListDocument(supported: false)),
                false
            ),
            // A Mac with no network interface at all: no Wi-Fi, no cable.
            //
            // `network: false` and not an absent field. Absent is every `tcr`
            // shipped before the field, which draws the tab exactly as it
            // always drew it; only a REPORTED false turns the Find card and
            // the card under it into their no-network arms, which is the pair
            // of sentences this scene exists to let somebody read.
            //
            // `finding: true` with it, and that is the whole point of the
            // arm: the Find card replaces its LOOKING line, because a Mac
            // with no interface is not looking whatever the switch says.
            // Switched off, the off state is the operator's own doing and
            // keeps the headline, so a fixture with the switch off would
            // picture the old card and prove nothing.
            (
                "64-peers-no-network",
                peersSnapshot(PeerListDocument(finding: true, network: false)),
                false
            ),
            // A trusted Mac whose live half this build could not read at all.
            //
            // Built through `mergingLive(.unsupported)`, the real path an
            // older `tcr` takes, rather than by setting the flag by hand: the
            // row then says `path not reported`, which is an absence this
            // panel did not measure, against `w12-path-none` above, where it
            // looked and there is no way through right now. Two absences, two
            // sentences, and the pair only reads as deliberate side by side.
            (
                "w12-path-not-reported",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-4b8we1r0zp", name: "studio-mac",
                                address: "studio-mac.local:7749", trusted: true,
                                lastSeenMs: peerMsAgo(2), carries: true)
                        ]
                    ).mergingLive(.unsupported)),
                false
            ),
            // Two trusted Macs and one of them unreachable: a dotted grey edge
            // and a `no path now` plate beside a solid measured one.
            //
            // The card takes that fact from the row rather than from the
            // figures, and until it was handed over every tile claimed a path.
            // A scene with both pictures in one frame is the only way to see
            // that the two are drawn differently at all.
            (
                "w12-mesh-one-path-missing",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-92hbq5t7yv", name: "attic-nuc",
                                address: "10.0.1.31:7749", trusted: true,
                                lastSeenMs: peerMsAgo(2), carries: true,
                                paths: [
                                    .init(
                                        endpoint: "10.0.1.31:7749", kind: .direct,
                                        rttMs: 16, lossPct: 0.01)
                                ]),
                            .init(
                                id: "tcr-4b8we1r0zp", name: "loft-mini",
                                address: "10.0.1.24:7749", trusted: true,
                                lastSeenMs: peerMsAgo(3), carries: true),
                        ],
                        name: "desk-mac")),
                false
            ),
            // One trusted Mac, reached directly, with a round trip and no loss
            // figure at all.
            //
            // The graph says that by DROPPING the dot, so the only way to
            // check that it is dropped is a picture of a tile that should not
            // have one. The reading beside it reads `direct · 22 ms` and stops
            // there rather than printing a nought nothing counted.
            (
                "w12-mesh-loss-unread",
                peersSnapshot(
                    PeerListDocument(
                        finding: true, sharing: true,
                        peers: [
                            .init(
                                id: "tcr-92hbq5t7yv", name: "attic-nuc",
                                address: "10.0.1.31:7749", trusted: true,
                                lastSeenMs: peerMsAgo(2), carries: true,
                                paths: [
                                    .init(
                                        endpoint: "10.0.1.31:7749", kind: .direct,
                                        rttMs: 22)
                                ])
                        ],
                        name: "desk-mac")),
                false
            ),
        ]
    }

    /// Scene 47's peers: two found, neither trusted, the second announcing no name.
    private static let foundUntrustedPeers = PeerListDocument(
        finding: true,
        peers: [
            .init(
                name: "studio-mac", address: "studio-mac.local:7749",
                lastSeenMs: peerMsAgo(2)),
            .init(address: "10.0.1.24:7749", lastSeenMs: peerMsAgo(6)),
        ])

    /// Scene 47's two found Macs, with the tailnet keys: the first seen on
    /// both this network and the tailnet, the second on the tailnet only. Its
    /// pair is scene 47 itself, which must match it pixel for pixel outside
    /// the two `tailnet` pills (the slot is reserved on every found row).
    private static let foundOverTailnetPeers = PeerListDocument(
        finding: true,
        peers: [
            .init(
                name: "studio-mac", address: "studio-mac.local:7749",
                lastSeenMs: peerMsAgo(2), sources: [.lan, .tailnet]),
            .init(address: "10.0.1.24:7749", lastSeenMs: peerMsAgo(6), sources: [.tailnet]),
        ])

    /// Scene 48's peers: one Mac trusted and carrying, sharing still off.
    private static let trustedShareOffPeers = PeerListDocument(
        finding: true,
        peers: [
            .init(
                id: "tcr-4b8we1r0zp", name: "studio-mac",
                address: "studio-mac.local:7749", trusted: true,
                lastSeenMs: peerMsAgo(3), carries: true),
            .init(
                name: "attic-nuc", address: "attic-nuc.local:7749",
                lastSeenMs: peerMsAgo(40)),
        ])

    /// Scene 49's peers: sharing on, two Macs trusted and serving.
    private static let shareOnPeers = PeerListDocument(
        finding: true, sharing: true,
        peers: [
            .init(
                id: "tcr-4b8we1r0zp", name: "studio-mac",
                address: "studio-mac.local:7749", trusted: true,
                lastSeenMs: peerMsAgo(2), carries: true, serves: true,
                leaseSpent: 0.34, leaseTtlSeconds: 240),
            .init(
                id: "tcr-92hbq5t7yv", name: "attic-nuc",
                address: "attic-nuc.local:7749", trusted: true,
                lastSeenMs: peerMsAgo(5), carries: true, serves: true,
                leaseSpent: 0, leaseTtlSeconds: 300),
        ])

    /// Scenes 47 to 49 again: the same peers, with a Codex account in the fleet.
    ///
    /// No Mac lends, serves or carries a Codex request, so each promise names Claude,
    /// and a line under the Share card says where Codex requests go instead.
    private static var mixedPeerScenes:
        [(
            name: String, snapshot: PeersSnapshot, dry: Bool, refusal: PeerRefusal,
            detailsOpen: Bool
        )]
    {
        [
            (
                "47b-peers-found-untrusted-mixed",
                peersSnapshot(foundUntrustedPeers, holdsCodex: true), false, PeerRefusal(), false
            ),
            (
                "48b-peers-trusted-share-off-mixed",
                peersSnapshot(trustedShareOffPeers, holdsCodex: true), false, PeerRefusal(), false
            ),
            (
                "49b-peers-share-on-mixed",
                peersSnapshot(shareOnPeers, holdsCodex: true), false, PeerRefusal(), false
            ),
        ]
    }

    /// The fixture Settings > Peers renders under `--render-settings`, which
    /// is the Settings > Peers mockup (kept outside the tree)'s own scene 58:
    /// sharing on, two Macs trusted, one asleep, and the five This Mac
    /// readouts present so the pane draws its real values rather than six
    /// "not read yet" rows.
    ///
    /// Lives here, with the other fixtures, rather than in the pane: a
    /// fixture inside a production view is a value that can be shipped by
    /// accident, and every other pinned state this app draws is authored in a
    /// `Render*` file. `PeersSettingsPane`'s own initialiser reaches for it
    /// only when `RenderSettings.requestedDirectory()` says this process was
    /// launched to write PNGs.
    ///
    /// The id and the address are obviously fake, per `CLAUDE.md`: this
    /// repository is public and no real account, host or key goes into a
    /// fixture.
    static var settingsPaneFixture: PeersSnapshot {
        peersSnapshot(
            PeerListDocument(
                finding: true, sharing: true,
                peers: [
                    .init(
                        id: "tcr-4b8we1r0zp", name: "studio-mac",
                        address: "studio-mac.local:7749", trusted: true,
                        lastSeenMs: peerMsAgo(3), carries: true, serves: true,
                        leaseSpent: 0.34, leaseTtlSeconds: 240,
                        // The tailnet mockup's scene 1: one Mac over the
                        // tailnet directly, one through a relay.
                        tailnet: .init(route: .direct)),
                    .init(
                        id: "tcr-92hbq5t7yv", name: "attic-nuc",
                        address: "attic-nuc.local:7749", trusted: true,
                        lastSeenMs: peerMsAgo(420), carries: true,
                        lend: [
                            .init(
                                leaseId: "ls-4b1f", scope: .group("work"),
                                window: .week, fraction: 0.20, ttlSeconds: 300,
                                maxInFlight: 2,
                                until: Int64(peerNow.timeIntervalSince1970) + 3600),
                            .init(
                                leaseId: "ls-2e77", scope: .accounts(["alice"]),
                                window: .fiveHour, fraction: 0.20, ttlSeconds: 300,
                                maxInFlight: 2,
                                until: Int64(peerNow.timeIntervalSince1970) - 1800,
                                ended: true),
                        ],
                        tailnet: .init(route: .relayed)),
                ],
                name: "studio-mac",
                // Decision row 10 turned this default OFF, and the fixture is
                // the state a fresh config is in: the render then pictures the
                // switch an operator actually meets.
                announceName: false,
                nodeId: "tcr-7f3k9m2q4x",
                listenAddress: "0.0.0.0:7749",
                via: "auto",
                maxHops: 1,
                // The panel-state rows: one Mac knocking, one blocked, one
                // muted, and the caps as `src/main.rs:1202-1208` reports
                // them. Every address is private-range and the labels are
                // sanitized, this repository is public and these PNGs are
                // review artifacts.
                pending: [
                    .init(
                        addr: "10.0.1.24", instanceId: "8f2c1ad63b0e4471",
                        proposedName: "loft-mini", wireVersion: 1,
                        firstSeenMs: peerMsAgo(30), lastSeenMs: peerMsAgo(4))
                ],
                blocked: [
                    .init(
                        addr: "10.0.1.99", sinceMs: peerMsAgo(10800),
                        reason: .forgottenAndBlocked)
                ],
                muted: [.init(addr: "10.0.1.55", untilMs: peerMsAgo(-2820))],
                limited: 3,
                caps: .init(
                    foundRows: 12, foundPerAddress: 2, pending: 8, knockIntervalMs: 10000,
                    knockBurst: 3, unauthenticatedSockets: 16),
                lentTo: [
                    "alice": [
                        .init(
                            peer: "attic-nuc", scope: .group("work"), window: .week,
                            fraction: 0.20),
                        .init(
                            peer: "studio-mac", scope: .accounts(["alice"]), window: .fableWeek,
                            fraction: 1.0),
                    ]
                ]))
    }

    /// The Tailscale row of the pane under `--render-settings`: the mockup's
    /// scene 1, the app mode working. Documentation-range values only.
    static var settingsTailnetFixture: PeerTailnetRead {
        .read(tailnetReading(.app, held: .init(
            mode: .app, state: .ready, address: "100.64.0.7", found: 2,
            recordedAtMs: peerMsAgo(5))))
    }

    static func tailnetReading(
        _ mode: PeerTailnetMode, nodeSupported: Bool = false, held: PeerTailnetReading.Held?
    ) -> PeerTailnetReading {
        PeerTailnetReading(mode: mode, nodeSupported: nodeSupported, held: held)
    }

    /// Seconds ago, as Unix milliseconds against ``peerNow``, so a rendered
    /// age is the same on every run, unlike the running panel's live clock.
    private static func peerMsAgo(_ seconds: TimeInterval) -> Int64 {
        Int64(peerNow.addingTimeInterval(-seconds).timeIntervalSince1970 * 1000)
    }

    /// The instant every peer scene is rendered AT. Pinned for the reason the
    /// whole harness is pinned: an age that read "2s ago" on one run and
    /// "3s ago" on the next would make every peer PNG differ from the last
    /// one for no design reason.
    ///
    /// Not private: the Settings pane reads it too, under `--render-settings`
    /// and only there. Its lease rows ask the clock what has ended, so a pane
    /// drawn against the REAL clock read every fixture lease as expired the
    /// day this instant fell behind today, and scene 63's "2 running, 1 ended"
    /// rendered as three ended rows.
    static let peerNow = Date(timeIntervalSince1970: 1_786_000_000)

    /// `holdsCodex` builds the snapshot a fleet holding a Codex account reads.
    private static func peersSnapshot(_ document: PeerListDocument, holdsCodex: Bool = false)
        -> PeersSnapshot
    {
        PeersSnapshotBuilder.snapshot(from: document, now: peerNow, holdsCodex: holdsCodex)
    }

    /// One Peers-tab state, drawn inside the REAL panel shell.
    ///
    /// ``PanelV4`` takes its content as a closure precisely so a tab can be
    /// composed into it, which is what lets these scenes carry the header, the
    /// summary line and the four-tab strip with Peers selected, the mockup's
    /// own frame, without `FleetView` needing a way to inject fixture peers.
    ///
    /// `holdsCodex` draws the tab of a fleet holding a Codex account.
    @MainActor
    private static func renderPeer(
        _ scene: (
            name: String, snapshot: PeersSnapshot, dry: Bool, refusal: PeerRefusal,
            detailsOpen: Bool
        ),
        appearance: Appearance,
        holdsCodex: Bool = false,
        into directory: URL
    ) -> Bool {
        withDrawingAppearance(appearance.nsAppearance) {
            rasterise(
                peersPanel(
                    snapshot: scene.snapshot, dry: scene.dry, refusal: scene.refusal,
                    detailsOpen: scene.detailsOpen, holdsCodex: holdsCodex,
                    appearance: appearance),
                named: "\(scene.name)-\(appearance.rawValue).png", into: directory)
        }
    }

    /// The five states of the Trust sheet, over the tab it opens from.
    ///
    /// **This is the surface that shipped unreviewable.** The sheet is where
    /// the whole trust ritual happens and the harness had no fixture for it at
    /// all, which is exactly how a sheet whose Trust button could never be
    /// pressed passed every gate: nobody could look at it. One PNG per state,
    /// both appearances.
    ///
    /// The runs are ``PeerPairRun/init(pinned:)``: no process, no pipe, no
    /// subprocess of any kind, the same door ``PeerController/pinned(_:)``
    /// gives the tab underneath.
    private static var peerSheetScenes:
        [(name: String, state: PeerPairState, typed: String, expiresIn: TimeInterval?)]
    {
        [
            // 1. The knock is away and nobody over there has answered. The
            //    instance id is the argument the OTHER operator types, so the
            //    sheet prints it; this one is obviously fake, as every id in
            //    this file is.
            //
            //    The one scene that counts a deadline, because it is the one
            //    state that has one: the request this panel sent expires, the
            //    sheet says how long is left, and the running panel counts
            //    from the press that opened it. A pinned number rather than a
            //    live count, the rule every other figure in this file keeps,
            //    so the PNG is the same on every run.
            (
                "57-trust-waiting", .asking(instance: "8f2c1ad63b0e4471"), "",
                PeerAdmission.knockExpirySeconds - 45
            ),
            // 2. The pivotal screen: this Mac's six digits, and an empty field
            //    for the six the other screen is showing. Trust is drawn
            //    disabled here, which is the state an operator meets first.
            ("58-trust-compare", .comparing(code: "418902"), "", nil),
            // 3. The same screen with the other Mac's digits typed in full, so
            //    the enabled control has a picture too. Without this one the
            //    only rendered Trust button is a dim one, and "the control is
            //    reachable" would again be a claim with no fixture behind it.
            ("59-trust-compare-typed", .comparing(code: "418902"), "418902", nil),
            // 4. Pinned.
            ("60-trust-done", .done(peer: "tcr-4b8we1r0zp"), "", nil),
            // 5. Refused, in the CLI's own words. A MISMATCH, which is the
            //    one refusal this path exists to produce.
            (
                "61-trust-refused",
                .refused(
                    "peer pair: refused, 418902 here, 418903 there. A mismatch is the one "
                        + "signal this path exists to produce, so it is not a retry prompt"),
                "418903", nil
            ),
            // 6. The operator stopped it.
            ("62-trust-cancelled", .cancelled, "", nil),
        ]
    }

    /// Scenes 58 and 60 again, with a Codex account in the fleet: a trusted Mac
    /// never carries a Codex request, so what the pin buys is named as Claude
    /// traffic, over the tab that fleet draws.
    private static var mixedPeerSheetScenes:
        [(name: String, state: PeerPairState, typed: String, expiresIn: TimeInterval?)]
    {
        [
            ("58b-trust-compare-mixed", .comparing(code: "418902"), "", nil),
            ("60b-trust-done-mixed", .done(peer: "tcr-4b8we1r0zp"), "", nil),
        ]
    }

    /// One Trust sheet state, over the found-rows tab it opens from.
    ///
    /// `holdsCodex` draws the sheet, and the tab under it, in a fleet holding a
    /// Codex account.
    @MainActor
    private static func renderPeerSheet(
        _ scene: (name: String, state: PeerPairState, typed: String, expiresIn: TimeInterval?),
        appearance: Appearance,
        holdsCodex: Bool = false,
        into directory: URL
    ) -> Bool {
        withDrawingAppearance(appearance.nsAppearance) {
            let run = PeerPairRun(pinned: scene.state)
            run.compare.set(scene.typed)
            let view =
                peersPanel(
                    snapshot: trustSheetTab(holdsCodex: holdsCodex), dry: false,
                    holdsCodex: holdsCodex, appearance: appearance
                )
                .overlay {
                    ZStack {
                        Color.black.opacity(sheetScrimAlpha)
                        PeerTrustSheet(
                            peerName: "studio-mac",
                            state: scene.state,
                            compare: .constant(run.compare),
                            expiresIn: scene.expiresIn,
                            snapshotMode: true,
                            holdsCodex: holdsCodex
                        )
                        .background(RoundedRectangle(cornerRadius: V4.cardRadius).fill(Tok.panel))
                        .shadow(radius: sheetShadowRadius)
                    }
                }
                .environment(\.colorScheme, appearance == .dark ? .dark : .light)
            return rasterise(
                view, named: "\(scene.name)-\(appearance.rawValue).png", into: directory)
        }
    }

    /// The three states of the link sheet, over the row it opens from.
    ///
    /// Every one of them is a screen a person meets with no terminal open, and
    /// two of the three say that nothing was made: a mint that cannot happen
    /// yet and a run that did not happen at all have different fixes and had
    /// to be looked at side by side rather than trusted to be distinguishable.
    ///
    /// The strings are the ones the verb prints, quoted as it prints them, and
    /// the link is nonsense of the right shape: this repository is public and
    /// a real sealed record is a real Mac's addresses.
    private static var peerMovedSheetScenes: [(name: String, outcome: PeerMovedMint.Outcome?)] {
        [
            (
                "w13-moved-link",
                .minted(
                    link: "tcr://peer/moved?v=1&r=6YFN2TQ8ZKW3H0JA5RC1XVB7MEPD9GNT4SZQ0W8KY2FH"
                        + "6JR3BXM5VCTQ81NPZDGA7KW4YFS2JHR0BXVC93MTQ8ZNPD5G",
                    sentences: "peer moved: sealed for studio-mac; only that Mac can read it, "
                        + "and it goes stale in 24 hours\npeer moved: it carries 2 address(es) "
                        + "and nothing else: it joins nothing, grants nothing and pairs nothing")
            ),
            // The one pair this feature cannot serve, said with the fix in it.
            // The sentence is the CLI's, in place of the link box.
            (
                "w13-moved-no-secret",
                .refused(
                    "moved link: this pair has no shared secret yet, so there is nothing only "
                        + "the two of you can read; let the two Macs complete one session "
                        + "together and try again")
            ),
            // Not a refusal: nothing ran. The remedy is about this Mac's own
            // installation, which is why it may not be drawn in tcr's voice.
            (
                "w13-moved-failed",
                // The remedy is not written out here: it is the one the app
                // really prints, so a picture of this screen cannot show a fix
                // that does not work.
                .couldNotRun("tcr not found (searched 4 locations). " + TcrTool.overrideRemedy)
            ),
        ]
    }

    /// One link-sheet state, over the trusted row whose line opened it.
    @MainActor
    private static func renderPeerMovedSheet(
        _ scene: (name: String, outcome: PeerMovedMint.Outcome?),
        appearance: Appearance,
        into directory: URL
    ) -> Bool {
        withDrawingAppearance(appearance.nsAppearance) {
            let view =
                peersPanel(snapshot: movedSheetTab, dry: false, appearance: appearance)
                .overlay {
                    ZStack {
                        Color.black.opacity(sheetScrimAlpha)
                        PeerMovedSheet(peerName: "studio-mac", outcome: scene.outcome)
                            .background(
                                RoundedRectangle(cornerRadius: V4.cardRadius).fill(Tok.panel)
                            )
                            .shadow(radius: sheetShadowRadius)
                    }
                }
                .environment(\.colorScheme, appearance == .dark ? .dark : .light)
            return rasterise(
                view, named: "\(scene.name)-\(appearance.rawValue).png", into: directory)
        }
    }

    /// Six states of the invite sheet, `w14` since `w13` is the moved sheet's.
    ///
    /// The strings are the ones the verb prints, quoted as it prints them, and
    /// every address is a documentation range: this repository is public and
    /// a real key is a real Mac's addresses.
    private static var peerInviteSheetScenes: [(name: String, outcome: PeerInviteMint.Outcome?)] {
        [
            // Still running: the sheet opens before tcr has answered, so a
            // slow mint reads as a screen working rather than a press that
            // did nothing.
            ("w14-invite-minting", nil),
            // The common case a friend off this network meets: no address a
            // friend outside this Wi-Fi could reach, said in the CLI's own
            // words.
            (
                "w14-invite-key",
                .minted(
                    key: "tcr-join:v2:192.0.2.10:7755,198.51.100.20:7755:"
                        + "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA:BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
                    sentences: "peer invite: ok id=3 label=joining-mac ttl_s=600 uses=1\n"
                        + "peer invite: lan 192.0.2.10:7755\n"
                        + "peer invite: tailscale 198.51.100.20:7755\n"
                        + "peer invite: this key carries no internet address, so a friend who "
                        + "is not on this network or this tailnet needs this Mac's router to "
                        + "forward the port; `tcr peer reach` reports where that stands\n"
                        + "peer invite: this key is join-capable by anything that can read "
                        + "peers.json until it is used or expires, `tcr peer invite --revoke 3` "
                        + "ends it early")
            ),
            // The two have to be told apart at a glance, or the honest answer
            // about reach is not being given: this one carries a path the
            // open internet can dial.
            (
                "w14-invite-internet",
                .minted(
                    key: "tcr-join:v2:198.51.100.20:7755,192.0.2.10:7755:"
                        + "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC:DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD",
                    sentences: "peer invite: ok id=4 label=joining-mac ttl_s=600 uses=1\n"
                        + "peer invite: internet 198.51.100.20:7755\n"
                        + "peer invite: lan 192.0.2.10:7755\n"
                        + "peer invite: this key is join-capable by anything that can read "
                        + "peers.json until it is used or expires, `tcr peer invite --revoke 4` "
                        + "ends it early")
            ),
            // No listener at all: `tcr peer find on` is the fix, inside the
            // sentence.
            (
                "w14-invite-no-listener",
                .refused(
                    "peer invite: this node has no peer listener, so a token would carry no "
                        + "address to dial (`tcr peer find on` opens one)")
            ),
            // Eight outstanding is the cap, reached by pressing the new
            // button repeatedly, which this change makes easy for the first
            // time.
            (
                "w14-invite-cap",
                .refused(
                    "peer invite: 8 invites are already outstanding, which is the cap "
                        + "(MAX_OUTSTANDING_INVITES); every one of them is a PSK the registrar "
                        + "must try against message 1, so `tcr peer invite --revoke <id>` "
                        + "first")
            ),
            // Not a refusal: nothing ran. About this Mac's own installation,
            // which is why it is not drawn in tcr's voice.
            (
                "w14-invite-failed",
                .couldNotRun("tcr not found (searched 4 locations). " + TcrTool.overrideRemedy)
            ),
        ]
    }

    /// One invite-sheet state, over the empty tab: this sheet is opened by
    /// somebody with nobody to show yet, and the moved sheet's fixture (a
    /// trusted Mac) would tell the wrong story here.
    @MainActor
    private static func renderPeerInviteSheet(
        _ scene: (name: String, outcome: PeerInviteMint.Outcome?),
        appearance: Appearance,
        into directory: URL
    ) -> Bool {
        withDrawingAppearance(appearance.nsAppearance) {
            let view =
                peersPanel(snapshot: inviteSheetTab, dry: false, appearance: appearance)
                .overlay {
                    ZStack {
                        Color.black.opacity(sheetScrimAlpha)
                        PeerInviteSheet(
                            mode: .constant(.key), invite: scene.outcome, sealed: nil
                        )
                        .background(
                            RoundedRectangle(cornerRadius: V4.cardRadius).fill(Tok.panel)
                        )
                        .shadow(radius: sheetShadowRadius)
                    }
                }
                .environment(\.colorScheme, appearance == .dark ? .dark : .light)
            return rasterise(
                view, named: "\(scene.name)-\(appearance.rawValue).png", into: directory)
        }
    }

    /// Six states of the sealed sheet, `w15` since `w14` is the invite
    /// sheet's plain key. Two ways in: minting an ask and waiting for a
    /// reply, or opening one that a friend sent back. Both end without a
    /// six-digit compare: opening a reply runs the join it carries
    /// immediately, `src/peer/ask.rs`'s own doc.
    private static var peerSealedSheetScenes:
        [(name: String, mode: PeerInviteSheet.Mode, sealed: PeerSealedMint.Outcome?)]
    {
        [
            // Neither segment pressed yet: the picker alone, and the two
            // sentences under it are the whole of the security explanation.
            ("w15-sealed-pick", .none, nil),
            // The sealed segment pressed, the ask still minting: the same
            // in-flight sentence mode A's own nil case draws.
            ("w15-sealed-ask", .sealed, nil),
            // The ask minted, the paste field waiting: nothing on this panel
            // has asked a person to come back later with a string before.
            (
                "w15-sealed-waiting",
                .sealed,
                .asked(
                    ask: "tcr-invite:v1:71HFE865V215DE1CTEWH0AVNH9DN65R6PPG2X7R5JQC42EJ5E6RHXQAQPDH4",
                    sentences: "peer invite: this names no address and grants nothing; "
                        + "whoever holds it can send you an address and nothing else\n"
                        + "peer invite: it is good for ten minutes; paste what comes back "
                        + "with `tcr peer invite --reply --stdin`")
            ),
            // A reply opened: no knock, no digits, joined on the spot.
            (
                "w15-sealed-joined",
                .sealed,
                .joined(
                    sentences: "peer invite: ok addr=198.51.100.20:7755 file=tcr-peers.json")
            ),
            // A reply that does not open: a stale ask, a second paste of one
            // already spent, or one sealed to a different Mac all read
            // identically on purpose (`ask::ReplyRefusal`'s own doc).
            (
                "w15-sealed-wrong-reply",
                .sealed,
                .refused(
                    "Error: peer join: this reply did not open against anything outstanding "
                        + "here; it may answer an ask that already expired, was already "
                        + "spent, or belongs to a different Mac")
            ),
            // The reply opened but the dial after it found nobody: the
            // common failure, and the one a fallback direction would exist
            // for.
            (
                "w15-sealed-no-answer",
                .sealed,
                .refused(
                    "Error: peer join: nothing answered at any address this key carries: "
                        + "198.51.100.20:7755 (connection refused). That Mac may be asleep "
                        + "or on another network; ask for a fresh key, or `tcr peer reach` "
                        + "on that Mac says whether its router forwards the port")
            ),
            // Same refusal as `w15-sealed-no-answer`, drawn to show the
            // swap it now offers: the sheet's own predicate reads this
            // sentence the same way, so the ask field and `Answer invite`
            // draw underneath it.
            (
                "w15-sealed-swap",
                .sealed,
                .refused(
                    "Error: peer join: nothing answered at any address this key carries: "
                        + "198.51.100.20:7755 (connection refused). That Mac may be asleep "
                        + "or on another network; ask for a fresh key, or `tcr peer reach` "
                        + "on that Mac says whether its router forwards the port")
            ),
            // This Mac answering the other side's invite: the arm that
            // drew nothing before the swap shipped.
            (
                "w15-sealed-answered",
                .sealed,
                .answered(
                    reply:
                        "tcr-reply:v1:M9S346Q3D25VT4F5V37E3S3E28JT97KB6CQ643DZVMXXQKFBF5KZNWJ47TAN9ZT24MNPZX45HY43KWJRP1XPA7Z3DJ8FSSZ5AWSH8VHTPRE95B9EE0ZBGJ09TQM83XSSSS6YS3C4DWA7N36096Q14DR9GPQY77ZXYYK596NGYA1DQ91",
                    sentences: "peer join: send this back to whoever sent you the invite; "
                        + "nothing in it says where you are except to them\n"
                        + "peer join: opening it joins you immediately, pinned and trusted "
                        + "on both sides, the same as a pasted key")
            ),
        ]
    }

    /// The sealed sheet's own scenes, `peerInviteSheetScenes`' shape, over
    /// the same empty-tab fixture ``inviteSheetTab`` uses.
    @MainActor
    private static func renderPeerSealedSheet(
        _ scene: (name: String, mode: PeerInviteSheet.Mode, sealed: PeerSealedMint.Outcome?),
        appearance: Appearance,
        into directory: URL
    ) -> Bool {
        withDrawingAppearance(appearance.nsAppearance) {
            let view =
                peersPanel(snapshot: inviteSheetTab, dry: false, appearance: appearance)
                .overlay {
                    ZStack {
                        Color.black.opacity(sheetScrimAlpha)
                        PeerInviteSheet(
                            mode: .constant(scene.mode), invite: nil, sealed: scene.sealed
                        )
                        .background(
                            RoundedRectangle(cornerRadius: V4.cardRadius).fill(Tok.panel)
                        )
                        .shadow(radius: sheetShadowRadius)
                    }
                }
                .environment(\.colorScheme, appearance == .dark ? .dark : .light)
            return rasterise(
                view, named: "\(scene.name)-\(appearance.rawValue).png", into: directory)
        }
    }

    /// The tab underneath every invite sheet: finding on, nobody found yet,
    /// so the footer's `Invite…` button is drawn on the panel behind the
    /// scrim, which is what a person actually sees before they press it.
    private static var inviteSheetTab: PeersSnapshot {
        peersSnapshot(
            PeerListDocument(finding: true, peers: [], name: "desk-mac"))
    }

    /// The tab underneath every link sheet: the trusted Mac this one cannot
    /// reach, whose own path line is the press that opened the sheet.
    private static var movedSheetTab: PeersSnapshot {
        peersSnapshot(
            PeerListDocument(
                finding: true, sharing: true,
                peers: [
                    .init(
                        id: "tcr-4b8we1r0zp", name: "studio-mac",
                        address: "studio-mac.local:7749", trusted: true,
                        lastSeenMs: peerMsAgo(172_800), carries: true)
                ],
                name: "desk-mac"))
    }

    /// The tab underneath every Trust sheet: the found row the sheet was
    /// opened from, already waiting, which is what the live panel draws.
    /// `holdsCodex` builds it for a fleet holding a Codex account.
    private static func trustSheetTab(holdsCodex: Bool) -> PeersSnapshot {
        PeersSnapshotBuilder.waiting(
            peersSnapshot(
                PeerListDocument(
                    finding: true,
                    peers: [
                        .init(
                            name: "studio-mac", address: "studio-mac.local:7749",
                            lastSeenMs: peerMsAgo(8))
                    ]),
                holdsCodex: holdsCodex),
            knocked: ["studio-mac.local:7749"])
    }

    /// The Peers tab as a panel, for the scene renderers above.
    @MainActor
    ///
    /// `holdsCodex` is the pinned controller's: the tab of a fleet holding a Codex
    /// account.
    private static func peersPanel(
        snapshot: PeersSnapshot, dry: Bool, refusal: PeerRefusal = PeerRefusal(),
        detailsOpen: Bool = false, holdsCodex: Bool = false,
        appearance: Appearance
    ) -> some View {
        // Density: absent, which is `PanelDensityPreference`'s own
        // definition of the shipped default, the same write-then-remove
        // every other scene in this file does.
        UserDefaults.standard.removeObject(forKey: PanelDensityPreference.key)
        let scene = (snapshot: snapshot, dry: dry)
        return
            PanelV4(
                freshness: "updated 2s ago",
                tabs: PanelTab.allCases,
                selected: .peers,
                badges: [:],
                onSelect: { _ in },
                onSettings: {},
                // Thirteen, the fleet every other scene in this file
                // draws, so `PanelDensity.auto` resolves the same way it
                // does on the Accounts tab and these PNGs are comparable
                // with those.
                accountCount: 13,
                // The Peers tab alone: `PanelV4` asks every tab for its
                // layers, and the other three have nothing to draw here.
                summary: { tab in
                    if tab == .peers {
                        SummaryLine(
                            lines: [
                                scene.dry
                                    ? [
                                        .init(text: "13 accounts", tint: Tok.dim),
                                        .init(text: "none with headroom", tint: Tok.near),
                                    ]
                                    : [
                                        .init(text: "13 accounts", tint: Tok.dim),
                                        .init(
                                            text: "6 with headroom", tint: Tok.ok,
                                            emphasised: true),
                                    ]
                            ])
                    }
                },
                content: { tab in
                    if tab == .peers {
                        PeersTabV4(
                            controller: PeerController.pinned(
                                scene.snapshot, refusal: refusal, holdsCodex: holdsCodex),
                            snapshotMode: true,
                            refusalDetailsOpen: detailsOpen)
                    }
                },
                footer: { _ in EmptyView() }
            )
            .environment(\.colorScheme, appearance == .dark ? .dark : .light)
            // A FIXED size, for the reason the fleet scenes give: the
            // panel sizes itself from a GeometryReader preference and
            // `ImageRenderer` performs no second layout pass.
            .fixedSize()
    }

    // MARK: - The tab strip at its badge extremes

    /// One strip, one selection, one set of counts.
    private struct TabStripScene {
        let name: String
        let selected: PanelTab
        let badges: [PanelTab: Int]
    }

    /// The strip alone, at every badge extreme and two selections, so
    /// `apps/macos/scripts/check-tab-strip-still.swift` can prove that nothing in it
    /// moves when a count changes: every PNG of one selection must match the
    /// no-badge one outside the badge corners (Gil, 2026-10-14: "avoid layout
    /// shifts"). `100` draws the cap, `99+`.
    private static var tabStripScenes: [TabStripScene] {
        let sets: [(name: String, count: Int?)] = [
            ("none", nil), ("1", 1), ("9", 9), ("10", 10), ("99", 99), ("cap", 100),
        ]
        var out: [TabStripScene] = []
        for selected in [PanelTab.accounts, .sessions] {
            for set in sets {
                var badges: [PanelTab: Int] = [:]
                if let count = set.count {
                    badges = [.sessions: count, .tools: count, .peers: count]
                }
                out.append(
                    TabStripScene(
                        name: "70-tab-strip-\(selected.rawValue)-\(set.name)",
                        selected: selected, badges: badges))
            }
            out.append(
                TabStripScene(
                    name: "70-tab-strip-\(selected.rawValue)-mixed",
                    selected: selected, badges: [.sessions: 120, .tools: 3, .peers: 1]))
        }
        return out
    }

    @MainActor
    private static func renderTabStrip(
        _ scene: TabStripScene, appearance: Appearance, into directory: URL
    ) -> Bool {
        UserDefaults.standard.removeObject(forKey: PanelDensityPreference.key)
        PanelDensityPreference.setAccountCount(13)
        return withDrawingAppearance(appearance.nsAppearance) {
            let view =
                SegmentedTabs(
                    tabs: PanelTab.allCases, selected: scene.selected, badges: scene.badges,
                    onSelect: { _ in }
                )
                .padding(.top, V4.panelPaddingTop)
                .padding(.horizontal, V4.panelPaddingSide)
                .frame(width: V4.panelWidth)
                .background(Tok.panel)
                .environment(\.colorScheme, appearance == .dark ? .dark : .light)
                .fixedSize()
            return rasterise(
                view, named: "\(scene.name)-\(appearance.rawValue).png", into: directory)
        }
    }

    // MARK: - Settings > Peers controls

    /// One control, one state, one PNG.
    ///
    /// A FOURTH scene array, and the reason is the same class as
    /// ``peerSceneList``'s: these controls live on the Settings pane, whose own
    /// harness (`RenderSettings`) hosts a real window and can draw exactly ONE
    /// fixture per pane. A control with four states needs four pictures, so
    /// each one is rasterised here on its own, in the panel harness that needs
    /// no window at all.
    ///
    /// What that costs, said rather than hidden: this pictures the CONTROL,
    /// not the pane around it. The pane's own render stays `--render-settings`.
    private struct ControlScene {
        let name: String
        let view: AnyView
    }

    /// Settings > General's "Send claude through tcr" row, one PNG per thing
    /// it can say: off, each route, and a conflict in the way. The live row is a menu `Picker`, which
    /// `ImageRenderer` cannot draw, so the row's snapshot mode shows the
    /// current choice as text.
    @MainActor
    private static var claudeRouteSettingsScenes: [ControlScene] {
        let row = { (reading: ClaudeRouteCommand.Reading) in
            AnyView(
                ClaudeRouteSettingsRow(
                    controller: ClaudeRouteController(pinned: reading), snapshotMode: true))
        }
        return [
            ControlScene(
                name: "26-claude-route-settings-off",
                view: row(.status(ClaudeRouteStatus(route: .off)))),
            ControlScene(
                name: "26b-claude-route-settings-shell",
                view: row(.status(ClaudeRouteStatus(route: .shell)))),
            ControlScene(
                name: "26c-claude-route-settings-every-launch",
                view: row(.status(ClaudeRouteStatus(route: .everyLaunch)))),
            ControlScene(
                name: "26d-claude-route-settings-conflict",
                view: row(
                    .status(
                        ClaudeRouteStatus(
                            route: .off,
                            conflicts: [
                                ClaudeRouteConflict(
                                    route: "shell", kind: "alias",
                                    path: "/Users/alice/.zshrc", line: 40,
                                    text: "alias claude='claude --verbose'")
                            ])))),
        ]
    }

    /// The Tailscale row, one PNG per state. The first seven are the tailnet
    /// mockup's scene 3 plus stale, and they are the set whose frames must
    /// match pixel for pixel outside the picker label and the line's glyphs
    /// (no layout shift). The rest are the states the mockup does not draw.
    ///
    /// Built the way the pane builds it: `onOpenApp` is wired and the join
    /// and details presses are not, because their sheets are not built yet.
    @MainActor
    private static var tailnetRowScenes: [ControlScene] {
        let at = { (seconds: TimeInterval) in peerMsAgo(seconds) }
        let scene = { (name: String, read: PeerTailnetRead, failure: PeerTailnetPressFailure?) in
            ControlScene(
                name: "tn-row-\(name)",
                view: AnyView(
                    PeerTailnetRow(
                        read: read,
                        state: PeerTailnetState.state(read, pressFailure: failure, now: peerNow),
                        onOpenApp: {}, snapshotMode: true)))
        }
        let held = { (mode: PeerTailnetMode, state: PeerTailnetReading.State) in
            PeerTailnetReading.Held(mode: mode, state: state, recordedAtMs: at(5))
        }
        func read(_ mode: PeerTailnetMode, _ held: PeerTailnetReading.Held?, node: Bool = true)
            -> PeerTailnetRead
        {
            .read(tailnetReading(mode, nodeSupported: node, held: held))
        }
        return [
            scene("1-off", read(.off, nil), nil),
            scene(
                "2-app-ready",
                read(
                    .app,
                    .init(
                        mode: .app, state: .ready, address: "100.64.0.7", found: 2,
                        recordedAtMs: at(5))), nil),
            scene("3-app-not-running", read(.app, held(.app, .appNotRunning)), nil),
            scene("4-node-not-joined", read(.node, held(.node, .nodeNotJoined)), nil),
            scene("5-node-connecting", read(.node, held(.node, .nodeConnecting)), nil),
            scene(
                "6-node-ready",
                read(
                    .node,
                    .init(
                        mode: .node, state: .ready, address: "100.64.0.9",
                        nodeName: "tcr-studio", connected: 1, recordedAtMs: at(5))), nil),
            scene(
                "7-stale",
                read(
                    .app,
                    .init(
                        mode: .app, state: .ready, address: "100.64.0.7", found: 2,
                        recordedAtMs: at(90))), nil),
            scene("x-not-read", .notRead, nil),
            scene("x-unsupported-tcr", .unsupported, nil),
            scene("x-not-reported", read(.app, nil), nil),
            scene("x-applying", read(.app, held(.off, .off)), nil),
            scene("x-app-missing", read(.app, held(.app, .appMissing)), nil),
            scene(
                "x-app-unreachable",
                read(
                    .app,
                    .init(
                        mode: .app, state: .appNotRunning,
                        detail: "tailscale status exited 1", recordedAtMs: at(5))), nil),
            scene("x-unknown-word", read(.app, held(.app, .unknown("unknown"))), nil),
            scene(
                "x-unknown-setting",
                .read(
                    PeerTailnetReading(
                        mode: .off, unknownModeWord: "both", nodeSupported: false, held: nil)),
                nil),
            scene(
                "x-read-failed",
                read(
                    .app,
                    .init(
                        mode: .app, state: .readFailed,
                        detail: "tailscale status timed out after 2 s", recordedAtMs: at(5))),
                nil),
            scene(
                "x-node-unsupported", read(.node, held(.node, .nodeUnsupported), node: false),
                nil),
            scene(
                "x-unreadable",
                .unreadable("tcr peer tailnet answered something this build cannot read"), nil),
            scene(
                "x-pick-refused", read(.off, nil),
                .pickRefused("tcr peer tailnet node failed (exit 1): not in this build")),
        ]
    }

    @MainActor
    private static var controlScenes: [ControlScene] {
        claudeRouteSettingsScenes + tailnetRowScenes + [
            // The "Reachable from the internet" row, one PNG per state it can
            // be in: the switch off, the router being asked, a mapping held,
            // a router that never answered, that question asked again from
            // the row's own button, and `tcr` itself refusing. Six states,
            // six pictures, because the row draws a different sentence and a
            // different control for each and a sentence nobody can look at is
            // a sentence nobody reviews.
            ControlScene(
                name: "w12-internet-off",
                view: AnyView(PeerInternetRow(on: false, state: .off))),
            ControlScene(
                name: "w12-internet-asking",
                view: AnyView(PeerInternetRow(on: true, state: .asking))),
            ControlScene(
                name: "w12-internet-mapped",
                view: AnyView(
                    PeerInternetRow(
                        on: true,
                        state: .reachable(
                            // Documentation range (RFC 5737), like every other
                            // address in this file: the repository is public.
                            address: "203.0.113.44", port: 51413,
                            expires: peerNow.addingTimeInterval(120))))),
            ControlScene(
                name: "w12-internet-silent",
                view: AnyView(PeerInternetRow(on: true, state: .routerSilent))),
            // The row's own button pressed, which is a different state from
            // the switch being turned on: same waiting line, and the button
            // itself now reads `Asking…` and is dim. Both facts come off the
            // one value, so a picture of the row is the only way to see that
            // the button changed with it.
            ControlScene(
                name: "w12-internet-retrying",
                view: AnyView(PeerInternetRow(on: true, state: .retrying))),
            // And `tcr` itself refusing or answering something this build
            // cannot read, in its own words. Amber, like the silent router,
            // but a different sentence: a probe that failed is not a router
            // that said no, and drawing one as the other hides a broken `tcr`
            // behind a sentence about somebody's hardware.
            ControlScene(
                name: "w12-internet-unreadable",
                view: AnyView(
                    PeerInternetRow(
                        on: true, state: .unreadable("peer reach: no reply from the router")))),
            // Item 2, the mockup's scenes 2a to 2c. The third is the one a
            // plain two-option toggle would hide: switched back to serve, and
            // the borrower's old key still winding down on its own clock.
            ControlScene(
                name: "w12-mode-serve",
                view: AnyView(
                    LendModeControl(peer: "studio-mac", mode: .serve, status: nil))),
            ControlScene(
                name: "w12-mode-hand-active",
                view: AnyView(
                    LendModeControl(
                        peer: "studio-mac", mode: .hand,
                        status: PeerLease.handedKeyLine(
                            mode: .hand,
                            handedKeyUntil: Int64(peerNow.timeIntervalSince1970) + 262,
                            peer: "studio-mac", now: peerNow)))),
            // Item 3, the mockup's scenes 3a to 3d, on the real account card
            // rather than the row alone: 3d's "waiting for studio-mac" is a
            // PILL in the card header, so a picture of the row on its own
            // could not show the state the lead ruled in.
            ControlScene(
                name: "w12-exits-local",
                view: (exitsCard(.init(route: .local)))),
            ControlScene(
                name: "w12-exits-peer-soft",
                view: (exitsCard(.init(route: .via(exitPeerId))))),
            ControlScene(
                name: "w12-exits-peer-must",
                view: (exitsCard(.init(route: .via(exitPeerId), strict: true)))),
            ControlScene(
                name: "w12-exits-peer-must-waiting",
                view: exitsCard(
                    .init(
                        route: .via(exitPeerId), strict: true, peerDown: true,
                        waitingSeconds: 40))),
            ControlScene(
                name: "w12-mode-hand-winding",
                view: AnyView(
                    LendModeControl(
                        peer: "studio-mac", mode: .serve,
                        status: PeerLease.handedKeyLine(
                            mode: .serve,
                            handedKeyUntil: Int64(peerNow.timeIntervalSince1970) + 166,
                            peer: "studio-mac", now: peerNow)))),
            // A Codex account's card with the waiting scene's exit: must-locked to
            // a Mac that is down. A Codex request always leaves from this Mac, so
            // the card draws no picker, no must switch and no waiting pill, and
            // says instead that tcr refuses the account, and how to clear the lock.
            ControlScene(
                name: "w12-exits-codex",
                view: exitsCodexCard(
                    .init(
                        route: .via(exitPeerId), strict: true, peerDown: true,
                        waitingSeconds: 40))),
        ]
    }

    /// One account card in one exit state, at the panel's own width.
    ///
    /// The card is the real ``AccountCard`` over the real `alice` fixture, so
    /// a scene cannot show a card the panel would not draw; only the exit
    /// value differs between the four.
    /// The wire id an exit lock actually stores, and the row that turns it
    /// back into a name.
    ///
    /// The scenes used to pin to the string `studio-mac`, which no peers file
    /// ever holds: `egress` is `via <52-character id>`. Drawing the name
    /// without the join made a picture of a case that cannot happen and hid
    /// the one that does, a picker showing an id nobody can read.
    private static let exitPeerId = String(repeating: "0", count: 52)

    private static var exitPeerRows: [PeerListDocument.PeerEntry] {
        [PeerListDocument.PeerEntry(id: exitPeerId, name: "studio-mac", trusted: true)]
    }

    /// `healthyJSON`'s `alice` row, verbatim, except its three reset offsets
    /// are measured from ``peerNow`` rather than the real clock, the one
    /// difference `exitsCard` needs, since it is the one caller that draws
    /// this account against a `now` that never moves.
    private static var exitsAliceJSON: String {
        let alice = account(
            "alice@example.com", quota: "0.12", state: "ok", fiveHourResetInMinutes: 130,
            sevenDayResetInMinutes: 4_320, sevenDayOi: "0.21", sevenDayOiState: "ok",
            sevenDayOiResetInMinutes: 6_498, plan: "Max 20x",
            orgUuid: "11111111-1111-1111-1111-111111111111", now: peerNow)
        return "[\(alice)]"
    }

    @MainActor
    private static func exitsCard(_ exit: AccountExit) -> AnyView {
        // `alice`, same figures `healthyJSON` draws elsewhere in this file,
        // through ``exitsAliceJSON`` rather than `healthyJSON` itself so her
        // reset captions are pinned to ``peerNow`` instead of the real clock
        // this card is never drawn against. An empty fleet would mean this
        // file's own fixture stopped decoding, so the scene draws nothing
        // rather than a card invented here.
        guard let account = fleet(exitsAliceJSON).accounts.first else {
            return AnyView(EmptyView())
        }
        return AnyView(
            AccountCard(
                account: account, shape: .full, now: peerNow,
                exit: exit, exitPeers: ["studio-mac", "attic-nuc"],
                exitPeerRows: exitPeerRows, snapshotMode: true,
                actions: { EmptyView() }
            )
            .frame(width: V4.panelWidth))
    }

    /// ``exitsCard(_:)`` over canonical bob, a Codex account, with every instant
    /// pinned to ``peerNow``: the same card arguments, so the only difference is
    /// what a Codex card draws for an exit.
    @MainActor
    private static func exitsCodexCard(_ exit: AccountExit) -> AnyView {
        guard let account = fleet("[\(codexRow(.bob, now: peerNow))]").accounts.first else {
            return AnyView(EmptyView())
        }
        return AnyView(
            AccountCard(
                account: account, shape: .full, now: peerNow,
                exit: exit, exitPeers: ["studio-mac", "attic-nuc"],
                exitPeerRows: exitPeerRows, snapshotMode: true,
                actions: { EmptyView() }
            )
            .frame(width: V4.panelWidth))
    }

    /// One control scene, drawn on the pane's own card fill at the pane's own
    /// width.
    @MainActor
    private static func renderControl(
        _ scene: ControlScene, appearance: Appearance, into directory: URL
    ) -> Bool {
        withDrawingAppearance(appearance.nsAppearance) {
            let view =
                scene.view
                .padding(14)
                // 460 pt: the detail column of the Settings window, which is
                // the width these controls really get (`settings-peers-short`
                // and `PeerPaneLayout`).
                .frame(width: 460, alignment: .leading)
                .background(Tok.cardFill)
                .environment(\.colorScheme, appearance == .dark ? .dark : .light)
                .fixedSize(horizontal: false, vertical: true)

            return rasterise(
                view, named: "\(scene.name)-\(appearance.rawValue).png", into: directory)
        }
    }

    /// Rasterise one view to a PNG under `directory`. The single writer for
    /// every scene in this file, so a panel render and a sheet render cannot
    /// drift onto two different scales.
    @MainActor
    private static func rasterise<V: View>(_ view: V, named name: String, into directory: URL)
        -> Bool
    {
        let renderer = ImageRenderer(content: view)
        // 2x so the PNG shows what a Retina panel draws — hairlines and 10pt text
        // are exactly where a 1x render would flatter the design.
        renderer.scale = 2
        renderer.proposedSize = .unspecified

        guard let image = renderer.nsImage,
            let tiff = image.tiffRepresentation,
            let rep = NSBitmapImageRep(data: tiff),
            let png = rep.representation(using: .png, properties: [:])
        else {
            FileHandle.standardError.write(Data("render failed: \(name)\n".utf8))
            return false
        }

        let url = directory.appendingPathComponent(name)
        do {
            try png.write(to: url)
            print("  \(name)  \(Int(image.size.width))x\(Int(image.size.height))pt")
            return true
        } catch {
            FileHandle.standardError.write(Data("write failed \(name): \(error)\n".utf8))
            return false
        }
    }

    /// Draw `view` in a real window, sized the way the popover sizes the panel,
    /// and capture what the window server composited (``liveListScenes``).
    ///
    /// `ImageRenderer` does not draw a `ScrollView` and performs one layout
    /// pass, so the measured list heights never arrive; a window does both.
    /// The capture is `CGWindowListCreateImage` on this process's own window,
    /// as `RenderSettings` does it, which needs no Screen Recording grant. The
    /// window is ordered front without activating the app and closed after.
    @MainActor
    private static func rasteriseInWindow<V: View>(
        _ view: V, appearance: Appearance, named name: String, into directory: URL
    ) -> Bool {
        let hosting = NSHostingController(rootView: view)
        // What `MenuBarShell.hosting(rootView:)` sets: the popover takes the
        // panel's own ideal size.
        hosting.sizingOptions = [.preferredContentSize]
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 400, height: 400),
            styleMask: [.borderless], backing: .buffered, defer: false)
        window.appearance = appearance.nsAppearance
        window.contentViewController = hosting
        window.center()
        window.orderFrontRegardless()
        // Until two turns in a row agree: the lists measure their content in
        // one pass and take that height in the next.
        var size = NSSize.zero
        var stable = 0
        for _ in 0..<40 where stable < 2 {
            RunLoop.current.run(until: Date().addingTimeInterval(0.05))
            let preferred = hosting.preferredContentSize
            if preferred == size {
                stable += 1
            } else {
                stable = 0
                size = preferred
                window.setContentSize(preferred)
                window.contentView?.layoutSubtreeIfNeeded()
            }
        }
        for _ in 0..<3 {
            RunLoop.current.run(until: Date().addingTimeInterval(0.05))
        }
        guard stable >= 2, size.height > 0,
            let cgImage = CGWindowListCreateImage(
                .null, .optionIncludingWindow, CGWindowID(window.windowNumber),
                [.boundsIgnoreFraming, .bestResolution])
        else {
            window.close()
            FileHandle.standardError.write(
                Data("render failed: \(name) (size \(size), stable \(stable))\n".utf8))
            return false
        }
        window.close()
        let rep = NSBitmapImageRep(cgImage: cgImage)
        guard let png = rep.representation(using: .png, properties: [:]) else {
            FileHandle.standardError.write(Data("render failed: \(name)\n".utf8))
            return false
        }
        let url = directory.appendingPathComponent(name)
        do {
            try png.write(to: url)
            print(
                "  \(name)  panel \(String(format: "%.1f", size.width))x"
                    + "\(String(format: "%.1f", size.height))pt  \(cgImage.width)x\(cgImage.height)px")
            return true
        } catch {
            FileHandle.standardError.write(Data("write failed \(name): \(error)\n".utf8))
            return false
        }
    }

    /// `wait` open on the one scene that reviews the disclosure — the
    /// biggest class, and the one `sessionsFixture` gives commands to from
    /// both of its tool-carrying sessions.
    private static func expandedTimeoutClassesFixture(for sceneName: String) -> Set<String> {
        sceneName == "17b-tools-tab-timeout-class-open" ? ["wait"] : []
    }

    /// One running Bash row's process reading, fixed — the mockup's own
    /// `640% · 2.1G`. Never ``ProcessTable/read()``: the harness must
    /// draw the same pixels on a quiet laptop and on a box mid-build, the
    /// same rule ``machineFixture`` states for the machine line.
    ///
    /// The key can only be derived from THIS scene's own fleet, not written
    /// down: ``SessionToolEntry/id`` contains the call's `startedMs`, and
    /// `sessionsFixture` stamps those off the real clock so its ages read
    /// like a live fleet's.
    ///
    /// Exactly ONE of the two running Bash calls is seeded, on purpose. The
    /// other renders the case that is just as common live and twice as easy
    /// to get wrong: a call this build matched no process to, which draws no
    /// cpu/memory clause and NO ✕ at all. A fixture that seeded both would
    /// leave the refusal unreviewed.
    private static func runningProcessesFixture(for sceneName: String, state: PollState)
        -> [String: RunningCallStats]
    {
        guard
            sceneName == "17-tools-tab" || sceneName == "17b-tools-tab-timeout-class-open"
                || sceneName == "17c-tools-tab-codex",
            case .loaded(let fleet) = state,
            let entry = fleet.toolsRunning.first(where: { $0.call.tool == "Bash" })
        else { return [:] }
        return [
            entry.id: RunningCallStats(
                pid: 48765,
                processGroup: 48765,
                cpuSeconds: 1_294.7,
                residentBytes: 2_254_857_830,
                cpuPercent: 640,
                readAt: referenceDate)
        ]
    }

    /// The machine line's numbers, fixed — `docs/design/tools-tab.md`'s own
    /// line, verbatim. Never ``MachineStats/read()``: a render must produce
    /// the same pixels on a quiet laptop and on a box running five compiles,
    /// or the harness is comparing this machine's load rather than the
    /// panel's layout.
    ///
    /// The tint here is `calm`, not amber: 7.1 on 14 cores is half a load
    /// unit per core, and this build tints by Gil's dispatch rule (amber from
    /// 1x, red past 2x) rather than by the mockup's own `warn` class on this
    /// same line, which the rule contradicts. Measured on the render, not
    /// read off the code — a pixel scan for `#ffd16b` finds amber in the four
    /// TIMED OUT bars and nowhere on the machine line. The bands themselves
    /// are pinned by `MachineStatsTests.testLoadTintBands`, which is where a
    /// threshold belongs; a fixture chosen to picture one band would have
    /// cost this scene the mockup's own numbers, and scene 17 is compared
    /// against the mockup crop.
    private static let machineFixture = MachineStats(
        loadAverage: 7.1,
        cores: 14,
        memoryUsedBytes: 48 * 1_073_741_824,
        memoryTotalBytes: 64 * 1_073_741_824,
        compiles: 5,
        diskFreeBytes: 210_000_000_000)

    /// The machine reading of each heat scene, `docs/design/fan-readout-mockup.html`.
    /// Panels 2, 4 and 5 are ``machineFixture`` with a heat reading added, so
    /// the numbers line is the only thing that differs from scene 17. The
    /// heat figures of panel 2 are the ones measured on a Mac16,7 on
    /// 2026-10-18; panels 4 and 5 are the mockup's illustrative ones. Panel
    /// 6 is every clause at its widest, design figures, none of them measured:
    /// it is the line that must still fit the reserved lines.
    private static let heatScenes: [String: MachineStats] = {
        func machine(_ heat: HeatReading, _ fanControl: FanControl? = nil) -> MachineStats {
            MachineStats(
                loadAverage: machineFixture.loadAverage, cores: machineFixture.cores,
                memoryUsedBytes: machineFixture.memoryUsedBytes,
                memoryTotalBytes: machineFixture.memoryTotalBytes,
                compiles: machineFixture.compiles, diskFreeBytes: machineFixture.diskFreeBytes,
                heat: heat, fanControl: fanControl)
        }
        func fanScene(_ panel: FanScene) -> MachineStats {
            machine(panel.heat, panel.fanControl)
        }
        func fan(_ index: Int, _ actual: Double, manual: Bool) -> FanReading {
            FanReading(index: index, actualRPM: actual, minRPM: 1_350, maxRPM: 5_777, isManual: manual)
        }
        return [
            // "CPU 93°C · fans 87% manual"
            "17d-tools-tab-heat-manual": machine(
                HeatReading(
                    cpuCelsius: 93.3, fans: [fan(0, 5_013, manual: true), fan(1, 4_976, manual: true)])),
            // "CPU 46°C · fans off"
            "17e-tools-tab-heat-fans-off": machine(
                HeatReading(cpuCelsius: 46, fans: [fan(0, 0, manual: false), fan(1, 0, manual: false)])),
            // "CPU 61°C", a Mac with no fans
            "17f-tools-tab-heat-no-fans": machine(HeatReading(cpuCelsius: 61, fans: [])),
            // "load 168.4/14 · CPU 105°C · fans 100% manual · 128/128 GB ·
            // 999 compiles · 1024 GB free", then the fleet's "99 err today ·
            // 9999 timed out" from ``heatWorstCaseFleet``. Three-digit
            // compiles and four-digit timeouts are design figures, not
            // measured: wider than any seen, so the reserve is tested past them.
            "17g-tools-tab-heat-worst-case": MachineStats(
                loadAverage: 168.4, cores: 14, memoryUsedBytes: 128 * 1_073_741_824,
                memoryTotalBytes: 128 * 1_073_741_824, compiles: 999,
                diskFreeBytes: 1_024_000_000_000,
                heat: HeatReading(
                    cpuCelsius: 105, fans: [fan(0, 5_777, manual: true), fan(1, 5_777, manual: true)])),
            // "CPU 46°C · fans off", panels 7 and 8.
            "17h-tools-tab-fan-install": fanScene(.install),
            "17i-tools-tab-fan-approval": fanScene(.approval),
            // "CPU 93°C · fans 87% fixed", panel 9.
            "17j-tools-tab-fan-fixed": fanScene(.fixed),
            // "CPU 93°C · fans 100%", panel 10.
            "17k-tools-tab-fan-follow-cpu": fanScene(.followCPU),
            // "CPU 93°C · fans 87% manual", panel 11.
            "17l-tools-tab-fan-another-app": fanScene(.anotherApp),
        ]
    }()

    // MARK: - Fan control, mockup panels 7 to 11

    /// The facts behind each fan panel: the poll's `heat` and `fanControl`,
    /// and macOS's answer for the helper. Panel 11's heat is the Mac16,7
    /// reading of 2026-10-18 with Macs Fan Control holding both fans at 5,000
    /// rpm; the rest are the mockup's illustrative figures.
    enum FanScene: CaseIterable {
        case install, approval, fixed, followCPU, anotherApp

        private static func fan(_ index: Int, _ actual: Double, target: Double?) -> FanReading {
            FanReading(
                index: index, actualRPM: actual, minRPM: 1_350, maxRPM: 5_777,
                isManual: target != nil, targetRPM: target)
        }

        /// A quiet box, fans automatic and stopped (7, 8); both held at 5,000
        /// (9, 11); Follow CPU past full speed (10).
        var heat: HeatReading {
            switch self {
            case .install, .approval:
                return HeatReading(
                    cpuCelsius: 46, fans: [Self.fan(0, 0, target: nil), Self.fan(1, 0, target: nil)])
            case .fixed, .anotherApp:
                return HeatReading(
                    cpuCelsius: 93.3,
                    fans: [Self.fan(0, 5_013, target: 5_000), Self.fan(1, 4_976, target: 5_000)])
            case .followCPU:
                return HeatReading(
                    cpuCelsius: 93,
                    fans: [Self.fan(0, 5_777, target: 5_777), Self.fan(1, 5_777, target: 5_777)])
            }
        }

        /// 7 and 8: Fixed asked for at the slowest speed, since the fans were at
        /// rest, and the proxy cannot reach a helper yet.
        var fanControl: FanControl {
            switch self {
            case .install, .approval:
                return FanControl(
                    desired: .fixed(rpm: 1_350), state: .unavailable, reason: .helperUnreachable)
            case .fixed:
                return FanControl(
                    desired: .fixed(rpm: 5_000), state: .holding,
                    targets: [FanTarget(fan: 0, rpm: 5_000), FanTarget(fan: 1, rpm: 5_000)])
            case .followCPU:
                return FanControl(
                    desired: .followCPU(.starting), state: .holding,
                    targets: [FanTarget(fan: 0, rpm: 5_777), FanTarget(fan: 1, rpm: 5_777)])
            case .anotherApp:
                return FanControl(desired: .auto, state: .foreign, reason: .macsFanControl)
            }
        }

        var registration: FanHelperRegistration.Status {
            switch self {
            case .install: return .notRegistered
            case .approval: return .requiresApproval
            case .fixed, .followCPU, .anotherApp: return .enabled
            }
        }

        var popoverSceneName: String {
            switch self {
            case .install: return "17h-fan-popover-install"
            case .approval: return "17i-fan-popover-approval"
            case .fixed: return "17j-fan-popover-fixed"
            case .followCPU: return "17k-fan-popover-follow-cpu"
            case .anotherApp: return "17l-fan-popover-another-app"
            }
        }
    }

    /// The popover's content, one PNG per mockup panel, from a pinned model:
    /// no `tcr fan`, no registration read or write.
    private static var fanPopoverScenes: [(name: String, scene: FanScene)] {
        FanScene.allCases.map { ($0.popoverSceneName, $0) }
    }

    @MainActor
    private static func renderFanPopover(
        _ entry: (name: String, scene: FanScene), appearance: Appearance, into directory: URL
    ) -> Bool {
        withDrawingAppearance(appearance.nsAppearance) {
            let model = FanControlModel.pinned(
                fanControl: entry.scene.fanControl, heat: entry.scene.heat,
                registration: entry.scene.registration)
            let view = FanPopover(fans: model)
                .background(Tok.panel)
                .environment(\.colorScheme, appearance == .dark ? .dark : .light)
                .fixedSize()
            return rasterise(view, named: "\(entry.name)-\(appearance.rawValue).png", into: directory)
        }
    }

    /// Scene 17's fleet plus one session that carries no running call, sized
    /// so the fleet's totals read `99 err today · 9999 timed out`: the
    /// mockup's widest errors and a four-digit timeout count, a design figure
    /// wider than the mockup's 128. Derived from the base totals rather than
    /// written down, so a change to `sessionsFixture` cannot quietly shorten
    /// the worst case.
    private static var heatWorstCaseFleet: Fleet {
        let base = sessionsTabFleet
        let now = Int64(referenceDate.timeIntervalSince1970 * 1000)
        let padding = Session(
            sessionId: "cccccccc-1111-2222-3333-444444444444", account: base.accounts.first?.name,
            model: "claude-sonnet-5", firstSeenMs: now - 3_600_000, lastSeenMs: now, requests: 10,
            tools: SessionTools(
                calls: 300, errors: max(0, 99 - base.toolsTotalErrors),
                timeouts: max(0, 9_999 - base.toolsTotalTimeouts)))
        return Fleet(
            accounts: base.accounts, unreadable: base.unreadable, sessions: base.sessions + [padding],
            sessionsSupported: true)
    }

    /// Tall enough that thirteen rows are all visible rather than scrolled. This
    /// is a review artifact, so seeing everything beats fidelity to the clip.
    private static let renderHeight: CGFloat = 900

    /// The three scenes that are compared against the mockup crops. Each is
    /// rendered with a SUPERVISED server for the reason
    /// `ServerController.harness(pinned:)` states: the mockup's proxy was
    /// running, and a panel drawing "Start server" and "Take over port…" differs
    /// from it by a fact about this machine rather than by a layout decision.
    private static let parityScenes: Set<String> = [
        "19-accounts-tab-parity", "16-sessions-tab", "17-tools-tab",
    ]

    /// Every scene drawn with a running, supervised proxy: the parity scenes,
    /// the README's top image, which should look like a reader's own panel
    /// rather than a stopped one, and the heat scenes, which must differ from
    /// scene 17 by the numbers line alone.
    private static let supervisedScenes: Set<String> =
        parityScenes.union(["01b-healthy-running"]).union(heatScenes.keys)

    /// The poll time every scene's header shows. "updated … ago" is measured
    /// against the real clock, so a poll pinned to `referenceDate` read
    /// "updated 50d ago" and grew by a day every day. A poll two seconds
    /// before the render reads "updated 2s ago" whatever the date.
    private static var recentPoll: Date { Date().addingTimeInterval(-2) }

    // MARK: - Fixtures
    //
    // Fake accounts only. This repository is public and real account addresses
    // never enter it. A fixed reference date keeps renders byte-comparable
    // between runs, so a diff means the UI changed rather than the clock did.

    private static let referenceDate = Date(timeIntervalSince1970: 1_786_000_000)

    private static func fleet(_ json: String) -> Fleet {
        (try? Fleet.decode(Data(json.utf8))) ?? Fleet(accounts: [])
    }

    /// A fleet with a genuinely undecodable row, which is otherwise almost
    /// impossible to observe on demand.
    private static func partiallyUnreadableFleet() -> Fleet {
        let good = fleet(healthyJSON)
        return Fleet(
            accounts: good.accounts,
            unreadable: [
                Fleet.UnreadableRow(
                    index: 2,
                    message: "valueNotFound: expected Double, found null at .quota")
            ]
        )
    }

    private static func account(
        _ name: String,
        quota: String,
        state: String,
        disabled: Bool = false,
        probe: String = "ok",
        held: String = "[]",
        source: String = "live",
        status: String = "active",
        // Per-window overrides, all defaulting to the composite `quota`/`state`
        // — every EXISTING call site keeps rendering exactly the "5h == 7d"
        // fixture it always has. Only `divergentWindowsJSON` below passes
        // these explicitly, to build the ONE scene where the two windows
        // genuinely disagree — the shape every other fixture here cannot
        // exercise and a swapped 5h/7d binding would render identically to
        // the correct one against.
        fiveHour: String? = nil,
        fiveHourState: String? = nil,
        sevenDay: String? = nil,
        sevenDayState: String? = nil,
        // Minutes from NOW until each window resets, written onto the wire as
        // epoch milliseconds the way `tcr` sends them.
        //
        // Relative rather than a fixed timestamp because
        // `QuotaFormat.resetCaption` returns nil for a reset that is not in the
        // future: a hard-coded epoch draws a caption today and nothing a month
        // from now, deleting the thing these scenes exist to show. The cost is
        // that the caption's digits differ between renders.
        fiveHourResetInMinutes: Int? = nil,
        sevenDayResetInMinutes: Int? = nil,
        // The FABLE weekly window — a third window with its own reset, gating
        // Fable requests only. Independent of the two above on purpose: a row
        // whose 7d window is spent can have Fable headroom and the reverse, and
        // a fixture that tied them together could not show it.
        //
        // NOT MEASURED by default — an older server, or a window never learned
        // for this account, which is the majority of a real fleet: the window
        // is only learned once a Fable request has been served on that account.
        // It must draw an EMPTY slot, never `n/a` and never `0%`, so every
        // scene that does not opt in is also the negative case. Scenes 01 and
        // 14 opt in.
        sevenDayOi: String = "null",
        // `"null"` is the shape a proxy predating `sevenDayOiState` sends: a
        // real fraction with no state word beside it, which draws the figure in
        // the neutral tint rather than borrowing the composite state. Scene 14
        // carries one of those too.
        sevenDayOiState: String = "null",
        sevenDayOiResetInMinutes: Int? = nil,
        // The `usage` object, as raw JSON. Defaults to the measured shape, so
        // every existing scene shows the spend line the panel now draws.
        // `"null"` is the not-measured row — an older proxy, or an offline
        // read — and it must render as an EMPTY slot, never as `$0.00`; see
        // `unmeasuredUsage` and scene `14-usage-stats`.
        usage: String = measuredUsage,
        // Group labels, wire fields `"groups"`/`"reservedGroups"`
        // (`FleetStatus.swift:933-953`). `nil` omits both keys entirely, the
        // shape every existing call site keeps decoding — see
        // `Account.groups`'s own doc-comment on why a missing key and an
        // empty array are kept distinct rather than collapsed.
        groups: [String]? = nil,
        reservedGroups: [String]? = nil,
        // The parked subset, wire field `"parkedGroups"`. Rides with `groups`
        // exactly as `reservedGroups` does, so a scene that passes no `groups`
        // is also the negative case for this key.
        parkedGroups: [String]? = nil,
        // Fleet-wide group colours, wire field `"groupColors"`, repeated per
        // row the way the server sends them. `nil` omits the key — the
        // older-server shape, which every pre-existing scene keeps, and which
        // draws every tag in the neutral fallback.
        groupColors: [String: String]? = nil,
        // The plan label the SERVER derived, wire field `"plan"`. `nil` omits
        // the key entirely — the never-profiled row and the older-server row
        // alike, both of which must draw NO tag rather than a guessed one, so
        // every scene that does not opt in is also the negative case.
        plan: String? = nil,
        // The org this row belongs to, wire field `"orgUuid"`. It is what makes
        // two rows sharing a name distinct: `Account.id` is org-qualified, so a
        // fixture pair with one email and two orgs renders as TWO rows here and
        // collapsed to one before that fix. `nil` omits the key, which is the
        // older-server shape.
        orgUuid: String? = nil,
        gate: String? = nil,
        // Requests served since this proxy started, wire field `"requests"`.
        // Every existing call site keeps the measured 102 it always had;
        // only the no-requests banner scene passes 0, which is the whole
        // fact ``NoRequestsBanner`` reads off this field.
        requests: Int = 102,
        // What "NOW" means for the three `*ResetInMinutes` offsets above.
        // Defaults to the real clock, which every existing call site keeps:
        // those scenes draw through `FleetView`, whose own `now:` is also
        // the real clock (`AccountsTabV4`'s `now: Date()`), so fixture and
        // consumer read the same moving instant and the caption stays
        // correct. `exitsCard` is the one consumer pinned to `peerNow`
        // instead: it passes `now: peerNow` here so its own fixture's
        // reset offsets are measured from the same fixed instant its card
        // renders against, rather than a real `Date()` that keeps moving
        // out from under a `now` that does not.
        now: Date = Date(),
        // Wire `"provider"`. `nil` omits the key, the Anthropic row and the
        // older-server row alike, which every existing scene keeps.
        provider: String? = nil,
        // Any other wire keys, laid over the row last, so they win. Empty for
        // every existing scene, which therefore builds the same text it always
        // has.
        extra: [String: Any] = [:]
    ) -> String {
        func resetAtMs(_ minutes: Int?) -> String {
            guard let minutes else { return "null" }
            let at = now.addingTimeInterval(Double(minutes) * 60)
            return "\(Int64(at.timeIntervalSince1970 * 1000))"
        }
        func jsonArray(_ values: [String]) -> String {
            "[" + values.map { "\"\($0)\"" }.joined(separator: ",") + "]"
        }
        let groupsFragment =
            groups.map { g in
                "\"groups\":\(jsonArray(g)),\"reservedGroups\":\(jsonArray(reservedGroups ?? [])),"
                    + "\"parkedGroups\":\(jsonArray(parkedGroups ?? [])),"
            } ?? ""
        let colorsFragment =
            groupColors.map { colors in
                let pairs =
                    colors
                    .sorted { $0.key < $1.key }
                    .map { "\"\($0.key)\":\"\($0.value)\"" }
                    .joined(separator: ",")
                return "\"groupColors\":{\(pairs)},"
            } ?? ""
        let fh = fiveHour ?? quota
        let fhState = fiveHourState ?? state
        let sd = sevenDay ?? quota
        let sdState = sevenDayState ?? state
        // `fiveHourState`/`sevenDayState` are JSON string fields on the wire
        // ("ok"/"near"/"spent") but the proof fixture for the unmeasured-
        // window overclaim needs to write a genuine JSON `null`, not the
        // string `"null"` — `QuotaState?` decodes the STRING "null" as
        // `.unknown("null")`, a real (if odd) value, which would silently
        // defeat the one scene built to prove a window has NO reading.
        // `quote(_:)` keeps every other call site (a real state word)
        // wrapped in quotes and only passes `null` through bare.
        func quote(_ raw: String) -> String {
            raw == "null" ? "null" : "\"\(raw)\""
        }
        let planFragment = plan.map { "\"plan\":\"\($0)\"," } ?? ""
        let orgFragment = orgUuid.map { "\"orgUuid\":\"\($0)\"," } ?? ""
        let gateFragment = gate.map { "\"gate\":\"\($0)\"," } ?? ""
        let row = """
            {"name":"\(name)","priority":0,"status":"\(status)","disabled":\(disabled),
             \(planFragment)\(orgFragment)\(gateFragment)
             "quota":\(quota),"quotaState":"\(state)","fiveHour":\(fh),
             "fiveHourState":\(quote(fhState)),"sevenDay":\(sd),"sevenDayState":\(quote(sdState)),
             "sevenDayOi":\(sevenDayOi),"sevenDayOiState":\(quote(sevenDayOiState)),
             \(groupsFragment)\(colorsFragment)"held":\(held),
             "fiveHourResetAtMs":\(resetAtMs(fiveHourResetInMinutes)),
             "sevenDayResetAtMs":\(resetAtMs(sevenDayResetInMinutes)),
             "sevenDayOiResetAtMs":\(resetAtMs(sevenDayOiResetInMinutes)),
             "requests":\(requests),"inputTokens":8781926,"outputTokens":31860,
             "cacheReadTokens":7407414,"cacheCreationTokens":\(usage == "null" ? "null" : "1200000"),
             "cacheHitRatio":0.84,"probeStatus":"\(probe)",
             "probeError":null,"lastStreamError":null,"streamErrorCount":0,
             "source":"\(source)","serverSha":"abc1234","serverDirty":false,
             "usage":\(usage)}
            """
        guard provider != nil || !extra.isEmpty else { return row }
        guard
            var object = (try? JSONSerialization.jsonObject(with: Data(row.utf8)))
                as? [String: Any]
        else { preconditionFailure("account(\(name)) did not build a JSON object") }
        if let provider { object["provider"] = provider }
        return jsonText(object, laying: extra)
    }

    // MARK: Usage fixtures
    //
    // The numbers RECONCILE with the row-level counters above, because a
    // fixture that does not is a fixture that teaches a reader a wrong
    // relationship: the row's `inputTokens` is the QUOTA counter and folds all
    // three input dimensions together, so base input (174512) + cache creation
    // (1200000) + cache reads (7407414) == 8781926, and 7407414/8781926 is the
    // 0.84 `cacheHitRatio` already on the row. The per-model buckets sum to
    // `today` on every field, the same way the server's own do.
    //
    // `cacheCreation1hTokens` is a SUBSET of `cacheCreationTokens`, never an
    // addend — 400000 of today's 1200000 written at the long TTL — which is
    // what the server sends and what `UsageTotals.cacheHitRatio` divides by. A
    // fixture carrying 0 there could not tell the two readings apart.

    /// A fully measured, fully priced account: two models, a named quota
    /// window, and nothing unpriced.
    private static let measuredUsage = """
        {"today":{"requests":102,"inputTokens":174512,"cacheCreationTokens":1200000,
          "cacheCreation1hTokens":400000,"cacheReadTokens":7407414,"outputTokens":31860,
          "costUsd":14.1657,"unpricedRequests":0},
         "window":{"requests":40,"inputTokens":68000,"cacheCreationTokens":471000,
          "cacheCreation1hTokens":157000,"cacheReadTokens":2900000,"outputTokens":12476,
          "costUsd":5.6141,"unpricedRequests":0,"since":1767207600000},
         "lastHour":{"requests":12,"inputTokens":20000,"cacheCreationTokens":141000,
          "cacheCreation1hTokens":47000,"cacheReadTokens":705000,"outputTokens":3756,
          "costUsd":1.6413,"unpricedRequests":0},
         "todayByModel":{
           "claude-opus-5":{"requests":70,"inputTokens":122158,"cacheCreationTokens":840000,
            "cacheCreation1hTokens":280000,"cacheReadTokens":5185190,"outputTokens":21140,
            "costUsd":12.0785,"unpricedRequests":0},
           "claude-sonnet-5":{"requests":32,"inputTokens":52354,"cacheCreationTokens":360000,
            "cacheCreation1hTokens":120000,"cacheReadTokens":2222224,"outputTokens":10720,
            "costUsd":2.0872,"unpricedRequests":0}}}
        """

    /// The same measured shape as ``measuredUsage`` with the three figures the
    /// panel actually PRINTS dialled to a caller's numbers: today's spend (a
    /// collapsed group's "· $8.42 today"), and the window's spend and output
    /// tokens (an account card's "$540 · 1.5M out").
    ///
    /// Built as a format, not by rewriting `measuredUsage`'s text: a
    /// string-replace on a shared JSON literal would hit whichever bucket
    /// happened to carry the same digits, and the parity fixtures below need
    /// exactly these three to move and the rest to stay put.
    private static func measuredUsage(
        todayCost: Double, windowCost: Double, windowOutputTokens: Int
    ) -> String {
        """
        {"today":{"requests":102,"inputTokens":174512,"cacheCreationTokens":1200000,
          "cacheCreation1hTokens":400000,"cacheReadTokens":7407414,"outputTokens":31860,
          "costUsd":\(todayCost),"unpricedRequests":0},
         "window":{"requests":40,"inputTokens":68000,"cacheCreationTokens":471000,
          "cacheCreation1hTokens":157000,"cacheReadTokens":2900000,
          "outputTokens":\(windowOutputTokens),
          "costUsd":\(windowCost),"unpricedRequests":0,"since":1767207600000},
         "lastHour":{"requests":12,"inputTokens":20000,"cacheCreationTokens":141000,
          "cacheCreation1hTokens":47000,"cacheReadTokens":705000,"outputTokens":3756,
          "costUsd":1.6413,"unpricedRequests":0},
         "todayByModel":{
           "claude-opus-5":{"requests":70,"inputTokens":122158,"cacheCreationTokens":840000,
            "cacheCreation1hTokens":280000,"cacheReadTokens":5185190,"outputTokens":21140,
            "costUsd":12.0785,"unpricedRequests":0},
           "claude-sonnet-5":{"requests":32,"inputTokens":52354,"cacheCreationTokens":360000,
            "cacheCreation1hTokens":120000,"cacheReadTokens":2222224,"outputTokens":10720,
            "costUsd":2.0872,"unpricedRequests":0}}}
        """
    }

    /// Nothing this account served could be priced: `costUsd` is null in every
    /// bucket and `unpricedRequests` says how many requests are missing from
    /// the figure. The card must print the token count ALONE — no `$0.00` —
    /// and the header must append `N unpriced`.
    private static let unpricedUsage = """
        {"today":{"requests":102,"inputTokens":174512,"cacheCreationTokens":1200000,
          "cacheCreation1hTokens":400000,"cacheReadTokens":7407414,"outputTokens":31860,
          "costUsd":null,"unpricedRequests":102},
         "window":{"requests":40,"inputTokens":68000,"cacheCreationTokens":471000,
          "cacheCreation1hTokens":157000,"cacheReadTokens":2900000,"outputTokens":12476,
          "costUsd":null,"unpricedRequests":40,"since":1767207600000},
         "lastHour":{"requests":12,"inputTokens":20000,"cacheCreationTokens":141000,
          "cacheCreation1hTokens":47000,"cacheReadTokens":705000,"outputTokens":3756,
          "costUsd":null,"unpricedRequests":12},
         "todayByModel":{
           "claude-sonnet-4-5-20250929":{"requests":102,"inputTokens":174512,
            "cacheCreationTokens":1200000,"cacheCreation1hTokens":400000,
            "cacheReadTokens":7407414,"outputTokens":31860,"costUsd":null,
            "unpricedRequests":102}}}
        """

    /// Measured and priced, but the server cannot name when this account's
    /// 5-hour window started, so `window` is null and the card falls back to
    /// the DAY's figures — which are still a measurement.
    ///
    /// Its day is split across FOUR priced models, which is what carries the
    /// header line's own case. It was one model until the line stopped
    /// collapsing its tail to `"+N"`; with two labels in the whole fleet, the
    /// scene built to review that line could not show what it now does. Four
    /// here plus the unpriced `sonnet-4-5` the other two rows carry gives the
    /// header five entries — one more than the live fleet this was measured
    /// against ran on 2026-08-30 — so the scene reviews the wrap at a width
    /// past the ordinary case rather than short of it.
    ///
    /// `claude-haiku-4-5` is $0.0102 of $22.25 on purpose: 0.05% of the fleet's
    /// day, which is the slice that rounds to zero. It draws `haiku-4-5 <1%`,
    /// and a scene showing `haiku-4-5 0%` is the regression — a model that
    /// served 2 requests reported as having spent nothing. See
    /// ``QuotaFormat/share(_:)``.
    ///
    /// The buckets still sum to `today` on every field, the way the rest of
    /// these fixtures do: 60+25+15+2 requests, and $6.90 + $2.00 + $0.50 +
    /// $0.0102 = the $9.4102 above.
    private static let noWindowUsage = """
        {"today":{"requests":102,"inputTokens":174512,"cacheCreationTokens":1200000,
          "cacheCreation1hTokens":400000,"cacheReadTokens":7407414,"outputTokens":31860,
          "costUsd":9.4102,"unpricedRequests":0},
         "window":null,
         "lastHour":{"requests":12,"inputTokens":20000,"cacheCreationTokens":141000,
          "cacheCreation1hTokens":47000,"cacheReadTokens":705000,"outputTokens":3756,
          "costUsd":1.1021,"unpricedRequests":0},
         "todayByModel":{
           "claude-opus-5":{"requests":60,"inputTokens":100000,
            "cacheCreationTokens":700000,"cacheCreation1hTokens":230000,
            "cacheReadTokens":4400000,"outputTokens":18000,"costUsd":6.9,
            "unpricedRequests":0},
           "claude-fable-5":{"requests":25,"inputTokens":45000,
            "cacheCreationTokens":300000,"cacheCreation1hTokens":100000,
            "cacheReadTokens":1900000,"outputTokens":8000,"costUsd":2.0,
            "unpricedRequests":0},
           "claude-sonnet-5":{"requests":15,"inputTokens":27000,
            "cacheCreationTokens":190000,"cacheCreation1hTokens":65000,
            "cacheReadTokens":1050000,"outputTokens":5300,"costUsd":0.5,
            "unpricedRequests":0},
           "claude-haiku-4-5-20251001":{"requests":2,"inputTokens":2512,
            "cacheCreationTokens":10000,"cacheCreation1hTokens":5000,
            "cacheReadTokens":57414,"outputTokens":560,"costUsd":0.0102,
            "unpricedRequests":0}}}
        """

    /// Priced, but not all of it: 12 of today's 102 requests ran on a model
    /// this build has no rate for, so `costUsd` is a FLOOR and
    /// `unpricedRequests` says by how much. The card must print `$5.61+` —
    /// the fleet header's `N unpriced` clause is computed from `today` and
    /// cannot speak for one account's window.
    ///
    /// Its `todayByModel` carries both the priced model and the unpriced one,
    /// summing to `today` the way the server's own buckets do, so the header
    /// has a model whose share is genuinely unknown to render as `?`.
    private static let partiallyPricedUsage = """
        {"today":{"requests":102,"inputTokens":174512,"cacheCreationTokens":1200000,
          "cacheCreation1hTokens":400000,"cacheReadTokens":7407414,"outputTokens":31860,
          "costUsd":12.8402,"unpricedRequests":12},
         "window":{"requests":40,"inputTokens":68000,"cacheCreationTokens":471000,
          "cacheCreation1hTokens":157000,"cacheReadTokens":2900000,"outputTokens":12476,
          "costUsd":5.6141,"unpricedRequests":12,"since":1767207600000},
         "lastHour":{"requests":12,"inputTokens":20000,"cacheCreationTokens":141000,
          "cacheCreation1hTokens":47000,"cacheReadTokens":705000,"outputTokens":3756,
          "costUsd":1.6413,"unpricedRequests":0},
         "todayByModel":{
           "claude-opus-5":{"requests":90,"inputTokens":154512,"cacheCreationTokens":1100000,
            "cacheCreation1hTokens":360000,"cacheReadTokens":6907414,"outputTokens":27860,
            "costUsd":12.8402,"unpricedRequests":0},
           "claude-sonnet-4-5-20250929":{"requests":12,"inputTokens":20000,
            "cacheCreationTokens":100000,"cacheCreation1hTokens":40000,
            "cacheReadTokens":500000,"outputTokens":4000,"costUsd":null,
            "unpricedRequests":12}}}
        """

    /// Not measured: an offline read, or a proxy built before `usage` existed.
    /// The 5h slot stays empty and the header line is not drawn at all.
    private static let unmeasuredUsage = "null"

    private static let hold =
        #"[{"window":"7d","minutesUntilReset":6498,"resetAtMs":1786406400224}]"#

    /// A `held` array holding the 5-hour window until `minutes` from the
    /// render clock, the way `tcr` sends it. Relative for the reason
    /// `fiveHourResetInMinutes` is: a fixed epoch stops drawing a countdown
    /// once it has passed.
    private static func heldFiveHour(inMinutes minutes: Int) -> String {
        let resetAtMs = Int64((Date().timeIntervalSince1970 + Double(minutes) * 60) * 1000)
        return #"[{"window":"5h","minutesUntilReset":\#(minutes),"resetAtMs":\#(resetAtMs)}]"#
    }

    /// Two healthy accounts, and the row's two shapes side by side: alice has a
    /// reset on every window and carries a caption beside each percentage; bob
    /// has none on the wire and must still read as a complete row. The caption
    /// is drawn when there is one, never reserved as blank space.
    ///
    /// Both carry a Fable weekly reading, which is what the README's own shot is
    /// cut from — the two shapes of that slot, a percentage with its countdown
    /// and a percentage alone, in the panel a reader sees first. Neither is
    /// `near` or `spent`: this is the healthy scene, and the two tinted shapes
    /// are scene 14's job.
    private static var healthyJSON: String {
        "[\(account("alice@example.com", quota: "0.12", state: "ok", fiveHourResetInMinutes: 130, sevenDayResetInMinutes: 4_320, sevenDayOi: "0.21", sevenDayOiState: "ok", sevenDayOiResetInMinutes: 6_498, plan: "Max 20x", orgUuid: "11111111-1111-1111-1111-111111111111")),"
            + "\(account("bob@example.com", quota: "0.31", state: "ok", sevenDayOi: "0.44", sevenDayOiState: "ok", groups: ["research"], reservedGroups: ["research"], plan: "Team 5x", orgUuid: "22222222-2222-2222-2222-222222222222"))]"
    }

    /// Five plain accounts, `alice@example.com` first: scene 13's own fleet.
    ///
    /// Five, not `healthyJSON`'s two: `.auto` only resolves `.compact` above
    /// ``PanelDensityPreference/comfortableCeiling`` (four) accounts, and
    /// scene 13 is the one scene rendered at both `.auto` and a forced
    /// `.comfortable`. A fleet at or under the ceiling draws the identical
    /// picture either way, which is why the two density variants used to be
    /// indistinguishable.
    private static var controlAccountJSON: String {
        "["
            + (1...5).map { i in
                i == 1
                    ? account(
                        "alice@example.com", quota: "0.12", state: "ok", plan: "Max 20x",
                        orgUuid: "11111111-1111-1111-1111-111111111111")
                    : account("member\(i)@example.com", quota: "0.\(i)0", state: "ok")
            }.joined(separator: ",") + "]"
    }

    /// Scene 22: a single account, a LIVE read (so
    /// ``NoRequestsBanner/totalRequests(_:)`` sees a measured zero rather than
    /// an offline `nil`), and zero requests served: the colleague's fleet
    /// this feature exists for.
    private static var zeroRequestsJSON: String {
        "[\(colleagueRow)]"
    }

    /// Scene 22's one account: live, and at zero requests.
    private static var colleagueRow: String {
        account(
            "colleague@example.com", quota: "0.0", state: "ok", source: "live", plan: "Team Standard",
            orgUuid: "33333333-3333-3333-3333-333333333333", requests: 0)
    }

    /// F1's wire shape, attached to
    /// `alice`'s row the way the server sends it — see ``Account/sessions``'s
    /// doc-comment for why nothing here is wired to a live fetch yet: this
    /// build has no safe channel for real session data, so the review
    /// fixture builds ``Session`` values directly rather than decoding them
    /// off `tcr status --json`'s bare account array, the same way
    /// `FleetStatusTests` now does. Scenes 16 and 17 both use this Fleet;
    /// only ``initialTab(for:)`` decides which tab opens.
    ///
    /// The mockup's own five sessions (`docs/design/panel-tabs-mockup.html`'s
    /// Sessions panel), verbatim: `tcr-c7` (busy, two running
    /// tools — a Bash call and the Agent call that is 20s from the 600s
    /// timeout) and `orchard-c2` (waiting 12m) under `henry10@example.com`;
    /// `m-075377` (idle 40m) rounds out that account's three; `toolkit-c1`
    /// (busy, one running Bash call) and `token-b4` (idle 2h) are
    /// `henry1@example.com`'s two. Panel-parity round: previously this
    /// fixture held 3 sessions on 1 account, none matching the mockup's
    /// names, models or per-row metrics — this round matches all five
    /// exactly (requests, cache%, the running-tool ages) so the tab compares
    /// layout and styling, not five wrong numbers.
    ///
    /// The mockup's own summary line ("12 sessions · 7 busy · 1 waiting · 4
    /// idle") and its "Show 7 more sessions" / "3 accounts have no sessions"
    /// disclosure are NOT reproduced here: this build has no disclosure
    /// feature (`FleetView.sessionsList` renders every session it is given,
    /// unclipped — `snapshotMode`'s own doc-comment says so on purpose), so
    /// seven more fixture sessions would render as seven more full cards the
    /// mockup does not have, which would widen the diff this round exists to
    /// close rather than shrink it. Recorded in `product-wave-findings.md`
    /// as the next round's prerequisite, the same call the previous round
    /// made about the Accounts tab's own disclosure gap.
    private static var sessionsFixture: [Session] {
        // `lastSeenMs`/`firstSeenMs` are epoch milliseconds, and the age
        // label reads real wall-clock `Date()` (`FleetView.trailingStatus`,
        // the same "views re-render often enough" idiom
        // `HeldWindow.countdownLabel` already uses) — not this file's fixed
        // `referenceDate`, which only pins the POLL timestamp shown in the
        // header. These are minutes-ago offsets from the real clock so the
        // rendered age reads like a live fleet's.
        func msAgo(_ seconds: TimeInterval) -> Int64 {
            Int64(Date().addingTimeInterval(-seconds).timeIntervalSince1970 * 1000)
        }
        return [
            // "412 req · cache 97% · 2 running · oldest 9m 40s" — the Agent
            // call (started 9m40s/580s ago) is older than the Bash one
            // (4m12s/252s ago), so it is what `oldest` reads; 580s is 20s
            // short of the 600s Bash timeout, matching the mockup's "20s to
            // timeout" on the Tools tab's RUNNING NOW row for this same call.
            Session(
                sessionId: "aaaaaaaa-1111-2222-3333-444444444444", account: "henry10@example.com",
                model: "claude-fable-5", firstSeenMs: msAgo(3 * 3600), lastSeenMs: msAgo(3 * 60),
                requests: 410, inputTokens: 30_000, outputTokens: 41_000, cacheReadTokens: 970_000,
                tools: SessionTools(
                    // `calls` is the sum of `byTool` below (15,000 + 200 +
                    // 2,000) — the two must agree, per `panel-tabs-review.md`
                    // finding 3: the headline IS the total, never one
                    // category standing in for it.
                    // 23 = this session's own `timeoutsByClass` below
                    // (8 + 9 + 6), and with the sibling's 8 the summary line's
                    // "31 hit the 600s timeout" IS the TIMED OUT TODAY card's
                    // own total. The two numbers are the same fact; a fixture
                    // that let them disagree would render a panel contradicting
                    // itself two lines apart.
                    calls: 17_000, errors: 1, timeouts: 23,
                    running: [
                        ToolCall(
                            tool: "Bash", commandHead: "cargo test --release > test.log",
                            startedMs: msAgo(4 * 60 + 12)),
                        ToolCall(
                            tool: "Agent",
                            commandHead: "Agent · F7 prove time-to-reset value",
                            startedMs: msAgo(9 * 60 + 40)),
                    ],
                    slowest: [
                        ToolCall(
                            tool: "Bash",
                            commandHead: "git -C ~/src/example push > push.log",
                            endedMs: msAgo(5 * 60), seconds: 47.5)
                    ],
                    overOneMinute: 3,
                    // Combined with the sibling session's below, sums to the
                    // mockup's exact BY TOOL numbers: Bash 19,913, Agent 412,
                    // Read·Grep·Edit 4,352 — 24,677 total.
                    byTool: [
                        ToolBucketRow(tool: "Bash", calls: 15_000, secondsP50: 2.0),
                        ToolBucketRow(tool: "Agent", calls: 200, secondsP50: 380),
                        ToolBucketRow(tool: "Read", calls: 2_000, secondsP50: 0.2),
                    ],
                    // The mockup's own TIMED OUT TODAY card: wait 12, build 9,
                    // git-net 4, other 6 — 31, which is the number this tab's
                    // summary line already says hit the 600s timeout. Split
                    // across the two sessions that carry tool data, so the
                    // render also proves the fleet-wide sum rather than one
                    // session's dictionary drawn straight through.
                    timeoutsByClass: ["wait": 8, "build": 9, "other": 6],
                    timedOut: [
                        ToolCall(
                            tool: "Bash",
                            commandHead: "until grep -q \"Ready in\" /tmp/dev.log; do sleep 1; done",
                            commandClass: "wait",
                            endedMs: msAgo(18 * 60), seconds: 600),
                        ToolCall(
                            tool: "Bash",
                            commandHead: "cargo build --release --all-features",
                            commandClass: "build",
                            endedMs: msAgo(52 * 60), seconds: 600),
                    ]),
                // Rising, per the mockup's own aria-label on this session's spark:
                // "Requests per minute over the last 30 minutes: rising".
                // "$4.12" — `SessionRow::cost_usd`, wire 2.
                reqPerMinute: [
                    2, 3, 2, 4, 3, 5, 4, 6, 5, 7, 6, 8, 7, 9, 8,
                    10, 9, 11, 10, 12, 11, 13, 12, 14, 13, 15, 14, 16, 15, 17,
                ],
                costUsd: 4.10),
            // "5,756 req · cache 94% · waiting 12m".
            Session(
                sessionId: "bbbbbbbb-1111-2222-3333-444444444444", account: "henry10@example.com",
                model: "claude-opus-5", firstSeenMs: msAgo(6 * 3600), lastSeenMs: msAgo(12 * 60),
                requests: 5800, inputTokens: 60_000, outputTokens: 88_000, cacheReadTokens: 940_000,
                tools: SessionTools(
                    // 8 = `wait` 4 + `git-net` 4 below.
                    calls: 7_500, errors: 4, timeouts: 8,
                    // Five, the mockup's own count for SLOWEST TODAY — and the
                    // reason there are five: `Fleet.toolsSlowest` pools ten
                    // across every session and the tab draws the top five, so a
                    // fixture with two rows cannot tell "the cap works" apart
                    // from "there was nothing to cap". The seconds are the
                    // mockup's: one at the 600 s timeout, then 583, 556, 343.
                    slowest: [
                        ToolCall(
                            tool: "Bash",
                            commandHead: "/opt/homebrew/bin/bash disk-scan.sh 2>&1 | tee",
                            endedMs: msAgo(11 * 60), seconds: 600.0),
                        ToolCall(
                            tool: "Bash",
                            commandHead: "bash retro-review-wait.sh --until green",
                            endedMs: msAgo(23 * 60), seconds: 583.0),
                        ToolCall(
                            tool: "Bash",
                            commandHead: "until grep -qE \"^(error|warning)\" build.log",
                            endedMs: msAgo(36 * 60), seconds: 556.0),
                        ToolCall(
                            tool: "Bash",
                            // Deliberately longer than the row: the overlap
                            // this fixture exists to catch only appears once
                            // the label cannot fit beside the pill, and every
                            // earlier fixture command fitted.
                            commandHead:
                                "cd ~/src/example && ./scripts/cargo-q.sh test -p tcr "
                                + "--all-features 2>&1 | tee test.log",
                            endedMs: msAgo(48 * 60), seconds: 343.0),
                    ],
                    overOneMinute: 5,
                    byTool: [
                        ToolBucketRow(tool: "Bash", calls: 4_900, secondsP50: 2.3),
                        ToolBucketRow(tool: "Agent", calls: 210, secondsP50: 400),
                        ToolBucketRow(tool: "Grep", calls: 2_400, secondsP50: 0.2),
                    ],
                    // The other four of the mockup's `wait` twelve, and its
                    // `git-net` four. A class with a count and no commands is
                    // deliberate here too: `git-net` opens to nothing, which
                    // is the row shape a server sending counts alone draws.
                    timeoutsByClass: ["wait": 4, "git-net": 4],
                    timedOut: [
                        ToolCall(
                            tool: "Bash",
                            commandHead: "until [ -f /tmp/merge-gate.done ]; do sleep 5; done",
                            commandClass: "wait",
                            endedMs: msAgo(31 * 60), seconds: 600)
                    ]),
                // Falling, per the mockup's aria-label on this session's spark.
                // "$5.29"; with the sibling above, the account block's header
                // reads the mockup's "3 sessions · $9.41".
                reqPerMinute: [
                    15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 5, 4, 4, 3,
                    3, 3, 2, 2, 2, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0,
                ],
                costUsd: 5.30),
            // "idle 40m" — no metrics line at all in the mockup, and
            // `FleetView.sessionRow`'s compact idle layout now matches that.
            Session(
                sessionId: "cccccccc-1111-2222-3333-444444444444", account: "henry10@example.com",
                model: "claude-sonnet-5", firstSeenMs: msAgo(45 * 60), lastSeenMs: msAgo(40 * 60),
                requests: 3, inputTokens: 900, outputTokens: 80, cacheReadTokens: 600),
            // "6,479 req · cache 95% · 1 running · 1m 03s".
            Session(
                sessionId: "dddddddd-1111-2222-3333-444444444444", account: "henry1@example.com",
                model: "claude-opus-5", firstSeenMs: msAgo(4 * 3600), lastSeenMs: msAgo(63),
                requests: 6500, inputTokens: 50_000, outputTokens: 63_000, cacheReadTokens: 950_000,
                // `calls` (and `byTool`) deliberately left at their zero
                // default: this session's requests (6,479) are a wire fact
                // independent of tool-call volume, and the Tools tab's
                // headline is the sum of every session's `tools.calls` —
                // `panel-tabs-review.md` finding 3's own bug, reproduced
                // here once already this round by a first draft that set
                // `calls: 6_500` with no matching `byTool` entries and
                // pushed the headline to 31,156 against a BY TOOL section
                // still summing to the mockup's 24,677. Only the running
                // call below is this session's contribution to the Tools
                // tab.
                tools: SessionTools(
                    running: [
                        ToolCall(
                            tool: "Bash",
                            commandHead: "swift build -c release --product TcrBar",
                            startedMs: msAgo(63))
                    ]),
                // "$7.90"; with `token-b4`'s $0.12 below, this account's
                // block header reads the mockup's "2 sessions · $8.02".
                reqPerMinute: [
                    4, 5, 4, 6, 5, 7, 6, 8, 7, 9, 8, 10, 9, 11, 10,
                    12, 11, 13, 12, 14, 13, 15, 14, 16, 15, 17, 16, 18, 17, 19,
                ],
                costUsd: 7.90),
            // "idle 2h" — compact layout, same as `m-075377` above.
            Session(
                sessionId: "eeeeeeee-1111-2222-3333-444444444444", account: "henry1@example.com",
                model: "claude-opus-5", firstSeenMs: msAgo(5 * 3600), lastSeenMs: msAgo(2 * 3600),
                requests: 5, inputTokens: 1500, outputTokens: 120, cacheReadTokens: 900,
                // Priced, and deliberately never DRAWN: an idle row is one
                // line with no metrics, so this figure only ever reaches the
                // account block's own total. `m-075377` above is left
                // unpriced (`nil`) so the same block also carries the
                // server-did-not-send case.
                costUsd: 0.12),
        ]
    }

    /// 2 solo cards + a 5-member parked group (`henry-token`) + a 6-member
    /// active group (`orchard`) — the mockup's own account count and group
    /// shape (`docs/design/panel-tabs-mockup.html`'s Accounts panel), built
    /// entirely from the existing `account()`/group machinery.
    ///
    /// The counts are the point, not decoration: five parked accounts is what
    /// puts two of them behind `FleetView`'s "Show 2 more accounts" button,
    /// and six live ones is what trips ``FleetSection/collapsesByDefault``, so
    /// this scene is the render-harness proof that both disclosure shapes
    /// draw. The spend figures are dialled so the collapsed group's summary
    /// line reads the mockup's own "6 accounts · $8.42 today".
    private static var accountsParityJSON: String {
        // "$540 · 1.5M out" and "$1,190 · 3.1M out" on the two solo cards'
        // plan lines, the mockup's own figures.
        // `henry10` carries a Fable weekly window and `henry5` below does not,
        // so this one scene shows both halves of the rule the card follows: a
        // third `fable` row when ``Account/sevenDayOi`` is present, and NO row
        // at all when it is absent. An empty `fable` track on an account with
        // no such window would claim a window that does not exist, and a
        // fixture where every row has one could not tell the two apart.
        let solo1 = account(
            "henry10@example.com", quota: "0.07", state: "ok", sevenDay: "0.30",
            sevenDayState: "ok",
            fiveHourResetInMinutes: 182, sevenDayResetInMinutes: 6_540,
            sevenDayOi: "0.71", sevenDayOiState: "near", sevenDayOiResetInMinutes: 6_498,
            usage: measuredUsage(
                todayCost: 540.12, windowCost: 540.12, windowOutputTokens: 1_500_000),
            plan: "Max 20x", orgUuid: "11111111-1111-1111-1111-111111111111")
        // `near`, not `warn`: the wire's only three quota-state tokens are
        // `ok`/`near`/`spent` (`src/cli.rs`'s `quota_state_token`), and the
        // composite is the most-spent of the two windows — so an account whose
        // 7d window sits at 98 % reports `near` on BOTH. The fixture used to
        // send `warn`, a token no `tcr` emits: it decoded to `.unknown`, which
        // painted the 98 % bar the unmeasured violet and left the card's pill
        // reading OK — the very defect the parity pass was opened to fix,
        // reproduced by the fixture rather than by the panel.
        let solo2 = account(
            "henry5@example.com", quota: "0.98", state: "near",
            fiveHour: "0.04", fiveHourState: "ok",
            sevenDay: "0.98", sevenDayState: "near",
            fiveHourResetInMinutes: 182, sevenDayResetInMinutes: 5_640,
            usage: measuredUsage(
                todayCost: 1_190.4, windowCost: 1_190.4, windowOutputTokens: 3_100_000),
            plan: "Max 20x", orgUuid: "22222222-2222-2222-2222-222222222222")
        let tokenColors = ["henry-token": "#92d188", "orchard": "#c79ae8"]
        // Five parked members — three drawn, two behind the button.
        let parkedPlans = ["Team 5x", "Team Standard", "Team 5x", "Team Standard", "Team 5x"]
        // Row two is the UNMEASURED case, and unmeasured means NO READING —
        // `quota: "null"`, the shape `tcr` sends for an account nothing has
        // been learned about. It used to say `("0.0", "unmeasured")`: a real
        // zero reading with an invented state word, which rendered as the
        // `UNKNOWN` pill (a state this build cannot name) where the mockup
        // draws `UNMEASURED` (no state to name yet). Opposite meanings.
        let parkedStates = [
            ("0.10", "ok"), ("null", "ok"), ("0.55", "near"),
            ("0.22", "ok"), ("0.31", "ok"),
        ]
        let parkedNames = [
            "gil@example.com", "henry1@example.com", "henry2@example.com",
            "henry3@example.com", "henry4@example.com",
        ]
        let tokenRows = (0..<5).map { i in
            account(
                parkedNames[i], quota: parkedStates[i].0, state: parkedStates[i].1,
                groups: ["henry-token"], parkedGroups: ["henry-token"], groupColors: tokenColors,
                plan: parkedPlans[i], orgUuid: "33333333-3333-3333-3333-333333333333")
        }
        // 5 x $1.40 + $1.42 = $8.42, the mockup's own collapsed-group total.
        let orchardRows = (1...6).map { i in
            account(
                "orchard\(i)@example.com", quota: i == 6 ? "0.60" : "0.15",
                state: i == 6 ? "near" : "ok",
                usage: measuredUsage(
                    todayCost: i == 6 ? 1.42 : 1.40, windowCost: 0.9,
                    windowOutputTokens: 12_476),
                groups: ["orchard"], groupColors: tokenColors,
                plan: "Team Standard", orgUuid: "44444444-4444-4444-4444-444444444444")
        }
        let all = [solo1, solo2] + tokenRows + orchardRows
        return "[\(all.joined(separator: ","))]"
    }

    /// The parity scene's fleet: ``accountsParityJSON``'s accounts PLUS a
    /// session list, because the strip's badges are shared chrome and the
    /// mockup's Accounts panel draws both of them (`Sessions 12`, `Tools 3`).
    /// Rendered from a fleet with no sessions, the two badges vanish and the
    /// parity comparison silently loses the delta it was meant to prove.
    ///
    /// ``sessionsFixture``'s five sessions carry all three running tool calls,
    /// so `Tools` reads 3 with no help. `Sessions` needs the mockup's twelve:
    /// the seven added here are quiet rows — no tools, no sparkline — which is
    /// the only thing a COUNT needs them to be, and they are never drawn on
    /// this tab.
    private static var accountsParityFleet: Fleet {
        let base = fleet(accountsParityJSON)
        let quiet = (1...7).map { index in
            Session(
                sessionId: "f000000\(index)-1111-2222-3333-444444444444",
                account: "henry10@example.com", model: "claude-sonnet-5",
                firstSeenMs: Int64(Date().addingTimeInterval(-3600).timeIntervalSince1970 * 1000),
                lastSeenMs: Int64(
                    Date().addingTimeInterval(-Double(index) * 300).timeIntervalSince1970 * 1000),
                requests: 4, inputTokens: 900, outputTokens: 80, cacheReadTokens: 600)
        }
        return Fleet(
            accounts: base.accounts, unreadable: base.unreadable,
            sessions: sessionsFixture + quiet, sessionsSupported: true)
    }

    /// A healthy fleet whose sessions channel failed because the BUNDLED `tcr`
    /// has no `sessions` subcommand — see ``Fleet/SessionsChannel``.
    private static var oldToolFleet: Fleet {
        let base = fleet(healthyJSON)
        return Fleet(
            accounts: base.accounts, unreadable: base.unreadable,
            sessionsChannel: .toolPredatesSessions)
    }

    /// The same fleet with the channel's longest sentence: `tcr sessions`
    /// exited non-zero and its stderr rides into the banner.
    private static var sessionsFailedFleet: Fleet {
        let base = fleet(healthyJSON)
        return Fleet(
            accounts: base.accounts, unreadable: base.unreadable,
            sessionsChannel: .commandFailed(
                "could not read live status from the proxy on :3456 (connection refused)"))
    }

    private static var sessionsTabFleet: Fleet {
        let base = fleet(
            "[\(account("henry10@example.com", quota: "0.12", state: "ok", plan: "Max 20x", orgUuid: "11111111-1111-1111-1111-111111111111")),"
                + "\(account("henry1@example.com", quota: "0.31", state: "ok", plan: "Team Standard", orgUuid: "22222222-2222-2222-2222-222222222222"))]"
        )
        return Fleet(
            accounts: base.accounts, unreadable: base.unreadable, sessions: sessionsFixture,
            sessionsSupported: true)
    }

    /// Both tab badges at the widest they go: 120 sessions, 12 of them with a
    /// call running.
    ///
    /// The strip is the subject, not the sessions. `Sessions` plus a two-digit
    /// badge already wants more than an equal quarter of the panel, and until
    /// this scene existed the worst case any PNG carried was whatever counts a
    /// fixture built for some other purpose happened to have. Three digits and
    /// two is the width a real fleet can hand it, so that is what the picture
    /// has to show.
    ///
    /// Deliberately NOT a Sessions-tab scene: it opens on Accounts, where the
    /// strip sits above cards rather than above 120 rows, because what is being
    /// reviewed is one line of chrome and a long list underneath it reviews
    /// nothing.
    private static var widestTabBadgesFleet: Fleet {
        let base = fleet(healthyJSON)
        let firstSeen = Int64(referenceDate.addingTimeInterval(-3600).timeIntervalSince1970 * 1000)
        let lastSeen = Int64(referenceDate.timeIntervalSince1970 * 1000)
        // Twelve of the 120 carry one running call each, so `toolsRunning`
        // reads 12 without inventing a second fixture to count it from.
        let sessions = (0..<120).map { index in
            Session(
                sessionId: String(format: "%08x-0000-0000-0000-000000000000", index),
                account: base.accounts.first?.name, model: "claude-sonnet-5",
                firstSeenMs: firstSeen, lastSeenMs: lastSeen, requests: 10,
                tools: index < 12
                    ? SessionTools(
                        calls: 1, errors: 0, timeouts: 0,
                        running: [
                            ToolCall(tool: "Bash", commandHead: "cargo build", startedMs: lastSeen)
                        ])
                    : SessionTools())
        }
        return Fleet(
            accounts: base.accounts, unreadable: base.unreadable, sessions: sessions,
            sessionsSupported: true)
    }

    /// Twenty-four accounts and forty working sessions, ten of them with a
    /// call running and each with a slow call today: an Accounts list at its
    /// budget, and a Sessions list and a Tools list each longer than the whole
    /// room the panel has under their summaries, so the Accounts tab, with its
    /// strip and buttons, is the tallest of the three (``liveListScenes``).
    /// Every session has made tool calls, so none folds as a warm-up.
    private static var longListsFleet: Fleet {
        let rows = (1...24).map {
            account("acct-\($0)@example.com", quota: "0.\(10 + $0)", state: "ok")
        }
        let base = fleet("[\(rows.joined(separator: ","))]")
        func msAgo(_ seconds: TimeInterval) -> Int64 {
            Int64(Date().addingTimeInterval(-seconds).timeIntervalSince1970 * 1000)
        }
        let sessions: [Session] = (0..<40).map { (index: Int) -> Session in
            let running: [ToolCall] =
                index < 10
                ? [
                    ToolCall(
                        tool: "Bash", commandHead: "cargo test --workspace > run-\(index).log",
                        startedMs: msAgo(TimeInterval(30 + index * 25)))
                ]
                : []
            let slowest = ToolCall(
                tool: "Bash", commandHead: "swift build -c release --target T\(index)",
                commandClass: "build", endedMs: msAgo(TimeInterval(600 + index * 60)),
                seconds: Double(60 + index * 5))
            // Four sessions with timeouts, so TIMED OUT TODAY draws its card
            // too: with the Running and Slowest cards folded to their first
            // rows, it is what takes the Tools list past the panel's room.
            let timeoutsByClass: [String: Int] =
                index < 4 ? ["build": 3, "test": 2, "git-net": 1, "wait": 4, "other": 1] : [:]
            let tools = SessionTools(
                calls: 20 + index, errors: 0, timeouts: timeoutsByClass.values.reduce(0, +),
                running: running, slowest: [slowest], timeoutsByClass: timeoutsByClass)
            let model = index.isMultiple(of: 3) ? "claude-opus-5" : "claude-sonnet-5"
            let account: String = base.accounts[index % base.accounts.count].name
            return Session(
                sessionId: String(format: "%08x-1111-2222-3333-444444444444", index),
                account: account, model: model,
                firstSeenMs: msAgo(TimeInterval(2 * 3600 + index * 60)),
                lastSeenMs: msAgo(TimeInterval(30 + index * 20)),
                requests: 40 + index * 7, tools: tools)
        }
        return Fleet(
            accounts: base.accounts, unreadable: base.unreadable, sessions: sessions,
            sessionsSupported: true)
    }

    /// A fleet at the LIVE fleet's measured dimensions rather than the
    /// mockup's, with every identity invented.
    ///
    /// Why a SECOND Tools/Sessions fixture exists. ``sessionsTabFleet`` is the
    /// MOCKUP's fleet, and until now it was the only reader this panel's
    /// design ever had. Measured against the live proxy on 2026-09-17 with
    /// `scripts/fleet-shape.py`, the two had drifted this far apart:
    ///
    ///     dimension          mockup fixture     live
    ///     running calls                   3       68
    ///     sessions                        5       31
    ///     warm-up sessions                0       18
    ///     timeouts today                 31        0
    ///
    /// The last row hid an entire card. TIMED OUT TODAY draws only when the
    /// server reports a timeout class, so on the real fleet it does not draw
    /// at all, and nobody reviewing the mockup fixture had ever seen the tab
    /// without it. This scene has no timeouts for exactly that reason: it is
    /// the Tools tab as the operator actually meets it.
    ///
    /// It also pins the running list's ORDER. `busy-1` below holds a three
    /// hour `Agent` and a `Bash` call fifty seconds from the 600s timeout;
    /// ``Fleet/toolsRunning`` must put the `Bash` on top, because only a
    /// ``ToolCall/capped`` call has a deadline to be near. Sorted by age
    /// alone, the `Agent` would lead and the call about to be killed would sit
    /// under the five-row fold.
    ///
    /// Everything here is SHAPE, never DATA. This repository is public, so the
    /// accounts, session ids, project names and commands are all invented;
    /// only the COUNTS come from the measurement. When the fleet changes,
    /// re-run `scripts/fleet-shape.py` and compare it against the table above
    /// rather than trusting that this still resembles anything.
    /// Every figure below is INVENTED and deliberately round. This fixture
    /// exists to render the panel at the MAGNITUDES a busy fleet produces, and
    /// a magnitude is all it needs: whether a cost fits its column does not
    /// depend on the cost being anyone's real one.
    ///
    /// The first version of this fixture was filled in by reading the live
    /// proxy, which put an operator's session costs, request counts and token
    /// volumes into a public repository wearing fake names. Names, emails,
    /// UUIDs and paths were anonymised; the economics were not, and an
    /// operator's spend is the more sensitive half. If a future change wants
    /// these numbers to look more realistic, invent more realistic numbers.
    /// Do not read them off a running fleet.
    private static var realShapeFleet: Fleet {
        func msAgo(_ seconds: TimeInterval) -> Int64 {
            Int64(Date().addingTimeInterval(-seconds).timeIntervalSince1970 * 1000)
        }
        let base = fleet(
            "[\(account("alice@example.com", quota: "0.95", state: "near", plan: "Max 20x", orgUuid: "33333333-3333-3333-3333-333333333333")),"
                + "\(account("bob@example.com", quota: "0.40", state: "ok", plan: "Max 20x", orgUuid: "44444444-4444-4444-4444-444444444444"))]"
        )
        // The four running calls whose ORDER is the point. Declared here
        // oldest-first on purpose: if the sort ever regresses to age, this
        // fixture renders in exactly this order and the scene shows it.
        let busy = Session(
            sessionId: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa", account: "alice@example.com",
            model: "claude-opus-5", firstSeenMs: msAgo(5 * 3600), lastSeenMs: msAgo(20),
            requests: 1_500, inputTokens: 250_000, outputTokens: 180_000,
            cacheReadTokens: 300_000_000,
            tools: SessionTools(
                calls: 4_000, errors: 90, timeouts: 0,
                running: [
                    ToolCall(
                        tool: "Agent", commandHead: "explorer: map the retry paths",
                        startedMs: msAgo(3 * 3600)),
                    ToolCall(
                        tool: "Write", commandHead: "Write /Users/alice/git/demo/report.md",
                        startedMs: msAgo(40 * 60)),
                    ToolCall(
                        tool: "Bash", commandHead: "cargo test --all --release > suite.log",
                        commandClass: "build", startedMs: msAgo(9 * 60 + 50)),
                    ToolCall(
                        tool: "Bash", commandHead: "git -C ~/src/demo fetch --all",
                        commandClass: "git-net", startedMs: msAgo(4 * 60 + 12)),
                ],
                slowest: [
                    ToolCall(
                        tool: "Bash", commandHead: "swift build -c release", commandClass: "build",
                        endedMs: msAgo(11 * 60), seconds: 90.0)
                ],
                overOneMinute: 25,
                byTool: [
                    ToolBucketRow(tool: "Bash", calls: 3_300, secondsP50: 3.0),
                    ToolBucketRow(tool: "Read", calls: 320, secondsP50: 2.0),
                    ToolBucketRow(tool: "Write", calls: 220, secondsP50: 1.0),
                    ToolBucketRow(tool: "Edit", calls: 160, secondsP50: 1.5),
                ]),
            costUsd: 300.00)
        let second = Session(
            sessionId: "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb", account: "bob@example.com",
            model: "claude-sonnet-5", firstSeenMs: msAgo(2 * 3600), lastSeenMs: msAgo(45),
            requests: 800, inputTokens: 20_000, outputTokens: 90_000,
            cacheReadTokens: 200_000_000,
            tools: SessionTools(
                calls: 1_400, errors: 20, timeouts: 0,
                running: [
                    ToolCall(
                        tool: "Bash", commandHead: "rg -n \"retry\" src/ > hits.log",
                        commandClass: "search", startedMs: msAgo(52))
                ],
                slowest: [
                    ToolCall(
                        tool: "Bash", commandHead: "bun test --coverage", commandClass: "build",
                        endedMs: msAgo(6 * 60), seconds: 60.0)
                ],
                overOneMinute: 15,
                byTool: [
                    ToolBucketRow(tool: "Bash", calls: 1_400, secondsP50: 1.5)
                ]),
            costUsd: 120.00)
        // The 18 the fold exists for: two requests, no tool call, then
        // silence. Idle spread 10 to 50 minutes, matching the live spread, so
        // not one of them is near the five-minute boundary and the scene
        // cannot pass by accident.
        let warmups = (1...18).map { index in
            Session(
                sessionId: String(format: "cccccccc-cccc-cccc-cccc-%012d", index),
                account: index.isMultiple(of: 2) ? "alice@example.com" : "bob@example.com",
                model: "claude-sonnet-5", firstSeenMs: msAgo(3_600 + Double(index) * 120),
                lastSeenMs: msAgo(600 + Double(index) * 140),
                requests: 2, inputTokens: 1_000, outputTokens: 30, cacheReadTokens: 10_000,
                costUsd: 0.10)
        }
        return Fleet(
            accounts: base.accounts, unreadable: base.unreadable,
            sessions: [busy, second] + warmups, sessionsSupported: true)
    }

    /// The three plan labels a real fleet produces, side by side — `Max 20x`,
    /// `Team 5x`, `Team Standard` — plus one row with NO plan at all.
    ///
    /// The fourth row is the point as much as the first three: an account that
    /// has never been profiled must draw no tag, not a guessed one, and a
    /// screenshot is the only place that negative is actually visible.
    private static var planLabelsJSON: String {
        let rows = [
            account(
                "alice@example.com", quota: "0.12", state: "ok",
                plan: "Max 20x", orgUuid: "11111111-1111-1111-1111-111111111111"),
            account(
                "bob@example.com", quota: "0.31", state: "ok",
                plan: "Team 5x", orgUuid: "22222222-2222-2222-2222-222222222222"),
            account(
                "carol@example.com", quota: "0.44", state: "ok",
                plan: "Team Standard", orgUuid: "22222222-2222-2222-2222-222222222222"),
            account("dave@example.com", quota: "0.08", state: "ok"),
        ]
        return "[\(rows.joined(separator: ","))]"
    }

    /// THE WIDEST ROW THE FLEET CAN PRODUCE — the shape that overflowed the
    /// panel, reproduced so a PNG can prove it does not any more.
    ///
    /// Everything that competes for width at once: the control marker, a
    /// reserved group (`GROUP ONLY`), a quota pill, TWO group tags one of which
    /// is long, the longest plan label, and a 30-character email. On the live
    /// fleet this row rendered wider than `Tok.panelWidth`, so both card edges
    /// were clipped and the header truncated to "…eady · 15 ok".
    ///
    /// The pass condition is not "it looks tidy" — it is that NOTHING is
    /// clipped and the name is still readable. A second, ordinary row is
    /// included as the control: if the panel is over-wide for a structural
    /// reason, both rows show it, and the bug is not the one this scene names.
    ///
    /// Carries `groupColors` (`widestRowSceneColors`), which every earlier
    /// version of this fixture omitted. This is also the one scene that puts
    /// ONE account in TWO group sections at once (the group-outline design's
    /// decision #4) — the case the group-outline
    /// feature most needs a real render of, and a fixture with no colours
    /// draws the section-outline feature's neutral FALLBACK stroke in both
    /// sections, which reads as identical grey regardless of whether the two
    /// sections are wired to two different colours or none at all. Without
    /// this, the scene cannot tell "outline colour is broken" apart from
    /// "outline colour was never given one to draw" — the exact ambiguity
    /// that made a rendered PNG of this scene, by itself, prove nothing about
    /// the feature it was chosen to demonstrate.
    private static var widestRowJSON: String {
        let worst = account(
            "henry.fitzgerald@example.com", quota: "0.62", state: "ok",
            groups: ["gil", "henry-team-parked"],
            reservedGroups: ["gil"],
            groupColors: widestRowSceneColors,
            plan: "Team Standard",
            orgUuid: "22222222-2222-2222-2222-222222222222")
        let ordinary = account(
            "bob@example.com", quota: "0.31", state: "ok",
            plan: "Max 20x",
            orgUuid: "11111111-1111-1111-1111-111111111111")
        return "[\(worst),\(ordinary)]"
    }

    /// A PARKED GROUP BESIDE A LIVE ONE — the state a screenshot is the only
    /// honest check on, because every part of it is visual.
    ///
    /// Six rows, and the four `henry-team` members are the point:
    ///  - four members of parked `henry-team`, one of which is ALSO disabled
    ///    by hand. All draw `PARKED`, but for different reasons, and the
    ///    fixture exists to show that the panel does not need them to look
    ///    different: the consequence is identical, and the group tag says
    ///    which is which.
    ///  - a member of live `dev`, the control: if the dimming is wrong, or
    ///    applied to every tag, this row shows it.
    ///  - an ungrouped row, so the scene also carries a tag-less baseline.
    ///
    /// The pass condition is that the parked tags read as held back: dim
    /// wash, pause glyph — while `DEV` beside them stays at full strength and
    /// still identifiable by colour. A tag that dims into illegibility fails
    /// this scene as surely as one that does not dim at all.
    ///
    /// Four members, not two: a wholly-parked group shows its first three
    /// rows and a "Show N more" button, collapsed or expanded, and with only
    /// two members there is nothing left to hide, so the collapsed and
    /// expanded renders drew the same picture. The fourth member is the row
    /// that only the expanded scene shows.
    private static var parkedGroupJSON: String {
        let parkedLive = account(
            "alice@example.com", quota: "0.12", state: "ok",
            groups: ["henry-team"], parkedGroups: ["henry-team"],
            groupColors: parkedSceneColors,
            plan: "Max 20x", orgUuid: "11111111-1111-1111-1111-111111111111",
            gate: "parked")
        let parkedAndDisabled = account(
            "bob@example.com", quota: "0.31", state: "ok", disabled: true,
            groups: ["henry-team"], parkedGroups: ["henry-team"],
            groupColors: parkedSceneColors,
            plan: "Team 5x", orgUuid: "22222222-2222-2222-2222-222222222222",
            gate: "disabled")
        let parkedThird = account(
            "erin@example.com", quota: "0.55", state: "ok",
            groups: ["henry-team"], parkedGroups: ["henry-team"],
            groupColors: parkedSceneColors,
            plan: "Team 5x", orgUuid: "22222222-2222-2222-2222-222222222222",
            gate: "parked")
        let parkedFourth = account(
            "frank@example.com", quota: "0.09", state: "ok",
            groups: ["henry-team"], parkedGroups: ["henry-team"],
            groupColors: parkedSceneColors,
            plan: "Team 5x", orgUuid: "22222222-2222-2222-2222-222222222222",
            gate: "parked")
        let live = account(
            "carol@example.com", quota: "0.44", state: "ok",
            groups: ["dev"], groupColors: parkedSceneColors,
            plan: "Team Standard", orgUuid: "22222222-2222-2222-2222-222222222222",
            gate: "ok")
        let ungrouped = account("dave@example.com", quota: "0.08", state: "ok", gate: "ok")
        return
            "[\(parkedLive),\(parkedAndDisabled),\(parkedThird),\(parkedFourth),\(live),\(ungrouped)]"
    }

    /// Real colours for the parked scene: dimming is invisible against the
    /// neutral fallback every other scene draws, so this one carries the
    /// `groupColors` the server actually sends.
    private static let parkedSceneColors: [String: String] = [
        "henry-team": "#32d74b", "dev": "#0a84ff",
    ]

    /// Real colours for the widest-row scene, deliberately DIFFERENT hues
    /// from `parkedSceneColors` (orange, purple, rather than green, blue) —
    /// two scenes both carrying blue/green would leave a swapped-colour bug
    /// invisible if the swap happened to land the same pair back on the same
    /// two sections. `henry.fitzgerald@example.com` sits in BOTH `gil` and
    /// `henry-team-parked` at once, so this is the fixture that shows the
    /// group-outline feature's account-in-two-groups case in colour.
    private static let widestRowSceneColors: [String: String] = [
        "gil": "#ff9f0a", "henry-team-parked": "#bf5af2",
    ]

    /// ONE PERSON, TWO ORGS — the shape that broke the panel, and the names it
    /// wears now that `tcr` has fixed it.
    ///
    /// Both rows used to carry `henry@example.com`; only the org differed. They
    /// collapsed to one SwiftUI identity, so the panel drew the FIRST row's
    /// numbers on both and neither wore its own gate pill, while `tcr status
    /// --json` reported the two correctly and differently. `tcr` now gives the
    /// second row its own name (`src/config.rs`, `migrate_duplicate_names`), so
    /// the PNG must show BOTH halves of the Team row's name and truncate
    /// neither: the email on line one, the `/example-team` half as the first tag
    /// on the designations line. Read left to right across the two lines, that
    /// is exactly what a person types to address the row.
    ///
    /// The email is deliberately a realistic length rather than a short one.
    /// A short address leaves slack that hides the very overflow this scene
    /// exists to make visible — the previous fixture's `henry@example.com`
    /// rendered as `henry@ex…ample-team` the moment the suffix shared its line.
    ///
    /// The two rows are deliberately as unlike each other as a pair can be —
    /// one spent and rejected, one fresh and never probed, and different plans
    /// — because that is what makes the failure legible in a PNG: if the render
    /// shows two identical rows, the collapse is back.
    private static var duplicateEmailJSON: String {
        let old = account(
            "henry.mitchell@example.com", quota: "1.0", state: "spent", probe: "ok", held: hold,
            plan: "Max 20x", orgUuid: "11111111-1111-1111-1111-111111111111",
            gate: "rejected")
        let fresh = account(
            "henry.mitchell@example.com/example-team", quota: "null", state: "ok", probe: "never",
            usage: unmeasuredUsage,
            plan: "Team Standard", orgUuid: "22222222-2222-2222-2222-222222222222",
            gate: "ok")
        return "[\(old),\(fresh)]"
    }

    /// The bug this scene exists to catch: a 7d-red account must not paint
    /// its 5h bar red, and the inverse — a 5h-red account must not paint its
    /// 7d bar red. Every OTHER fixture in this file sets `fiveHour` and
    /// `sevenDay` to the same value as the composite `quota`, so a binding
    /// bug (5h fraction wired to the 7d bar, or both bars reading the same
    /// state) would render byte-identical to the correct code against every
    /// other scene — this is the one scene that can actually distinguish
    /// them. Two rows, each diverging the OTHER way:
    ///
    ///  - `divergent-low-high`: 5h ~8% (green, `ok`) under 7d ~96% (amber,
    ///    `near`) — the top bar must stay green while the bottom is amber.
    ///  - `divergent-high-low`: 5h ~99% (amber, `near`) over 7d ~15% (green,
    ///    `ok`) — the top bar must be amber while the bottom stays green.
    ///
    /// `near`, not `spent`: the server's own rule (`src/manager/snapshot.rs`)
    /// is `>= 1.0 => Exhausted`, `>= threshold => NearLimit`, else `Normal` —
    /// 0.96 and 0.99 are both under 1.0, so the server can never emit
    /// `"spent"` for them. An earlier version of this fixture painted them
    /// `"spent"` anyway, which is a state the real server cannot produce and
    /// would have taught a future reader a threshold that doesn't exist. The
    /// swap this scene exists to catch shows just as clearly at `near`
    /// (amber) against `ok` (green) as it would at `spent` (red).
    private static var divergentWindowsJSON: String {
        let lowHigh = account(
            "divergent-low-high@example.com", quota: "0.96", state: "near",
            fiveHour: "0.08", fiveHourState: "ok",
            sevenDay: "0.96", sevenDayState: "near")
        let highLow = account(
            "divergent-high-low@example.com", quota: "0.99", state: "near",
            fiveHour: "0.99", fiveHourState: "near",
            sevenDay: "0.15", sevenDayState: "ok")
        return "[\(lowHigh),\(highLow)]"
    }

    /// PROOF fixture for the unmeasured-window overclaim: `sevenDay` is
    /// genuinely spent (1.0/"spent") while `fiveHour`/`fiveHourState` are
    /// BOTH absent — the shape `src/quota.rs` produces whenever the 5h
    /// window has not reported yet but the 7d window already has (the two
    /// populate independently from separate response headers). The 5h bar
    /// must render as a NEUTRAL dashed outline (no reading), never inheriting
    /// the 7d window's red — that is the exact overclaim this whole feature
    /// exists to prevent.
    private static var unmeasuredWindowJSON: String {
        let row = account(
            "unmeasured-5h@example.com", quota: "1.0", state: "spent",
            fiveHour: "null", fiveHourState: "null",
            sevenDay: "1.0", sevenDayState: "spent")
        return "[\(row)]"
    }

    /// The shape that broke: thirteen rows, mixed states, one never-probed.
    private static var mixedJSON: String {
        var rows = (1...4).map {
            account("ok-\($0)@example.com", quota: "0.\($0)2", state: "ok")
        }
        // The near and spent rows carry resets on both windows — what the real
        // fleet looks like, and the only way this scene shows a caption in its
        // window's own colour (`FleetView.captionTint`).
        rows.append(
            account(
                "near@example.com", quota: "0.94", state: "near", held: hold,
                fiveHourResetInMinutes: 47, sevenDayResetInMinutes: 6_498))
        // Out of tokens the way most of a real fleet is: the 5-hour window
        // spent and HELD, the week only partly used. The hold is relative to
        // the render clock (``heldFiveHour(inMinutes:)``), because the fixed
        // `hold` timestamp is already in the past and a one-line row draws no
        // "back in" for a reset that has passed.
        let weeks = ["0.6", "0.5", "0.7", "0.4", "0.6", "0.5"]
        rows += (1...6).map {
            account(
                "spent-\($0)@example.com", quota: "1.0", state: "spent",
                held: heldFiveHour(inMinutes: 12 + $0 * 30),
                fiveHour: "1.0", fiveHourState: "spent",
                sevenDay: weeks[$0 - 1], sevenDayState: "ok",
                fiveHourResetInMinutes: 12 + $0 * 30, sevenDayResetInMinutes: 6_498)
        }
        rows.append(
            account(
                "never@example.com", quota: "null", state: "ok",
                disabled: true, probe: "never"))
        rows.append(account("parked@example.com", quota: "0.2", state: "ok", disabled: true))
        return "[\(rows.joined(separator: ","))]"
    }

    private static var exhaustedJSON: String {
        "[\((1...3).map { account("spent-\($0)@example.com", quota: "1.0", state: "spent", held: hold) }.joined(separator: ","))]"
    }

    private static var unmeasuredJSON: String {
        "[\(account("alice@example.com", quota: "0.12", state: "ok")),"
            + "\(account("never@example.com", quota: "null", state: "ok", probe: "never"))]"
    }

    /// `usage` is null here for the same reason the four serving counters are
    /// on a real offline row: there is no serving process to have measured
    /// anything. The spend line and the 5h figures must both be absent.
    private static var offlineJSON: String {
        "[\(account("alice@example.com", quota: "0.12", state: "ok", source: "offline", usage: unmeasuredUsage))]"
    }

    /// Every branch of the spend rendering in one panel, because each one is a
    /// DIFFERENT null and the whole feature is the claim that they never look
    /// alike:
    ///
    ///  - `partial@` — measured, with its own quota window, and 12 of its
    ///    requests unpriceable: `$5.61+ · 12k out`, the `+` saying the figure
    ///    is a floor.
    ///  - `unpriced@` — measured but unpriceable: the token count alone, with
    ///    its unit — `12k out` — and the header's `N unpriced` clause.
    ///  - `no-window@` — priced, but the server cannot name the window's start,
    ///    so the card shows the DAY and says so: `$9.41 today · 32k out`.
    ///  - `unmeasured@` — no `usage` at all: an empty slot, not a zero.
    ///
    /// The header line is the fleet's sum over the three measured rows, so it
    /// also proves the unmeasured one contributes nothing rather than zero. It
    /// names every model: four priced ones, then `sonnet-4-5 ?` — the unpriced
    /// model has real traffic here and must not vanish from the line, and a
    /// percentage is exactly what nobody can compute for it.
    ///
    /// Those five entries are also this scene's LAYOUT case, and the reason it
    /// is worth looking at rather than only asserting on. The line wraps, and a
    /// wrap is charged to the account list (`PanelHeight.headerOverflow`), so
    /// what a reader must check here is that the list still has rows and the
    /// footer has not moved. It read `opus-5 100% · sonnet-4-5 ?` while only
    /// the top two models were named and the rest became `"+2"`.
    ///
    /// The fully-priced card (`$5.61 · 12k out`, no marker) is scene 01's.
    /// The same four rows also carry all four branches of the FABLE weekly
    /// slot on their 7d line, because that slot has the same shape of rule and
    /// the same way of being got wrong:
    ///
    ///  - `partial@` — `near`: `fable 71% · in 4d 12h`, in that window's amber.
    ///  - `unpriced@` — `spent`: `fable 100% · in 4d 12h`, in red. Its 7d
    ///    window is `ok` in the same frame, which is the point: the two are
    ///    independent windows and a reader must be able to see one spent while
    ///    the other has headroom.
    ///  - `no-window@` — a real fraction with NO state word, which is what a
    ///    proxy predating `sevenDayOiState` sends and therefore what the live
    ///    one sends today. Neutral tint, real percentage: nothing borrows the
    ///    composite state to colour it.
    ///  - `unmeasured@` — no fraction at all: an EMPTY slot, never `n/a`.
    private static var usageStatsJSON: String {
        let partial = account(
            "partial@example.com", quota: "0.42", state: "ok",
            fiveHourResetInMinutes: 130, sevenDayResetInMinutes: 4_320,
            sevenDayOi: "0.71", sevenDayOiState: "near", sevenDayOiResetInMinutes: 6_498,
            usage: partiallyPricedUsage)
        let unpriced = account(
            "unpriced@example.com", quota: "0.31", state: "ok",
            sevenDayOi: "1.0", sevenDayOiState: "spent", sevenDayOiResetInMinutes: 6_498,
            usage: unpricedUsage)
        let noWindow = account(
            "no-window@example.com", quota: "0.18", state: "ok",
            sevenDayOi: "0.34", sevenDayOiState: "null",
            usage: noWindowUsage)
        let unmeasured = account(
            "unmeasured@example.com", quota: "0.07", state: "ok",
            sevenDayOi: "null", sevenDayOiState: "null",
            usage: unmeasuredUsage)
        return "[\(partial),\(unpriced),\(noWindow),\(unmeasured)]"
    }

    /// The bug this whole scene set exists for: a dead-credential account
    /// (`status:"error"`, `probeStatus:"never"`) beside a healthy one, so the
    /// pill, the status word's tint, the row order and the header clause are
    /// all reviewable together. The Re-login button draws as a placeholder —
    /// `ImageRenderer` cannot rasterise AppKit controls — but its presence
    /// beside Disable still shows in the row's width.
    private static var needsReloginJSON: String {
        let alice = account("alice@example.com", quota: "0.12", state: "ok")
        let dave = account(
            "dave@example.com", quota: "null", state: "ok", probe: "never", status: "error")
        return "[\(alice),\(dave)]"
    }

    /// The blind spot an adversarial review found: `04b` above is the OTHER
    /// way an account breaks — never probed, `quota: null`. This is the shape
    /// that actually happens in production: a credential that dies AFTER
    /// being probed keeps its last-learned `quota` and a real `probeStatus`
    /// (`probe_account`, `src/manager/probing.rs:128-139`, early-returns on
    /// an `Error` row instead of clearing anything; `refresh.rs:93-101` sets
    /// only `status`). `carol@example.com` here carries `status:"error"` WITH
    /// `quota:0.12` and `probeStatus:"ok"` — a real prior reading, not an
    /// absent one — sat beside a genuinely healthy account so the header's
    /// arithmetic ("1 of 2 ready · 1 need re-login", not "2 of 2 ready") is
    /// reviewable in the same frame this scene renders.
    private static var probedThenBrokenJSON: String {
        let alice = account("alice@example.com", quota: "0.12", state: "ok")
        let carol = account(
            "carol@example.com", quota: "0.12", state: "ok", probe: "ok", status: "error")
        return "[\(alice),\(carol)]"
    }
}

// MARK: - The canonical Codex rows

extension RenderStates {
    /// The ten rows of `apps/macos/Tests/Fixtures/codex-status.json`, by name; alice is the one
    /// Claude row. A Codex scene draws these, so it shows what the fixture says.
    enum CanonicalCodexRow: String, CaseIterable {
        case alice = "alice@example.com"
        case bob = "bob@example.com/codex"
        case carol = "carol@example.com"
        case dana = "dana@example.com"
        case erin = "erin@example.com/codex"
        case gil = "gil@example.com"
        case hal = "hal@example.com"
        case ivy = "ivy@example.com"
        case jo = "jo@example.com"
        case kim = "kim@example.com"
    }

    /// One canonical row as a JSON object for a scene's array: the fixture's facts, with every
    /// absolute instant moved to sit as far from `now` as it sits from the fixture's own clock,
    /// so carol frees in 3d 4h and hal is held for 47m whenever the render runs. `extra` is
    /// laid over the row last, as `account(...)`'s is, for a scene that varies one fact.
    /// `RenderFixtureRow`, in TcrBarCore, builds the row, where the shim tests it.
    static func codexRow(_ row: CanonicalCodexRow, now: Date, extra: [String: Any] = [:]) -> String {
        guard
            let text = RenderFixtureRow.json(
                named: row.rawValue, in: canonicalCodexStatusJSON, now: now, extra: extra)
        else { preconditionFailure("the canonical Codex rows hold no \(row.rawValue)") }
        return text
    }

    /// `object` with `extra` laid over it (`extra` wins), as JSON text with sorted keys, written
    /// by `RenderFixtureRow.jsonText(_:laying:)`. A value is anything `JSONSerialization` writes:
    /// a string, a number, a `Bool`, `NSNull()` for `null`, or an array or dictionary of those.
    private static func jsonText(_ object: [String: Any], laying extra: [String: Any]) -> String {
        guard let text = RenderFixtureRow.jsonText(object, laying: extra)
        else { preconditionFailure("a render fixture row is not valid JSON: \(object), with \(extra)") }
        return text
    }

    /// Codex traffic with nothing refused and nothing sent around the pool in the last ten
    /// minutes. The canonical Codex rows carry refusals, which draw the refusal banner, so a
    /// scene about something else lays this over each of its Codex rows. Never over a Claude
    /// row, which reports no Codex traffic at all.
    private static let quietCodexTraffic: [String: Any] = ["refused10m": 0, "unpooled10m": 0]

    /// Scene `24-mixed-fleet`, at the real clock: Claude's two accounts both out of tokens
    /// (``mixedClaudeRows(now:)``), and Codex's two split: bob is ready and carol is held on
    /// her week, both canonical, with quiet traffic.
    private static var mixedFleetJSON: String {
        let now = Date()
        let bob = codexRow(.bob, now: now, extra: ["codexTraffic": quietCodexTraffic])
        let carol = codexRow(.carol, now: now, extra: ["codexTraffic": quietCodexTraffic])
        return "[\((mixedClaudeRows(now: now) + [bob, carol]).joined(separator: ","))]"
    }

    /// Scene `24e-mixed-fleet-unpriced`: scene `24-mixed-fleet` with three of alice's Claude
    /// requests today on a model the config has no price for, so the money line carries
    /// the unpriced clause beside the Codex one.
    private static var mixedFleetUnpricedJSON: String {
        let now = Date()
        let rows = mixedClaudeRows(now: now)
        guard
            let alice = try? JSONSerialization.jsonObject(with: Data(rows[0].utf8))
                as? [String: Any],
            var usage = alice["usage"] as? [String: Any],
            var today = usage["today"] as? [String: Any]
        else { preconditionFailure("canonical alice carries no usage today") }
        today["unpricedRequests"] = 3
        usage["today"] = today
        let unpriced = jsonText(alice, laying: ["usage": usage])
        let bob = codexRow(.bob, now: now, extra: ["codexTraffic": quietCodexTraffic])
        let carol = codexRow(.carol, now: now, extra: ["codexTraffic": quietCodexTraffic])
        return "[\(([unpriced, rows[1], bob, carol]).joined(separator: ","))]"
    }

    /// Scene `24-mixed-fleet`'s two Claude accounts, both out of tokens, which scene
    /// `24d-mixed-codex-all-parked` draws too.
    ///
    /// - alice is the canonical row with her 5-hour window full and held, her week at 41%,
    ///   and in no group.
    /// - frank is a second Claude account: his 5-hour window at 22%, his week spent and
    ///   held, and alice's day of usage, so the money line totals two of her days.
    private static func mixedClaudeRows(now: Date) -> [String] {
        func atMs(_ minutes: Int) -> Int64 {
            Int64((now.timeIntervalSince1970 + Double(minutes) * 60) * 1000)
        }
        // `extra` is laid over the row after its instants move, so its own instants are
        // written from `now` here.
        let alice = codexRow(
            .alice, now: now,
            extra: [
                "quotaState": "spent", "quota": 1.0, "fiveHour": 1.0, "fiveHourState": "spent",
                "held": [["window": "5h", "minutesUntilReset": 159, "resetAtMs": atMs(159)]],
                "sevenDay": 0.41, "groups": [String](),
            ])
        let frank = account(
            "frank@example.com", quota: "1.0", state: "spent",
            held: #"[{"window":"7d","minutesUntilReset":4320,"resetAtMs":\#(atMs(4320))}]"#,
            fiveHour: "0.22", fiveHourState: "ok", sevenDay: "1.0", sevenDayState: "spent",
            fiveHourResetInMinutes: 250, sevenDayResetInMinutes: 4320,
            sevenDayOi: "0.40", sevenDayOiState: "ok", usage: frankUsage(now: now),
            plan: "Max 20x", now: now)
        return [alice, frank]
    }

    /// Scene `24b-mixed-fleet-compact`, at the real clock: seven accounts, so `.auto` draws
    /// it compact. Claude's are nina (near her 5-hour limit), grace (switched off), henry and
    /// canonical alice, both in ORCHARD; Codex's are bob, erin (signed out) and ivy, in
    /// ORCHARD too, with quiet traffic. ORCHARD holds members of both providers, so it is
    /// drawn once under each head.
    private static var mixedFleetCompactJSON: String {
        let now = Date()
        let claude = [
            account(
                "nina@example.com", quota: "0.91", state: "near", fiveHour: "0.91",
                fiveHourState: "near", sevenDay: "0.25", sevenDayState: "ok",
                fiveHourResetInMinutes: 72, sevenDayResetInMinutes: 7200, plan: "Max 5x",
                orgUuid: "44444444-4444-4444-8444-444444444444"),
            account(
                "grace@example.com", quota: "0.12", state: "ok", disabled: true, fiveHour: "0.08",
                sevenDay: "0.12", fiveHourResetInMinutes: 181, sevenDayResetInMinutes: 8640,
                plan: "Max 20x", orgUuid: "55555555-5555-4555-8555-555555555555", requests: 0),
            account(
                "henry@example.com", quota: "0.30", state: "ok", fiveHour: "0.30", sevenDay: "0.15",
                fiveHourResetInMinutes: 100, sevenDayResetInMinutes: 5760, groups: ["orchard"],
                groupColors: ["orchard": "#30d158"], plan: "Team 5x",
                orgUuid: "66666666-6666-4666-8666-666666666666"),
            codexRow(.alice, now: now),
        ]
        let codex = [CanonicalCodexRow.bob, .erin, .ivy].map {
            codexRow($0, now: now, extra: ["codexTraffic": quietCodexTraffic])
        }
        return "[\((claude + codex).joined(separator: ","))]"
    }

    /// Scene `24d-mixed-codex-all-parked`: scene 24's two Claude accounts, and bob and carol
    /// switched off. No Codex account can serve, so no Codex line can show, and the rows
    /// need no quiet traffic.
    private static var mixedCodexAllParkedJSON: String {
        let now = Date()
        let codex = [CanonicalCodexRow.bob, .carol].map {
            codexRow($0, now: now, extra: ["disabled": true])
        }
        return "[\((mixedClaudeRows(now: now) + codex).joined(separator: ","))]"
    }

    /// The canonical `rows` at the real clock, each with `extra` laid over it, as one scene's
    /// array. Unlike ``codexScene(_:)``, each row keeps the fixture's traffic, twelve
    /// refusals, unless `extra` lays its own: these scenes are about the Codex line.
    private static func codexRows(_ rows: [CanonicalCodexRow], extra: [String: Any] = [:]) -> String {
        let now = Date()
        return "[\(rows.map { codexRow($0, now: now, extra: extra) }.joined(separator: ","))]"
    }

    /// The route of a proxy that cannot pool codex, in the words `tcr status --json` gives it.
    private static let unpooledRoute: [String: Any] = [
        "pooled": false, "code": "not-pooled",
        "reason":
            "the proxy on :3456 does not route codex traffic and a restart will not change this; see docs/troubleshooting.md",
    ]

    /// Scenes `26d`, `26e` and `26g`: bob and dana at zero requests, with quiet traffic.
    private static var idleCodexJSON: String {
        codexRows([.bob, .dana], extra: ["requests": 0, "codexTraffic": quietCodexTraffic])
    }

    /// Scene `26h`: scene 22's live Claude account at zero requests, beside canonical bob,
    /// who has served 42, with quiet traffic.
    private static var mixedClaudeQuietJSON: String {
        "[\(colleagueRow),\(codexRow(.bob, now: Date(), extra: ["codexTraffic": quietCodexTraffic]))]"
    }

    /// Scene `25d-codex-relogin-refused`: canonical alice, and erin, signed out, with quiet
    /// traffic so no refusal banner draws. alice reports no Codex traffic at all.
    private static var codexReloginRefusedJSON: String {
        let now = Date()
        return
            "[\(codexRow(.alice, now: now)),\(codexRow(.erin, now: now, extra: ["codexTraffic": quietCodexTraffic]))]"
    }

    /// Scene `25f-codex-group-add-warning`: canonical alice and ivy, both in ORCHARD, ivy with
    /// quiet traffic.
    private static var codexGroupAddWarningJSON: String {
        let now = Date()
        return
            "[\(codexRow(.alice, now: now)),\(codexRow(.ivy, now: now, extra: ["codexTraffic": quietCodexTraffic]))]"
    }

    /// One Codex row of scenes 25d and 25f, decoded as those scenes decode it, so a note is
    /// recorded against the row the scene draws.
    private static func sceneRow(_ row: CanonicalCodexRow) -> Account {
        let json = "[\(codexRow(row, now: Date(), extra: ["codexTraffic": quietCodexTraffic]))]"
        guard let account = try? Fleet.decode(Data(json.utf8)).accounts.first else {
            preconditionFailure("the canonical \(row.rawValue) does not decode")
        }
        return account
    }

    /// tcr's refusal of a Codex login whose ChatGPT workspace is served away from
    /// chatgpt.com, as `tcr login --codex` sends it.
    private static let routedRefusal =
        "this Codex login's ChatGPT workspace is not served at https://chatgpt.com with no "
        + "routing of its own (workspace_backend_origin is \"https://gov.chatgpt.com\"): codex "
        + "sends that workspace's traffic to its own backend, or routes it by residency, and tcr "
        + "pools a Codex login only at https://chatgpt.com. Nothing was written."

    /// What `tcr group add orchard ivy@example.com` prints on stderr when it succeeds.
    private static let orchardWarning =
        "warning: 'ivy@example.com' is a Codex account. A Codex session cannot ask for a group, "
        + "so 'orchard' labels it and can park it, but never routes to it; while 'orchard' is "
        + "reserved, it serves nothing."

    /// frank's `usage`: canonical alice's, so his day is hers, with his own window on his
    /// card, `$9.02 · 31k`.
    private static func frankUsage(now: Date) -> String {
        guard
            let row = try? JSONSerialization.jsonObject(
                with: Data(codexRow(.alice, now: now).utf8)) as? [String: Any],
            var usage = row["usage"] as? [String: Any],
            var window = usage["window"] as? [String: Any]
        else { preconditionFailure("canonical alice carries no usage window") }
        window["costUsd"] = 9.02
        window["outputTokens"] = 31_000
        usage["window"] = window
        return jsonText(usage, laying: [:])
    }

    /// The canonical rows `rows`, at the real clock, as one scene's array. Every Codex row
    /// lays ``quietCodexTraffic`` over its own, because the canonical rows carry refusals,
    /// which draw the refusal banner; alice, the one Claude row, reports no Codex traffic.
    private static func codexScene(_ rows: [CanonicalCodexRow]) -> String {
        let now = Date()
        let built = rows.map { row in
            row == .alice
                ? codexRow(row, now: now)
                : codexRow(row, now: now, extra: ["codexTraffic": quietCodexTraffic])
        }
        return "[\(built.joined(separator: ","))]"
    }

    /// Scene `25e-codex-unmeasured`: one Codex account before its first probe, with no
    /// quota, no usage, and no word yet on which windows its plan has.
    private static var unmeasuredCodexJSON: String {
        "["
            + account(
                "lee@example.com/codex", quota: "null", state: "ok", probe: "never",
                usage: "null", plan: "ChatGPT Plus", provider: "codex")
            + "]"
    }

    // MARK: The Sessions and Tools scenes

    /// Scene `16b-sessions-tab-mixed`: canonical alice and bob, with both canonical sessions.
    private static var sessionsTabMixedFleet: Fleet {
        let now = Date()
        return fleet("[\(codexRow(.alice, now: now)),\(codexRow(.bob, now: now))]")
            .withSessions(canonicalSessionsRead(now: now))
    }

    /// Scene `16c-sessions-tab-codex-only-empty`: canonical bob and dana with an hour that
    /// served nothing, and no session, on a live channel. The canonical rows served 7 and 9
    /// requests in the last hour, which would rightly draw the old-server line instead.
    private static var codexOnlyQuietSessionsFleet: Fleet {
        let now = Date()
        let rows = [CanonicalCodexRow.bob, .dana].map { row in
            codexRow(row, now: now, extra: ["usage": usageWithQuietHour(row, now: now)])
        }
        return fleet("[\(rows.joined(separator: ","))]")
            .withSessions(Fleet.SessionsRead(sessions: [], channel: .live))
    }

    /// Scene `16d-sessions-tab-old-server-codex`: canonical alice and bob, bob's last hour
    /// at 7 requests, and alice's session alone, as a proxy built before Codex sessions lists
    /// them: no session carries a provider.
    private static var oldServerCodexSessionsFleet: Fleet {
        let now = Date()
        return fleet("[\(codexRow(.alice, now: now)),\(codexRow(.bob, now: now))]")
            .withSessions(canonicalSessionsRead(now: now, includingBob: false))
    }

    /// Scene `17c-tools-tab-codex`: canonical alice and bob with both sessions, bob's whole
    /// timeline 55 s earlier so his call, started 100 s ago, still starts after his last
    /// request. That call is codex's one-shot `exec_command`, the one schema that carries a
    /// timeout (`timeout_ms` 120000, so 120 s), which the canonical call, codex's default
    /// interactive `exec_command`, does not: 100 s into its 120 its ring is red with 20 s left,
    /// where the Bash tool's 600 s would call it calm.
    private static var toolsTabCodexFleet: Fleet {
        let now = Date()
        return fleet("[\(codexRow(.alice, now: now)),\(codexRow(.bob, now: now))]")
            .withSessions(
                canonicalSessionsRead(now: now, bobEarlierBy: 55, bobTimeoutSeconds: 120))
    }

    /// The canonical row's `usage` with a last hour that served nothing, shaped as the wire
    /// writes an idle hour, and every instant already moved to `now`.
    private static func usageWithQuietHour(_ row: CanonicalCodexRow, now: Date) -> [String: Any] {
        guard
            let object = try? JSONSerialization.jsonObject(
                with: Data(codexRow(row, now: now).utf8)) as? [String: Any],
            var usage = object["usage"] as? [String: Any]
        else { preconditionFailure("canonical \(row.rawValue) carries no usage") }
        usage["lastHour"] = [
            "cacheCreation1hTokens": 0, "cacheCreationTokens": 0, "cacheReadTokens": 0,
            "costUsd": 0.0, "inputTokens": 0, "outputTokens": 0, "requests": 0,
            "unpricedRequests": 0,
        ]
        return usage
    }

    /// `apps/macos/Tests/Fixtures/codex-sessions.json`'s two sessions, every instant written
    /// as far before `now` as it sits before the fixture's clock, decoded through
    /// `Fleet.decodeSessions`: alice first seen 40 min ago and last seen 3 s ago, her Bash
    /// `cargo test` started 20 s ago; bob first seen 175 s ago and last seen 50 s ago, his
    /// `cargo build` started 45 s ago.
    ///
    /// `bobEarlierBy` moves every one of bob's instants that many seconds earlier, and
    /// `bobTimeoutSeconds` lays a timeout on his running call. `includingBob: false` leaves his
    /// session out.
    private static func canonicalSessionsRead(
        now: Date, includingBob: Bool = true, bobEarlierBy: TimeInterval = 0,
        bobTimeoutSeconds: Int? = nil
    ) -> Fleet.SessionsRead {
        func ago(_ seconds: TimeInterval) -> Int64 {
            Int64(((now.timeIntervalSince1970 - seconds) * 1000).rounded())
        }
        func bobAgo(_ seconds: TimeInterval) -> Int64 { ago(seconds + bobEarlierBy) }
        let alice = """
            {"account": "alice@example.com", "cacheCreationTokens": 12000,
             "cacheReadTokens": 160000, "cacheTtlSecs": 3600, "costUsd": 1.2034,
             "firstSeenMs": \(ago(2400)), "inputTokens": 180000, "lastSeenMs": \(ago(3)),
             "model": "claude-opus-5", "outputTokens": 9200,
             "reqPerMinute": [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                              1, 2, 3, 2, 4, 3, 5, 0],
             "requests": 24, "sessionId": "a1f3c2d4-0000-4000-8000-000000000001",
             "tools": {
               "byTool": [
                 {"calls": 8, "errors": 0, "overOneMinute": 0, "secondsP50": 2.5, "tool": "Bash"},
                 {"calls": 4, "errors": 0, "overOneMinute": 0, "secondsP50": 0.1, "tool": "Read"}],
               "calls": 12, "errors": 0, "overOneMinute": 0,
               "running": [
                 {"commandClass": "build", "commandHead": "cargo test",
                  "startedMs": \(ago(20)), "tool": "Bash"}],
               "slowest": [
                 {"commandClass": "build", "commandHead": "cargo build --release",
                  "endedMs": \(ago(295)), "seconds": 12.0, "tool": "Bash"},
                 {"commandClass": "build", "commandHead": "cargo build",
                  "endedMs": \(ago(230)), "seconds": 6.0, "tool": "Bash"},
                 {"commandClass": "search", "commandHead": "rg -n TODO",
                  "endedMs": \(ago(175)), "seconds": 3.0, "tool": "Bash"},
                 {"commandClass": "build", "commandHead": "cargo test -p example",
                  "endedMs": \(ago(250)), "seconds": 2.5, "tool": "Bash"},
                 {"commandClass": "git-local", "commandHead": "git status",
                  "endedMs": \(ago(160)), "seconds": 2.5, "tool": "Bash"},
                 {"commandClass": "git-local", "commandHead": "git status",
                  "endedMs": \(ago(320)), "seconds": 2.0, "tool": "Bash"},
                 {"commandClass": "build", "commandHead": "cargo test",
                  "endedMs": \(ago(110)), "seconds": 2.0, "tool": "Bash"},
                 {"commandClass": "git-local", "commandHead": "git status",
                  "endedMs": \(ago(125)), "seconds": 1.5, "tool": "Bash"},
                 {"commandClass": null,
                  "commandHead": "Read /Users/example/git/tcr/src/session_wire.rs",
                  "endedMs": \(ago(280)), "seconds": 0.4, "tool": "Read"},
                 {"commandClass": null,
                  "commandHead": "Read /Users/example/git/tcr/src/proxy.rs",
                  "endedMs": \(ago(350)), "seconds": 0.1, "tool": "Read"}],
               "subagentsRunning": 0, "timedOut": [], "timeouts": 0, "timeoutsByClass": {}}}
            """
        let timeout = bobTimeoutSeconds.map { #", "timeoutSeconds": \#($0)"# } ?? ""
        let bob = """
            {"account": "bob@example.com/codex", "cacheCreationTokens": 0,
             "cacheReadTokens": 80000, "costUsd": 0.0, "firstSeenMs": \(bobAgo(175)),
             "inputTokens": 95000, "lastSeenMs": \(bobAgo(50)), "model": "gpt-5.1-codex",
             "outputTokens": 4100, "provider": "codex", "reasoningTokens": 1200,
             "reqPerMinute": [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                              0, 0, 0, 0, 2, 3, 2, 0],
             "requests": 7, "sessionId": "019a3c5e-0000-7000-8000-000000000002",
             "tools": {
               "byTool": [
                 {"calls": 2, "errors": 0, "overOneMinute": 0, "secondsP50": 4.0,
                  "tool": "exec_command"},
                 {"calls": 1, "errors": 0, "overOneMinute": 0, "secondsP50": 0.2,
                  "tool": "apply_patch"}],
               "calls": 3, "errors": 0, "overOneMinute": 0,
               "running": [
                 {"commandClass": "build", "commandHead": "cargo build",
                  "startedMs": \(bobAgo(45)), "tool": "exec_command"\(timeout)}],
               "slowest": [
                 {"commandClass": "build", "commandHead": "cargo test",
                  "endedMs": \(bobAgo(115)), "seconds": 4.0, "tool": "exec_command"},
                 {"commandClass": "build", "commandHead": "cargo check",
                  "endedMs": \(bobAgo(55.5)), "seconds": 4.0, "tool": "exec_command"},
                 {"commandClass": null, "commandHead": "apply_patch",
                  "endedMs": \(bobAgo(94.8)), "seconds": 0.2, "tool": "apply_patch"}],
               "subagentsRunning": 0, "timedOut": [], "timeouts": 0, "timeoutsByClass": {}},
             "unpricedRequests": 7}
            """
        let sessions = includingBob ? [alice, bob] : [alice]
        let document = #"{"supported": true, "sessions": [\#(sessions.joined(separator: ","))]}"#
        guard let read = try? Fleet.decodeSessions(Data(document.utf8)),
            read.unreadable.isEmpty, read.sessions.count == sessions.count
        else { preconditionFailure("the canonical sessions no longer decode") }
        return read
    }

    /// `apps/macos/Tests/Fixtures/codex-status.json`, verbatim, indented to sit here.
    /// `RenderStatesCanonicalRowsTests` holds it equal to the fixture.
    private static let canonicalCodexStatusJSON = #"""
        [
          {
            "cacheCreationTokens": 1600000,
            "cacheHitRatio": 0.75,
            "cacheReadTokens": 6000000,
            "control": false,
            "controlAllowedGroups": [],
            "disabled": false,
            "fiveHour": 0.47,
            "fiveHourResetAtMs": 1767235140000,
            "fiveHourState": "ok",
            "freeAtMs": null,
            "gate": "ok",
            "groupColors": {
              "orchard": "#30d158",
              "research": "#bf5af2"
            },
            "groups": [
              "orchard"
            ],
            "held": [],
            "http1Only": false,
            "inputTokens": 8000000,
            "lastProbeMs": null,
            "lastStreamError": "overloaded_error",
            "name": "alice@example.com",
            "orgName": "Example Personal",
            "orgUuid": "11111111-1111-1111-1111-111111111111",
            "organizationType": "claude_max",
            "outputTokens": 31860,
            "parkedGroups": [],
            "plan": "Max 20x",
            "priority": 0,
            "probeError": null,
            "probeStatus": "ok",
            "quota": 0.62,
            "quotaState": "ok",
            "quotaUpdatedAtMs": null,
            "rateLimitTier": "default_claude_max_20x",
            "rateLimitedUntilMs": null,
            "requests": 102,
            "reservedGroups": [],
            "seatTier": null,
            "secondsUntilFree": null,
            "serverDirty": false,
            "serverSha": "abc1234",
            "sevenDay": 0.2,
            "sevenDayOi": 0.62,
            "sevenDayOiResetAtMs": 1767571200000,
            "sevenDayOiState": "ok",
            "sevenDayResetAtMs": 1767571200000,
            "sevenDayState": "ok",
            "source": "live",
            "status": "active",
            "streamErrorCount": 2,
            "usage": {
              "lastHour": {
                "cacheCreation1hTokens": 47000,
                "cacheCreationTokens": 188000,
                "cacheReadTokens": 705000,
                "costUsd": 1.6413,
                "inputTokens": 47000,
                "outputTokens": 3756,
                "requests": 12,
                "unpricedRequests": 0
              },
              "today": {
                "cacheCreation1hTokens": 400000,
                "cacheCreationTokens": 1600000,
                "cacheReadTokens": 6000000,
                "costUsd": 14.1657,
                "inputTokens": 400000,
                "outputTokens": 31860,
                "requests": 102,
                "unpricedRequests": 0
              },
              "todayByModel": {
                "claude-opus-5": {
                  "cacheCreation1hTokens": 280000,
                  "cacheCreationTokens": 1120000,
                  "cacheReadTokens": 4200000,
                  "costUsd": 12.0785,
                  "inputTokens": 280000,
                  "outputTokens": 21140,
                  "requests": 70,
                  "unpricedRequests": 0
                },
                "claude-sonnet-5": {
                  "cacheCreation1hTokens": 120000,
                  "cacheCreationTokens": 480000,
                  "cacheReadTokens": 1800000,
                  "costUsd": 2.0872,
                  "inputTokens": 120000,
                  "outputTokens": 10720,
                  "requests": 32,
                  "unpricedRequests": 0
                }
              },
              "window": {
                "cacheCreation1hTokens": 157000,
                "cacheCreationTokens": 628000,
                "cacheReadTokens": 2355000,
                "costUsd": 5.6141,
                "inputTokens": 157000,
                "outputTokens": 12476,
                "requests": 40,
                "since": 1767217140000,
                "unpricedRequests": 0
              }
            }
          },
          {
            "cacheCreationTokens": 0,
            "cacheHitRatio": 0.82,
            "cacheReadTokens": 902000,
            "codexRoute": {
              "pooled": true
            },
            "codexTraffic": {
              "lastRefusal": "usage_limit_reached",
              "lastRefusalAtMs": 1767225560000,
              "refused10m": 12,
              "unpooled10m": 3
            },
            "control": false,
            "controlAllowedGroups": [],
            "disabled": false,
            "fiveHour": null,
            "fiveHourResetAtMs": null,
            "fiveHourState": null,
            "freeAtMs": null,
            "gate": "ok",
            "groupColors": {
              "orchard": "#30d158",
              "research": "#bf5af2"
            },
            "groups": [],
            "held": [],
            "http1Only": false,
            "inputTokens": 1100000,
            "lastProbeMs": 1767225570000,
            "lastStreamError": null,
            "name": "bob@example.com/codex",
            "orgName": null,
            "orgUuid": "55555555-5555-4555-8555-555555555555",
            "organizationType": "pro",
            "outputTokens": 22000,
            "parkedGroups": [],
            "plan": "ChatGPT Pro",
            "priority": 1,
            "probeError": null,
            "probeStatus": "ok",
            "provider": "codex",
            "quota": 0.34,
            "quotaState": "ok",
            "quotaUpdatedAtMs": null,
            "quotaWindows": [
              "sevenDay"
            ],
            "rateLimitTier": null,
            "rateLimitedUntilMs": null,
            "requests": 42,
            "reservedGroups": [],
            "seatTier": null,
            "secondsUntilFree": null,
            "serverDirty": false,
            "serverSha": "abc1234",
            "sevenDay": 0.34,
            "sevenDayOi": null,
            "sevenDayOiResetAtMs": null,
            "sevenDayOiState": null,
            "sevenDayResetAtMs": 1767571200000,
            "sevenDayState": "ok",
            "sevenDayWindowMinutes": 10080,
            "source": "live",
            "status": "active",
            "streamErrorCount": 0,
            "usage": {
              "lastHour": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 160000,
                "costUsd": null,
                "inputTokens": 30000,
                "outputTokens": 4100,
                "reasoningTokens": 1200,
                "requests": 7,
                "unpricedRequests": 7
              },
              "sevenDayWindow": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 50000000,
                "costUsd": null,
                "inputTokens": 11000000,
                "outputTokens": 1100000,
                "reasoningTokens": 420000,
                "requests": 1480,
                "since": 1766966400000,
                "unpricedRequests": 1480
              },
              "today": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 902000,
                "costUsd": null,
                "inputTokens": 198000,
                "outputTokens": 22000,
                "reasoningTokens": 9000,
                "requests": 42,
                "unpricedRequests": 42
              },
              "todayByModel": {
                "gpt-5.1-codex": {
                  "cacheCreation1hTokens": 0,
                  "cacheCreationTokens": 0,
                  "cacheReadTokens": 902000,
                  "costUsd": null,
                  "inputTokens": 198000,
                  "outputTokens": 22000,
                  "reasoningTokens": 9000,
                  "requests": 42,
                  "unpricedRequests": 42
                }
              }
            }
          },
          {
            "cacheCreationTokens": 0,
            "cacheHitRatio": 0.89,
            "cacheReadTokens": 4806000,
            "codexRoute": {
              "pooled": true
            },
            "codexTraffic": {
              "lastRefusal": "usage_limit_reached",
              "lastRefusalAtMs": 1767225560000,
              "refused10m": 12,
              "unpooled10m": 3
            },
            "control": false,
            "controlAllowedGroups": [],
            "credits": {
              "balance": "12.50",
              "hasCredits": true,
              "unlimited": false
            },
            "disabled": false,
            "fiveHour": null,
            "fiveHourResetAtMs": null,
            "fiveHourState": null,
            "freeAtMs": 1767499200000,
            "gate": "seven-day",
            "groupColors": {
              "orchard": "#30d158",
              "research": "#bf5af2"
            },
            "groups": [],
            "held": [
              {
                "minutesUntilReset": 4560,
                "resetAtMs": 1767499200000,
                "window": "7d"
              }
            ],
            "http1Only": false,
            "inputTokens": 5400000,
            "lastProbeMs": 1767225570000,
            "lastStreamError": null,
            "limitReachedType": "rate_limit_reached",
            "name": "carol@example.com",
            "orgName": null,
            "orgUuid": "66666666-6666-4666-8666-666666666666",
            "organizationType": "pro",
            "outputTokens": 61000,
            "parkedGroups": [],
            "plan": "ChatGPT Pro",
            "priority": 2,
            "probeError": null,
            "probeStatus": "ok",
            "provider": "codex",
            "quota": 1.0,
            "quotaState": "spent",
            "quotaUpdatedAtMs": null,
            "quotaWindows": [
              "sevenDay"
            ],
            "rateLimitTier": null,
            "rateLimitedUntilMs": null,
            "requests": 310,
            "reservedGroups": [],
            "seatTier": null,
            "secondsUntilFree": 273600,
            "serverDirty": false,
            "serverSha": "abc1234",
            "sevenDay": 1.0,
            "sevenDayOi": null,
            "sevenDayOiResetAtMs": null,
            "sevenDayOiState": null,
            "sevenDayResetAtMs": 1767499200000,
            "sevenDayState": "spent",
            "sevenDayWindowMinutes": 10080,
            "source": "live",
            "status": "active",
            "streamErrorCount": 0,
            "usage": {
              "lastHour": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 0,
                "costUsd": 0.0,
                "inputTokens": 0,
                "outputTokens": 0,
                "requests": 0,
                "unpricedRequests": 0
              },
              "sevenDayWindow": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 140000000,
                "costUsd": null,
                "inputTokens": 20000000,
                "outputTokens": 5400000,
                "reasoningTokens": 2100000,
                "requests": 2900,
                "since": 1766894400000,
                "unpricedRequests": 2900
              },
              "today": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 4806000,
                "costUsd": null,
                "inputTokens": 594000,
                "outputTokens": 61000,
                "reasoningTokens": 21000,
                "requests": 310,
                "unpricedRequests": 310
              },
              "todayByModel": {
                "gpt-5.1-codex": {
                  "cacheCreation1hTokens": 0,
                  "cacheCreationTokens": 0,
                  "cacheReadTokens": 4806000,
                  "costUsd": null,
                  "inputTokens": 594000,
                  "outputTokens": 61000,
                  "reasoningTokens": 21000,
                  "requests": 310,
                  "unpricedRequests": 310
                }
              }
            }
          },
          {
            "cacheCreationTokens": 0,
            "cacheHitRatio": 0.85,
            "cacheReadTokens": 1700000,
            "codexRoute": {
              "pooled": true
            },
            "codexTraffic": {
              "lastRefusal": "usage_limit_reached",
              "lastRefusalAtMs": 1767225560000,
              "refused10m": 12,
              "unpooled10m": 3
            },
            "control": false,
            "controlAllowedGroups": [],
            "disabled": false,
            "fiveHour": 0.18,
            "fiveHourResetAtMs": 1767236520000,
            "fiveHourState": "ok",
            "fiveHourWindowMinutes": 300,
            "freeAtMs": null,
            "gate": "ok",
            "groupColors": {
              "orchard": "#30d158",
              "research": "#bf5af2"
            },
            "groups": [],
            "held": [],
            "http1Only": false,
            "inputTokens": 2000000,
            "lastProbeMs": 1767225570000,
            "lastStreamError": null,
            "name": "dana@example.com",
            "orgName": null,
            "orgUuid": "77777777-7777-4777-8777-777777777777",
            "organizationType": "plus",
            "outputTokens": 260000,
            "parkedGroups": [],
            "plan": "ChatGPT Plus",
            "priority": 3,
            "probeError": null,
            "probeStatus": "ok",
            "provider": "codex",
            "quota": 0.25,
            "quotaState": "ok",
            "quotaUpdatedAtMs": null,
            "quotaWindows": [
              "fiveHour",
              "sevenDay"
            ],
            "rateLimitTier": null,
            "rateLimitedUntilMs": null,
            "requests": 96,
            "reservedGroups": [],
            "seatTier": null,
            "secondsUntilFree": null,
            "serverDirty": false,
            "serverSha": "abc1234",
            "sevenDay": 0.25,
            "sevenDayOi": null,
            "sevenDayOiResetAtMs": null,
            "sevenDayOiState": null,
            "sevenDayResetAtMs": 1767657600000,
            "sevenDayState": "ok",
            "sevenDayWindowMinutes": 10080,
            "source": "live",
            "status": "active",
            "streamErrorCount": 0,
            "usage": {
              "lastHour": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 150000,
                "costUsd": null,
                "inputTokens": 30000,
                "outputTokens": 24000,
                "reasoningTokens": 6000,
                "requests": 9,
                "unpricedRequests": 9
              },
              "sevenDayWindow": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 7600000,
                "costUsd": null,
                "inputTokens": 1400000,
                "outputTokens": 640000,
                "reasoningTokens": 180000,
                "requests": 400,
                "since": 1767052800000,
                "unpricedRequests": 400
              },
              "today": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 1700000,
                "costUsd": null,
                "inputTokens": 300000,
                "outputTokens": 260000,
                "reasoningTokens": 70000,
                "requests": 96,
                "unpricedRequests": 96
              },
              "todayByModel": {
                "gpt-5.1-codex": {
                  "cacheCreation1hTokens": 0,
                  "cacheCreationTokens": 0,
                  "cacheReadTokens": 1700000,
                  "costUsd": null,
                  "inputTokens": 300000,
                  "outputTokens": 260000,
                  "reasoningTokens": 70000,
                  "requests": 96,
                  "unpricedRequests": 96
                }
              },
              "window": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 540000,
                "costUsd": null,
                "inputTokens": 100000,
                "outputTokens": 210000,
                "reasoningTokens": 55000,
                "requests": 31,
                "since": 1767218520000,
                "unpricedRequests": 31
              }
            }
          },
          {
            "cacheCreationTokens": 0,
            "cacheHitRatio": null,
            "cacheReadTokens": 0,
            "codexHome": "/Users/example/.codex",
            "codexRoute": {
              "pooled": true
            },
            "codexTraffic": {
              "lastRefusal": "usage_limit_reached",
              "lastRefusalAtMs": 1767225560000,
              "refused10m": 12,
              "unpooled10m": 3
            },
            "control": false,
            "controlAllowedGroups": [],
            "disabled": false,
            "fiveHour": 0.0,
            "fiveHourResetAtMs": null,
            "fiveHourState": "ok",
            "fiveHourWindowMinutes": 300,
            "freeAtMs": null,
            "gate": "login",
            "groupColors": {
              "orchard": "#30d158",
              "research": "#bf5af2"
            },
            "groups": [],
            "held": [],
            "http1Only": false,
            "inputTokens": 0,
            "lastProbeMs": 1767225540000,
            "lastStreamError": null,
            "loginError": "refresh_token_reused",
            "name": "erin@example.com/codex",
            "orgName": null,
            "orgUuid": "88888888-8888-4888-8888-888888888888",
            "organizationType": "team",
            "outputTokens": 0,
            "parkedGroups": [],
            "plan": "ChatGPT Team",
            "priority": 4,
            "probeError": "refresh_token_reused",
            "probeStatus": "error",
            "provider": "codex",
            "quota": 0.4,
            "quotaState": "ok",
            "quotaUpdatedAtMs": null,
            "quotaWindows": [
              "fiveHour",
              "sevenDay"
            ],
            "rateLimitTier": null,
            "rateLimitedUntilMs": null,
            "requests": 0,
            "reservedGroups": [],
            "seatTier": null,
            "secondsUntilFree": null,
            "serverDirty": false,
            "serverSha": "abc1234",
            "sevenDay": 0.4,
            "sevenDayOi": null,
            "sevenDayOiResetAtMs": null,
            "sevenDayOiState": null,
            "sevenDayResetAtMs": 1767398400000,
            "sevenDayState": "ok",
            "sevenDayWindowMinutes": 10080,
            "source": "live",
            "status": "error",
            "streamErrorCount": 0,
            "usage": {
              "lastHour": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 0,
                "costUsd": 0.0,
                "inputTokens": 0,
                "outputTokens": 0,
                "requests": 0,
                "unpricedRequests": 0
              },
              "sevenDayWindow": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 4000000,
                "costUsd": null,
                "inputTokens": 1000000,
                "outputTokens": 140000,
                "reasoningTokens": 50000,
                "requests": 120,
                "since": 1766793600000,
                "unpricedRequests": 120
              },
              "today": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 0,
                "costUsd": 0.0,
                "inputTokens": 0,
                "outputTokens": 0,
                "requests": 0,
                "unpricedRequests": 0
              },
              "todayByModel": {}
            }
          },
          {
            "cacheCreationTokens": 0,
            "cacheHitRatio": 0.71,
            "cacheReadTokens": 284000,
            "codexRoute": {
              "pooled": true
            },
            "codexTraffic": {
              "lastRefusal": "usage_limit_reached",
              "lastRefusalAtMs": 1767225560000,
              "refused10m": 12,
              "unpooled10m": 3
            },
            "control": false,
            "controlAllowedGroups": [],
            "disabled": false,
            "fiveHour": 0.15,
            "fiveHourResetAtMs": 1767236520000,
            "fiveHourState": "ok",
            "fiveHourWindowMinutes": 300,
            "freeAtMs": null,
            "gate": "rejected",
            "groupColors": {
              "orchard": "#30d158",
              "research": "#bf5af2"
            },
            "groups": [],
            "held": [],
            "http1Only": false,
            "inputTokens": 400000,
            "lastProbeMs": 1767225540000,
            "lastStreamError": null,
            "limitReachedType": "rate_limit_reached",
            "name": "gil@example.com",
            "orgName": null,
            "orgUuid": "99999999-9999-4999-8999-999999999999",
            "organizationType": "team",
            "outputTokens": 52000,
            "parkedGroups": [],
            "plan": "ChatGPT Team",
            "priority": 5,
            "probeError": null,
            "probeStatus": "ok",
            "provider": "codex",
            "quota": 0.15,
            "quotaState": "ok",
            "quotaUpdatedAtMs": null,
            "quotaWindows": [
              "fiveHour",
              "sevenDay"
            ],
            "rateLimitTier": null,
            "rateLimitedUntilMs": null,
            "requests": 24,
            "reservedGroups": [],
            "seatTier": null,
            "secondsUntilFree": null,
            "serverDirty": false,
            "serverSha": "abc1234",
            "sevenDay": 0.1,
            "sevenDayOi": null,
            "sevenDayOiResetAtMs": null,
            "sevenDayOiState": null,
            "sevenDayResetAtMs": 1767744000000,
            "sevenDayState": "ok",
            "sevenDayWindowMinutes": 10080,
            "source": "live",
            "spendControlReached": true,
            "status": "active",
            "streamErrorCount": 0,
            "usage": {
              "lastHour": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 21000,
                "costUsd": null,
                "inputTokens": 9000,
                "outputTokens": 3000,
                "reasoningTokens": 800,
                "requests": 2,
                "unpricedRequests": 2
              },
              "sevenDayWindow": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 2100000,
                "costUsd": null,
                "inputTokens": 900000,
                "outputTokens": 310000,
                "reasoningTokens": 90000,
                "requests": 140,
                "since": 1767139200000,
                "unpricedRequests": 140
              },
              "today": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 284000,
                "costUsd": null,
                "inputTokens": 116000,
                "outputTokens": 52000,
                "reasoningTokens": 14000,
                "requests": 24,
                "unpricedRequests": 24
              },
              "todayByModel": {
                "gpt-5.1-codex": {
                  "cacheCreation1hTokens": 0,
                  "cacheCreationTokens": 0,
                  "cacheReadTokens": 284000,
                  "costUsd": null,
                  "inputTokens": 116000,
                  "outputTokens": 52000,
                  "reasoningTokens": 14000,
                  "requests": 24,
                  "unpricedRequests": 24
                }
              },
              "window": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 234000,
                "costUsd": null,
                "inputTokens": 96000,
                "outputTokens": 48000,
                "reasoningTokens": 13000,
                "requests": 20,
                "since": 1767218520000,
                "unpricedRequests": 20
              }
            }
          },
          {
            "cacheCreationTokens": 0,
            "cacheHitRatio": 0.8,
            "cacheReadTokens": 1520000,
            "codexRoute": {
              "pooled": true
            },
            "codexTraffic": {
              "lastRefusal": "usage_limit_reached",
              "lastRefusalAtMs": 1767225560000,
              "refused10m": 12,
              "unpooled10m": 3
            },
            "control": false,
            "controlAllowedGroups": [],
            "disabled": false,
            "fiveHour": null,
            "fiveHourResetAtMs": null,
            "fiveHourState": null,
            "freeAtMs": 1767228420000,
            "gate": "hold",
            "groupColors": {
              "orchard": "#30d158",
              "research": "#bf5af2"
            },
            "groups": [],
            "held": [],
            "http1Only": false,
            "inputTokens": 1900000,
            "lastProbeMs": 1767225480000,
            "lastStreamError": null,
            "limitReachedType": "rate_limit_reached",
            "name": "hal@example.com",
            "orgName": null,
            "orgUuid": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "organizationType": "pro",
            "outputTokens": 40000,
            "parkedGroups": [],
            "plan": "ChatGPT Pro",
            "priority": 6,
            "probeError": null,
            "probeStatus": "ok",
            "provider": "codex",
            "quota": 0.6,
            "quotaState": "ok",
            "quotaUpdatedAtMs": null,
            "quotaWindows": [
              "sevenDay"
            ],
            "rateLimitTier": null,
            "rateLimitedUntilMs": 1767228420000,
            "requests": 77,
            "reservedGroups": [],
            "seatTier": null,
            "secondsUntilFree": 2820,
            "serverDirty": false,
            "serverSha": "abc1234",
            "sevenDay": 0.6,
            "sevenDayOi": null,
            "sevenDayOiResetAtMs": null,
            "sevenDayOiState": null,
            "sevenDayResetAtMs": 1767657600000,
            "sevenDayState": "ok",
            "sevenDayWindowMinutes": 10080,
            "source": "live",
            "status": "throttled",
            "streamErrorCount": 0,
            "usage": {
              "lastHour": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 0,
                "costUsd": 0.0,
                "inputTokens": 0,
                "outputTokens": 0,
                "requests": 0,
                "unpricedRequests": 0
              },
              "sevenDayWindow": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 32000000,
                "costUsd": null,
                "inputTokens": 8000000,
                "outputTokens": 1900000,
                "reasoningTokens": 700000,
                "requests": 900,
                "since": 1767052800000,
                "unpricedRequests": 900
              },
              "today": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 1520000,
                "costUsd": null,
                "inputTokens": 380000,
                "outputTokens": 40000,
                "reasoningTokens": 15000,
                "requests": 77,
                "unpricedRequests": 77
              },
              "todayByModel": {
                "gpt-5.1-codex": {
                  "cacheCreation1hTokens": 0,
                  "cacheCreationTokens": 0,
                  "cacheReadTokens": 1520000,
                  "costUsd": null,
                  "inputTokens": 380000,
                  "outputTokens": 40000,
                  "reasoningTokens": 15000,
                  "requests": 77,
                  "unpricedRequests": 77
                }
              }
            }
          },
          {
            "cacheCreationTokens": 0,
            "cacheHitRatio": 0.78,
            "cacheReadTokens": 1014000,
            "codexRoute": {
              "pooled": true
            },
            "codexTraffic": {
              "lastRefusal": "usage_limit_reached",
              "lastRefusalAtMs": 1767225560000,
              "refused10m": 12,
              "unpooled10m": 3
            },
            "control": false,
            "controlAllowedGroups": [],
            "disabled": false,
            "fiveHour": 0.88,
            "fiveHourResetAtMs": 1767228720000,
            "fiveHourState": "near",
            "fiveHourWindowMinutes": 300,
            "freeAtMs": 1767228720000,
            "gate": "five-hour",
            "groupColors": {
              "orchard": "#30d158",
              "research": "#bf5af2"
            },
            "groups": [
              "orchard"
            ],
            "held": [
              {
                "minutesUntilReset": 52,
                "resetAtMs": 1767228720000,
                "window": "5h"
              }
            ],
            "http1Only": false,
            "inputTokens": 1300000,
            "lastProbeMs": 1767225560000,
            "lastStreamError": null,
            "name": "ivy@example.com",
            "orgName": null,
            "orgUuid": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
            "organizationType": "plus",
            "outputTokens": 260000,
            "parkedGroups": [],
            "plan": "ChatGPT Plus",
            "priority": 7,
            "probeError": null,
            "probeStatus": "ok",
            "provider": "codex",
            "quota": 0.88,
            "quotaState": "near",
            "quotaUpdatedAtMs": null,
            "quotaWindows": [
              "fiveHour",
              "sevenDay"
            ],
            "rateLimitTier": null,
            "rateLimitedUntilMs": null,
            "requests": 60,
            "reservedGroups": [],
            "seatTier": null,
            "secondsUntilFree": 3120,
            "serverDirty": false,
            "serverSha": "abc1234",
            "sevenDay": 0.17,
            "sevenDayOi": null,
            "sevenDayOiResetAtMs": null,
            "sevenDayOiState": null,
            "sevenDayResetAtMs": 1767744000000,
            "sevenDayState": "ok",
            "sevenDayWindowMinutes": 10080,
            "source": "live",
            "status": "active",
            "streamErrorCount": 0,
            "usage": {
              "lastHour": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 330000,
                "costUsd": null,
                "inputTokens": 90000,
                "outputTokens": 80000,
                "reasoningTokens": 22000,
                "requests": 20,
                "unpricedRequests": 20
              },
              "sevenDayWindow": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 5400000,
                "costUsd": null,
                "inputTokens": 1600000,
                "outputTokens": 900000,
                "reasoningTokens": 250000,
                "requests": 300,
                "since": 1767139200000,
                "unpricedRequests": 300
              },
              "today": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 1014000,
                "costUsd": null,
                "inputTokens": 286000,
                "outputTokens": 260000,
                "reasoningTokens": 70000,
                "requests": 60,
                "unpricedRequests": 60
              },
              "todayByModel": {
                "gpt-5.1-codex": {
                  "cacheCreation1hTokens": 0,
                  "cacheCreationTokens": 0,
                  "cacheReadTokens": 1014000,
                  "costUsd": null,
                  "inputTokens": 286000,
                  "outputTokens": 260000,
                  "reasoningTokens": 70000,
                  "requests": 60,
                  "unpricedRequests": 60
                }
              },
              "window": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 860000,
                "costUsd": null,
                "inputTokens": 240000,
                "outputTokens": 210000,
                "reasoningTokens": 60000,
                "requests": 52,
                "since": 1767210720000,
                "unpricedRequests": 52
              }
            }
          },
          {
            "cacheCreationTokens": 0,
            "cacheHitRatio": 0.75,
            "cacheReadTokens": 225000,
            "codexRoute": {
              "pooled": true
            },
            "codexTraffic": {
              "lastRefusal": "usage_limit_reached",
              "lastRefusalAtMs": 1767225560000,
              "refused10m": 12,
              "unpooled10m": 3
            },
            "control": false,
            "controlAllowedGroups": [],
            "credits": {
              "hasCredits": true,
              "unlimited": true
            },
            "disabled": false,
            "fiveHour": null,
            "fiveHourResetAtMs": null,
            "fiveHourState": null,
            "freeAtMs": null,
            "gate": "ok",
            "groupColors": {
              "orchard": "#30d158",
              "research": "#bf5af2"
            },
            "groups": [],
            "held": [],
            "http1Only": false,
            "inputTokens": 300000,
            "lastProbeMs": 1767225550000,
            "lastStreamError": null,
            "name": "jo@example.com",
            "orgName": null,
            "orgUuid": "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
            "organizationType": "business",
            "outputTokens": 6000,
            "parkedGroups": [],
            "plan": "ChatGPT Business",
            "priority": 8,
            "probeError": null,
            "probeStatus": "ok",
            "provider": "codex",
            "quota": null,
            "quotaState": "ok",
            "quotaUpdatedAtMs": null,
            "quotaWindows": [],
            "rateLimitTier": null,
            "rateLimitedUntilMs": null,
            "requests": 30,
            "reservedGroups": [],
            "seatTier": null,
            "secondsUntilFree": null,
            "serverDirty": false,
            "serverSha": "abc1234",
            "sevenDay": null,
            "sevenDayOi": null,
            "sevenDayOiResetAtMs": null,
            "sevenDayOiState": null,
            "sevenDayResetAtMs": null,
            "sevenDayState": null,
            "source": "live",
            "status": "active",
            "streamErrorCount": 0,
            "usage": {
              "lastHour": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 0,
                "costUsd": 0.0,
                "inputTokens": 0,
                "outputTokens": 0,
                "requests": 0,
                "unpricedRequests": 0
              },
              "today": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 225000,
                "costUsd": null,
                "inputTokens": 75000,
                "outputTokens": 6000,
                "reasoningTokens": 1000,
                "requests": 30,
                "unpricedRequests": 30
              },
              "todayByModel": {
                "gpt-5.1-codex": {
                  "cacheCreation1hTokens": 0,
                  "cacheCreationTokens": 0,
                  "cacheReadTokens": 225000,
                  "costUsd": null,
                  "inputTokens": 75000,
                  "outputTokens": 6000,
                  "reasoningTokens": 1000,
                  "requests": 30,
                  "unpricedRequests": 30
                }
              }
            }
          },
          {
            "cacheCreationTokens": 0,
            "cacheHitRatio": null,
            "cacheReadTokens": 0,
            "codexRoute": {
              "pooled": true
            },
            "codexTraffic": {
              "lastRefusal": "usage_limit_reached",
              "lastRefusalAtMs": 1767225560000,
              "refused10m": 12,
              "unpooled10m": 3
            },
            "control": false,
            "controlAllowedGroups": [],
            "disabled": false,
            "fiveHour": null,
            "fiveHourResetAtMs": null,
            "fiveHourState": null,
            "freeAtMs": null,
            "gate": "reserved",
            "groupColors": {
              "orchard": "#30d158",
              "research": "#bf5af2"
            },
            "groups": [
              "research"
            ],
            "held": [],
            "http1Only": false,
            "inputTokens": 0,
            "lastProbeMs": 1767225420000,
            "lastStreamError": null,
            "name": "kim@example.com",
            "orgName": null,
            "orgUuid": "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
            "organizationType": "pro",
            "outputTokens": 0,
            "parkedGroups": [],
            "plan": "ChatGPT Pro",
            "priority": 9,
            "probeError": null,
            "probeStatus": "ok",
            "provider": "codex",
            "quota": 0.05,
            "quotaState": "ok",
            "quotaUpdatedAtMs": null,
            "quotaWindows": [
              "sevenDay"
            ],
            "rateLimitTier": null,
            "rateLimitedUntilMs": null,
            "requests": 0,
            "reservedGroups": [
              "research"
            ],
            "seatTier": null,
            "secondsUntilFree": null,
            "serverDirty": false,
            "serverSha": "abc1234",
            "sevenDay": 0.05,
            "sevenDayOi": null,
            "sevenDayOiResetAtMs": null,
            "sevenDayOiState": null,
            "sevenDayResetAtMs": 1767744000000,
            "sevenDayState": "ok",
            "sevenDayWindowMinutes": 10080,
            "source": "live",
            "status": "active",
            "streamErrorCount": 0,
            "usage": {
              "lastHour": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 0,
                "costUsd": 0.0,
                "inputTokens": 0,
                "outputTokens": 0,
                "requests": 0,
                "unpricedRequests": 0
              },
              "sevenDayWindow": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 700000,
                "costUsd": null,
                "inputTokens": 200000,
                "outputTokens": 30000,
                "reasoningTokens": 9000,
                "requests": 40,
                "since": 1767139200000,
                "unpricedRequests": 40
              },
              "today": {
                "cacheCreation1hTokens": 0,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 0,
                "costUsd": 0.0,
                "inputTokens": 0,
                "outputTokens": 0,
                "requests": 0,
                "unpricedRequests": 0
              },
              "todayByModel": {}
            }
          }
        ]
        """#
}
