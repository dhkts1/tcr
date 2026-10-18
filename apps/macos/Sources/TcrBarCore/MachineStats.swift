import Darwin
import Foundation

/// What the box itself is doing — the Tools tab's machine line, "load 7.1/14
/// · 48/64 GB · 5 compiles · 210 GB free".
///
/// The words `of`, `used` and `disk` came out of that sentence on 2026-09-13:
/// the long form is 59 characters and wrapped to two lines inside the 372 pt
/// panel, which put the machine reading on the same footing as the summary
/// above it. The slash is the form each pair is already read in elsewhere
/// (`uptime`'s load against cores, a disk's used-against-capacity), so
/// nothing is lost but the connective tissue. This deliberately diverges from
/// `docs/design/panel-tabs-mockup.html`'s own long line, the same way the
/// ring's 28 pt diverges from the mockup's 34: the mockup sets this line in a
/// wider column than the panel has.
///
/// `docs/design/tools-tab.md` question 2: "is the box overloaded?", which an
/// operator answers today by typing `uptime` and counting `rustc` processes by
/// hand. Nothing on the panel answered it at all before this.
///
/// The reading and the RULE are separate on purpose. ``read()`` touches the
/// kernel and cannot run in a test that means anything; ``line`` and
/// ``loadTint`` are pure functions of the six numbers, so both are tested
/// against values a test writes itself.
public struct MachineStats: Equatable, Sendable {
    /// The 1-minute load average — `getloadavg`'s first sample, the same
    /// number `uptime` prints first.
    public let loadAverage: Double
    /// `ProcessInfo.processorCount`. The denominator the load is read
    /// against: 7.1 means nothing until it is 7.1 *of 14*.
    public let cores: Int
    /// Active + wired + compressed pages. Not "total minus free": on macOS
    /// the free list is nearly empty by design, and a line built on it reads
    /// 63 of 64 on an idle machine.
    public let memoryUsedBytes: UInt64
    public let memoryTotalBytes: UInt64
    /// Live compiler processes — `rustc`, `swift-frontend`, `clang`,
    /// `cc1plus`, `ld`. The load average says the box is busy; this says
    /// whether it is busy with a build, which is the half that clears on its
    /// own.
    public let compiles: Int
    public let diskFreeBytes: UInt64
    /// CPU temperature and fans, as the proxy read them through AppleSMC and
    /// the last poll carried (``Fleet/heat``). Nil where the proxy reads no
    /// SMC (a VM), from a proxy too old to say, and with no proxy at all; the
    /// line is then exactly the line without it.
    public let heat: HeatReading?

    public init(
        loadAverage: Double,
        cores: Int,
        memoryUsedBytes: UInt64,
        memoryTotalBytes: UInt64,
        compiles: Int,
        diskFreeBytes: UInt64,
        heat: HeatReading? = nil
    ) {
        self.loadAverage = loadAverage
        self.cores = cores
        self.memoryUsedBytes = memoryUsedBytes
        self.memoryTotalBytes = memoryTotalBytes
        self.compiles = compiles
        self.diskFreeBytes = diskFreeBytes
        self.heat = heat
    }

    /// How hard the box is being pushed, by Gil's own dispatch rule
    /// (`docs/design/tools-tab.md`: "load above 2x cores means one lane, not
    /// three"). A band, not a colour: the view picks the colour, and the
    /// threshold stays where a test can read it.
    public enum LoadTint: String, Equatable, Sendable {
        /// Below one load unit per core — nothing to say.
        case calm
        /// At or above cores, up to twice cores. Amber.
        case busy
        /// Above twice cores. Red: one more lane is the wrong call.
        case overloaded
    }

    /// The boundaries are INCLUSIVE at the bottom of each band: exactly `1x`
    /// cores is already `busy` and exactly `2x` is still `busy`, so
    /// `overloaded` means strictly past the rule's own number rather than at
    /// it.
    public var loadTint: LoadTint {
        guard cores > 0 else { return .calm }
        let perCore = loadAverage / Double(cores)
        if perCore < 1 { return .calm }
        if perCore <= 2 { return .busy }
        return .overloaded
    }

    /// "load 7.1/14 · 48/64 GB · 5 compiles · 210 GB free", with
    /// ``heatClause`` after the load when there is one.
    public var line: String {
        [loadClause, heatClause, restClause].compactMap { $0 }.joined(separator: " · ")
    }

    /// The load clause alone — the one part a view TINTS, split out here so
    /// the tint and the words it colours can never drift apart.
    public var loadClause: String { "load \(String(format: "%.1f", loadAverage))/\(cores)" }

    /// Everything after the load — memory, compiles, disk. Never tinted: one
    /// coloured clause on a line is a signal, three are decoration.
    public var restClause: String {
        let compileNoun = compiles == 1 ? "compile" : "compiles"
        return
            "\(Self.gibibytes(memoryUsedBytes))/\(Self.gibibytes(memoryTotalBytes)) GB"
            + " · \(compiles) \(compileNoun) · \(Self.gigabytes(diskFreeBytes)) GB free"
    }

