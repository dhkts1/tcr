import SwiftUI
import TcrBarCore

/// The panel's own sign-in sheet: what `tcr login --non-interactive` is doing,
/// while it does it.
///
/// Takes a ``LoginPhase`` rather than the ``LoginSession`` that produces one.
/// That is what lets `--render-states` draw all four states without spawning a
/// login — the harness renders this view directly with a pinned phase, which is
/// the only way a state like `.failed` gets looked at before it happens to a
/// person.
///
/// Not in `PanelV4/`: that directory is a class-by-class transcription of
/// `docs/design/panel-tabs-mockup.html`, and the mockup has no sheet. This
/// reuses the sheet's components (``V4Button``, ``V4.font``, the `Tok` palette)
/// without claiming to be transcribed from a design that does not cover it.
struct LoginSheet: View {
    let phase: LoginPhase
    /// What this Mac shows of codex, decided by the caller once per panel open.
    /// Nothing reads it yet. It is a parameter for the same reason
    /// ``snapshotMode`` is: the render harness passes `.absent`.
    let codexPresence: CodexPresence
    /// The authorize URL, for the one recovery this flow needs: the browser did
    /// not come forward, or opened as the wrong user.
    var authorizeURL: URL?
    var onCopyLink: () -> Void = {}
    var onCancel: () -> Void = {}
    var onDone: () -> Void = {}
    /// Draw a still glyph where the spinner goes. `--render-states` only:
    /// `ImageRenderer` rasterises a `ProgressView` as the macOS "prohibited"
    /// placeholder, so a fixture of the waiting states showed a red
    /// crossed-out circle where a person sees motion — a picture of a state
    /// this app never draws. Same switch the panel already threads for the
    /// same reason (``FleetView/snapshotMode``).
    var snapshotMode: Bool = false
    /// Everything beyond the phase: which sign-in this is, tcr's notices, the plan a
    /// save named, the code a refusal named. Empty by default, which draws the Claude
    /// sheet exactly as it always was.
    var facts: LoginSheetFacts = LoginSheetFacts()
    /// The home a codex folder is shortened against (`~/.codex`). The render harness
    /// names `/Users/example`, never the rendering Mac's home.
    var displayHome: String = HomePath.current
    /// The chooser's two accounts: a Claude or a Codex sign-in.
    var onChoose: (LoginRequest) -> Void = { _ in }
    /// The chooser's import link, which asks for the codex folder to share.
    var onImportLink: () -> Void = {}
    /// The import's confirmation, accepted.
    var onConfirmImport: () -> Void = {}
    /// `Choose codex…`, when tcr could not find codex.
    var onChooseCodex: () -> Void = {}

