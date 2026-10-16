import XCTest

@testable import TcrBarCore

/// The one Codex line the Accounts tab may draw under its summary, beside the Claude one.
///
/// Every account is a canonical row of `apps/macos/Tests/Fixtures/codex-status.json`, with the
/// route and the traffic laid over the Codex rows the way the server sends them: one reading,
/// copied onto every Codex row. Every sentence is pinned word for word.
final class CodexBannerTests: XCTestCase {
    private let bobAndDana = ["bob@example.com/codex", "dana@example.com"]

    /// Nothing refused and nothing sent around the pool in the last ten minutes.
    private let quiet: [String: Any] = ["refused10m": 0, "unpooled10m": 0]

    /// A route the proxy cannot pool, as `tcr status --json` reports it.
    private let notPooled: [String: Any] = [
        "pooled": false, "code": "mitm-off",
        "reason":
            "the proxy on :3456 is running without MITM: its TLS material failed to load at boot — see its log",
    ]

    /// Twelve refusals, the last for a usage limit, and three requests sent around the pool.
    private let refusing: [String: Any] = [
        "refused10m": 12, "lastRefusal": "usage_limit_reached", "unpooled10m": 3,
    ]

    private let tb43Plural =
        "2 codex processes are running and no Codex request has reached tcr in 5 minutes — start codex with `tcr run --codex`."

    /// The banner for `fleet`, with what the panel measures beside it.
    private func call(
        _ fleet: Fleet, processes: Int? = nil, quietFor: TimeInterval? = nil, sharing: Bool = false,
        bundled: String? = nil
    ) -> CodexBanner? {
        CodexBanner.current(
            fleet: fleet, codexProcesses: processes, codexQuietFor: quietFor, peerSharing: sharing,
            bundledTcrWithoutCLI: bundled)
    }

    func testCredentialRefusalNamesTheLoginOrWorkspaceRemedy() throws {
        for count in [1, 12] {
            let fleet = try codexFleet(
                bobAndDana,
                traffic: ["refused10m": count, "lastRefusal": "credential_refused", "unpooled10m": 0])
            let banner = try XCTUnwrap(call(fleet))
            XCTAssertTrue(banner.text.contains("Check the workspace or sign in again."))
            XCTAssertFalse(banner.text.contains("retry"))
        }
    }

    func testModelRefusalNamesTheModelChoiceRemedy() throws {
        for count in [1, 12] {
            let fleet = try codexFleet(
                bobAndDana,
                traffic: ["refused10m": count, "lastRefusal": "model_unavailable", "unpooled10m": 0])
            let banner = try XCTUnwrap(call(fleet))
            XCTAssertTrue(banner.text.contains("Choose another model"))
            XCTAssertFalse(banner.text.contains("asked codex to retry"))
            XCTAssertFalse(banner.text.contains("sign in again"))
        }
    }

    // MARK: - Precedence

    /// One line at a time, in the order an operator can act on: a route that is not pooled
    /// explains everything below it, then refusals, then requests sent around the pool, then
    /// codex running with nothing reaching tcr.
    func testOneBannerAtATimeInPrecedence() throws {
        let notPooledBanner = try XCTUnwrap(
            call(try codexFleet(bobAndDana, route: notPooled, traffic: refusing)))
        XCTAssertEqual(
            notPooledBanner.text,
            "Codex is not pooled: the proxy on :3456 is running without MITM: its TLS material failed to load at boot — see its log"
        )
        XCTAssertEqual(notPooledBanner.tint, .near)

        let refused = try XCTUnwrap(call(try codexFleet(bobAndDana, traffic: refusing)))
        XCTAssertEqual(
            refused.text,
            "tcr refused 12 Codex requests in the last 10 min; the last one told codex its usage limit is reached."
        )
        XCTAssertEqual(refused.tint, .near)

        let unpooled = try XCTUnwrap(
            call(try codexFleet(bobAndDana, traffic: ["refused10m": 0, "unpooled10m": 3])))
        XCTAssertEqual(
            unpooled.text,
            "3 Codex requests in the last 10 min ran on their own login: their ChatGPT workspace is FedRAMP or residency-routed, which tcr does not pool."
        )
        XCTAssertEqual(unpooled.tint, .dim)

        let idle = try codexFleet(bobAndDana, traffic: quiet, extra: ["requests": 0])
        let noRequests = try XCTUnwrap(call(idle, processes: 2, quietFor: 600))
        XCTAssertEqual(noRequests.text, tb43Plural)
        XCTAssertEqual(noRequests.tint, .near)

        XCTAssertNil(call(try codexFleet(bobAndDana, traffic: quiet)))
    }

