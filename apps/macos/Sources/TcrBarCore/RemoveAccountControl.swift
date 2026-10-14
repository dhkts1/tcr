import Combine
import Foundation

/// Deleting a single account from the config — `tcr remove <query>`
/// (`src/cli.rs`'s `remove_account`, dispatched from `src/main.rs`'s
/// `run_remove`). Same shape as ``AccountCommand``/``GroupCommand``, and for
/// the same reasons:
///
///  1. **This app never writes the tcr config.** `~/.config/teamclaude.json`
///     holds OAuth access and refresh tokens; deleting an account is a
///     subprocess, never a direct edit.
///  2. **This is not reversible from the UI.** `remove_account` deletes the
///     matched entry outright — there is no `tcr un-remove`. Getting the
///     account back means `tcr login` from scratch, which is why the panel
///     confirms before calling this at all.
///  3. **The account stops serving at once and leaves the panel on the next
///     poll.** `remove_account` parks the account in the running proxy, then
///     deletes it from the config; the proxy drops a row the config no longer
///     holds the next time it reads the file, which every `tcr status` makes
///     it do. Until 2026-10-14 the row stayed listed until a restart, so a
///     delete of an account that was already parked changed nothing on
///     screen. A proxy built before that still keeps the row, and `tcr` says
///     so on stderr, which lands in ``RemoveAccountController/notice(for:)``.
///  4. **Exit 0 with anything on stderr is not a clean success.** Same
///     three-arm ``Outcome`` as ``AccountCommand``/``GroupCommand``.
public enum RemoveAccountCommand {
    /// `tcr remove <query>`. `query` is passed positionally and verbatim — no
    /// shell involved, so nothing here needs to escape it.
    public static func arguments(query: String) -> [String] {
        ["remove", query]
    }

    /// Why a delete did not happen. `tcr`'s own words, unparaphrased.
    public struct Failure: Error, Equatable, Sendable {
        public let exitCode: Int32
        public let message: String

        public init(exitCode: Int32, message: String) {
            self.exitCode = exitCode
            self.message = message
        }

        /// One line, always non-empty, safe to render in the row.
        public var summary: String {
            let detail = message.isEmpty ? "no output" : message
            return "delete failed (exit \(exitCode)): \(detail)"
        }
    }

    /// Mirrors ``AccountCommand/Outcome``/``GroupCommand/Outcome`` exactly,
    /// same three arms and same reason: exit 0 has two meanings.
    public enum Outcome: Equatable, Sendable {
        case clean
        case spoke(notice: String)
        case failed(Failure)
    }

    /// Pure classification of a finished invocation.
    public static func classify(exitCode: Int32, stderr: String) -> Outcome {
        let text = stderr.trimmingCharacters(in: .whitespacesAndNewlines)
        guard exitCode == 0 else {
            return .failed(Failure(exitCode: exitCode, message: text))
        }
        return text.isEmpty ? .clean : .spoke(notice: text)
    }

    /// Blocking invocation — always called off the main actor.
    nonisolated static func perform(query: String) -> Outcome {
        switch TcrTool.resolve() {
        case .failure(let notFound):
            return .failed(
                Failure(
                    exitCode: -1,
                    message: "tcr not found (searched \(notFound.searched.count) locations)"
                ))
        case .success(let executable):
            do {
                let output = try TcrTool.run(
                    executable: executable, arguments: arguments(query: query))
                return classify(exitCode: output.exitCode, stderr: output.stderr)
            } catch {
                return .failed(Failure(exitCode: -1, message: error.localizedDescription))
            }
        }
    }
}

/// Panel-facing state for account deletion: which calls are in flight, which
/// have an unsuperseded failure, and what `tcr` said on stderr while a delete
/// succeeded.
///
/// A delete that lands needs no state here: the running proxy drops the row
/// the next time it reads the config, which `tcr status` makes it do, so the
/// row leaves the panel on the next poll. A `notice` is what `tcr` printed
/// when that will NOT happen (a proxy built before live removal, one it could
/// not reach), and it stays on the row for as long as the row is listed.
///
/// Keyed by ``AccountRef/id``, same as ``AccountController``.
@MainActor
public final class RemoveAccountController: ObservableObject {
    @Published public private(set) var pending: Set<String> = []
    @Published public private(set) var failures: [String: RemoveAccountCommand.Failure] = [:]
    @Published public private(set) var notices: [String: String] = [:]

    /// Runs `tcr remove <query>` to completion, off the main actor. The real
    /// subprocess by default; a test hands in a scripted outcome.
    private let perform: @Sendable (String) -> RemoveAccountCommand.Outcome

    public init(perform: (@Sendable (String) -> RemoveAccountCommand.Outcome)? = nil) {
        self.perform = perform ?? { RemoveAccountCommand.perform(query: $0) }
    }

    /// A controller that already holds these outcomes and runs nothing, for
    /// `--render-states`, which draws a refused delete without spawning `tcr`.
    public static func harness(
        failures: [AccountRef: RemoveAccountCommand.Failure] = [:],
        notices: [AccountRef: String] = [:]
    ) -> RemoveAccountController {
        let controller = RemoveAccountController(perform: { _ in .clean })
        for (account, failure) in failures { controller.failures[account.id] = failure }
        for (account, notice) in notices { controller.notices[account.id] = notice }
        return controller
    }

    public func isPending(_ account: AccountRef) -> Bool { pending.contains(account.id) }
    public func failure(for account: AccountRef) -> RemoveAccountCommand.Failure? {
        failures[account.id]
    }
    /// What `tcr` said on stderr while a delete of this account succeeded,
    /// verbatim, or `nil`.
    public func notice(for account: AccountRef) -> String? { notices[account.id] }

    /// What a call did, as far as the subprocess can say — mirrors
    /// ``AccountController/Attempt``/``GroupController/Attempt``.
    public enum Attempt: Equatable, Sendable {
        case skipped
        case refused
        case accepted(notice: String?)
    }

    /// `tcr remove <name>`. The name is the whole address: it is unique across
    /// the config, so it names this row or nothing. It used to name two rows on
    /// this fleet, and `tcr` refused rather than guessing — so a delete of
    /// either one failed as ambiguous.
    @discardableResult
    public func remove(account: AccountRef) async -> Attempt {
        let key = account.id
        guard !pending.contains(key) else { return .skipped }
        pending.insert(key)
        failures[key] = nil
        notices[key] = nil
        defer { pending.remove(key) }

        let perform = self.perform
        let query = account.name
        let outcome = await Task.detached(priority: .userInitiated) {
            perform(query)
        }.value

        switch outcome {
        case .clean:
            return .accepted(notice: nil)
        case .spoke(let notice):
            notices[key] = notice
            return .accepted(notice: notice)
        case .failed(let failure):
            failures[key] = failure
            return .refused
        }
    }
}
