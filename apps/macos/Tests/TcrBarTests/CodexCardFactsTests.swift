import XCTest

@testable import TcrBarCore

/// What a Codex account's card, line and help say that a Claude account's do not.
///
/// The test target links `TcrBarCore` only, so the card itself cannot be built here.
/// Every word it draws for a Codex row comes from `CodexCardFacts` and the `Account`
/// properties it reads, and those can be tested. Each expected string is the copy the
/// card draws, word for word.
///
/// The canonical rows come from `apps/macos/Tests/Fixtures/codex-status.json`, at the
/// fixture's own clock. Account names are obviously fake: this repository is public.
final class CodexCardFactsTests: XCTestCase {

    func testPendingTokenSyncIsVisibleWithoutChangingTheStatePill() throws {
        let ready = try canonical("bob@example.com/codex")
        let pending = try canonical("bob@example.com/codex", ["tokenSyncPending": true])
        XCTAssertEqual(FleetTally.Kind(account: pending), FleetTally.Kind(account: ready))
        XCTAssertNil(CodexCardFacts.stateHelp(ready, now: Date()))
        XCTAssertTrue(CodexCardFacts.stateHelp(pending, now: Date())?.contains("auth.json") == true)
        XCTAssertTrue(CodexCardFacts.stateHelp(pending, now: Date())?.contains("retry") == true)
    }

    // MARK: - Which windows draw a row

    func testOnlyReportedWindowsAreDrawn() throws {
        XCTAssertEqual(try canonical("bob@example.com/codex").reportedWindows, [.sevenDay])
        XCTAssertEqual(try canonical("carol@example.com").reportedWindows, [.sevenDay])
        XCTAssertEqual(try canonical("dana@example.com").reportedWindows, [.fiveHour, .sevenDay])
        XCTAssertEqual(try canonical("jo@example.com").reportedWindows, [])
    }

    func testAbsentQuotaWindowsDrawsBothRows() throws {
        XCTAssertEqual(try canonical("alice@example.com").reportedWindows, [.fiveHour, .sevenDay])
        XCTAssertEqual(
            try canonical("bob@example.com/codex", ["quotaWindows": NSNull()]).reportedWindows,
            [.fiveHour, .sevenDay])
        XCTAssertEqual(try unmeasuredBob().reportedWindows, [.fiveHour, .sevenDay])
    }

    func testAFirstCodexProbeCannotShrinkTheQuotaArea() throws {
        let unknown = try unmeasuredBob()
        let weekly = try canonical("bob@example.com/codex")
        let none = try canonical("jo@example.com")
        XCTAssertEqual(CodexCardFacts.reservedQuotaRows(unknown), 2)
        XCTAssertEqual(CodexCardFacts.reservedQuotaRows(weekly), 2)
        XCTAssertEqual(CodexCardFacts.reservedQuotaRows(none), 2)
        // Reserving space must never claim a plan has an absent quota window.
        XCTAssertEqual(weekly.reportedWindows, [.sevenDay])
        XCTAssertEqual(none.reportedWindows, [])
    }

    // MARK: - The state pill's word

    func testTheStatePillWords() throws {
        let expected: [(String, [String: Any], String)] = [
            ("bob@example.com/codex", [:], "OK"),
            ("carol@example.com", [:], "Spent"),
            ("gil@example.com", [:], "Refused"),
            ("hal@example.com", [:], "Refused"),
            ("erin@example.com/codex", [:], "Needs re-login"),
            ("jo@example.com", [:], "OK"),
            ("kim@example.com", [:], "Unreachable"),
            ("bob@example.com/codex", forbiddenHold, "Refused"),
        ]
        for (name, overrides, word) in expected {
            let account = try canonical(name, overrides)
            XCTAssertEqual(
                FleetTally.Kind(account: account).pillText(for: .codex), word,
                "\(name) \(overrides.isEmpty ? "" : "under a 403 hold")")
        }
    }

    // MARK: - The state pill's help