    /// A Codex line needs a live read and a Codex account that is not switched off or parked:
    /// an offline proxy has refused nothing, and a pool with nothing enabled is not in use.
    func testABannerNeedsALiveReadAndAnEnabledCodexAccount() throws {
        XCTAssertEqual(
            call(try canonicalFleet()), .refused(count: 12, last: .usageLimit, peerSharing: false))

        let offline = try decodeFleet(
            try canonicalFleet().accounts.map { try canonicalRow($0.name, ["source": "offline"]) })
        XCTAssertNil(call(offline))

        let disabled = try decodeFleet([
            try canonicalRow("alice@example.com"),
            try canonicalRow("bob@example.com/codex", ["disabled": true]),
            try canonicalRow("carol@example.com", ["disabled": true]),
        ])
        XCTAssertNil(call(disabled))

        let groupParked: [String: Any] = [
            "groups": ["night"], "parkedGroups": ["night"], "gate": "parked", "freeAtMs": NSNull(),
        ]
        let parked = try decodeFleet([
            try canonicalRow("alice@example.com"),
            try canonicalRow("bob@example.com/codex", groupParked),
            try canonicalRow("carol@example.com", groupParked),
        ])
        XCTAssertNil(call(parked))

        XCTAssertNil(call(try decodeFleet([try canonicalRow("alice@example.com")])))
        XCTAssertNil(call(Fleet(accounts: [])))
    }

    /// The last refusal's type says what codex was told; a type this build does not know, or
    /// none at all, reads as the pool having nothing to serve it with.
    func testAUsageLimitRefusalAndAnyOtherRefusalReadApart() throws {
        let retry =
            "tcr refused 12 Codex requests in the last 10 min; the last one asked codex to retry: no Codex account could serve it."
        XCTAssertEqual(
            call(
                try codexFleet(
                    bobAndDana,
                    traffic: ["refused10m": 12, "lastRefusal": "pool_unavailable", "unpooled10m": 0])
            )?.text,
            retry)
        XCTAssertEqual(
            call(try codexFleet(bobAndDana, traffic: ["refused10m": 12, "unpooled10m": 0]))?.text,
            retry)
        XCTAssertEqual(
            call(
                try codexFleet(
                    bobAndDana,
                    traffic: ["refused10m": 12, "lastRefusal": "no_codex_account", "unpooled10m": 0])
            )?.text,
            "tcr refused 12 Codex requests in the last 10 min; the last one found no usable Codex account in the pool."
        )
    }

    /// Each sentence has its own form for one.
    func testTheSingularForms() throws {
        func refusedOnce(_ last: String?) throws -> String? {
            var traffic: [String: Any] = ["refused10m": 1, "unpooled10m": 0]
            traffic["lastRefusal"] = last
            return call(try codexFleet(bobAndDana, traffic: traffic))?.text
        }
        XCTAssertEqual(
            try refusedOnce("usage_limit_reached"),
            "tcr refused 1 Codex request in the last 10 min and told codex its usage limit is reached.")
        XCTAssertEqual(
            try refusedOnce("no_codex_account"),
            "tcr refused 1 Codex request in the last 10 min: no usable Codex account was in the pool.")
        XCTAssertEqual(
            try refusedOnce("pool_unavailable"),
            "tcr refused 1 Codex request in the last 10 min and asked codex to retry: no Codex account could serve it."
        )
        XCTAssertEqual(
            call(try codexFleet(bobAndDana, traffic: ["refused10m": 0, "unpooled10m": 1]))?.text,
            "1 Codex request in the last 10 min ran on its own login: its ChatGPT workspace is FedRAMP or residency-routed, which tcr does not pool."
        )
        XCTAssertEqual(
            call(
                try codexFleet(bobAndDana, traffic: quiet, extra: ["requests": 0]), processes: 1,
                quietFor: 600)?.text,
            "1 codex process is running and no Codex request has reached tcr in 5 minutes — start codex with `tcr run --codex`."
        )
    }

