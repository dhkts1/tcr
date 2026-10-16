import AppKit
import SwiftUI
import TcrBarCore

/// One badge — `applied live` / `restart to apply` / `read-only`
/// (``SettingsRowTiming``) — drawn beside a section header or a row that
/// disagrees with its section, matching the mockup's `.tag.live` / `.tag.boot`
/// (`docs/design/panel-tabs-mockup.html`).
private struct SettingsTag: View {
    let timing: SettingsRowTiming

    private var tint: Color {
        switch timing {
        case .live: return Tok.ok
        case .boot: return Tok.near
        case .readOnly: return Tok.inkFaint
        case .nextLaunch: return Tok.near
        }
    }

    var body: some View {
        Text(timing.label)
            .font(.caption2.weight(.semibold))
            .foregroundStyle(tint)
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .overlay(
                RoundedRectangle(cornerRadius: 6).strokeBorder(Tok.line(tint), lineWidth: 1)
            )
    }
}

/// A `Section` header carrying its badge — every section in this window uses
/// this rather than a bare `Text`, so a section can never be added without
/// also saying when its rows apply (S3, `docs/design/panel-tabs-review.md`).
private struct SectionHeader: View {
    let title: String
    let timing: SettingsRowTiming
    /// Stated ONCE for a section where every row shares it — "Edit in
    /// ~/.config/teamclaude.json" no longer repeats under each of a
    /// section's read-only values (review #30): four copies of one
    /// sentence out-inked the four numbers they annotated.
    var hint: String? = nil

    var body: some View {
        HStack(spacing: 8) {
            Text(title)
            SettingsTag(timing: timing)
            if let hint {
                Text(hint)
                    .font(.caption2)
                    .foregroundStyle(Tok.inkFaint)
            }
        }
    }
}

/// A row whose timing DIFFERS from its section's own badge — drawn beside the
/// row's label rather than repeating the section header's tag, the same
/// pattern the mockup's `.rowtag` uses for "Start the server at launch".
private struct RowTag: View {
    let timing: SettingsRowTiming
    var body: some View { SettingsTag(timing: timing) }
}

/// Draws a ``RowTag`` only when `timing` disagrees with its section's own
/// badge. A section already reading "applied live" saying nothing extra on a
/// live row beneath it is not a missing badge — the section header already
/// covers it — and repeating the same word on every row was noise a reader
/// had to see past to find the rows that actually differ.
@ViewBuilder
private func rowTag(_ timing: SettingsRowTiming, inSection section: SettingsRowTiming)
    -> some View
{
    if timing != section {
        RowTag(timing: timing)
    }
}

/// Every read-only row's hint (bridge, § Panes): "a key with no write path
/// today gets a read-only row and a hint", never a hand-rolled rewrite of the
/// live config, which holds real credentials (`CLAUDE.md`). Names the file this
/// Mac's `tcr` reads, `tcr.json` or an older install's `teamclaude.json`
/// (``ConfigFileName``), resolved once when the window first draws a hint.
private let readOnlyHint = "Edit in \(ConfigFileName.displayPath())"

/// `"1 account"` / `"2 accounts"` — this window's one plural count.
private func pluralizedAccounts(_ count: Int) -> String {
    "\(count) account\(count == 1 ? "" : "s")"
}

extension View {
    /// `.contentMargins(.top, _, for: .scrollContent)` is macOS 14+; this
    /// package's floor is macOS 13 (`Package.swift`). Same availability-guard
    /// shape `macos-settings-ui`'s own reference uses for
    /// `scrollEdgeEffectStyle`.
    @ViewBuilder
    fileprivate func formContentTopMarginIfAvailable(_ value: CGFloat) -> some View {
        if #available(macOS 14.0, *) {
            self.contentMargins(.top, value, for: .scrollContent)
        } else {
            self
        }
    }
}

// MARK: - General

struct GeneralSettingsPane: View {
    let dependencies: SettingsDependencies

    @ObservedObject private var server: ServerController
    @ObservedObject private var preference: LaunchPreference
    @ObservedObject private var loginItem: LoginItem
    @ObservedObject private var awake: AwakeController
    @ObservedObject private var poller: StatusPoller

    init(dependencies: SettingsDependencies) {
        self.dependencies = dependencies
        self.server = dependencies.server
        self.preference = dependencies.preference
        self.loginItem = dependencies.loginItem
        self.awake = dependencies.awake
        self.poller = dependencies.poller
    }

