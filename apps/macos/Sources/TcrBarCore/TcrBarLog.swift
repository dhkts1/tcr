import os

/// Where every TcrBar log line goes: one `os.Logger` per area, all under the
/// app's bundle id, so `log show --predicate 'subsystem == "io.github.dhkts1.tcrbar"'`
/// finds all of them and `category == "…"` narrows to one area.
///
/// Not `NSLog`, which the app used until 1.2.22: `NSLog` hands its arguments to
/// the unified log as dynamic strings, and the unified log redacts those by
/// default, so every line read `(Foundation) <private>` in `log show` and
/// nobody could read any of them. A `Logger` call states each value's privacy
/// where it is written. The rule at every call site: the line is
/// `privacy: .public`, and a value that can carry an account email, a token, a
/// key, an account or org UUID, a file under the user's config, a whole URL or
/// a Bash command line stays `.private`. `log show` prints a private value as
/// `<private>`, so the rest of its line still reads.
///
/// The text of each line is unchanged from the `NSLog` it replaced, so a grep
/// for a line (`TcrBar: PATH tcr:`, ``PanelSizeDelta/marker``,
/// ``UncaughtExceptionReport/marker``) still matches. `Logger` takes no format
/// string, so a `%` in a value is printed as a `%`.
public enum TcrBarLog {
    /// The app's bundle id, which is also the subsystem.
    public static let subsystem = "io.github.dhkts1.tcrbar"

    /// Launch, the `PATH` repair, and the uncaught-exception report.
    public static let app = Logger(subsystem: subsystem, category: "app")
    /// `tcrbar://` URLs and the update check.
    public static let update = Logger(subsystem: subsystem, category: "update")
    /// `tcr://` join and moved links.
    public static let peer = Logger(subsystem: subsystem, category: "peer")
    /// The panel: size measurements and the Tools tab's kill button.
    public static let panel = Logger(subsystem: subsystem, category: "panel")
    /// Knock notifications.
    public static let knock = Logger(subsystem: subsystem, category: "knock")
    /// The proxy's machine readings (heat, fan control).
    public static let machine = Logger(subsystem: subsystem, category: "machine")
}