    /// While this Mac shares its accounts with trusted Macs, a refusal says that those Macs
    /// cannot take Codex requests either. No other line says it.
    func testTheTrustedMacsTailWhilePeerSharingIsOn() throws {
        XCTAssertEqual(
            call(try canonicalFleet(), sharing: true)?.text,
            "tcr refused 12 Codex requests in the last 10 min; the last one told codex its usage limit is reached. Trusted Macs cannot serve Codex requests."
        )
        XCTAssertEqual(
            call(try canonicalFleet(), sharing: false)?.text,
            "tcr refused 12 Codex requests in the last 10 min; the last one told codex its usage limit is reached."
        )

        let unpooled = call(
            try codexFleet(bobAndDana, traffic: ["refused10m": 0, "unpooled10m": 3]), sharing: true)
        let idle = call(
            try codexFleet(bobAndDana, traffic: quiet, extra: ["requests": 0]), processes: 2,
            quietFor: 600, sharing: true)
        for text in [unpooled?.text, idle?.text] {
            XCTAssertNotNil(text, "positive control: the line shows")
            XCTAssertFalse(
                text?.contains("Trusted Macs") ?? true, "\(text ?? "nil") took the sharing tail")
        }
    }

    /// With no command-line tcr on this Mac, `tcr run --codex` is a command that fails; the
    /// line then names the tcr inside TcrBar, quoted for the shell, and only this line does.
    func testTheNoRequestsLineNamesTheBundledTcrWhenNoOtherIsFound() throws {
        let idle = try codexFleet(bobAndDana, traffic: quiet, extra: ["requests": 0])
        let bundled = "/Applications/TcrBar.app/Contents/MacOS/tcr"
        let tail =
            " TcrBar found no tcr besides its own: install the command-line tcr (README › Install), or start codex with `/Applications/TcrBar.app/Contents/MacOS/tcr run --codex`, the tcr inside TcrBar, which keeps TcrBar from updating until that codex exits."
        XCTAssertEqual(CodexBanner.noCLITail(bundledTcr: bundled), tail)
        XCTAssertEqual(
            call(idle, processes: 2, quietFor: 600, bundled: bundled)?.text, tb43Plural + tail)

        let spaced = call(
            idle, processes: 2, quietFor: 600,
            bundled: "/Applications/My Apps/TcrBar.app/Contents/MacOS/tcr")?.text
        XCTAssertTrue(
            spaced?.contains("`'/Applications/My Apps/TcrBar.app/Contents/MacOS/tcr' run --codex`")
                ?? false,
            "\(spaced ?? "nil") does not quote a path holding a space")

        XCTAssertEqual(call(idle, processes: 2, quietFor: 600)?.text, tb43Plural)

        let others = [
            call(try codexFleet(bobAndDana, route: notPooled, traffic: refusing), bundled: bundled),
            call(try codexFleet(bobAndDana, traffic: refusing), bundled: bundled),
            call(
                try codexFleet(bobAndDana, traffic: ["refused10m": 0, "unpooled10m": 3]),
                bundled: bundled),
        ]
        for banner in others {
            XCTAssertNotNil(banner, "positive control: the line shows")
            XCTAssertFalse(
                banner?.text.contains("TcrBar found no tcr") ?? true,
                "\(banner?.text ?? "nil") took the no-command-line tail")
        }
    }