    func testTheRefusedHelpSaysWhy() throws {
        let tail =
            "no usage window is full. tcr may still send it a request, and another refusal "
            + "marks it again; a probe that reads it as allowed, or sees one of its usage "
            + "windows roll, clears the refusal."
        XCTAssertEqual(
            try help(canonical("gil@example.com")),
            "ChatGPT refused this account's last request (the workspace's spend limit is "
                + "reached), and " + tail)
        XCTAssertEqual(
            try help(canonical("gil@example.com", ["spendControlReached": false])),
            "ChatGPT refused this account's last request (limit: rate_limit_reached), and " + tail)
        XCTAssertEqual(
            try help(
                canonical(
                    "gil@example.com",
                    ["spendControlReached": false, "limitReachedType": NSNull()])),
            "ChatGPT refused this account's last request, and " + tail)

        var removed = try canonicalRow("gil@example.com", ["held": [Any]()])
        removed.removeValue(forKey: "limitReachedType")
        removed.removeValue(forKey: "spendControlReached")
        XCTAssertEqual(
            try help(decoded(removed)), "ChatGPT refused this account's last request, and " + tail)

        // A rejected login can have a full window too; report both facts.
        let refused = try canonical("carol@example.com", rejectedFullWindow)
        XCTAssertEqual(FleetTally.Kind(account: refused).pillText(for: .codex), "Refused")
        XCTAssertTrue(help(refused)?.contains("A usage window is also full.") == true)
        XCTAssertTrue(refused.stateHelp?.contains("ChatGPT") == true)

    }

    func testTheRefusedHelpWithAKnownHold() throws {
        XCTAssertEqual(
            try help(canonical("hal@example.com")),
            "ChatGPT refused this account's last request (limit: rate_limit_reached) though no "
                + "usage window is full. tcr holds it until 00:47, in 47m.")
    }

    /// The other cause of a refused Codex row: ChatGPT answered 403, and the server
    /// holds the account for a short while, then tries it again.
    func testTheForbiddenHoldHelp() throws {
        XCTAssertEqual(
            try help(canonical("bob@example.com/codex", forbiddenHold)),
            "ChatGPT answered this account's last request with 403 Forbidden. tcr holds it "
                + "until 00:01, in 2m, then tries it again.")
        let lastSeconds: [String: Any] = [
            "gate": "refused", "freeAtMs": 1_767_225_620_000,
            "rateLimitedUntilMs": 1_767_225_620_000, "secondsUntilFree": 20,
        ]
        XCTAssertEqual(
            try help(canonical("bob@example.com/codex", lastSeconds)),
            "ChatGPT answered this account's last request with 403 Forbidden. tcr holds it "
                + "until 00:00, then tries it again.")
        var named = forbiddenHold
        named["limitReachedType"] = "rate_limit_reached"
        XCTAssertEqual(
            try help(canonical("bob@example.com/codex", named)),
            "ChatGPT answered this account's last request with 403 Forbidden. tcr holds it "
                + "until 00:01, in 2m, then tries it again.")
        XCTAssertNil(
            try help(canonical("alice@example.com", forbiddenHold)),
            "a 403-held Claude row says what it always said")
    }

    func testTheUnreachableHelpNamesItsGroup() throws {
        XCTAssertEqual(
            try help(canonical("kim@example.com")),
            "Reserved for research, and a Codex session cannot ask for a group, so no Codex "
                + "request can reach this account. Remove it from research, or unreserve research.")
        XCTAssertEqual(
            try help(
                canonical(
                    "kim@example.com",
                    [
                        "groups": ["research", "orchard"],
                        "reservedGroups": ["research", "orchard"],
                    ])),
            "Reserved for research and orchard, and a Codex session cannot ask for a group, so "
                + "no Codex request can reach this account. Remove it from each of research and "
                + "orchard, or unreserve them.")
    }

