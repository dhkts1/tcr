import AppKit
import TcrBarCore

/// Rasterise the menu-bar mark itself — the composed `NSImage`
/// ``MenuBarMark/image(fraction:tint:)`` builds, plus the opt-in
/// `ready/enabled` label (F5) beside it
/// when a scene turns it on — to PNG, in-process, then exit.
///
/// ## Why this exists, separate from `--render-states`
///
/// `RenderStates` draws `FleetView` with `ImageRenderer`, which never touches the
/// real `NSStatusItem` — the mark is composed straight onto `button.image`/
/// `button.attributedTitle` (`MenuBarShell.updateMark`), a path `ImageRenderer`
/// cannot reach because it is not a SwiftUI view at all. `MenuBarMark.image` IS
/// rasterisable headless, though: it is a plain `NSImage(size:flipped:drawingHandler:)`,
/// the same shape `AppIcon.image(size:)` already rasterises for `--render-icon`. So
/// this composes gauge + label the same way `updateMark` does, onto a canvas sized
/// like a real `NSStatusBarButton`, instead of adding a `--render-mark` scene to
/// `RenderStates` — a file this feature's own bridge keeps out of, matching the
/// concurrent lane already working there.
///
/// ## Usage
///
///     TcrBar.app/Contents/MacOS/TcrBar --render-mark <output-directory>
///
/// Writes one PNG per scene and exits without ever showing a menu-bar item,
/// polling `tcr`, or touching a server.
enum RenderMark {
    static let flag = "--render-mark"

    static func requestedDirectory(_ arguments: [String] = CommandLine.arguments) -> URL? {
        guard let i = arguments.firstIndex(of: flag), i + 1 < arguments.count else { return nil }
        return URL(fileURLWithPath: arguments[i + 1])
    }

    /// The coffee-cup mark's own state set (`docs/design/menubar-mark-mockup.html`
    /// § "Coffee-mark rule"), one scene per named point in the acceptance gate —
    /// "0%, 40%, 100%, near, failed, off" — each carrying keep-awake ON except
    /// the one scene named `off`, plus a light-appearance repeat of the 100%
    /// scene the same way the predecessor mockup repeated one state under the
    /// other appearance rather than a different fleet. `appearance` is `nil`
    /// for the process default (dark) and `.aqua` only for the light scene.
    /// `knocks` is how many Macs are waiting on an answer. Its own scenes
    /// rather than a flag on an existing one: the segment is drawn whether or
    /// not the counts label is on, so it takes a scene with counts OFF (the
    /// default Mac) and one with them on beside a second asking Mac, and the
    /// glyph choice at 13 pt cannot be judged in prose.
    private static var scenes:
        [
            (
                name: String, state: PollState, awake: Bool, showCounts: Bool,
                showRunningTools: Bool, appearance: NSAppearance.Name?, knocks: Int
            )
        ]
    {
        [
            ("01-dark-awake-0pct", .loaded(noneReadyFleet), true, false, false, nil, 0),
            ("02-dark-awake-40pct", .loaded(partialReadyFleet), true, false, false, nil, 0),
            ("03-dark-awake-100pct", .loaded(fullReadyFleet), true, false, false, nil, 0),
            ("04-dark-near", .loaded(nearTheLimitFleet), true, false, false, nil, 0),
            (
                "05-dark-failed", .commandFailed(exitCode: 1, message: "connection refused"),
                true, false, false, nil, 0
            ),
            ("06-dark-off-template", .loaded(partialReadyFleet), false, false, false, nil, 0),
            ("07-dark-counts-on", .loaded(mixedFleet), true, true, false, nil, 0),
            ("08-dark-running-tools-count", .loaded(runningToolsFleet), true, true, true, nil, 0),
            ("09-light-awake-100pct", .loaded(fullReadyFleet), true, false, false, .aqua, 0),
            // One Mac asking, counts OFF, which is the default Mac: the glyph
            // and the cup, nothing else.
            ("10-dark-knock-one", .loaded(partialReadyFleet), true, false, false, nil, 1),
            // Two asking, counts and running tools on: the fullest the item
            // ever gets, and the order it is fixed in: what wants an answer,
            // then what the fleet is doing.
            ("11-dark-knock-two-counts-on", .loaded(runningToolsFleet), true, true, true, nil, 2),
            ("12-light-knock-one", .loaded(partialReadyFleet), true, false, false, .aqua, 1),
            ("13-light-knock-two-counts-on", .loaded(mixedFleet), true, true, false, .aqua, 2),
        ]
    }