    /// The no-requests line needs every one of its inputs, measured: a process count above
    /// zero, five quiet minutes, a pooled route and no request sent around the pool. A
    /// reading that is missing draws nothing, never a false line.
    func testNoRequestsIsSuppressedWhileRequestsRunUnpooled() throws {
        let unpooledIdle = try codexFleet(
            bobAndDana, traffic: ["refused10m": 0, "unpooled10m": 2], extra: ["requests": 0])
        XCTAssertEqual(call(unpooledIdle, processes: 2, quietFor: 600), .unpooled(count: 2))

        let idle = try codexFleet(bobAndDana, traffic: quiet, extra: ["requests": 0])
        XCTAssertNotNil(call(idle, processes: 2, quietFor: 600), "positive control: every input holds")
        XCTAssertNil(call(idle, processes: 0, quietFor: 600))
        XCTAssertNil(call(idle, processes: nil, quietFor: 600))
        XCTAssertNil(call(idle, processes: 2, quietFor: 299))
        XCTAssertNil(call(idle, processes: 2, quietFor: nil))
        XCTAssertNil(
            call(try codexFleet(bobAndDana, traffic: quiet), processes: 2, quietFor: 600),
            "bob and dana served requests, whatever the quiet clock last read")

        let routeDown = try codexFleet(
            bobAndDana, route: notPooled, traffic: quiet, extra: ["requests": 0])
        XCTAssertEqual(
            call(routeDown, processes: 2, quietFor: 600)?.text,
            "Codex is not pooled: the proxy on :3456 is running without MITM: its TLS material failed to load at boot — see its log"
        )

        let unread = try decodeFleet(
            bobAndDana.map {
                try canonicalRow(
                    $0, ["codexRoute": NSNull(), "codexTraffic": NSNull(), "requests": 0])
            })
        XCTAssertNil(call(unread, processes: 2, quietFor: 600))
    }

    /// The quiet clock runs on the requests of the Codex accounts that can serve, on a live
    /// read only.
    func testTheQuietClockCountsEnabledCodexRowsOnly() throws {
        XCTAssertEqual(
            CodexBanner.codexRequests(
                try decodeFleet([
                    try canonicalRow("bob@example.com/codex", ["requests": 0]),
                    try canonicalRow("dana@example.com", ["requests": 0]),
                    try canonicalRow("alice@example.com"),
                ])),
            0)
        XCTAssertEqual(
            CodexBanner.codexRequests(
                try decodeFleet([
                    try canonicalRow("bob@example.com/codex"),
                    try canonicalRow("carol@example.com", ["disabled": true]),
                ])),
            42, "carol's 310, switched off, were counted")
        XCTAssertNil(
            CodexBanner.codexRequests(
                try decodeFleet(bobAndDana.map { try canonicalRow($0, ["source": "offline"]) })))
        XCTAssertNil(CodexBanner.codexRequests(try decodeFleet([try canonicalRow("alice@example.com")])))
    }

    /// Each provider has its own line: the Claude one counts Claude rows, the Codex one Codex
    /// rows, and a Claude-only fleet keeps the Claude banner it always drew.
    func testTheClaudeBannerIsIndependent() throws {
        let both = try codexFleet(
            ["alice@example.com"] + bobAndDana, traffic: quiet, extra: ["requests": 0])
        XCTAssertEqual(NoRequestsBanner.totalRequests(both), 0)
        XCTAssertEqual(
            call(both, processes: 1, quietFor: 600)?.text,
            "1 codex process is running and no Codex request has reached tcr in 5 minutes — start codex with `tcr run --codex`."
        )

        let codexOnly = try codexFleet(bobAndDana, traffic: quiet, extra: ["requests": 0])
        XCTAssertNil(NoRequestsBanner.totalRequests(codexOnly), "no Claude account to nag about")
        XCTAssertNotNil(call(codexOnly, processes: 1, quietFor: 600))

        let claudeOnly = try decodeFleet([
            try canonicalRow("alice@example.com", ["disabled": true, "requests": 0])
        ])
        XCTAssertEqual(NoRequestsBanner.totalRequests(claudeOnly), 0)
        XCTAssertNil(call(claudeOnly, processes: 1, quietFor: 600))
    }

    // MARK: - The panel