    func testTheSpentHelpOnlyWithCreditsOrAReachedType() throws {
        XCTAssertEqual(
            try help(canonical("carol@example.com")),
            "ChatGPT's weekly limit is reached (limit: rate_limit_reached); it returns in 3d 4h. "
                + "This account also holds 12.50 credits, which tcr does not spend.")

        let plain = try canonical(
            "carol@example.com", ["credits": NSNull(), "limitReachedType": NSNull()])
        XCTAssertNil(help(plain))
        XCTAssertNil(plain.stateHelp)

        XCTAssertEqual(
            try help(
                canonical(
                    "carol@example.com",
                    [
                        "credits": ["hasCredits": true, "unlimited": true],
                        "limitReachedType": NSNull(),
                    ])),
            "ChatGPT's weekly limit is reached; it returns in 3d 4h. This account also has "
                + "unlimited credits, which tcr does not spend.")

        let spentOnFiveHours: [String: Any] = [
            "gate": "five-hour", "quotaState": "spent", "fiveHourState": "spent",
            "held": [["window": "5h", "minutesUntilReset": 52, "resetAtMs": 1_767_228_720_000]],
            "limitReachedType": "rate_limit_reached",
        ]
        XCTAssertEqual(
            try help(canonical("dana@example.com", spentOnFiveHours)),
            "ChatGPT's 5-hour limit is reached (limit: rate_limit_reached); it returns in 52m.")
    }

    /// A Codex line speaks its state's help as its value, the sentence its pill would
    /// show on hover: a line has no pill. A signed-out line says nothing more there,
    /// because the same sentence is its notice, read next.
    func testALineSpeaksItsStateHelp() throws {
        for name in ["gil@example.com", "hal@example.com", "kim@example.com", "carol@example.com"] {
            let account = try canonical(name)
            let value = CodexCardFacts.lineValue(
                account, now: fixtureNow, timeZone: utc, home: "/Users/example")
            XCTAssertNotNil(value, name)
            XCTAssertEqual(value, help(account), name)
        }
        for name in [
            "erin@example.com/codex", "bob@example.com/codex", "jo@example.com",
            "alice@example.com",
        ] {
            XCTAssertNil(
                CodexCardFacts.lineValue(
                    try canonical(name), now: fixtureNow, timeZone: utc, home: "/Users/example"),
                name)
        }
    }

    // MARK: - The line a plan with no window draws

    func testTheNoWindowsLineAndItsCredits() throws {
        let jo = try XCTUnwrap(CodexCardFacts.noWindowsLine(try canonical("jo@example.com")))
        XCTAssertEqual(jo.text, "No usage windows reported · credits unlimited")
        XCTAssertEqual(
            jo.help,
            "This plan reported no usage window on the last successful probe. 6.0k output "
                + "tokens today.")
        XCTAssertEqual(QuotaFormat.tokens(6000), "6.0k")

        func text(_ credits: Any) throws -> String? {
            CodexCardFacts.noWindowsLine(try canonical("jo@example.com", ["credits": credits]))?
                .text
        }
        XCTAssertEqual(
            try text(["hasCredits": true, "unlimited": false, "balance": "12.50"]),
            "No usage windows reported · credits 12.50")
        XCTAssertEqual(try text(NSNull()), "No usage windows reported")
        XCTAssertEqual(
            try text(["hasCredits": false, "unlimited": false]), "No usage windows reported")
        XCTAssertEqual(
            try text(["hasCredits": false, "unlimited": true]),
            "No usage windows reported · credits unlimited")

        XCTAssertEqual(
            CodexCardFacts.noWindowsLine(try canonical("jo@example.com", ["usage": NSNull()]))?
                .help,
            "This plan reported no usage window on the last successful probe.")

        XCTAssertNil(CodexCardFacts.noWindowsLine(try canonical("bob@example.com/codex")))
        XCTAssertNil(CodexCardFacts.noWindowsLine(try canonical("alice@example.com")))

        XCTAssertEqual(
            CodexCardFacts.creditsText(try canonical("carol@example.com")), "credits 12.50")
        XCTAssertEqual(CodexCardFacts.creditsText(try canonical("jo@example.com")), "credits unlimited")
        XCTAssertEqual(
            CodexCardFacts.creditsText(
                try canonical(
                    "jo@example.com", ["credits": ["hasCredits": false, "unlimited": true]])),
            "credits unlimited")
    }

    // MARK: - The name's help

