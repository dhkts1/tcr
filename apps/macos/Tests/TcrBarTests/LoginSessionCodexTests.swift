import Foundation
import XCTest

@testable import TcrBarCore

/// The Codex sign-in, the import and their Terminal fallbacks: the argv each request
/// runs, what reaches the child's environment, the sheet's two new phases, the events'
/// facts, the scripts an older `tcr` gets, and what Cancel signals.
///
/// Every name is fake, and nothing here runs the real codex or the real `tcr`: the
/// scripts run fakes in temporary folders, and the sessions run `/usr/bin/true` or a
/// script of this test's own.
final class LoginSessionCodexTests: XCTestCase {

    private let routedReason =
        "this Codex login's ChatGPT workspace is not served at https://chatgpt.com with no "
        + "routing of its own (workspace_backend_origin is \"https://gov.chatgpt.com\"): "
        + "codex sends that workspace's traffic to its own backend, or routes it by residency, "
        + "and tcr pools a Codex login only at https://chatgpt.com. Nothing was written."

    // MARK: - The argv

    /// A fresh Codex sign-in.
    func testTheCodexSignInArguments() {
        XCTAssertEqual(
            LoginSession.arguments(for: .codex(account: nil)),
            ["login", "--codex", "--non-interactive"])
    }

    /// A re-login names the row it repairs, and no request ever forces past a guard.
    /// A Claude request keeps today's argv.
    func testTheReloginArgumentsNameTheRow() {
        XCTAssertEqual(
            LoginSession.arguments(for: .codex(account: "erin@example.com/codex")),
            ["login", "--codex", "--non-interactive", "--account", "erin@example.com/codex"])
        let requests: [LoginRequest] = [
            .claude(account: nil), .claude(account: "alice@example.com"), .codex(account: nil),
            .codex(account: "erin@example.com/codex"),
            .codexImport(home: "/Users/example/.codex", account: nil),
            .codexImport(home: "/Users/example/.codex", account: "erin@example.com/codex"),
        ]
        for request in requests {
            XCTAssertFalse(
                LoginSession.arguments(for: request).contains("--force"), "\(request) forces")
        }
        XCTAssertEqual(
            LoginSession.arguments(for: .claude(account: "alice@example.com")),
            LoginSession.arguments(account: "alice@example.com"))
    }

    /// The import names the folder it reads, and a re-import the row it repairs.
    func testTheImportArguments() {
        XCTAssertEqual(
            LoginSession.arguments(for: .codexImport(home: "/Users/example/.codex", account: nil)),
            ["login", "--from-codex", "--codex-home", "/Users/example/.codex", "--non-interactive"])
        XCTAssertEqual(
            LoginSession.arguments(
                for: .codexImport(home: "/Users/example/.codex", account: "erin@example.com/codex")),
            [
                "login", "--from-codex", "--codex-home", "/Users/example/.codex", "--account",
                "erin@example.com/codex", "--non-interactive",
            ])
    }

    // MARK: - The child's environment

    /// The codex a person chose wins over the one this Mac's search found, and either
    /// reaches the Codex sign-in as `TCR_CODEX_BIN`: a TcrBar started from Finder has no
    /// shell `PATH`, so tcr would not find an npm, nvm or volta codex on its own. Nothing
    /// changes for any other request, or with no path at all.
    func testTheCodexPathRidesTheEnvironment() {
        let inherited = ["PATH": "/usr/bin:/bin"]
        let stored = LoginSession.childEnvironment(
            for: .codex(account: nil), inherited: inherited,
            storedCodexExecutable: "/Users/example/.npm-global/bin/codex",
            resolvedCodexExecutable: "/opt/homebrew/bin/codex")
        XCTAssertEqual(stored["TCR_CODEX_BIN"], "/Users/example/.npm-global/bin/codex")
        XCTAssertEqual(stored["PATH"], "/usr/bin:/bin")

        for resolved in [
            "/Users/example/.nvm/versions/node/v22.11.0/bin/codex", "/Users/example/.volta/bin/codex",
        ] {
            let environment = LoginSession.childEnvironment(
                for: .codex(account: nil), inherited: inherited, storedCodexExecutable: nil,
                resolvedCodexExecutable: resolved)
            XCTAssertEqual(environment["TCR_CODEX_BIN"], resolved)
        }

        for (stored, resolved) in [(nil, nil), ("", "")] as [(String?, String?)] {
            XCTAssertEqual(
                LoginSession.childEnvironment(
                    for: .codex(account: nil), inherited: inherited,
                    storedCodexExecutable: stored, resolvedCodexExecutable: resolved),
                inherited)
        }

        for request in [
            LoginRequest.claude(account: nil),
            .codexImport(home: "/Users/example/.codex", account: nil),
        ] {
            XCTAssertEqual(
                LoginSession.childEnvironment(
                    for: request, inherited: inherited,
                    storedCodexExecutable: "/Users/example/.npm-global/bin/codex",
                    resolvedCodexExecutable: "/opt/homebrew/bin/codex"),
                inherited, "\(request) gained a codex path")
        }
    }