    /// The route the running proxy reports for Codex. It is the same on every Codex
    /// row, so the first that carries one is read; `nil` until the server says.
    private func codexRoute(in fleet: Fleet) -> CodexRoute? {
        fleet.accounts.lazy.filter { $0.provider == .codex }.compactMap(\.codexRoute).first
    }

    var body: some View {
        Form {
            Section {
                LabeledContent("Proxy") {
                    HStack(spacing: 8) {
                        Text(server.summary)
                            .foregroundStyle(Tok.inkDim)
                            .lineLimit(2)
                        Spacer(minLength: 8)
                        Button("Restart…") { confirmRestart() }
                            .controlSize(.small)
                    }
                }
                LabeledContent {
                    HStack(spacing: 6) {
                        Toggle(isOn: $preference.startServerAtLaunch) {
                            Text(SettingsRowBadge.startServerAtLaunchLabel)
                        }
                        .labelsHidden()
                        rowTag(
                            SettingsRowBadge.timing(for: SettingsRowBadge.startServerAtLaunch)
                                ?? .nextLaunch,
                            inSection: SettingsRowBadge.timing(for: SettingsRowBadge.proxyRestart)
                                ?? .live)
                    }
                } label: {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(SettingsRowBadge.startServerAtLaunchLabel)
                        Text("TcrBar supervises a server it starts itself.")
                            .font(.caption).foregroundStyle(Tok.inkDim)
                    }
                }
                LabeledContent("Read the server every") {
                    // Both paces: this window is open, so the one it would
                    // otherwise show is the fast one, which is not what the
                    // menu bar runs at the rest of the time.
                    Text("\(Int(poller.interval)) s open · \(Int(poller.closedInterval)) s closed")
                        .foregroundStyle(Tok.inkDim)
                        .monospacedDigit()
                }
                // Only on a fleet holding a Codex account: whether the running
                // proxy pools codex, as it reported. Read-only, because nothing
                // in this window changes it.
                if let fleet = poller.state.loadedFleet, fleet.holdsCodex {
                    LabeledContent {
                        HStack(spacing: 6) {
                            Text(CodexSignInCopy.settingsCodexValue(route: codexRoute(in: fleet)))
                                .foregroundStyle(Tok.inkDim)
                                .lineLimit(2)
                            rowTag(
                                SettingsRowBadge.timing(for: SettingsRowBadge.serverCodex) ?? .readOnly,
                                inSection: SettingsRowBadge.timing(for: SettingsRowBadge.proxyRestart)
                                    ?? .live)
                        }
                    } label: {
                        Text(CodexSignInCopy.settingsCodexLabel)
                    }
                }
            } header: {
                SectionHeader(
                    title: "Server",
                    timing: SettingsRowBadge.timing(for: SettingsRowBadge.proxyRestart) ?? .live)
            }

            Section {
                Toggle(
                    "Launch TcrBar at login",
                    isOn: Binding(
                        get: { loginItem.status.isOn },
                        set: { loginItem.set(enabled: $0) })
                )
                .toggleStyle(.switch)
                Toggle(
                    isOn: Binding(get: { awake.isOn }, set: { awake.setOn($0) })
                ) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Keep this Mac awake")
                        Text("While any session is busy.")
                            .font(.caption).foregroundStyle(Tok.inkDim)
                    }
                }
                .toggleStyle(.switch)
            } header: {
                SectionHeader(
                    title: "This Mac",
                    timing: SettingsRowBadge.timing(for: SettingsRowBadge.launchAtLogin) ?? .live)
            }

            Section {
                ClaudeRouteSettingsRow(controller: dependencies.claudeRoute)
            } header: {
                SectionHeader(title: "The claude command", timing: .live)
            }

            Section {
                HStack {
                    Text(
                        "Quitting stops the proxy this app supervises, so every live "
                            + "session loses its prompt cache."
                    )
                    .font(.caption).foregroundStyle(Tok.inkDim)
                    Spacer()
                    Button("Quit TcrBar…") { QuitConfirmation.confirm() }
                        .controlSize(.small)
                }
            }
        }
        .formStyle(.grouped)
        .scrollContentBackground(.hidden)
        .formContentTopMarginIfAvailable(8)
        .onAppear {
            loginItem.refresh()
            Task { await dependencies.claudeRoute.refresh() }
        }
    }

    private func confirmRestart() {
        confirmProxyRestart(
            server: server, holdsCodex: poller.state.loadedFleet?.holdsCodex == true)
    }

}