    func testTheNameHelpNamesTheWorkspace() throws {
        XCTAssertEqual(
            CodexCardFacts.nameHelp(try canonical("bob@example.com/codex")),
            "bob@example.com/codex · ChatGPT workspace 55555555")
        XCTAssertEqual(
            CodexCardFacts.nameHelp(try canonical("bob@example.com/codex", ["orgUuid": NSNull()])),
            "bob@example.com/codex")
        XCTAssertEqual(
            CodexCardFacts.nameHelp(try canonical("alice@example.com")), "alice@example.com")
    }

    // MARK: - The tails: output tokens, by window

    func testEachTailHelpNamesItsWindow() throws {
        let dana = try canonical("dana@example.com")
        try assertTail(
            dana, .fiveHour, "210k", "210k output tokens in this 5-hour window (55k reasoning)")
        try assertTail(dana, .sevenDay, "640k", "640k output tokens this week (180k reasoning)")

        let bob = try canonical("bob@example.com/codex")
        try assertTail(bob, .sevenDay, "1.1M", "1.1M output tokens this week (420k reasoning)")
        try assertTail(
            try canonical("bob@example.com/codex", ["sevenDayWindowMinutes": 43200]), .sevenDay,
            "1.1M", "1.1M output tokens in this 30d window (420k reasoning)")

        var olderBob = try canonicalRow("bob@example.com/codex")
        var usage = try XCTUnwrap(olderBob["usage"] as? [String: Any])
        usage.removeValue(forKey: "sevenDayWindow")
        olderBob["usage"] = usage
        try assertTail(
            try decoded(olderBob), .sevenDay, "22k", "22k output tokens today (9.0k reasoning)")

        try assertTail(
            try canonical("erin@example.com/codex"), .fiveHour, "0", "0 output tokens today")
    }

    func testATailNeverReadsZeroDollars() throws {
        // Today's Claude rule would print `$0.00 · 0` for erin's idle day.
        let erin = try XCTUnwrap(
            CodexCardFacts.tail(try canonical("erin@example.com/codex"), window: .fiveHour))
        XCTAssertEqual(erin.text, "0")

        var priced = try canonicalRow("dana@example.com")
        var usage = try XCTUnwrap(priced["usage"] as? [String: Any])
        var window = try XCTUnwrap(usage["window"] as? [String: Any])
        window["costUsd"] = 1.2
        window["unpricedRequests"] = 0
        usage["window"] = window
        priced["usage"] = usage
        try assertTail(
            try decoded(priced), .fiveHour, "$1.20 · 210k",
            "$1.20 · 210k output tokens in this 5-hour window (55k reasoning)")

        for name in codexNames {
            let account = try canonical(name)
            for window in account.reportedWindows {
                guard let tail = CodexCardFacts.tail(account, window: window) else { continue }
                XCTAssertFalse(tail.text.contains("$0.00"), "\(name) \(window): \(tail.text)")
                XCTAssertFalse(tail.help.contains("$0.00"), "\(name) \(window): \(tail.help)")
            }
        }
    }

    /// A bucket every request of which was priced, but for less than half a cent, rounds
    /// to `$0.00`: the first cached turn of a window under a `gpt-` price override, or an
    /// override priced at zero. The tail then keeps its tokens alone, by the rule a Codex
    /// box's spend follows.
    func testAPricedTailUnderHalfACentKeepsItsTokensAlone() throws {
        for cost in [0.004, 0.0] {
            var priced = try canonicalRow("dana@example.com")
            var usage = try XCTUnwrap(priced["usage"] as? [String: Any])
            var window = try XCTUnwrap(usage["window"] as? [String: Any])
            window["costUsd"] = cost
            window["unpricedRequests"] = 0
            usage["window"] = window
            priced["usage"] = usage
            try assertTail(
                try decoded(priced), .fiveHour, "210k",
                "210k output tokens in this 5-hour window (55k reasoning)")
        }

        // The rounding edge: half a cent and up is a cent, and leads.
        var cent = try canonicalRow("dana@example.com")
        var usage = try XCTUnwrap(cent["usage"] as? [String: Any])
        var window = try XCTUnwrap(usage["window"] as? [String: Any])
        window["costUsd"] = 0.0051
        window["unpricedRequests"] = 0
        usage["window"] = window
        cent["usage"] = usage
        try assertTail(
            try decoded(cent), .fiveHour, "$0.01 · 210k",
            "$0.01 · 210k output tokens in this 5-hour window (55k reasoning)")
    }

