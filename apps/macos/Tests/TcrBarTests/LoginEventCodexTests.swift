import XCTest

@testable import TcrBarCore

/// The Codex half of `tcr login --non-interactive`'s events, read from the canonical
/// login-events fixture, and the notices a login keeps while it runs.
final class LoginEventCodexTests: XCTestCase {

    /// Every line of the fixture, in order. The two long sentences are compared with the
    /// fixture's own text rather than typed out again.
    func testEveryCanonicalLoginEventParses() throws {
        let lines = try fixtureLines()
        XCTAssertEqual(lines.count, 5)

        XCTAssertEqual(
            lines.map(LoginProgressEvent.parse),
            [
                .browser(
                    url: "https://auth.openai.com/oauth/authorize?client_id=app_EXAMPLE&state=EXAMPLE",
                    opened: true),
                .waiting,
                .notice(text: try field("text", ofLine: 2)),
                .saved(account: "bob@example.com", provider: "codex", plan: "ChatGPT Pro"),
                .failed(reason: try field("reason", ofLine: 4), code: "codex-identity"),
            ])
        XCTAssertTrue(
            try field("text", ofLine: 2).hasPrefix("tcr intercepts chatgpt.com from now on: "))
        XCTAssertTrue(
            try field("reason", ofLine: 4).hasPrefix(
                "the ChatGPT login that came back is carol@example.com, "))
    }

    /// `opened` is new and optional: without it the caller opens the URL, as it always has.
    func testBrowserOpenedIsOptional() {
        let url = "https://auth.openai.com/oauth/authorize?state=abc"

        let bare = LoginProgressEvent.parse(#"{"event":"browser","url":"\#(url)"}"#)
        XCTAssertEqual(bare, .browser(url: url, opened: false))
        XCTAssertEqual(bare, .browser(url: url), "the same event as today's call shape")
        XCTAssertEqual(
            LoginProgressEvent.parse(#"{"event":"browser","opened":true,"url":"\#(url)"}"#),
            .browser(url: url, opened: true))
        XCTAssertEqual(
            LoginProgressEvent.parse(#"{"event":"browser","opened":false,"url":"\#(url)"}"#),
            .browser(url: url, opened: false))
    }

    /// A notice with no text is ignored, like any event missing the field that carries it.
    func testNoticeParses() {
        XCTAssertEqual(
            LoginProgressEvent.parse(#"{"event":"notice","text":"codex is pooled"}"#),
            .notice(text: "codex is pooled"))
        XCTAssertNil(LoginProgressEvent.parse(#"{"event":"notice"}"#))
        XCTAssertNil(LoginProgressEvent.parse(#"{"event":"notice","text":""}"#))
    }

    /// A Codex save names its provider and plan; Claude's save is unchanged.
    func testSavedCarriesProviderAndPlan() throws {
        let lines = try fixtureLines()
        XCTAssertEqual(
            LoginProgressEvent.parse(try XCTUnwrap(lines.dropFirst(3).first)),
            .saved(account: "bob@example.com", provider: "codex", plan: "ChatGPT Pro"))

        let claude = LoginProgressEvent.parse(#"{"event":"saved","account":"alice@example.com"}"#)
        XCTAssertEqual(claude, .saved(account: "alice@example.com", provider: nil, plan: nil))
        XCTAssertEqual(claude, .saved(account: "alice@example.com"))
    }

    /// The code names the refusal; the reason is still the whole sentence, and still required.
    func testErrorCarriesItsCode() {
        XCTAssertEqual(
            LoginProgressEvent.parse(
                #"{"code":"codex-missing","event":"error","reason":"could not find the codex CLI"}"#),
            .failed(reason: "could not find the codex CLI", code: "codex-missing"))
        XCTAssertEqual(
            LoginProgressEvent.parse(#"{"event":"error","reason":"could not find the codex CLI"}"#),
            .failed(reason: "could not find the codex CLI"))
        XCTAssertNil(LoginProgressEvent.parse(#"{"code":"codex-missing","event":"error"}"#))
    }

    /// A notice is kept for the sheet and never moves the login, let alone ends it.
    func testNoticesAreKeptInOrderAndNeverFailTheFlow() {
        var early = LoginFlow(requesting: nil)
        XCTAssertNil(early.apply(.notice(text: "early")))
        XCTAssertEqual(early.phase, .opening, "a notice before the browser moves nothing")

        let url = "https://auth.openai.com/oauth/authorize?state=abc"
        var flow = LoginFlow(requesting: nil)
        XCTAssertEqual(flow.apply(.browser(url: url))?.absoluteString, url)
        flow.apply(.waiting)
        XCTAssertNil(flow.apply(.notice(text: "the first notice")))
        XCTAssertEqual(flow.phase, .waitingForBrowser(email: nil))
        flow.apply(.notice(text: "the second notice"))
        XCTAssertEqual(flow.notices, ["the first notice", "the second notice"])

        flow.apply(.saved(account: "bob@example.com", provider: "codex", plan: "ChatGPT Pro"))
        XCTAssertEqual(flow.phase, .saved(account: "bob@example.com"))
        XCTAssertEqual(flow.notices, ["the first notice", "the second notice"])

        // tcr sends every notice before `saved` or `error`. One that comes late anyway is still
        // kept, and the result stands.
        flow.apply(.notice(text: "a late notice"))
        XCTAssertEqual(flow.notices.count, 3)
        XCTAssertEqual(flow.notices.last, "a late notice")
        XCTAssertEqual(flow.phase, .saved(account: "bob@example.com"))
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
}