    /// The cup and the count over a fleet holding Codex accounts, rendered
    /// after ``scenes``, each with the counts on and keep-awake off so the
    /// fill, the tint and the two-part count are all a scene shows.
    /// `cupPool` is "Icon shows": the cup follows the emptier pool unless it is
    /// pinned, and the count shows both pools, Claude first, whatever the pin.
    private static var poolScenes:
        [
            (
                scene: (
                    name: String, state: PollState, awake: Bool, showCounts: Bool,
                    showRunningTools: Bool, appearance: NSAppearance.Name?, knocks: Int
                ),
                cupPool: MenuBarCupPool
            )
        ]
    {
        [
            // Claude 3 of 4, Codex 2 of 2: the cup follows Claude, three
            // quarters full.
            (("14-dark-mixed-counts", .loaded(poolsReadyFleet), false, true, false, nil, 0), .emptier),
            // Codex has nothing ready and nothing near: an empty cup, not amber.
            (("15-dark-mixed-codex-out", .loaded(codexOutFleet), false, true, false, nil, 0), .emptier),
            // Codex has nothing ready and one near: the cup is amber, and so is
            // Codex's half of the count alone.
            (("16-dark-mixed-codex-near", .loaded(codexNearFleet), false, true, false, nil, 0), .emptier),
            // One provider: one count, and the cup half full.
            (("17-dark-codex-only", .loaded(codexOnlyFleet), false, true, false, nil, 0), .emptier),
            // Scene 15's fleet with the cup pinned to Claude.
            (
                ("18-dark-mixed-pinned-claude", .loaded(codexOutFleet), false, true, false, nil, 0),
                .claude
            ),
            // Scene 15's fleet at its fullest: a Mac asking, both counts, and
            // three running tools.
            (
                ("19-dark-mixed-running-knock", .loaded(codexOutRunningFleet), false, true, true, nil, 1),
                .emptier
            ),
            (("20-light-mixed-counts", .loaded(poolsReadyFleet), false, true, false, .aqua, 0), .emptier),
        ]
    }

    @MainActor
    static func run(into directory: URL) -> Never {
        do {
            try FileManager.default.createDirectory(
                at: directory, withIntermediateDirectories: true)
        } catch {
            FileHandle.standardError.write(
                Data("cannot create \(directory.path): \(error)\n".utf8))
            exit(1)
        }

        let total = scenes.count + poolScenes.count
        var written = 0
        for scene in scenes {
            if render(scene, into: directory) { written += 1 }
        }
        for (scene, cupPool) in poolScenes {
            if render(scene, cupPool: cupPool, into: directory) { written += 1 }
        }
        print("\nrendered \(written)/\(total) images into \(directory.path)")
        exit(written == total ? 0 : 1)
    }

    /// The width `NSFont.menuBarFont(ofSize: 0)`'s tallest glyph plus the label
    /// text needs, at 2x — wide enough that a two-digit `"12/13"` plus the
    /// running-tools segment is never clipped, generous rather than measured,
    /// since this is a review artifact and not a layout contract.
    private static let canvasSize = NSSize(width: 220, height: 44)