    // MARK: - The legacy panel's words

    func testTheLegacyPanelWordsCodexRowsAndKeepsClaudeRows() throws {
        let alice = try canonical("alice@example.com")
        XCTAssertEqual(
            LegacyRowCopy.planHelp(alice),
            "This account's plan, as Anthropic reports it for its organization.")
        XCTAssertEqual(
            LegacyRowCopy.orgTagHelp(alice),
            "The organization half of this account's name — the full name is alice@example.com, "
                + "which is what every `tcr` command takes.")
        XCTAssertEqual(
            LegacyRowCopy.rotatingHelp(alice),
            "In the rotation — this account can be picked for traffic, as far as this build can "
                + "tell. A quota rejection from Anthropic can also exclude an account without "
                + "changing its status; TcrBar cannot see that gate yet.")
        XCTAssertEqual(LegacyRowCopy.rejectedPillWord(alice), "rejected")
        XCTAssertEqual(
            LegacyRowCopy.rejectedPillHelp(alice, now: fixtureNow),
            "Anthropic has rejected this account, so `tcr` sends it no traffic whatever its quota "
                + "reads. It returns on their verdict, not on a reset.")
        XCTAssertEqual(
            LegacyRowCopy.rejectedSpoken(alice, now: fixtureNow),
            "rejected by Anthropic, not in rotation")

        let gil = try canonical("gil@example.com")
        XCTAssertEqual(LegacyRowCopy.planHelp(gil), CodexCardFacts.nameHelp(gil))
        XCTAssertEqual(LegacyRowCopy.orgTagHelp(gil), CodexCardFacts.nameHelp(gil))
        XCTAssertEqual(
            LegacyRowCopy.rotatingHelp(gil),
            "In the rotation — this account can be picked for traffic, as far as this build can "
                + "tell.")
        XCTAssertEqual(LegacyRowCopy.rejectedPillWord(gil), "refused")
        XCTAssertEqual(
            LegacyRowCopy.rejectedPillHelp(gil, now: fixtureNow),
            CodexCardFacts.stateHelp(gil, now: fixtureNow))
        XCTAssertNotNil(LegacyRowCopy.rejectedPillHelp(gil, now: fixtureNow))
        XCTAssertEqual(
            LegacyRowCopy.rejectedSpoken(gil, now: fixtureNow),
            "refused by ChatGPT, not in rotation")
    }

    /// A row held after ChatGPT answered 403 says, out loud, how long the hold has
    /// left, as its help does; in its last seconds the hold has no countdown to say.
    /// A usage refusal held to a known end speaks as any refused row does.
    func testTheLegacyPanelSpeaksAForbiddenHold() throws {
        let held = try canonical("bob@example.com/codex", forbiddenHold)
        XCTAssertEqual(
            LegacyRowCopy.rejectedSpoken(held, now: fixtureNow), "refused by ChatGPT, held in 2m")
        XCTAssertEqual(LegacyRowCopy.rejectedPillWord(held), "refused")
        XCTAssertEqual(
            LegacyRowCopy.rejectedPillHelp(held, now: fixtureNow),
            CodexCardFacts.stateHelp(held, now: fixtureNow))

        let lastSeconds = try canonical(
            "bob@example.com/codex",
            [
                "gate": "refused", "freeAtMs": 1_767_225_620_000,
                "rateLimitedUntilMs": 1_767_225_620_000, "secondsUntilFree": 20,
            ])
        XCTAssertEqual(
            LegacyRowCopy.rejectedSpoken(lastSeconds, now: fixtureNow), "refused by ChatGPT, held")

        XCTAssertEqual(
            LegacyRowCopy.rejectedSpoken(try canonical("hal@example.com"), now: fixtureNow),
            "refused by ChatGPT, not in rotation")
    }