/// Restart the proxy TcrBar supervises, after a confirmation that names the
/// cost: General's Restart… and Groups & Rotation's "Restart proxy" both come
/// here, so the two cannot drift on the words or on Cancel being the default.
///
/// Same cost statement `CLAUDE.md` gives, and the same confirm-before-act shape
/// `FleetView.confirmTakeover()` already uses for its own most-expensive action.
/// The words come from ``RestartAlertCopy``, read against the fleet of the last
/// poll: once it holds a Codex account, the alert names Codex sessions as well as
/// Claude Code ones.
@MainActor
private func confirmProxyRestart(server: ServerController, holdsCodex: Bool) {
    let alert = NSAlert()
    alert.alertStyle = .warning
    alert.messageText = "Restart the proxy?"
    alert.informativeText = RestartAlertCopy.restartInformative(holdsCodex: holdsCodex)
    let restart = alert.addButton(withTitle: "Restart")
    restart.hasDestructiveAction = true
    alert.addButton(withTitle: "Cancel")
    restart.keyEquivalent = ""
    alert.buttons.last?.keyEquivalent = "\r"
    guard alert.runModal() == .alertFirstButtonReturn else { return }
    server.stop()
    server.start()
}

// MARK: - Menu Bar

struct MenuBarSettingsPane: View {
    let dependencies: SettingsDependencies

    @ObservedObject private var countsPreference: MenuBarCountsPreference
    @ObservedObject private var runningToolsPreference: ShowRunningToolsPreference
    @ObservedObject private var defaultTabPreference: DefaultTabPreference
    @ObservedObject private var panelDensityPreference: PanelDensityPreference
    @ObservedObject private var poller: StatusPoller
    @ObservedObject private var cupPoolPreference: MenuBarCupPoolPreference

    init(dependencies: SettingsDependencies) {
        self.dependencies = dependencies
        self.countsPreference = dependencies.countsPreference
        self.runningToolsPreference = dependencies.runningToolsPreference
        self.defaultTabPreference = dependencies.defaultTabPreference
        self.panelDensityPreference = dependencies.panelDensityPreference
        self.poller = dependencies.poller
        self.cupPoolPreference = dependencies.cupPoolPreference
    }

    /// A fleet holding both Claude and Codex accounts: the count beside the cup
    /// has one part per pool, and the cup can be told which pool to follow.
    /// Every other fleet keeps this section exactly as it was.
    private var holdsBothProviders: Bool {
        poller.state.loadedFleet?.holdsBothProviders == true
    }

    var body: some View {
        Form {
            Section {
                Toggle(
                    isOn: $countsPreference.showCounts
                ) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Show the ready count beside the cup")
                        Text(
                            holdsBothProviders
                                ? MenuBarCupPool.mixedCountsCaption
                                : "The cup's fill level already shows capacity; this spells it out "
                                    + "as \u{201c}9/13\u{201d}, in tabular digits."
                        )
                        .font(.caption).foregroundStyle(Tok.inkDim)
                    }
                }
                .toggleStyle(.switch)
                if holdsBothProviders {
                    Picker(selection: $cupPoolPreference.pool) {
                        ForEach(MenuBarCupPool.allCases, id: \.self) { pool in
                            Text(pool.title).tag(pool)
                        }
                    } label: {
                        VStack(alignment: .leading, spacing: 2) {
                            Text(MenuBarCupPool.settingsLabel)
                            Text(MenuBarCupPool.settingsCaption)
                                .font(.caption).foregroundStyle(Tok.inkDim)
                        }
                    }
                    .pickerStyle(.menu)
                }
                Toggle(
                    isOn: $runningToolsPreference.showRunningToolCount
                ) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Show the running-tool count")
                        Text("Adds a second number beside the glyph.")
                            .font(.caption).foregroundStyle(Tok.inkDim)
                    }
                }
                .toggleStyle(.switch)
            } header: {
                SectionHeader(
                    title: "In the menu bar",
                    timing: SettingsRowBadge.timing(for: SettingsRowBadge.showReadyCount) ?? .live)
            }

            Section {
                // Every tab the panel has, from `PanelTab.allCases` rather
                // than four hand-written tags. The hand-written list offered
                // three of the four, `peers` was missing, so the one way to
                // store it was a hand edit of `UserDefaults`, and a list
                // that has to be kept in step with an enum is the same
                // drift `PanelTab`'s own doc-comment now describes. The tag
                // is the raw value, which is exactly what
                // `DefaultTabPreference` stores and `MenuBarShell`
                // reads back.
                Picker("Open on", selection: $defaultTabPreference.tab) {
                    ForEach(PanelTab.allCases, id: \.self) { tab in
                        Text(tab.title).tag(tab.rawValue)
                    }
                }
                .pickerStyle(.menu)
                Picker("Panel density", selection: $panelDensityPreference.density) {
                    Text(
                        "Automatic (compact above "
                            + "\(PanelDensityPreference.comfortableCeiling) accounts)"
                    ).tag(PanelDensity.auto)
                    Text("Compact").tag(PanelDensity.compact)
                    Text("Comfortable").tag(PanelDensity.comfortable)
                }
                .pickerStyle(.menu)
                LabeledContent("Text size") {
                    HStack(spacing: 6) {
                        Text("System").foregroundStyle(Tok.inkDim)
                        rowTag(
                            SettingsRowBadge.timing(for: SettingsRowBadge.textSize) ?? .readOnly,
                            inSection: SettingsRowBadge.timing(for: SettingsRowBadge.openOnTab)
                                ?? .live)
                    }
                }
            } header: {
                SectionHeader(
                    title: "When the panel opens",
                    timing: SettingsRowBadge.timing(for: SettingsRowBadge.openOnTab) ?? .live)
            }
        }
        .formStyle(.grouped)
        .scrollContentBackground(.hidden)
        .formContentTopMarginIfAvailable(8)
    }
}

