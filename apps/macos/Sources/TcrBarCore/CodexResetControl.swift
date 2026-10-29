import Combine
import Foundation

/// A reset uses the CLI's consent and local-route contract. This app never reads a login.
public enum CodexResetCommand {
    public typealias Runner = @Sendable ([String], String) throws -> TcrTool.Output

    /// Both streams are kept even when the command exits zero. Only the command writes
    /// outcome and warning words, except "Reset failed." when no command could start;
    /// the panel displays them without interpreting them.
    public struct Outcome: Equatable, Sendable {
        public let exitCode: Int32
        public let stdout: String
        public let stderr: String

        public init(exitCode: Int32, stdout: String, stderr: String) {
            self.exitCode = exitCode
            self.stdout = stdout
            self.stderr = stderr
        }

        public var text: String {
            [stdout, stderr].map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
                .filter { !$0.isEmpty }.joined(separator: "\n")
        }
    }

    /// Blocking, and called only by the controller's detached task. No shell is involved.
    public static func run(_ arguments: [String], _ stdin: String) throws -> TcrTool.Output {
        switch TcrTool.resolve() {
        case .success(let executable):
            return try TcrTool.run(executable: executable, arguments: arguments, stdin: stdin)
        case .failure(let failure):
            throw failure
        }
    }
}

/// One controller for every presentation of the panel's rows. Attempt keys stay private
/// and survive an explicit Retry, including after a lost response or a launch failure.
/// Only new, confirmed redemption creates a new attempt. Nothing schedules a retry.
@MainActor
public final class CodexResetController: ObservableObject {
    @Published public private(set) var pending: Set<String> = []
    @Published public private(set) var outcomes: [String: CodexResetCommand.Outcome] = [:]
    private var attempts: [String: String] = [:]
    private let runner: CodexResetCommand.Runner

    public init(
        runner: @escaping CodexResetCommand.Runner = { try CodexResetCommand.run($0, $1) }
    ) {
        self.runner = runner
    }

    public func isPending(_ account: AccountRef) -> Bool { pending.contains(account.id) }

    public func outcome(for account: AccountRef) -> CodexResetCommand.Outcome? {
        outcomes[account.id]
    }

    public enum Attempt: Equatable, Sendable {
        case skipped
        case finished(CodexResetCommand.Outcome)
    }

    @discardableResult
    public func redeem(account: AccountRef, confirmed: Bool) async -> Attempt {
        guard confirmed, !isPending(account) else { return .skipped }
        let key = UUID().uuidString
        attempts[account.id] = key
        return await perform(account: account, key: key)
    }

    @discardableResult
    public func retry(account: AccountRef) async -> Attempt {
        guard !isPending(account), let key = attempts[account.id] else { return .skipped }
        return await perform(account: account, key: key)
    }

    private func perform(account: AccountRef, key: String) async -> Attempt {
        pending.insert(account.id)
        outcomes[account.id] = nil
        defer { pending.remove(account.id) }
        let runner = self.runner
        let outcome = await Task.detached(priority: .userInitiated) {
            do {
                // EOF after consent leaves any CLI retry prompt unanswered. A retry
                // requires another operator click and reuses this private attempt.
                let output = try runner(
                    ["codex", "reset", account.name, "--idempotency-key", key], "y\n")
                return CodexResetCommand.Outcome(
                    exitCode: output.exitCode, stdout: String(decoding: output.stdout, as: UTF8.self),
                    stderr: output.stderr)
            } catch {
                // Every runner throw precedes a started child: `TcrTool.resolve()` found no
                // `tcr`, or `Process.run()` refused it. Nothing was sent, so this shows the
                // approved words and never the thrown text; the attempt stays for Retry.
                return CodexResetCommand.Outcome(exitCode: -1, stdout: "", stderr: "Reset failed.")
            }
        }.value
        outcomes[account.id] = outcome
        return .finished(outcome)
    }
}