    /// The panel draws the Codex line directly under the Claude one, above the tab bar and
    /// never in a tab's own content, and feeds it, the Claude line's guard and the empty
    /// fleet's hint from what it measured. The test target links TcrBarCore alone, so the
    /// panel is read as text: comment lines dropped, and all whitespace removed from the code
    /// and from each anchor. The guard in `noRequestsBannerText` has no other test.
    func testThePanelDrawsTheCodexBannerUnderTheClaudeOne() throws {
        let code = try fleetViewCode()

        let summary = try slice(
            code, from: "private func v4Summary(_ tab: PanelTab) -> some View {", to: "case .sessions:")
        let claude = try XCTUnwrap(
            summary.range(of: squashed("noRequestsBanner(fleet)")),
            "v4Summary no longer draws noRequestsBanner(fleet) — update this test's anchors")
        let codex = try XCTUnwrap(
            summary.range(of: squashed("codexBanner(fleet)")),
            "v4Summary does not draw codexBanner(fleet) — update this test's anchors")
        XCTAssertLessThan(
            claude.lowerBound, codex.lowerBound, "the Codex line is drawn above the Claude one")

        let content = try slice(
            code, from: "private func v4Content(_ tab: PanelTab) -> some View {",
            to: "private func refreshLentTo()")
        XCTAssertFalse(
            content.contains(squashed("codexBanner(")),
            "a Codex line is drawn inside the tab's content, under the tab bar")

        for anchor in [
            "CodexBanner.current(",
            "CodexProcesses.readLive()",
            "CodexBanner.codexRequests(",
            "fleet.holdsCodex else",
            "!fleet.holdsCodex || NoRequestsBanner.totalRequests(fleet) != nil",
            "namesClaude: fleet.holdsCodex",
            "bundledTcrWithoutCLI:",
            "codexPresence.emptyFleetHint(home: homeDirectory)",
        ] {
            XCTAssertTrue(
                code.contains(squashed(anchor)),
                "FleetView.swift no longer contains \(anchor) — update this test's anchors")
        }
    }

    /// FleetView.swift's code: comment lines dropped, then every whitespace character removed.
    private func fleetViewCode() throws -> String {
        let file = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // this file -> TcrBarTests
            .deletingLastPathComponent()  // TcrBarTests -> Tests
            .deletingLastPathComponent()  // Tests -> apps/macos
            .appendingPathComponent("Sources/TcrBar/FleetView.swift")
        let source = try XCTUnwrap(
            try? String(contentsOf: file, encoding: .utf8),
            "FleetView.swift has moved — update this test's path")
        let code = source.split(separator: "\n", omittingEmptySubsequences: false)
            .filter { !$0.trimmingCharacters(in: .whitespaces).hasPrefix("//") }
            .joined(separator: "\n")
        return squashed(code)
    }

    /// The code from `start` to the first `end` after it.
    private func slice(_ code: String, from start: String, to end: String) throws -> Substring {
        let open = try XCTUnwrap(
            code.range(of: squashed(start)), "FleetView.swift has no \(start) — update this test's anchors")
        let close = try XCTUnwrap(
            code.range(of: squashed(end), range: open.upperBound..<code.endIndex),
            "FleetView.swift has no \(end) after \(start) — update this test's anchors")
        return code[open.upperBound..<close.lowerBound]
    }

    private func squashed(_ text: String) -> String {
        text.components(separatedBy: .whitespacesAndNewlines).joined()
    }
}

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

/// All ten canonical rows: alice (Claude, in orchard) and nine Codex rows.
private func canonicalFleet() throws -> Fleet {
    try decodeFleet(
        [
            "alice@example.com", "bob@example.com/codex", "carol@example.com", "dana@example.com",
            "erin@example.com/codex", "gil@example.com", "hal@example.com", "ivy@example.com",
            "jo@example.com", "kim@example.com",
        ].map { try canonicalRow($0) })
}

/// Canonical rows with the same route and traffic laid on every Codex row, as the server sends them.
private func codexFleet(
    _ names: [String], route: [String: Any] = ["pooled": true],
    traffic: [String: Any], extra: [String: Any] = [:]
) throws -> Fleet {
    try decodeFleet(
        names.map { name in
            var overrides = extra
            if name != "alice@example.com" {
                overrides["codexRoute"] = route
                overrides["codexTraffic"] = traffic
            }
            return try canonicalRow(name, overrides)
        })
}