// MARK: - Groups & Rotation

struct GroupsRotationSettingsPane: View {
    let dependencies: SettingsDependencies

    @ObservedObject private var poller: StatusPoller
    @ObservedObject private var groupController: GroupController
    @ObservedObject private var configSettings: ConfigSettingsController

    init(dependencies: SettingsDependencies) {
        self.dependencies = dependencies
        self.poller = dependencies.poller
        self.groupController = dependencies.groupController
        self.configSettings = dependencies.configSettings
    }

    /// "Claude accounts only" under the four rotation rows that only Claude
    /// accounts follow, on a fleet holding a Codex account; nothing on any other.
    private var claudeOnlyScope: String? {
        poller.state.loadedFleet?.holdsCodex == true ? CodexSignInCopy.rotationScope : nil
    }

    /// Derived from the FULL fleet, not a filtered panel view — S8's own
    /// fix (`docs/design/panel-tabs-review.md`): every group the fleet has
    /// is a row here, not only the ones with an expanded card on the panel.
    private var groups: [GroupSummary] {
        guard case .loaded(let fleet) = poller.state else { return [] }
        return GroupSummary.summarize(fleet.accounts)
    }

    private var rotationSectionTiming: SettingsRowTiming {
        SettingsRowBadge.timing(for: SettingsRowBadge.switchThreshold) ?? .boot
    }
    private var limitsSectionTiming: SettingsRowTiming {
        SettingsRowBadge.timing(for: SettingsRowBadge.accountThrottle) ?? .boot
    }