    /// The legacy panel's plan line prints a Claude row's idle day as a measured
    /// `$0.00 today`. A Codex row never reads `$0.00`, so its idle day, and a day priced
    /// under half a cent, print the tokens alone, drawn and spoken.
    func testTheLegacyPlanLineNeverReadsZeroDollarsOnACodexRow() throws {
        for name in ["erin@example.com/codex", "kim@example.com"] {
            let idle = try canonical(name)
            XCTAssertEqual(idle.windowUsageLabel, "0 out today", name)
            XCTAssertEqual(idle.windowUsageSpokenLabel, "today: 0 output tokens", name)
        }

        var cheap = try canonicalRow("dana@example.com")
        var usage = try XCTUnwrap(cheap["usage"] as? [String: Any])
        var window = try XCTUnwrap(usage["window"] as? [String: Any])
        window["costUsd"] = 0.004
        window["unpricedRequests"] = 0
        usage["window"] = window
        cheap["usage"] = usage
        let dana = try decoded(cheap)
        XCTAssertEqual(dana.windowUsageLabel, "210k out")
        XCTAssertEqual(dana.windowUsageSpokenLabel, "this window: 210k output tokens")

        // A Claude row's measured zero is the line it always drew.
        var idleAlice = try canonicalRow("alice@example.com")
        var aliceUsage = try XCTUnwrap(idleAlice["usage"] as? [String: Any])
        aliceUsage.removeValue(forKey: "window")
        aliceUsage["today"] = [
            "requests": 0, "unpricedRequests": 0, "costUsd": 0.0, "inputTokens": 0,
            "outputTokens": 0, "cacheReadTokens": 0, "cacheCreationTokens": 0,
            "cacheCreation1hTokens": 0,
        ]
        idleAlice["usage"] = aliceUsage
        let alice = try decoded(idleAlice)
        XCTAssertEqual(alice.windowUsageLabel, "$0.00 today · 0 out")
        XCTAssertEqual(alice.windowUsageSpokenLabel, "today: $0.00, 0 output tokens")
    }

    // MARK: - The note under a signed-out row

    func testTheNoteLineTextFollowsTheLoginError() throws {
        let cases: [(String, String?, String)] = [
            (
                "refresh_token_reused", "/Users/example/.codex",
                "Signed out: the codex CLI in ~/.codex refreshed this shared login first. "
                    + "Re-login gives tcr its own."
            ),
            (
                "refresh_token_reused", nil,
                "Signed out: another copy of this login refreshed first. Re-login to sign it in "
                    + "again."
            ),
            (
                "refresh_token_expired", nil,
                "Signed out: the login expired. Re-login to sign it in again."
            ),
            (
                "refresh_token_invalidated", nil,
                "Signed out: the login was revoked. Re-login to sign it in again."
            ),
            (
                "invalid_grant", nil,
                "Signed out: ChatGPT rejected the refresh (invalid_grant). Re-login to sign it in "
                    + "again."
            ),
            (
                "http_401", nil,
                "Signed out: ChatGPT answered the refresh with 401. Re-login to sign it in again."
            ),
            (
                "token_gone", nil,
                "Signed out: ChatGPT refused the refresh (token_gone). Re-login to sign it in "
                    + "again."
            ),
        ]
        for (code, codexHome, text) in cases {
            let note = CodexCardFacts.noteLine(
                reloginRefusal: nil, loginError: code, codexHome: codexHome,
                groupAddWarning: nil, home: "/Users/example")
            XCTAssertEqual(note?.text, text, code)
            XCTAssertEqual(note?.tone, .spent, code)
        }

        XCTAssertEqual(HomePath.short("/Users/example/.codex", home: "/Users/example"), "~/.codex")
        XCTAssertEqual(HomePath.short("/Users/example", home: "/Users/example"), "~")
        for unchanged in ["/Users/examples/x", "/Users/example2/.codex", "/opt/codex"] {
            XCTAssertEqual(HomePath.short(unchanged, home: "/Users/example"), unchanged)
        }

        // The pill's help and the note say one sentence, so hover and the line agree.
        XCTAssertEqual(try help(canonical("erin@example.com/codex")), cases[0].2)
    }