    // MARK: - The phases

    /// The chooser opens only on evidence of codex; without it, Add account… starts the
    /// Claude sign-in as it always did. Neither new phase ends the sheet.
    func testTheChooserPhaseOnlyWithPresence() {
        XCTAssertEqual(LoginPhase.firstPhase(for: .absent), .opening)
        XCTAssertEqual(
            LoginPhase.firstPhase(
                for: CodexPresence(
                    fleetHoldsCodex: false, codexExecutable: nil, codexHome: "/Users/example/.codex",
                    storedExecutable: nil)),
            .choosing)
        XCTAssertFalse(LoginPhase.choosing.isTerminal)
        XCTAssertFalse(
            LoginPhase.confirmingImport(home: "/Users/example/.codex", account: nil).isTerminal)
    }

    /// `opened` means codex opened the browser itself, so TcrBar opens nothing, and still
    /// keeps the link for Copy link. Without it TcrBar opens the URL, exactly as today.
    func testOpenedMeansTcrBarOpensNothing() {
        let url = "https://auth.openai.com/oauth/authorize?client_id=app_EXAMPLE&state=EXAMPLE"
        var opened = LoginFlow(requesting: nil)
        XCTAssertNil(opened.apply(.browser(url: url, opened: true)))
        XCTAssertEqual(opened.authorizeURL?.absoluteString, url)

        var notOpened = LoginFlow(requesting: nil)
        XCTAssertEqual(notOpened.apply(.browser(url: url, opened: false))?.absoluteString, url)
        XCTAssertEqual(notOpened.authorizeURL?.absoluteString, url)
    }

    /// Copy link needs a link and a sign-in still running, and is never offered while
    /// nothing has started.
    func testCopyLinkOnlyAfterABrowserURL() throws {
        let url = try XCTUnwrap(URL(string: "https://auth.openai.com/oauth/authorize?state=EXAMPLE"))
        XCTAssertFalse(LoginPhase.waitingForBrowser(email: nil).offersCopyLink(authorizeURL: nil))
        XCTAssertTrue(LoginPhase.waitingForBrowser(email: nil).offersCopyLink(authorizeURL: url))
        XCTAssertFalse(LoginPhase.saved(account: "bob@example.com").offersCopyLink(authorizeURL: url))
        XCTAssertFalse(LoginPhase.choosing.offersCopyLink(authorizeURL: url))
        XCTAssertFalse(
            LoginPhase.confirmingImport(home: "/Users/example/.codex", account: nil)
                .offersCopyLink(authorizeURL: url))
    }

    // MARK: - The events' facts