    var body: some View {
        Form {
            Section {
                if groups.isEmpty {
                    Text("No groups yet.").foregroundStyle(Tok.inkDim)
                } else {
                    ForEach(groups) { group in
                        groupRow(group)
                    }
                }
            } header: {
                SectionHeader(
                    title: "Groups",
                    timing: SettingsRowBadge.timing(for: SettingsRowBadge.groupParked) ?? .live)
            }

            Section {
                editableRow(
                    "Switch threshold",
                    "Prefer another account once this share of quota is used.",
                    key: SettingsRowBadge.switchThreshold
                ) { s in
                    SettingNumberField(
                        label: "Switch threshold",
                        saved: ConfigSettingsFormat.percentField(s.switchThreshold),
                        step: 1, bounds: 1...100, unit: "%"
                    ) { await save(SettingsRowBadge.switchThreshold, ["\($0)%"]) }
                }
                editableRow(
                    "Control reserve",
                    "The control account is picked below threshold minus reserve.",
                    key: SettingsRowBadge.controlReserve, scope: claudeOnlyScope
                ) { s in
                    SettingNumberField(
                        label: "Control reserve", saved: ConfigSettingsFormat.percentField(s.controlReserve),
                        step: 1, bounds: 0...50, unit: "%"
                    ) { await save(SettingsRowBadge.controlReserve, ["\($0)%"]) }
                }
                editableRow(
                    "Fable weekly threshold",
                    "The separate ceiling for the weekly window. Empty follows the switch threshold.",
                    key: SettingsRowBadge.fableWeeklyThreshold, lines: 3, scope: claudeOnlyScope
                ) { s in
                    SettingNumberField(
                        label: "Fable weekly threshold",
                        // Following: empty, with the switch threshold as its dimmed
                        // placeholder, which is what "Empty follows" says.
                        saved: s.fableWeeklyThresholdFollows
                            ? "" : ConfigSettingsFormat.percentField(s.fableWeeklyThreshold),
                        placeholder: ConfigSettingsFormat.percentField(s.switchThreshold),
                        step: 1, bounds: 1...100, unit: "%"
                    ) { value in
                        await save(
                            SettingsRowBadge.fableWeeklyThreshold, [value.isEmpty ? "default" : "\(value)%"])
                    }
                }
                editableRow(
                    "Reset urgency tier",
                    "Weekly resets this close together count as equally soon.",
                    key: SettingsRowBadge.resetUrgencyTier
                ) { s in
                    menu(
                        "Reset urgency tier", selection: s.resetUrgencyTierHours,
                        choices: ConfigSettingsFormat.urgencyTierChoices(including: s.resetUrgencyTierHours),
                        title: { $0 == 0 ? "Off" : "\($0) hours" }
                    ) { await save(SettingsRowBadge.resetUrgencyTier, [String($0)]) }
                }
                editableRow(
                    "Session affinity",
                    "Pin a session to one account so its prompt cache survives.",
                    key: SettingsRowBadge.sessionAffinity
                ) { s in
                    toggle("Session affinity", isOn: s.sessionAffinity, key: SettingsRowBadge.sessionAffinity)
                }
                editableRow(
                    "Control account", "Claude accounts only.",
                    key: SettingsRowBadge.controlAccount, lines: 1
                ) { s in
                    controlAccountMenu(saved: s.controlAccount)
                }
                editableRow(
                    "Pooled control", "Let the control account serve ordinary traffic too.",
                    key: SettingsRowBadge.controlPooled, scope: claudeOnlyScope
                ) { s in
                    toggle("Pooled control", isOn: s.controlPooled, key: SettingsRowBadge.controlPooled)
                }
            } header: {
                SectionHeader(title: "Rotation", timing: rotationSectionTiming)
            }

            Section {
                editableRow(
                    "Per-account throttle", "Requests sent to one organization.",
                    key: SettingsRowBadge.accountThrottle,
                    controlHeight: SettingsControlMetrics.pairHeight
                ) { s in
                    SettingThrottlePair(label: "Per-account throttle", saved: s.accountThrottle) {
                        await save(SettingsRowBadge.accountThrottle, $0)
                    }
                }
                editableRow(
                    "Fleet throttle", "Requests sent across all accounts.",
                    key: SettingsRowBadge.fleetThrottle,
                    controlHeight: SettingsControlMetrics.pairHeight
                ) { s in
                    SettingThrottlePair(label: "Fleet throttle", saved: s.fleetThrottle) {
                        await save(SettingsRowBadge.fleetThrottle, $0)
                    }
                }
                editableRow(
                    "Pacing", "Skip an account that already has this many requests in flight.",
                    key: SettingsRowBadge.pacing
                ) { s in
                    pacingControl(saved: s.pacing)
                }
                editableRow(
                    "Keep the usage ledger for", nil,
                    key: SettingsRowBadge.usageRetentionDays
                ) { s in
                    menu(
                        "Keep the usage ledger for", selection: s.usageRetentionDays,
                        choices: ConfigSettingsFormat.retentionChoices(including: s.usageRetentionDays),
                        title: { ConfigSettingsFormat.days($0) }
                    ) { await save(SettingsRowBadge.usageRetentionDays, [String($0)]) }
                }
                editableRow(
                    "HTTP/1.1 only upstream", "Off means HTTP/2, the faster default.",
                    key: SettingsRowBadge.http1Only
                ) { s in
                    toggle("HTTP/1.1 only upstream", isOn: s.http1Only, key: SettingsRowBadge.http1Only)
                }
            } header: {
                SectionHeader(title: "Limits", timing: limitsSectionTiming)
            }

            Section {
                waitingLine
            }
        }
        .formStyle(.grouped)
        .scrollContentBackground(.hidden)
        .formContentTopMarginIfAvailable(8)
        // Each time the pane appears, the same shape as General's claude-route
        // read: `tcr config show --json` runs off the main thread.
        .onAppear {
            Task { await configSettings.refresh() }
        }
    }

    /// `Groups` section's own badge — every row below is checked against
    /// this, so a row whose timing matches draws no repeat tag.
    private var groupsSectionTiming: SettingsRowTiming {
        SettingsRowBadge.timing(for: SettingsRowBadge.groupParked) ?? .live
    }

