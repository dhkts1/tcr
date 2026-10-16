import Darwin
import os

/// How many times the panel's view tree was evaluated and laid out, counted for
/// `--measure-idle`.
///
/// Spawns are not the whole cost of a closed panel. A view tree that is
/// re-evaluated and re-measured on every poll costs main-thread time while
/// nobody can see it, and no spawn log records that. These two counters are
/// what lets the harness say "the panel drew nothing while shut" as a number,
/// and the open window, which must count above zero, is what proves the
/// counters are wired to the panel at all.
///
/// A lock and not a main-actor static: `Layout.sizeThatFits` is not declared
/// on the main actor, and a counter that cannot be bumped from where layout
/// runs would have to be bumped somewhere else, which is the counter agreeing
/// with its author.
enum PanelDrawCount {
    private static let counts = OSAllocatedUnfairLock(initialState: (bodies: 0, layouts: 0))

    /// `FleetView.body` was evaluated.
    static func body() { counts.withLock { $0.bodies += 1 } }

    /// `TallestTabLayout.sizeThatFits` ran, the layout that measures every tab.
    static func layout() { counts.withLock { $0.layouts += 1 } }

    static var now: (bodies: Int, layouts: Int) { counts.withLock { $0 } }
}

/// This process's CPU time and resident memory, the two figures `ps -o time=,rss=`
/// prints, read from inside so a measured window starts and ends on the exact
/// instants the harness chose.
struct SelfCost {
    /// User plus system time since the process started, in seconds.
    let cpuSeconds: Double
    /// Resident size in bytes; `nil` when the kernel refused the read.
    let residentBytes: UInt64?

    static func read() -> SelfCost {
        var usage = rusage()
        getrusage(RUSAGE_SELF, &usage)
        let cpu =
            Double(usage.ru_utime.tv_sec) + Double(usage.ru_utime.tv_usec) / 1_000_000
            + Double(usage.ru_stime.tv_sec) + Double(usage.ru_stime.tv_usec) / 1_000_000

        var info = mach_task_basic_info()
        var count = mach_msg_type_number_t(
            MemoryLayout<mach_task_basic_info>.size / MemoryLayout<natural_t>.size)
        let result = withUnsafeMutablePointer(to: &info) { pointer in
            pointer.withMemoryRebound(to: integer_t.self, capacity: Int(count)) {
                task_info(mach_task_self_, task_flavor_t(MACH_TASK_BASIC_INFO), $0, &count)
            }
        }
        return SelfCost(
            cpuSeconds: cpu, residentBytes: result == KERN_SUCCESS ? info.resident_size : nil)
    }
}
