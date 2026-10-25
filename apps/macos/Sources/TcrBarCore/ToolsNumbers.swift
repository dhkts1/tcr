import Foundation

/// The Tools tab's grey numbers block, under "N running, N near timeout": TWO
/// fixed rows, each one line, whose contents never change places (Gil,
/// 2026-10-21).
///
/// - Row 1 is the machine: ``MachineStats/loadClause`` then
///   ``MachineStats/restClause``. Empty, not removed, with no machine reading.
/// - Row 2 is ``MachineStats/heatPhrases``, then "N err today" when there are
///   any, then "N timed out", always.
///
/// It used to be one line that wrapped, with three lines reserved so the heat
/// clause appearing or wrapping never moved the cards below. In the usual
/// state that line filled one row and left two empty rows above RUNNING NOW.
/// Two rows that are always two rows keep the no-shift rule (Gil,
/// 2026-10-14) without the empty band. A row too long for the panel is cut at
/// its tail, so ``help`` carries the whole block.
public struct ToolsNumbers: Equatable, Sendable {
    /// One phrase of a row. The view draws each kind its own way and never
    /// splits a phrase across lines.
    public enum Phrase: Equatable, Sendable {
        /// The load clause, the block's only tinted phrase.
        case load(String)
        /// A heat phrase, a link that opens the fan popover.
        case heat(String)
        /// Everything else, the block's own grey.
        case plain(String)

        public var text: String {
            switch self {
            case .load(let text), .heat(let text), .plain(let text): return text
            }
        }
    }

    /// How many rows the block always has, filled or not.
    public static let rowCount = 2

    /// Exactly ``rowCount`` rows, top to bottom.
    public let rows: [[Phrase]]

    /// The hover text: each row in full on its own line, then the per-fan
    /// detail behind the heat phrases (``MachineStats/heatDetail``) after a
    /// blank line. Empty rows are left out.
    public let help: String

    public init(machine: MachineStats?, errors: Int, timeouts: Int) {
        var machineRow: [Phrase] = []
        var heatRow: [Phrase] = []
        if let machine {
            machineRow = [.load(machine.loadClause), .plain(machine.restClause)]
            heatRow = machine.heatPhrases.map(Phrase.heat)
        }
        if errors > 0 {
            heatRow.append(.plain("\(errors) err today"))
        }
        heatRow.append(.plain("\(timeouts) timed out"))
        rows = [machineRow, heatRow]

        let lines = rows.filter { !$0.isEmpty }.map { $0.map(\.text).joined(separator: " · ") }
        let detail = machine?.heatDetail.map { ["", $0] } ?? []
        help = (lines + detail).joined(separator: "\n")
    }
}