    /// "CPU 93°C · fans 87%": the other half of "is the box overloaded?",
    /// how hot it runs and how hard the fans work. Right after the load,
    /// because both describe the box. Never tinted, for the reason
    /// ``restClause`` gives (`docs/design/fan-readout-mockup.html`, panels 2
    /// to 6).
    ///
    /// - Whole degrees. Fans are the BUSIER fan's speed as a share of its own
    ///   maximum, whole percent: raw rpm means nothing without the maximum.
    /// - ` manual` when any fan's mode key says a program fixed its speed.
    ///   TcrBar sets no fan, so manual means another app, and fans that no
    ///   longer follow heat is the case worth seeing.
    /// - `fans off` when every fan reads 0 rpm; no fan part on a Mac with no
    ///   fans (`CPU 61°C`); no CPU part when no CPU sensor reads.
    /// - ` manual` survives a reading with no share to show (every maximum
    ///   reads 0): `fans manual`, because who holds the fans is the news.
    /// - Nil with no reading, so the line is exactly the line without heat.
    ///
    /// ``heatPhrases`` joined; the panel draws the two phrases apart so a wrap
    /// can fall between them.
    public var heatClause: String? {
        let phrases = heatPhrases
        return phrases.isEmpty ? nil : phrases.joined(separator: " · ")
    }

    /// The CPU phrase and the fan phrase, each present or not on its own.
    ///
    /// Never traps: the proxy drops non-finite and out-of-band readings at the
    /// source, but a `HeatReading` is a public value anyone can build, and
    /// `Int(_:)` of NaN stops the process. A figure that is not a number is
    /// left out, never drawn as one.
    public var heatPhrases: [String] {
        guard let heat else { return [] }
        var phrases: [String] = []
        if let celsius = heat.cpuCelsius, let degrees = Self.whole(celsius) {
            phrases.append("CPU \(degrees)°C")
        }
        let fans = heat.fans.filter { $0.actualRPM.isFinite }
        if !fans.isEmpty {
            let manual = fans.contains { $0.isManual } ? " manual" : ""
            if fans.allSatisfy({ $0.actualRPM <= 0 }) {
                phrases.append("fans off\(manual)")
            } else if let busiest = fans.compactMap(Self.shareOfMax).max(),
                let percent = Self.whole(busiest * 100)
            {
                phrases.append("fans \(percent)%\(manual)")
            } else if !manual.isEmpty {
                phrases.append("fans\(manual)")
            }
        }
        return phrases
    }

    /// The per-fan detail behind ``heatClause``, for the numbers line's
    /// tooltip (mockup panel 3): one decimal on the CPU, each fan's own rpm
    /// against its own maximum, and what `manual` means. Nil when
    /// ``heatClause`` is. Figures that are not numbers are left out here too.
    public var heatDetail: String? {
        guard let heat, heatClause != nil else { return nil }
        var lines: [String] = []
        if let celsius = heat.cpuCelsius, celsius.isFinite {
            lines.append("Hottest CPU sensor \(String(format: "%.1f", celsius)) °C")
        }
        for fan in heat.fans {
            guard let actual = Self.grouped(fan.actualRPM) else { continue }
            // A maximum of 0 is one that did not read, not a ceiling of 0.
            let of = fan.maxRPM > 0 ? Self.grouped(fan.maxRPM).map { " of \($0)" } ?? "" : ""
            lines.append("Fan \(fan.index + 1): \(actual)\(of) rpm")
        }
        if heat.fans.contains(where: { $0.isManual }) {
            lines.append("Manual: another app set these speeds, so they do not follow the heat.")
        }
        return lines.joined(separator: "\n")
    }

    /// Actual over maximum, or nil for a fan whose maximum did not read: a
    /// share of nothing is not 0 %, it is no figure.
    static func shareOfMax(_ fan: FanReading) -> Double? {
        guard fan.maxRPM.isFinite, fan.maxRPM > 0, fan.actualRPM.isFinite else { return nil }
        return fan.actualRPM / fan.maxRPM
    }

    /// `value` rounded to a whole number, or nil when it is not a number
    /// `Int` can hold: NaN, an infinity, or past ±10^15.
    static func whole(_ value: Double) -> Int? {
        guard value.isFinite, abs(value) < 1e15 else { return nil }
        return Int(value.rounded())
    }

    /// Whole rpm with a comma every three digits, "5,013": fixed rather than
    /// the user's locale, like every other figure on the panel. Nil for a
    /// figure that is not a number. No minus sign: the proxy drops negative rpm.
    static func grouped(_ rpm: Double) -> String? {
        guard let rounded = whole(rpm) else { return nil }
        let digits = String(rounded)
        var out = ""
        for (offset, digit) in digits.enumerated() {
            if offset > 0 && (digits.count - offset) % 3 == 0 { out.append(",") }
            out.append(digit)
        }
        return out
    }