    /// Every notice tcr sends is kept, in the order it came, and the save carries its
    /// plan. TcrBar never drops a notice, even one that arrives after the result, and
    /// nothing else moves once the login has ended.
    func testEveryNoticeIsShownInOrder() throws {
        let lines = try fixtureLines()
        var flow = LoginFlow(requesting: nil)
        for line in lines.dropLast() {
            flow.apply(try XCTUnwrap(LoginProgressEvent.parse(line), "unparsed: \(line)"))
        }
        XCTAssertEqual(flow.notices, [try field("text", ofLine: 2)])
        XCTAssertEqual(flow.savedPlan, "ChatGPT Pro")
        XCTAssertEqual(flow.phase, .saved(account: "bob@example.com"))

        var second = LoginFlow(requesting: nil)
        for text in ["the first notice", "the second notice", "the third notice"] {
            second.apply(.notice(text: text))
        }
        second.apply(.saved(account: "bob@example.com", provider: "codex", plan: "ChatGPT Plus"))
        XCTAssertEqual(second.notices, ["the first notice", "the second notice", "the third notice"])

        second.apply(.notice(text: "a late notice"))
        XCTAssertEqual(
            second.notices,
            ["the first notice", "the second notice", "the third notice", "a late notice"])

        second.apply(.failed(reason: "a late refusal", code: "codex-routed"))
        second.apply(.browser(url: "https://auth.openai.com/oauth/authorize?state=LATE"))
        second.apply(.saved(account: "carol@example.com", provider: "codex", plan: "ChatGPT Pro"))
        XCTAssertEqual(second.phase, .saved(account: "bob@example.com"))
        XCTAssertEqual(second.savedPlan, "ChatGPT Plus")
        XCTAssertNil(second.failureCode)
        XCTAssertNil(second.refusal)
        XCTAssertNil(second.authorizeURL)
    }

    /// A refusal keeps its code, which titles the sheet, and its reason, which is tcr's
    /// own sentence. A child that ended without an `error` event refused nothing.
    func testAFailedTitleKeysOnItsCode() throws {
        let last = try XCTUnwrap(try fixtureLines().last)
        let reason = try field("reason", ofLine: 4)
        var flow = LoginFlow(requesting: "erin@example.com/codex")
        flow.apply(try XCTUnwrap(LoginProgressEvent.parse(last)))
        XCTAssertEqual(flow.failureCode, "codex-identity")
        XCTAssertEqual(flow.refusal, reason)
        XCTAssertEqual(flow.phase, .failed(reason: reason))
        XCTAssertEqual(
            CodexSignInCopy.failedTitle(code: flow.failureCode),
            "That browser signed in a different ChatGPT account or workspace")

        var ended = LoginFlow(requesting: nil)
        ended.finish(exitCode: 1, stderr: "OAuth login failed: Login timed out after 2 minutes\n")
        XCTAssertNil(ended.refusal)
        XCTAssertNil(ended.failureCode)
    }

    // MARK: - An older tcr