    @MainActor
    private static func render(
        _ scene: (
            name: String, state: PollState, awake: Bool, showCounts: Bool,
            showRunningTools: Bool, appearance: NSAppearance.Name?, knocks: Int
        ),
        cupPool: MenuBarCupPool = .emptier,
        into directory: URL
    ) -> Bool {
        let tint = MenuBarShell.cupTint(for: scene.state, awake: scene.awake, cupPool: cupPool)
        let mark = MenuBarMark.image(
            fraction: scene.state.capacityFraction(cupPool: cupPool), tint: tint,
            knocks: scene.knocks, fleet: scene.state.loadedFleet)

        let label = scene.showCounts ? scene.state.countsLabel : nil
        let running = scene.state.runningToolsCount(showRunningTools: scene.showRunningTools)
        // The TITLE the live item draws, through the same builder
        // `updateMark` calls, so the knock segment cannot be drawn one way in
        // a fixture and another way on the bar. The knock glyph is LEFT of the
        // cup on the real status item, which composes image then title; this
        // canvas draws the cup first for the same reason it always has, so
        // read the two as one item rather than as a pixel-exact placement.
        let title = MenuBarShell.markTitle(
            state: scene.state, showCounts: scene.showCounts,
            showRunningTools: scene.showRunningTools, knocks: scene.knocks)

        let draw: (NSRect) -> Bool = { rect in
            NSColor.windowBackgroundColor.setFill()
            rect.fill()
            let markRect = NSRect(
                x: 8, y: (rect.height - mark.size.height) / 2,
                width: mark.size.width, height: mark.size.height)
            mark.draw(in: markRect, from: .zero, operation: .sourceOver, fraction: 1)
            if title.length > 0 {
                let titleOrigin = NSPoint(
                    x: markRect.maxX + 4, y: (rect.height - title.size().height) / 2)
                title.draw(at: titleOrigin)
            }
            return true
        }

        // Rasterised (`tiffRepresentation`, which is what actually invokes
        // `draw`) INSIDE `performAsCurrentDrawingAppearance` where an
        // appearance override applies, same as `MenuBarMarkTests.rasterise`.
        // `NSImage(size:flipped:drawingHandler:)`'s handler is lazy — it does
        // not run at construction, so building the image inside the block and
        // rasterising it outside would resolve every dynamic colour
        // (`NSColor.labelColor`, `Tok.nearNSColor`, `.windowBackgroundColor`)
        // against whatever appearance is current by the time something
        // outside this function first asks for pixels, not the one this scene
        // asked for.
        func rasterise() -> Data? {
            NSImage(size: canvasSize, flipped: false, drawingHandler: draw).tiffRepresentation
        }
        let tiffData: Data?
        if let appearanceName = scene.appearance, let appearance = NSAppearance(named: appearanceName)
        {
            var result: Data?
            appearance.performAsCurrentDrawingAppearance {
                result = rasterise()
            }
            tiffData = result
        } else {
            tiffData = rasterise()
        }

        let name = "\(scene.name).png"
        guard let tiff = tiffData,
            let rep = NSBitmapImageRep(data: tiff),
            let png = rep.representation(using: .png, properties: [:])
        else {
            FileHandle.standardError.write(Data("render failed: \(name)\n".utf8))
            return false
        }

        let url = directory.appendingPathComponent(name)
        do {
            try png.write(to: url)
            let runningNote = running.map { " running=\($0)" } ?? ""
            print(
                "  \(name)  label=\(label ?? "(hidden)")\(runningNote) knocks=\(scene.knocks)")
            return true
        } catch {
            FileHandle.standardError.write(Data("write failed \(name): \(error)\n".utf8))
            return false
        }
    }