    /// Memory in binary GB, which is the unit a Mac's own "64 GB" memory
    /// figure is stated in: 68,719,476,736 bytes IS the 64 in "64 GB of RAM".
    static func gibibytes(_ bytes: UInt64) -> Int {
        Int((Double(bytes) / 1_073_741_824).rounded())
    }

    /// Disk in decimal GB, which is the unit the disk's own capacity and
    /// every Finder readout are stated in. Two units in one line is not a
    /// slip: it is what each of the two figures means where the operator
    /// already reads it.
    static func gigabytes(_ bytes: UInt64) -> Int {
        Int((Double(bytes) / 1_000_000_000).rounded())
    }
}

extension MachineStats {
    /// Process names that count as a compile. `ld` is here because a release
    /// link is the part of a Rust build that pins one core for a minute with
    /// nothing else on the box moving.
    static let compilerProcessNames: Set<String> = [
        "rustc", "swift-frontend", "clang", "cc1plus", "ld",
    ]

    /// Read the box, now. Every figure comes from a kernel call — no
    /// subprocess, because this runs on the panel's poll and `ps` costs a
    /// fork every few seconds for a line of text.
    ///
    /// A reading that fails is a zero in that one field, never a `nil` whole:
    /// `getloadavg` returning -1 does not make the memory figure unknown.
    ///
    /// `heat` is not read here. The proxy reads the SMC and the poll carries
    /// it (``Fleet/heat``); the caller passes the latest one in.
    ///
    /// Blocking: the process walk is kernel calls, so the panel calls this
    /// off the main thread and assigns the result on it.
    public static func read(heat: HeatReading? = nil) -> MachineStats {
        MachineStats(
            loadAverage: readLoadAverage(),
            cores: ProcessInfo.processInfo.processorCount,
            memoryUsedBytes: readMemoryUsed(),
            memoryTotalBytes: ProcessInfo.processInfo.physicalMemory,
            compiles: readCompileCount(),
            diskFreeBytes: readDiskFree(),
            heat: heat
        )
    }

    static func readLoadAverage() -> Double {
        var samples = [Double](repeating: 0, count: 3)
        guard getloadavg(&samples, 3) > 0 else { return 0 }
        return samples[0]
    }

    /// Active + wired + compressed pages, per `vm_stat`'s own arithmetic.
    static func readMemoryUsed() -> UInt64 {
        var stats = vm_statistics64_data_t()
        var count = mach_msg_type_number_t(
            MemoryLayout<vm_statistics64_data_t>.size / MemoryLayout<integer_t>.size)
        let result = withUnsafeMutablePointer(to: &stats) { pointer in
            pointer.withMemoryRebound(to: integer_t.self, capacity: Int(count)) { rebound in
                host_statistics64(mach_host_self(), HOST_VM_INFO64, rebound, &count)
            }
        }
        guard result == KERN_SUCCESS else { return 0 }
        let pageSize = UInt64(vm_kernel_page_size)
        let pages =
            UInt64(stats.active_count) + UInt64(stats.wire_count)
            + UInt64(stats.compressor_page_count)
        return pages * pageSize
    }

    /// `statfs` on `/`. `f_bavail` rather than `f_bfree`: the blocks a
    /// non-root process may actually have, which is the number that decides
    /// whether the next worktree fits.
    static func readDiskFree() -> UInt64 {
        var buffer = statfs()
        guard statfs("/", &buffer) == 0 else { return 0 }
        return UInt64(buffer.f_bavail) * UInt64(buffer.f_bsize)
    }

    /// Every live pid, named, counted against ``compilerProcessNames``.
    ///
    /// `proc_listallpids` reports more pids than it can return when the
    /// buffer is short, so the buffer is sized from its own no-buffer answer
    /// plus slack for processes that start between the two calls.
    static func readCompileCount() -> Int {
        let reported = proc_listallpids(nil, 0)
        guard reported > 0 else { return 0 }
        let capacity = Int(reported) + 64
        var pids = [pid_t](repeating: 0, count: capacity)
        let byteCount = Int32(capacity * MemoryLayout<pid_t>.size)
        let written = pids.withUnsafeMutableBufferPointer { buffer in
            proc_listallpids(buffer.baseAddress, byteCount)
        }
        guard written > 0 else { return 0 }
        var count = 0
        var name = [CChar](repeating: 0, count: 256)
        for pid in pids.prefix(Int(written)) where pid > 0 {
            let length = proc_name(pid, &name, UInt32(name.count))
            guard length > 0 else { continue }
            if compilerProcessNames.contains(String(cString: name)) { count += 1 }
        }
        return count
    }
}