    @ViewBuilder
    private func groupRow(_ group: GroupSummary) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text(group.name).font(.headline)
                Spacer()
                Text(pluralizedAccounts(group.memberCount))
                    .foregroundStyle(Tok.inkDim)
                    .monospacedDigit()
            }
            // No tag: `.live`, same as the section's own badge — a toggle
            // whose change is real and immediate needs no repeated word.
            Toggle(
                "Parked",
                isOn: Binding(
                    get: { group.isParked },
                    set: { newValue in
                        Task { await groupController.setParked(group: group.name, parked: newValue) }
                    })
            )
            .toggleStyle(.switch)
            .help("Held out of rotation; quota keeps accruing.")

            // A PLAIN VALUE row, not a disabled toggle: `GroupController` has
            // no write path for "reserved" at all, so a switch a reader could
            // click — and would find does nothing — is a fake affordance a
            // read-only value row is not.
            HStack(spacing: 6) {
                Text("Reserved")
                Spacer()
                Text(group.isReserved ? "Yes" : "No").foregroundStyle(Tok.inkDim)
                rowTag(.readOnly, inSection: groupsSectionTiming)
            }
            .help(
                group.codexMemberCount > 0
                    ? "Only sessions tagged with this group route here. "
                        + "\(CodexSignInCopy.groupReservedCodexHelp) \(readOnlyHint)"
                    : "Only sessions tagged with this group route here. \(readOnlyHint)")

            HStack(spacing: 6) {
                // Each provider counted in a group with a Codex member; the header
                // above keeps the total.
                Text(
                    group.codexMemberCount > 0
                        ? CodexSignInCopy.groupMembers(
                            claude: group.claudeMemberCount, codex: group.codexMemberCount)
                        : "Members: \(group.memberCount)"
                ).monospacedDigit()
                Spacer()
                if let hex = group.colorHex {
                    Circle().fill(Color(hex: hex) ?? Tok.inkFaint).frame(width: 14, height: 14)
                } else {
                    Circle().strokeBorder(Tok.hairlineStrong).frame(width: 14, height: 14)
                }
                rowTag(.readOnly, inSection: groupsSectionTiming)
            }
            .font(.caption)
            .foregroundStyle(Tok.inkDim)
        }
        .padding(.vertical, 4)
    }

    /// The saved settings, or `nil` while the first read runs or when it
    /// failed. Every control is drawn from this; this app never reads the
    /// config file itself (`CLAUDE.md`: it holds credentials).
    private var saved: ConfigSettings? {
        guard case .settings(let settings) = configSettings.reading else { return nil }
        return settings
    }

    /// One editable row (`docs/design/settings-editable-mockup.html`): the
    /// label on the left, the control in a fixed column on the right. With no
    /// reading the column shows `—` with the reason as its tooltip, in the
    /// same fixed frame, so the first read landing moves nothing.
    /// - Parameter scope: one more caption under the detail, naming which
    ///   accounts the row applies to, or `nil` for none.
    @ViewBuilder
    private func editableRow<Control: View>(
        _ title: String, _ detail: String?, key: String, lines: Int = 2,
        scope: String? = nil, controlHeight: CGFloat = SettingsControlMetrics.controlHeight,
        @ViewBuilder control: (ConfigSettings) -> Control
    ) -> some View {
        LabeledContent {
            Group {
                if let saved {
                    control(saved)
                } else {
                    let shown = ConfigSettingsFormat.row(
                        for: key, reading: configSettings.reading, hint: "")
                    Text(shown.value).foregroundStyle(Tok.inkDim).help(shown.help)
                }
            }
            .frame(
                width: SettingsControlMetrics.columnWidth, height: controlHeight,
                alignment: .trailing)
        } label: {
            SettingRowLabel(
                title: title, detail: detail, refusal: configSettings.refusals[key],
                waiting: saved?.waitingKeys.contains(key) == true, lines: lines, scope: scope)
        }
    }

    /// Save `values` for the row `key` through `tcr config set`.
    private func save(_ key: String, _ values: [String]) async -> Bool {
        await configSettings.save(rowKey: key, values: values)
    }

    /// An on/off row, saved the moment it flips.
    private func toggle(_ label: String, isOn: Bool, key: String) -> some View {
        Toggle(
            label,
            isOn: Binding(
                get: { isOn },
                set: { on in Task { await save(key, [on ? "on" : "off"]) } })
        )
        .labelsHidden()
        .toggleStyle(.switch)
    }

    /// A menu row, saved the moment a choice is picked.
    private func menu(
        _ label: String, selection: Int, choices: [Int], title: @escaping (Int) -> String,
        commit: @escaping (Int) async -> Bool
    ) -> some View {
        Picker(
            label,
            selection: Binding(
                get: { selection },
                set: { picked in Task { await commit(picked) } })
        ) {
            ForEach(choices, id: \.self) { Text(title($0)).tag($0) }
        }
        .labelsHidden()
        .pickerStyle(.menu)
        .frame(width: SettingsControlMetrics.menuWidth)
    }

    /// The Claude accounts of the last poll, plus the saved control account
    /// when the poll does not list it, so the menu always shows what is saved.
    private func controlAccountChoices(saved: String?) -> [String] {
        let names = (poller.state.loadedFleet?.accounts ?? [])
            .filter { $0.provider == .anthropic }
            .map(\.name)
        guard let saved, !names.contains(saved) else { return names }
        return names + [saved]
    }

    /// The control account: a menu of Claude accounts and None, saved through
    /// `tcr control`, which applies it to the running proxy at once.
    private func controlAccountMenu(saved: String?) -> some View {
        Picker(
            "Control account",
            selection: Binding<String?>(
                get: { saved },
                set: { picked in Task { await configSettings.saveControlAccount(picked) } })
        ) {
            Text("None").tag(String?.none)
            ForEach(controlAccountChoices(saved: saved), id: \.self) { name in
                Text(name).tag(String?.some(name))
            }
        }
        .labelsHidden()
        .pickerStyle(.menu)
        .frame(width: SettingsControlMetrics.menuWidth)
    }

    /// Pacing: the in-flight cap with its stepper, and the switch. Switching
    /// on saves the cap in the field, or 4 when it is empty; the field saves
    /// on its own while pacing is on.
    private func pacingControl(saved pacing: ConfigPacing?) -> some View {
        let cap = pacing?.maxInFlightPerAccount.map(String.init) ?? ""
        return HStack(spacing: 8) {
            SettingNumberField(
                label: "Pacing, requests in flight per account", saved: cap, placeholder: "4",
                step: 1, bounds: 1...999
            ) { await save(SettingsRowBadge.pacing, [$0.isEmpty ? "off" : $0]) }
            .disabled(pacing == nil)
            Toggle(
                "Pacing",
                isOn: Binding(
                    get: { pacing != nil },
                    set: { on in
                        Task { await save(SettingsRowBadge.pacing, [on ? (cap.isEmpty ? "4" : cap) : "off"]) }
                    })
            )
            .labelsHidden()
            .toggleStyle(.switch)
        }
    }

    /// Three lines of `.callout` with room to spare: the longest sentence the
    /// line says wraps to three at the shipped window width.
    private static let waitingLineHeight: CGFloat = 50

    /// The keys waiting for a restart that the operator answered "Later" to.
    /// The buttons come back when the set changes.
    @State private var deferredWaiting: [String]?

    /// The fixed line under Limits: how many saved changes wait for a restart,
    /// with "Later" and "Restart proxy". Always present and one height, so a
    /// change appearing or clearing moves nothing; with nothing waiting the
    /// buttons keep their place, hidden.
    private var waitingLine: some View {
        let waiting = saved?.waitingKeys ?? []
        let showButtons = !waiting.isEmpty && deferredWaiting != waiting
        let text =
            saved.map {
                ConfigSettingsFormat.waitingLine(waiting: waiting.count, proxyAnswered: $0.running != nil)
            } ?? ConfigSettingsFormat.missing
        return HStack(spacing: 10) {
            // A fixed frame, not `lineLimit(3, reservesSpace: true)` alone: measured
            // in `--render-settings`, three wrapped lines came out half a point
            // taller than the three reserved, so the pane grew by 0.5 pt the moment
            // a change started waiting.
            Text(text)
                .font(.callout)
                .foregroundStyle(waiting.isEmpty ? Tok.inkDim : Tok.near)
                .lineLimit(3)
                .frame(
                    maxWidth: .infinity, minHeight: Self.waitingLineHeight,
                    maxHeight: Self.waitingLineHeight, alignment: .leading)
            Button("Later") { deferredWaiting = waiting }
                .opacity(showButtons ? 1 : 0)
                .disabled(!showButtons)
            Button("Restart proxy") {
                confirmProxyRestart(
                    server: dependencies.server,
                    holdsCodex: poller.state.loadedFleet?.holdsCodex == true)
            }
            .buttonStyle(.borderedProminent)
            .opacity(showButtons ? 1 : 0)
            .disabled(!showButtons)
        }
        .controlSize(.small)
    }
}