    /// Obviously-fake names only — this repository is public.
    private static let mixedFleetJSON = """
        [
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "alice@example.com", "priority": 1, "status": "active",
            "disabled": false, "quota": 0.1, "quotaState": "ok",
            "fiveHour": 0.1, "sevenDay": 0.1, "sevenDayOi": 0.0,
            "held": [], "requests": 10, "inputTokens": 100, "outputTokens": 10,
            "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          },
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "bob@example.com", "priority": 2, "status": "active",
            "disabled": false, "quota": 0.2, "quotaState": "ok",
            "fiveHour": 0.2, "sevenDay": 0.2, "sevenDayOi": 0.0,
            "held": [], "requests": 20, "inputTokens": 200, "outputTokens": 20,
            "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          },
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "carol@example.com", "priority": 3, "status": "held",
            "disabled": false, "quota": 0.85, "quotaState": "near",
            "fiveHour": 0.3, "sevenDay": 0.85, "sevenDayOi": 0.1,
            "held": [{"window": "5h", "minutesUntilReset": 93, "resetAtMs": 999000000000}],
            "requests": 5, "inputTokens": 50, "outputTokens": 5,
            "cacheReadTokens": 0, "cacheHitRatio": 0.4, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          },
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "dave@example.com", "priority": 4, "status": "active",
            "disabled": false, "quota": null, "quotaState": "ok",
            "fiveHour": null, "sevenDay": null, "sevenDayOi": null,
            "held": [], "requests": 0, "inputTokens": 0, "outputTokens": 0,
            "cacheReadTokens": 0, "cacheHitRatio": null, "probeStatus": "never",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          },
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "erin@example.com", "priority": 5, "status": "active",
            "disabled": true, "quota": 0.1, "quotaState": "ok",
            "fiveHour": 0.1, "sevenDay": 0.1, "sevenDayOi": 0.0,
            "held": [], "requests": 0, "inputTokens": 0, "outputTokens": 0,
            "cacheReadTokens": 0, "cacheHitRatio": null, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          }
        ]
        """

    private static var mixedFleet: Fleet {
        (try? Fleet.decode(Data(mixedFleetJSON.utf8))) ?? Fleet(accounts: [])
    }

    /// Two enabled accounts, neither ready and neither near — `0/2`, the cup's
    /// empty scene. Distinct from the near-the-limit fixture below: this one
    /// must NOT trip `capacityGlyphState == .near`, so the cup draws its
    /// default tint (template/awake) at fraction 0, not amber.
    private static let noneReadyJSON = """
        [
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "henry@example.com", "priority": 1, "status": "active",
            "disabled": false, "quota": 1.0, "quotaState": "spent",
            "fiveHour": 1.0, "sevenDay": 1.0, "sevenDayOi": 0.0,
            "held": [{"window": "5h", "minutesUntilReset": 200, "resetAtMs": 999000000000}],
            "requests": 9, "inputTokens": 90, "outputTokens": 9,
            "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          },
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "iris@example.com", "priority": 2, "status": "active",
            "disabled": false, "quota": 1.0, "quotaState": "spent",
            "fiveHour": 1.0, "sevenDay": 1.0, "sevenDayOi": 0.0,
            "held": [{"window": "5h", "minutesUntilReset": 150, "resetAtMs": 999000000000}],
            "requests": 4, "inputTokens": 40, "outputTokens": 4,
            "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          }
        ]
        """

    private static var noneReadyFleet: Fleet {
        (try? Fleet.decode(Data(noneReadyJSON.utf8))) ?? Fleet(accounts: [])
    }