    /// A `tcr` without `--codex` gets two commands in Terminal: codex signs in inside a
    /// folder of its own, then the old import reads it. The folder is made where tcr keeps
    /// its config, and both traps stand before codex runs. No line replaces the shell, which
    /// would drop the trap that removes the folder.
    func testAnOldTcrFallsBackToTerminal() throws {
        let tcr = "/opt/homebrew/bin/tcr"
        let script = LoginLauncher.codexLoginScript(forExecutableAt: tcr, reloggingIn: nil)
        let signIn =
            #"CODEX_HOME="$d" codex login && '/opt/homebrew/bin/tcr' login --from-codex"#
            + #" --codex-home "$d""#
        let run = try XCTUnwrap(script.range(of: signIn), script)
        for line in [
            #"d=$(mktemp -d "$HOME/.config/tcr-codex-login.XXXX") || exit 1"#,
            #"trap 'rm -rf "$d"' EXIT"#,
            "trap 'exit 1' HUP INT TERM",
        ] {
            let found = try XCTUnwrap(script.range(of: line), "the script lacks \(line)")
            XCTAssertLessThan(found.lowerBound, run.lowerBound, "\(line) comes after codex runs")
        }
        XCTAssertFalse(script.contains("exec"), script)
        XCTAssertFalse(script.contains("--account"), script)

        for name in ["erin@example.com/codex", "erin@example.com"] {
            XCTAssertTrue(
                LoginLauncher.codexLoginScript(forExecutableAt: tcr, reloggingIn: name)
                    .hasSuffix(" --account 'erin@example.com'"), name)
        }
        for name in ["work", "team/erin@example.com"] {
            XCTAssertFalse(
                LoginLauncher.codexLoginScript(forExecutableAt: tcr, reloggingIn: name)
                    .contains("--account"), name)
        }
        XCTAssertTrue(
            LoginLauncher.codexLoginScript(forExecutableAt: tcr, reloggingIn: "o'neil@example.com")
                .hasSuffix(#" --account 'o'\''neil@example.com'"#))

        let importScript = LoginLauncher.codexImportScript(
            forExecutableAt: tcr, codexHome: "/Users/example/.codex", reloggingIn: nil)
        XCTAssertTrue(
            importScript.contains(
                "exec '/opt/homebrew/bin/tcr' login --from-codex --codex-home '/Users/example/.codex'"))
        XCTAssertFalse(importScript.contains("--account"), importScript)
        let reimport = LoginLauncher.codexImportScript(
            forExecutableAt: tcr, codexHome: "/Users/example/.codex",
            reloggingIn: "erin@example.com/codex")
        XCTAssertTrue(reimport.hasSuffix(" --account 'erin@example.com'"), reimport)

        for text in [script, importScript, reimport] {
            XCTAssertFalse(text.contains("--force"), text)
            XCTAssertFalse(text.contains("--non-interactive"), text)
        }
    }

    /// The two-command script leaves no sign-in folder behind, whether the import succeeds
    /// or fails. Without a config folder to make it in, codex never runs, so it cannot fall
    /// back to signing in over the login in `~/.codex`.
    func testTheOldTcrScriptLeavesNoFolder() throws {
        let root = try temporaryFolder()
        let bin = root.appendingPathComponent("bin", isDirectory: true)
        try FileManager.default.createDirectory(at: bin, withIntermediateDirectories: true)
        try executable(
            bin.appendingPathComponent("codex"),
            """
            #!/bin/sh
            home="${CODEX_HOME:-$HOME/.codex}"
            /bin/mkdir -p "$home"
            echo '{}' > "$home/auth.json"
            """)

        for exitCode: Int32 in [0, 1] {
            let home = root.appendingPathComponent("home-\(exitCode)", isDirectory: true)
            let config = home.appendingPathComponent(".config", isDirectory: true)
            try FileManager.default.createDirectory(at: config, withIntermediateDirectories: true)
            let tcr = root.appendingPathComponent("tcr-\(exitCode)")
            try executable(tcr, "#!/bin/sh\nexit \(exitCode)\n")

            let status = try runScript(
                LoginLauncher.codexLoginScript(forExecutableAt: tcr.path, reloggingIn: nil),
                home: home, bin: bin, in: root)
            XCTAssertEqual(status, exitCode, "the script did not end the way the import did")
            let left = try FileManager.default.contentsOfDirectory(atPath: config.path)
                .filter { $0.hasPrefix("tcr-codex-login.") }
            XCTAssertEqual(left, [], "the import exiting \(exitCode) left \(left) behind")
        }

        let bare = root.appendingPathComponent("home-without-config", isDirectory: true)
        try FileManager.default.createDirectory(at: bare, withIntermediateDirectories: true)
        let tcr = root.appendingPathComponent("tcr-0")
        _ = try runScript(
            LoginLauncher.codexLoginScript(forExecutableAt: tcr.path, reloggingIn: nil),
            home: bare, bin: bin, in: root)
        XCTAssertFalse(
            FileManager.default.fileExists(atPath: bare.appendingPathComponent(".codex").path),
            "with no folder of its own, codex signed in over the login in ~/.codex")
    }

    // MARK: - The session

    /// The chooser's choice starts the sign-in at once, and nothing a second click asks for
    /// changes the request that is running. A session presented with a request names it
    /// from its first frame.
    @MainActor
    func testAChoiceOpensAtOnceAndASecondChoiceChangesNothing() {
        let session = LoginSession(
            request: nil, phase: .choosing, open: { _ in },
            resolve: { .success(URL(fileURLWithPath: "/usr/bin/true")) },
            storedCodexExecutable: { nil })

        session.begin(.codex(account: nil))
        XCTAssertEqual(session.phase, .opening)
        XCTAssertEqual(session.request, .codex(account: nil))
        XCTAssertEqual(session.facts.request, .codex(account: nil))

        session.begin(.claude(account: nil))
        session.confirmImport(home: "/Users/example/.codex", account: nil)
        XCTAssertEqual(session.request, .codex(account: nil))
        XCTAssertEqual(session.phase, .opening)
        XCTAssertEqual(session.facts.request, .codex(account: nil))

        XCTAssertEqual(
            LoginSession(request: .codex(account: "erin@example.com/codex")).facts.request,
            .codex(account: "erin@example.com/codex"))
    }

    /// Cancel reaches everything the sign-in started: `tcr login --codex` runs `codex login`
    /// as a child of its own, and a codex left running would keep its loopback listener and
    /// could still finish a login nobody wants.
    @MainActor
    func testCancelSignalsTheWholeProcessGroup() throws {
        let root = try temporaryFolder()
        let pidFile = root.appendingPathComponent("sleep.pid")
        let script = root.appendingPathComponent("fake-tcr")
        try executable(
            script,
            """
            #!/bin/sh
            /bin/sleep 60 &
            echo $! > '\(pidFile.path)'
            wait
            """)

        let session = LoginSession(
            request: .codex(account: nil), open: { _ in }, resolve: { .success(script) },
            storedCodexExecutable: { nil })
        session.start()

        var pid: pid_t?
        let started = Date()
        while pid == nil, Date().timeIntervalSince(started) < 5 {
            if let text = try? String(contentsOf: pidFile, encoding: .utf8),
                let value = pid_t(text.trimmingCharacters(in: .whitespacesAndNewlines))
            {
                pid = value
            } else {
                Thread.sleep(forTimeInterval: 0.02)
            }
        }
        let sleeper = try XCTUnwrap(pid, "the fake tcr never started its child")
        addTeardownBlock {
            if kill(sleeper, 0) == 0 { kill(sleeper, SIGKILL) }
        }

        session.cancel()

        var gone = false
        let cancelled = Date()
        while !gone, Date().timeIntervalSince(cancelled) < 5 {
            gone = kill(sleeper, 0) != 0 && errno == ESRCH
            if !gone { Thread.sleep(forTimeInterval: 0.02) }
        }
        XCTAssertTrue(gone, "the child the sign-in started outlived Cancel")
    }

    // MARK: - Helpers

    private func fixtureLines() throws -> [String] {
        String(decoding: try CanonicalFixtures.data("codex-login-events.jsonl"), as: UTF8.self)
            .split(separator: "\n").map(String.init)
    }

    /// One string field of one fixture line, read as JSON.
    private func field(_ key: String, ofLine index: Int) throws -> String {
        let lines = try fixtureLines()
        let line = try XCTUnwrap(lines.indices.contains(index) ? lines[index] : nil)
        let object = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(line.utf8)) as? [String: Any])
        return try XCTUnwrap(object[key] as? String, "line \(index) has no \(key)")
    }

    /// A fresh folder, removed when the test ends.
    private func temporaryFolder() throws -> URL {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("tcrbar-codex-login-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: url) }
        return url
    }

    /// `text` written to `url`, mode 0755.
    private func executable(_ url: URL, _ text: String) throws {
        try text.write(to: url, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: url.path)
    }

    /// Run `script` under `/bin/sh` with `HOME` and a `PATH` of `bin` and the system's own,
    /// and answer its exit status.
    private func runScript(_ script: String, home: URL, bin: URL, in root: URL) throws -> Int32 {
        let file = root.appendingPathComponent("script-\(UUID().uuidString).sh")
        try script.write(to: file, atomically: true, encoding: .utf8)
        let shell = Process()
        shell.executableURL = URL(fileURLWithPath: "/bin/sh")
        shell.arguments = [file.path]
        shell.environment = ["HOME": home.path, "PATH": "\(bin.path):/usr/bin:/bin"]
        shell.standardInput = FileHandle.nullDevice
        shell.standardOutput = FileHandle.nullDevice
        shell.standardError = FileHandle.nullDevice
        try shell.run()
        shell.waitUntilExit()
        return shell.terminationStatus
    }
}