// MARK: - Updates

struct UpdatesSettingsPane: View {
    let dependencies: SettingsDependencies
    @ObservedObject private var updater: Updater
    @ObservedObject private var server: ServerController
    @ObservedObject private var poller: StatusPoller
    /// The command-line tcr a shell would run, read off the main thread when the
    /// fleet holds a Codex account, and `nil` otherwise.
    @State private var pathTcr: TcrTool.PathTcr?

    init(dependencies: SettingsDependencies) {
        self.dependencies = dependencies
        self.updater = dependencies.updater
        self.server = dependencies.server
        self.poller = dependencies.poller
    }

    /// Only a loaded fleet holding a Codex account asks about the command-line tcr:
    /// one that predates Codex accounts cannot read such a config. A poll that first
    /// loads a Codex row asks again.
    private var holdsCodex: Bool {
        poller.state.loadedFleet?.holdsCodex == true
    }

    private var versionLine: String {
        AppBuild.label ?? "TcrBar (development build)"
    }

    /// The resolved `tcr` path this app would shell out to, per the same
    /// search `TcrTool.resolve()` uses for every poll and command — never a
    /// guess, and distinct from `TcrTool.overrideRemedy` (that string is the
    /// FIX for when nothing was found, not a path to display when something
    /// was).
    private var installedCliPath: String {
        switch TcrTool.resolve() {
        case .success(let url): return url.path
        case .failure: return "Not found. " + TcrTool.overrideRemedy
        }
    }

