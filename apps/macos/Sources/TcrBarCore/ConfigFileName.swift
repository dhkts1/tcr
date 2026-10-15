import Foundation

/// Which config file `tcr` reads on this Mac, for a hint that tells the user
/// where to edit it.
///
/// The same rule as `src/paths.rs`: `~/.config/tcr.json` if it exists, else
/// `~/.config/teamclaude.json` if that exists, else `~/.config/tcr.json`
/// (the file a first run creates). TcrBar never passes `--config`, so the
/// default is the file its `tcr` uses. This only asks whether a file exists;
/// it never opens one, because the config holds live credentials.
///
/// A copy of the Rust rule rather than a field on `tcr status --json`: the
/// status wire stays as it is for this change. If the two ever disagree, the
/// hint names the wrong file and nothing else changes.
public enum ConfigFileName {
    /// The name a fresh install gets.
    public static let current = "tcr.json"
    /// The name every install from before the rename has.
    public static let legacy = "teamclaude.json"

    /// `~/.config/<name>` for the file `tcr` resolves to under `home`.
    public static func displayPath(
        home: URL = FileManager.default.homeDirectoryForCurrentUser,
        fileExists: (String) -> Bool = { FileManager.default.fileExists(atPath: $0) }
    ) -> String {
        let configDir = home.appendingPathComponent(".config")
        let newExists = fileExists(configDir.appendingPathComponent(current).path)
        let oldExists = fileExists(configDir.appendingPathComponent(legacy).path)
        let name = !newExists && oldExists ? legacy : current
        return "~/.config/\(name)"
    }
}