    /// Five enabled accounts, two ready — `2/5`, `0.4`, the cup's 40% scene.
    private static let partialReadyJSON = """
        [
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "jack@example.com", "priority": 1, "status": "active",
            "disabled": false, "quota": 0.1, "quotaState": "ok",
            "fiveHour": 0.1, "sevenDay": 0.1, "sevenDayOi": 0.0,
            "held": [], "requests": 6, "inputTokens": 60, "outputTokens": 6,
            "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          },
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "kim@example.com", "priority": 2, "status": "active",
            "disabled": false, "quota": 0.2, "quotaState": "ok",
            "fiveHour": 0.2, "sevenDay": 0.2, "sevenDayOi": 0.0,
            "held": [], "requests": 3, "inputTokens": 30, "outputTokens": 3,
            "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          },
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "liam@example.com", "priority": 3, "status": "active",
            "disabled": false, "quota": 1.0, "quotaState": "spent",
            "fiveHour": 1.0, "sevenDay": 1.0, "sevenDayOi": 0.0,
            "held": [{"window": "5h", "minutesUntilReset": 100, "resetAtMs": 999000000000}],
            "requests": 2, "inputTokens": 20, "outputTokens": 2,
            "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          },
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "maya@example.com", "priority": 4, "status": "active",
            "disabled": false, "quota": 1.0, "quotaState": "spent",
            "fiveHour": 1.0, "sevenDay": 1.0, "sevenDayOi": 0.0,
            "held": [{"window": "5h", "minutesUntilReset": 110, "resetAtMs": 999000000000}],
            "requests": 1, "inputTokens": 10, "outputTokens": 1,
            "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          },
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "noah@example.com", "priority": 5, "status": "active",
            "disabled": false, "quota": 1.0, "quotaState": "spent",
            "fiveHour": 1.0, "sevenDay": 1.0, "sevenDayOi": 0.0,
            "held": [{"window": "5h", "minutesUntilReset": 120, "resetAtMs": 999000000000}],
            "requests": 1, "inputTokens": 10, "outputTokens": 1,
            "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          }
        ]
        """

    private static var partialReadyFleet: Fleet {
        (try? Fleet.decode(Data(partialReadyJSON.utf8))) ?? Fleet(accounts: [])
    }

    /// Two enabled accounts, both ready — `2/2`, `1.0`, the cup's full scene.
    private static let fullReadyJSON = """
        [
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "olive@example.com", "priority": 1, "status": "active",
            "disabled": false, "quota": 0.05, "quotaState": "ok",
            "fiveHour": 0.05, "sevenDay": 0.05, "sevenDayOi": 0.0,
            "held": [], "requests": 2, "inputTokens": 20, "outputTokens": 2,
            "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          },
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "pat@example.com", "priority": 2, "status": "active",
            "disabled": false, "quota": 0.05, "quotaState": "ok",
            "fiveHour": 0.05, "sevenDay": 0.05, "sevenDayOi": 0.0,
            "held": [], "requests": 1, "inputTokens": 10, "outputTokens": 1,
            "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          }
        ]
        """

    private static var fullReadyFleet: Fleet {
        (try? Fleet.decode(Data(fullReadyJSON.utf8))) ?? Fleet(accounts: [])
    }

    /// Zero ready, at least one near — the exact condition
    /// ``Fleet/capacityGlyphState``'s `.near` case tests, so this scene
    /// exercises the amber count for real rather than asserting the rule and
    /// drawing a fleet that happens not to trigger it. Neither account carries
    /// `quotaState: "ok"`, which is what keeps ``Account/isReady`` false for
    /// both.
    private static let nearTheLimitJSON = """
        [
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "frank@example.com", "priority": 1, "status": "held",
            "disabled": false, "quota": 0.9, "quotaState": "near",
            "fiveHour": 0.4, "sevenDay": 0.9, "sevenDayOi": 0.05,
            "held": [{"window": "5h", "minutesUntilReset": 40, "resetAtMs": 999000000000}],
            "requests": 8, "inputTokens": 80, "outputTokens": 8,
            "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          },
          {
            "source": "live", "serverSha": "abc1234", "serverDirty": false,
            "name": "grace@example.com", "priority": 2, "status": "held",
            "disabled": false, "quota": 1.0, "quotaState": "spent",
            "fiveHour": 1.0, "sevenDay": 1.0, "sevenDayOi": 0.0,
            "held": [{"window": "5h", "minutesUntilReset": 180, "resetAtMs": 999000000000}],
            "requests": 3, "inputTokens": 30, "outputTokens": 3,
            "cacheReadTokens": 0, "cacheHitRatio": 0.3, "probeStatus": "ok",
            "probeError": null, "lastStreamError": null, "streamErrorCount": 0
          }
        ]
        """