    private var tcrBarSectionTiming: SettingsRowTiming {
        SettingsRowBadge.timing(for: SettingsRowBadge.checkNow) ?? .live
    }

    var body: some View {
        Form {
            Section {
                LabeledContent(versionLine) {
                    Button("Check now") { updater.checkForUpdates() }
                        .disabled(!updater.canCheckForUpdates)
                        .controlSize(.small)
                }
                LabeledContent("What's new in this build") {
                    Button("Read…") { dependencies.onWhatsNew() }
                        .controlSize(.small)
                }
                readOnlyRow(
                    "Check automatically", "Controlled by Sparkle's own default.",
                    key: SettingsRowBadge.checkAutomatically, sectionTiming: tcrBarSectionTiming)
            } header: {
                SectionHeader(title: "TcrBar", timing: tcrBarSectionTiming)
            }

            Section {
                LabeledContent("Server on 127.0.0.1:3456") {
                    Text(server.summary).foregroundStyle(Tok.inkDim).lineLimit(2)
                }
                LabeledContent("Command-line tcr") {
                    Text(installedCliPath).font(.caption).foregroundStyle(Tok.inkDim)
                        .lineLimit(2)
                }
                if holdsCodex, let pathTcr, pathTcr.predatesCodex {
                    Text(CodexSignInCopy.pathTcrFlag(path: pathTcr.path, version: pathTcr.version))
                        .font(.caption).foregroundStyle(Tok.spent)
                }
                Text(
                    "Updating replaces both. TcrBar supervises the server, so an update "
                        + "restarts it and spends the cold prefix. You cannot replace the "
                        + "app bundle while the proxy is running."
                )
                .font(.caption).foregroundStyle(Tok.inkDim)
            } header: {
                SectionHeader(
                    title: "The running build",
                    timing: SettingsRowBadge.timing(for: SettingsRowBadge.runningServer) ?? .readOnly)
            }
        }
        .formStyle(.grouped)
        .scrollContentBackground(.hidden)
        .formContentTopMarginIfAvailable(8)
        .task(id: holdsCodex) {
            guard holdsCodex else {
                pathTcr = nil
                return
            }
            let read = dependencies.pathTcr
            pathTcr = await Task.detached(priority: .utility) { read() }.value
        }
    }

    @ViewBuilder
    private func readOnlyRow(
        _ title: String, _ detail: String, key: String, sectionTiming: SettingsRowTiming
    ) -> some View {
        LabeledContent {
            rowTag(SettingsRowBadge.timing(for: key) ?? .readOnly, inSection: sectionTiming)
        } label: {
            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                Text(detail).font(.caption).foregroundStyle(Tok.inkDim)
            }
        }
    }
}

extension Color {
    /// `#rrggbb` → `Color`, tolerant of a leading `#`. `nil` on anything else
    /// — a malformed wire colour draws the neutral fallback the caller
    /// already handles, never a guess.
    init?(hex: String) {
        var s = Substring(hex)
        if s.hasPrefix("#") { s = s.dropFirst() }
        guard s.count == 6, let v = UInt32(s, radix: 16) else { return nil }
        self.init(
            red: Double((v >> 16) & 0xff) / 255,
            green: Double((v >> 8) & 0xff) / 255,
            blue: Double(v & 0xff) / 255)
    }
}