    var body: some View {
        VStack(alignment: .leading, spacing: V4.buttonGap) {
            HStack(spacing: V4.rowGap) {
                glyph
                Text(title)
                    .font(V4.font(V4.summarySize, .semibold))
                    .foregroundStyle(Tok.ink)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if let detail {
                Text(detail)
                    .font(V4.font(V4.dimSize))
                    .foregroundStyle(Tok.inkDim)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            if case .choosing = phase {
                Button(CodexSignInCopy.importLink) { onImportLink() }
                    .buttonStyle(.plain)
                    .font(V4.font(V4.dimSize))
                    .foregroundStyle(Tok.accent)
                    .help(CodexSignInCopy.importLinkHelp)
            }
            // Why the codex a person picked cannot be used, under tcr's reason, so
            // the refusal and the way out read together.
            if case .failed = phase, let refused = facts.refusedCodexChoice {
                Text(refused)
                    .font(V4.font(V4.dimSize))
                    .foregroundStyle(Tok.near)
                    .fixedSize(horizontal: false, vertical: true)
            }
            // Every notice tcr sent, in order and verbatim: none is dropped or cut,
            // so the sheet grows to hold them.
            ForEach(Array(facts.notices.enumerated()), id: \.offset) { _, notice in
                Text(notice)
                    .font(V4.font(V4.dimSize))
                    .foregroundStyle(Tok.near)
                    .fixedSize(horizontal: false, vertical: true)
            }

            buttons
        }
        .padding(.vertical, V4.cardPaddingV)
        .padding(.horizontal, V4.cardPaddingH)
        .frame(width: V4.panelWidth, alignment: .leading)
        .background(Tok.panel)
    }

    /// The row of buttons, right-aligned: the chooser's three, the import's two,
    /// `Choose codex…` beside Done when tcr could not find codex, and otherwise the
    /// sheet's own Copy link, Cancel or Done.
    @ViewBuilder
    private var buttons: some View {
        HStack(spacing: V4.buttonGap) {
            Spacer(minLength: 0)
            switch phase {
            case .choosing:
                V4Button(title: CodexSignInCopy.cancelButton) { onCancel() }
                V4Button(
                    title: CodexSignInCopy.codexAccountButton,
                    help: facts.codexSupported == false
                        ? CodexSignInCopy.oldTcrHelp : CodexSignInCopy.codexAccountHelp
                ) { onChoose(.codex(account: nil)) }
                V4Button(title: CodexSignInCopy.claudeAccountButton) { onChoose(.claude(account: nil)) }
                    .keyboardShortcut(.defaultAction)
            case .confirmingImport:
                V4Button(title: CodexSignInCopy.cancelButton) { onCancel() }
                V4Button(title: CodexSignInCopy.importButton) { onConfirmImport() }
            case .opening, .waitingForBrowser, .saved, .failed:
                if phase.offersCopyLink(authorizeURL: authorizeURL) {
                    V4Button(
                        title: "Copy link",
                        help: "Copy the sign-in link, for when the browser did not come forward."
                    ) { onCopyLink() }
                }
                if case .failed = phase, facts.failureCode == "codex-missing" {
                    V4Button(title: CodexSignInCopy.chooseCodexButton) { onChooseCodex() }
                }
                if phase.isTerminal {
                    V4Button(title: "Done") { onDone() }
                } else {
                    V4Button(title: "Cancel", role: .danger) { onCancel() }
                }
            }
        }
    }

    /// A spinner while the login is live, a state glyph once it is over. The
    /// spinner is the whole point of the waiting states: the browser is in
    /// front of the panel by then, and coming back to a still sheet with no
    /// motion in it reads as a hang. The chooser draws a plus; the import's
    /// confirmation is a question, and draws none.
    @ViewBuilder
    private var glyph: some View {
        switch phase {
        case .choosing:
            Image(systemName: "plus")
                .foregroundStyle(Tok.inkDim)
        case .confirmingImport:
            EmptyView()
        case .opening, .waitingForBrowser:
            if snapshotMode {
                Image(systemName: "clock")
                    .foregroundStyle(Tok.inkDim)
            } else {
                ProgressView()
                    .controlSize(.small)
            }
        case .saved:
            Image(systemName: "checkmark.circle.fill")
                .foregroundStyle(Tok.ok)
        case .failed:
            Image(systemName: Tok.unreadableGlyph)
                .foregroundStyle(Tok.spent)
        }
    }

    private var title: String {
        switch phase {
        case .choosing:
            return CodexSignInCopy.chooserTitle
        case .confirmingImport(let home, let account):
            let shortPath = HomePath.short(home, home: displayHome)
            guard let account else { return CodexSignInCopy.importTitle(shortPath: shortPath) }
            return CodexSignInCopy.reimportTitle(name: account, shortPath: shortPath)
        case .opening:
            return "Starting sign-in…"
        case .waitingForBrowser(let email):
            guard let email else { return "Signing in in your browser…" }
            return "Signing in as \(email) in your browser…"
        case .saved(let account):
            switch facts.request {
            case .codex(let row):
                return CodexSignInCopy.savedTitle(account: account, relogin: row != nil)
            case .codexImport:
                return CodexSignInCopy.importedTitle(account: account)
            case .claude, nil:
                return "Signed in as \(account)"
            }
        case .failed(let reason):
            // A bundled tcr too old for a config holding a Codex account refuses
            // every sign-in with words that name a version, not the fix.
            if PredatesCodexRefusal.matches(reason) { return CodexSignInCopy.predatesCodexTitle }
            // A Codex refusal is titled by its code, never by tcr's words.
            guard facts.request?.isCodex == true else { return "Sign-in failed" }
            return CodexSignInCopy.failedTitle(code: facts.failureCode)
        }
    }

    /// The line under the title, or `nil` for none: an import has no sign-in link to
    /// ask for, so its opening has no detail.
    private var detail: String? {
        switch phase {
        case .choosing:
            return CodexSignInCopy.chooserDetail
        case .confirmingImport:
            return CodexSignInCopy.importText
        case .opening:
            switch facts.request {
            case .codex: return CodexSignInCopy.openingDetail
            case .codexImport: return nil
            case .claude, nil: return "Asking tcr for the sign-in link."
            }
        case .waitingForBrowser:
            if facts.request?.isCodex == true {
                return CodexSignInCopy.waitingDetail(urlSeen: authorizeURL != nil)
            }
            return "Finish in the browser window that just opened. Nothing is written until it "
                + "comes back. If no window appeared, copy the link and open it yourself."
        case .saved:
            switch facts.request {
            case .codex:
                return CodexSignInCopy.savedDetail(
                    plan: facts.savedPlan, bundledTcrWithoutCLI: facts.bundledTcrWithoutCLI)
            case .codexImport(let home, _):
                return CodexSignInCopy.importedDetail(
                    plan: facts.savedPlan, shortPath: HomePath.short(home, home: displayHome),
                    bundledTcrWithoutCLI: facts.bundledTcrWithoutCLI)
            case .claude, nil:
                // Deliberately says what is and is not live: an account added
                // through the running proxy serves immediately, while the panel's
                // own row list is a boot-time snapshot of the config
                // (`CLAUDE.md`'s config-reload rule), the same caveat
                // `RemoveAccountControl` draws for a delete.
                return "The account is saved. It starts serving right away; the list here fills in "
                    + "on the next poll."
            }
        case .failed(let reason):
            if PredatesCodexRefusal.matches(reason) { return CodexSignInCopy.predatesCodexDetail }
            // tcr's own words, unparaphrased — the same rule every other
            // failure surface in this panel follows.
            return reason
        }
    }
}

/// ``LoginSheet`` bound to a live ``LoginSession``.
///
/// The split is what makes the sheet renderable: `LoginSheet` is a function of
/// a phase and nothing else, and this is the only piece that needs a session to
/// observe. A `@State`-held `ObservableObject` publishes nothing on its own —
/// the `@ObservedObject` here is what actually redraws the sheet as the login
/// moves.
struct LoginSheetHost: View {
    @ObservedObject var session: LoginSession
    /// The tcr inside TcrBar when this Mac has no other, from the panel, so a saved
    /// Codex sheet can say how to start codex through the pool.
    var bundledTcrWithoutCLI: String? = nil
    var onCopyLink: (URL) -> Void
    var onCancel: () -> Void
    var onDone: () -> Void
    var onChoose: (LoginRequest) -> Void = { _ in }
    var onImportLink: () -> Void = {}
    var onConfirmImport: () -> Void = {}
    var onChooseCodex: () -> Void = {}

    var body: some View {
        LoginSheet(
            phase: session.phase,
            codexPresence: .absent,
            authorizeURL: session.authorizeURL,
            onCopyLink: { if let url = session.authorizeURL { onCopyLink(url) } },
            onCancel: onCancel,
            onDone: onDone,
            facts: facts,
            onChoose: onChoose,
            onImportLink: onImportLink,
            onConfirmImport: onConfirmImport,
            onChooseCodex: onChooseCodex
        )
    }

    private var facts: LoginSheetFacts {
        var facts = session.facts
        facts.bundledTcrWithoutCLI = bundledTcrWithoutCLI
        return facts
    }
}