    private static var nearTheLimitFleet: Fleet {
        (try? Fleet.decode(Data(nearTheLimitJSON.utf8))) ?? Fleet(accounts: [])
    }

    /// The mixed fleet plus three live sessions with a running Bash call each
    /// — matching the mockup's "3 tools running" example. `sessionsSupported:
    /// true` is load-bearing: it is what ``PollState/runningToolsCount(showRunningTools:)``
    /// checks before drawing the segment, exactly the field
    /// ``Fleet/decode(_:)`` never sets (see that method's own doc-comment), so
    /// this fixture is built directly rather than decoded, the same way
    /// `RenderStates.sessionsTabFleet` already does.
    private static var runningToolsFleet: Fleet {
        let base = mixedFleet
        func msAgo(_ seconds: TimeInterval) -> Int64 {
            Int64(Date().addingTimeInterval(-seconds).timeIntervalSince1970 * 1000)
        }
        let sessions = [
            Session(
                sessionId: "aaaaaaaa-0001", account: "alice@example.com",
                firstSeenMs: msAgo(3600), lastSeenMs: msAgo(30),
                tools: SessionTools(
                    calls: 12, running: [
                        ToolCall(tool: "Bash", commandHead: "cargo test", startedMs: msAgo(20))
                    ])),
            Session(
                sessionId: "bbbbbbbb-0002", account: "bob@example.com",
                firstSeenMs: msAgo(1800), lastSeenMs: msAgo(15),
                tools: SessionTools(
                    calls: 4, running: [
                        ToolCall(tool: "Bash", commandHead: "swift build", startedMs: msAgo(10))
                    ])),
            Session(
                sessionId: "cccccccc-0003", account: "bob@example.com",
                firstSeenMs: msAgo(900), lastSeenMs: msAgo(5),
                tools: SessionTools(
                    calls: 2, running: [
                        ToolCall(tool: "Read", commandHead: nil, startedMs: msAgo(3))
                    ])),
        ]
        return Fleet(
            accounts: base.accounts, unreadable: base.unreadable, sessions: sessions,
            sessionsSupported: true)
    }

    // MARK: - Fleets holding Codex accounts

    /// One Claude row in the shape the fixtures above use: ready below its
    /// limit, or spent with its 5-hour window held.
    private static func claudeRowJSON(_ name: String, spent: Bool = false) -> String {
        let quota = spent ? "1.0" : "0.2"
        return """
            {
              "source": "live", "serverSha": "abc1234", "serverDirty": false,
              "name": "\(name)", "priority": 1, "status": "active",
              "disabled": false, "quota": \(quota), "quotaState": "\(spent ? "spent" : "ok")",
              "fiveHour": \(quota), "sevenDay": 0.3, "sevenDayOi": 0.0,
              "held": \(spent ? #"[{"window": "5h", "minutesUntilReset": 90, "resetAtMs": 999000000000}]"# : "[]"),
              "requests": 3, "inputTokens": 30, "outputTokens": 3,
              "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
              "probeError": null, "lastStreamError": null, "streamErrorCount": 0
            }
            """
    }

    /// One Codex row reporting its weekly window only: `ok` below its limit,
    /// `near` close to it, `spent` with the week held (gate `seven-day`), or
    /// `signedOut`, whose login the server reports as failed.
    private enum CodexRowState { case ok, near, spent, signedOut }