    func testTheNoteLinePriority() {
        let reason =
            "this ChatGPT workspace is not served at https://chatgpt.com with no routing of its own"
        let warning =
            "'ivy@example.com' is a Codex account. A Codex session cannot ask for a group, so "
            + "'orchard' labels it and can park it, but never routes to it; while 'orchard' is "
            + "reserved, it serves nothing."
        XCTAssertEqual(
            CodexCardFacts.noteLine(
                reloginRefusal: reason, loginError: "refresh_token_expired",
                codexHome: "/Users/example/.codex", groupAddWarning: warning,
                home: "/Users/example"),
            CodexNoteLine(text: "Re-login refused: " + reason, tone: .spent))
        XCTAssertEqual(
            CodexCardFacts.noteLine(
                reloginRefusal: nil, loginError: "http_401", codexHome: nil,
                groupAddWarning: warning, home: "/Users/example"),
            CodexNoteLine(
                text: "Signed out: ChatGPT answered the refresh with 401. Re-login to sign it in "
                    + "again.",
                tone: .spent))
        XCTAssertEqual(
            CodexCardFacts.noteLine(
                reloginRefusal: nil, loginError: nil, codexHome: nil, groupAddWarning: warning,
                home: "/Users/example"),
            CodexNoteLine(text: warning, tone: .caution))
        XCTAssertNil(
            CodexCardFacts.noteLine(
                reloginRefusal: nil, loginError: nil, codexHome: nil, groupAddWarning: nil,
                home: "/Users/example"))
    }

    /// The note reaches the panel as one more line under the row, in the panel's own
    /// notice grammar: a signed-out reason is red and spoken as written, never as a
    /// failed action, and a group-add warning is a caution.
    func testTheCodexNoteIsARowNotice() throws {
        let signedOut = try XCTUnwrap(
            CodexCardFacts.noteLine(
                reloginRefusal: nil, loginError: "refresh_token_reused",
                codexHome: "/Users/example/.codex", groupAddWarning: nil, home: "/Users/example"))
        XCTAssertEqual(
            AccountRowNotice(codexNote: signedOut),
            AccountRowNotice(
                text: "Signed out: the codex CLI in ~/.codex refreshed this shared login first. "
                    + "Re-login gives tcr its own.",
                tone: .alert))

        let warned = try XCTUnwrap(
            CodexCardFacts.noteLine(
                reloginRefusal: nil, loginError: nil, codexHome: nil,
                groupAddWarning: "'orchard' labels it and can park it", home: "/Users/example"))
        XCTAssertEqual(AccountRowNotice(codexNote: warned).tone, .caution)
    }

    /// A group add's warning is a caution about how the row is set up: no Codex request
    /// reaches the group. It is not a quota state, so it never wears the near-limit amber
    /// a note on a success wears; a Claude row's note keeps it.
    func testAGroupAddWarningIsACaution() throws {
        let warned = try XCTUnwrap(
            CodexCardFacts.noteLine(
                reloginRefusal: nil, loginError: nil, codexHome: nil,
                groupAddWarning: "'orchard' labels it and can park it", home: "/Users/example"))
        XCTAssertEqual(warned.tone, .caution)
        XCTAssertEqual(AccountRowNotice(codexNote: warned).tone, .caution)
        XCTAssertEqual(
            AccountRowNotice.collect(
                deleteFailure: nil, deleteNotice: "keeps listing it until it restarts",
                groupFailure: nil, toggleFailure: nil
            ).map(\.tone), [.note])
    }

    func testTheCodexNoteSitsAfterTheFailuresAndBeforeTheNote() {
        let failure = RemoveAccountCommand.Failure(exitCode: 1, message: "no account matches")
        let notice = "keeps listing it until it restarts"
        let lines = AccountRowNotice.collect(
            deleteFailure: failure, deleteNotice: notice, groupFailure: nil, toggleFailure: nil,
            codexNote: AccountRowNotice(
                text: "Signed out: the login expired. Re-login to sign it in again.",
                tone: .alert))
        XCTAssertEqual(lines.map(\.tone), [.failure, .alert, .note])

        let mains = AccountRowNotice.collect(
            deleteFailure: failure, deleteNotice: notice, groupFailure: nil, toggleFailure: nil)
        XCTAssertEqual(mains.count, 2)
        XCTAssertEqual(
            AccountRowNotice.collect(
                deleteFailure: failure, deleteNotice: notice, groupFailure: nil,
                toggleFailure: nil, codexNote: nil),
            mains)
    }
}

// MARK: - Assertion helpers

private let utc = TimeZone(identifier: "UTC")!

/// `CodexCardFacts.stateHelp` at the fixture's clock, in UTC, under the fake home.
private func help(_ account: Account) -> String? {
    CodexCardFacts.stateHelp(account, now: fixtureNow, timeZone: utc, home: "/Users/example")
}

private func assertTail(
    _ account: Account, _ window: Account.QuotaWindow, _ text: String, _ help: String,
    file: StaticString = #filePath, line: UInt = #line
) throws {
    let tail = try XCTUnwrap(
        CodexCardFacts.tail(account, window: window), "no tail", file: file, line: line)
    XCTAssertEqual(tail.text, text, file: file, line: line)
    XCTAssertEqual(tail.help, help, file: file, line: line)
}

/// Every Codex row in the canonical fixture.
private let codexNames = [
    "bob@example.com/codex", "carol@example.com", "dana@example.com", "erin@example.com/codex",
    "gil@example.com", "hal@example.com", "ivy@example.com", "jo@example.com", "kim@example.com",
]

// MARK: - Fixture helpers

/// 2026-01-01T00:00:00Z: the clock every canonical timestamp is written against.
private let fixtureNow = Date(timeIntervalSince1970: 1_767_225_600)

/// One canonical row from apps/macos/Tests/Fixtures/codex-status.json, with `overrides`
/// written over its JSON object (`NSNull()` writes a JSON null).
private func canonicalRow(_ name: String, _ overrides: [String: Any] = [:]) throws -> [String: Any] {
    let url = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent()  // TcrBarTests
        .deletingLastPathComponent()  // Tests
        .appendingPathComponent("Fixtures/codex-status.json")
    let rows = try XCTUnwrap(
        JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [[String: Any]])
    var row = try XCTUnwrap(rows.first { $0["name"] as? String == name }, "no canonical \(name)")
    for (key, value) in overrides { row[key] = value }
    return row
}

private func decodeFleet(_ rows: [[String: Any]]) throws -> Fleet {
    try Fleet.decode(JSONSerialization.data(withJSONObject: rows))
}

/// One decoded row, failing the test if it does not decode.
private func decoded(_ row: [String: Any]) throws -> Account {
    let fleet = try decodeFleet([row])
    XCTAssertTrue(fleet.unreadable.isEmpty, "\(fleet.unreadable)")
    return try XCTUnwrap(fleet.accounts.first)
}

/// The canonical row, decoded.
private func canonical(_ name: String, _ overrides: [String: Any] = [:]) throws -> Account {
    try decoded(try canonicalRow(name, overrides))
}

/// The 403 hold over canonical bob: held for 90 seconds from `fixtureNow`.
private let forbiddenHold: [String: Any] = [
    "gate": "refused", "freeAtMs": 1_767_225_690_000,
    "rateLimitedUntilMs": 1_767_225_690_000, "secondsUntilFree": 90,
]

/// carol as a server older than the refusing-window gate reports her: a `rejected`
/// gate with the full window still held and no reached type.
private let rejectedFullWindow: [String: Any] = [
    "gate": "rejected", "freeAtMs": NSNull(), "secondsUntilFree": NSNull(),
    "limitReachedType": NSNull(), "credits": NSNull(), "quotaWindows": NSNull(),
    "sevenDayWindowMinutes": NSNull(),
]

/// bob before his first probe: no quota, no reported windows, never probed.
private func unmeasuredBob() throws -> Account {
    try canonical(
        "bob@example.com/codex",
        ["quota": NSNull(), "quotaWindows": NSNull(), "probeStatus": "never"])
}