    private static func codexRowJSON(_ name: String, _ state: CodexRowState) -> String {
        let week: String
        let quotaState: String
        var status = "active"
        var gate = "ok"
        var held = "[]"
        switch state {
        case .ok:
            week = "0.3"
            quotaState = "ok"
        case .near:
            week = "0.9"
            quotaState = "near"
        case .spent:
            week = "1.0"
            quotaState = "spent"
            gate = "seven-day"
            held = #"[{"window": "7d", "minutesUntilReset": 4560, "resetAtMs": 999000000000}]"#
        case .signedOut:
            week = "0.4"
            quotaState = "ok"
            status = "error"
        }
        return """
            {
              "source": "live", "serverSha": "abc1234", "serverDirty": false,
              "provider": "codex", "quotaWindows": ["sevenDay"],
              "name": "\(name)", "priority": 1, "status": "\(status)", "gate": "\(gate)",
              "disabled": false, "quota": \(week), "quotaState": "\(quotaState)",
              "fiveHour": null, "sevenDay": \(week), "sevenDayOi": null,
              "held": \(held), "requests": 3, "inputTokens": 30, "outputTokens": 3,
              "cacheReadTokens": 0, "cacheHitRatio": 0.5, "probeStatus": "ok",
              "probeError": null, "lastStreamError": null, "streamErrorCount": 0
            }
            """
    }

    private static func fleet(_ rows: [String]) -> Fleet {
        (try? Fleet.decode(Data("[\(rows.joined(separator: ","))]".utf8))) ?? Fleet(accounts: [])
    }

    /// Claude's pool of 3 of 4: three ready, one spent.
    private static var claudeThreeOfFourRows: [String] {
        [
            claudeRowJSON("rosa@example.com"), claudeRowJSON("sam@example.com"),
            claudeRowJSON("tara@example.com"), claudeRowJSON("uma@example.com", spent: true),
        ]
    }

    /// Claude 3 of 4, Codex 2 of 2: `3/4 · 2/2`.
    private static var poolsReadyFleet: Fleet {
        fleet(
            claudeThreeOfFourRows + [
                codexRowJSON("vic@example.com", .ok), codexRowJSON("wren@example.com", .ok),
            ])
    }

    /// Claude 3 of 4, Codex 0 of 2, both spent on the week: `3/4 · 0/2`.
    private static var codexOutFleet: Fleet {
        fleet(
            claudeThreeOfFourRows + [
                codexRowJSON("vic@example.com", .spent), codexRowJSON("wren@example.com", .spent),
            ])
    }

    /// Claude 3 of 4, Codex 0 of 2 with one near its limit: `3/4 · 0/2`, the
    /// second half amber.
    private static var codexNearFleet: Fleet {
        fleet(
            claudeThreeOfFourRows + [
                codexRowJSON("vic@example.com", .near), codexRowJSON("wren@example.com", .spent),
            ])
    }

    /// Codex alone: two ready, one spent, one signed out, `2/4`.
    private static var codexOnlyFleet: Fleet {
        fleet([
            codexRowJSON("vic@example.com", .ok), codexRowJSON("wren@example.com", .ok),
            codexRowJSON("xena@example.com", .spent), codexRowJSON("yuri@example.com", .signedOut),
        ])
    }

    /// ``codexOutFleet`` with three live sessions each running one call, built
    /// the way ``runningToolsFleet`` is: `sessionsSupported` is set by hand.
    private static var codexOutRunningFleet: Fleet {
        let base = codexOutFleet
        func msAgo(_ seconds: TimeInterval) -> Int64 {
            Int64(Date().addingTimeInterval(-seconds).timeIntervalSince1970 * 1000)
        }
        let sessions = [
            ("dddddddd-0004", "rosa@example.com", "cargo test"),
            ("eeeeeeee-0005", "sam@example.com", "swift build"),
            ("ffffffff-0006", "sam@example.com", "make check"),
        ].map { id, account, command in
            Session(
                sessionId: id, account: account, firstSeenMs: msAgo(1800), lastSeenMs: msAgo(10),
                tools: SessionTools(
                    calls: 3, running: [
                        ToolCall(tool: "Bash", commandHead: command, startedMs: msAgo(5))
                    ]))
        }
        return Fleet(
            accounts: base.accounts, unreadable: base.unreadable, sessions: sessions,
            sessionsSupported: true)
    }
}
