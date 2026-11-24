//! Tests for `tcr update`: the checkout rebuild and the release-installer path.

use super::*;
use std::cmp::Ordering;
use std::fs;

/// A unique scratch dir under the system temp root, following the repo's
/// pid-suffixed convention. `tempfile` is now a dependency and `TempDir`
/// would do this with auto-cleanup; this helper is simply not yet migrated.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tcr-update-test-{}-{}-{tag}",
        std::process::id(),
        // nanos disambiguate multiple scratch dirs in one process.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

#[test]
fn find_repo_root_walks_up_to_git_and_cargo() {
    let root = scratch("root");
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
    let nested = root.join("target").join("release");
    fs::create_dir_all(&nested).unwrap();
    let exe = nested.join("tcr");
    fs::write(&exe, b"fake").unwrap();

    assert_eq!(find_repo_root_from(&exe).as_deref(), Some(root.as_path()));

    fs::remove_dir_all(&root).ok();
}

/// Regression (found by a live `tcr update`): when the PATH entry is a
/// SYMLINK into the build output (`~/.local/bin/tcr` → `…/release/tcr`),
/// walking up from the symlink path never reaches the checkout;
/// canonicalizing first — what `find_repo_root`/`classify_install` now do —
/// resolves to the real binary. The injected-path unit tests missed this
/// because they never symlinked. Installs also ship the binary as a copied
/// regular file (the current shape on this machine), where canonicalization
/// is a no-op; this test covers the symlink shape specifically.
#[test]
fn find_repo_root_resolves_through_a_symlinked_exe() {
    use std::os::unix::fs::symlink;
    let base = scratch("symlink");
    let root = base.join("checkout");
    let nested = root.join("target").join("release");
    fs::create_dir_all(&nested).unwrap();
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
    let real_exe = nested.join("tcr");
    fs::write(&real_exe, b"fake").unwrap();

    // A PATH symlink in a sibling dir (no .git above it) → the real binary.
    let bindir = base.join("bin");
    fs::create_dir_all(&bindir).unwrap();
    let link = bindir.join("tcr");
    symlink(&real_exe, &link).unwrap();

    // The bug: walking up from the symlink path finds nothing.
    assert_eq!(find_repo_root_from(&link), None);
    // The fix: canonicalize first, then the walk reaches the checkout root.
    let resolved = fs::canonicalize(&link).unwrap();
    assert_eq!(
        find_repo_root_from(&resolved).map(|r| fs::canonicalize(r).unwrap()),
        Some(fs::canonicalize(&root).unwrap())
    );

    fs::remove_dir_all(&base).ok();
}

#[test]
fn find_repo_root_none_without_markers() {
    let root = scratch("bare");
    let nested = root.join("a").join("b");
    fs::create_dir_all(&nested).unwrap();
    let exe = nested.join("tcr");
    fs::write(&exe, b"fake").unwrap();

    assert_eq!(find_repo_root_from(&exe), None);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn classify_install_git_checkout() {
    let root = scratch("classify-git");
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
    let exe = root.join("target").join("release").join("tcr");
    fs::create_dir_all(exe.parent().unwrap()).unwrap();
    fs::write(&exe, b"fake").unwrap();

    assert_eq!(
        classify_install_from(&exe),
        InstallKind::GitCheckout(root.clone())
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn classify_install_installed_without_repo() {
    let root = scratch("classify-installed");
    let exe = root.join("bin").join("tcr");
    fs::create_dir_all(exe.parent().unwrap()).unwrap();
    fs::write(&exe, b"fake").unwrap();

    assert_eq!(classify_install_from(&exe), InstallKind::Installed);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn classify_install_app_bundle() {
    // TcrBar.app/Contents/MacOS/tcr — no checkout above it.
    let root = scratch("classify-bundle");
    let bundle = root.join("TcrBar.app");
    let exe = bundle.join("Contents").join("MacOS").join("tcr");
    fs::create_dir_all(exe.parent().unwrap()).unwrap();
    fs::write(&exe, b"fake").unwrap();

    assert_eq!(
        classify_install_from(&exe),
        InstallKind::AppBundle(bundle.clone())
    );
    assert_eq!(
        find_app_bundle_from(&exe).as_deref(),
        Some(bundle.as_path())
    );

    fs::remove_dir_all(&root).ok();
}

/// A bundle nested INSIDE a checkout still holds a copy of the binary, so
/// the bundle classification must win over `GitCheckout` — rebuilding the
/// checkout would not replace what is running.
#[test]
fn classify_install_app_bundle_beats_enclosing_checkout() {
    let root = scratch("bundle-in-checkout");
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
    let bundle = root.join("dist").join("TcrBar.app");
    let exe = bundle.join("Contents").join("MacOS").join("tcr");
    fs::create_dir_all(exe.parent().unwrap()).unwrap();
    fs::write(&exe, b"fake").unwrap();

    assert_eq!(classify_install_from(&exe), InstallKind::AppBundle(bundle));

    fs::remove_dir_all(&root).ok();
}

/// A directory literally named `.app` is a dotfile, not a bundle.
#[test]
fn find_app_bundle_ignores_bare_dot_app_and_plain_dirs() {
    let root = scratch("bundle-negative");
    let exe = root.join(".app").join("bin").join("tcr");
    fs::create_dir_all(exe.parent().unwrap()).unwrap();
    fs::write(&exe, b"fake").unwrap();

    assert_eq!(find_app_bundle_from(&exe), None);
    assert_eq!(classify_install_from(&exe), InstallKind::Installed);

    fs::remove_dir_all(&root).ok();
}

/// False positive the suffix-only check produced: an ordinary checkout that
/// happens to live under a directory someone named `…app` is NOT a bundle,
/// and misclassifying it beat the `GitCheckout` answer that would have
/// worked (the bundle test runs first), so `tcr update` refused to update a
/// checkout it could perfectly well have pulled and rebuilt. What makes a
/// bundle is `Contents/MacOS`, not the name.
#[test]
fn checkout_under_an_app_named_directory_is_not_a_bundle() {
    let base = scratch("app-named-parent");
    let root = base.join("myapp.app").join("checkout");
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
    let exe = root.join("target").join("release").join("tcr");
    fs::create_dir_all(exe.parent().unwrap()).unwrap();
    fs::write(&exe, b"fake").unwrap();

    assert_eq!(find_app_bundle_from(&exe), None);
    assert_eq!(classify_install_from(&exe), InstallKind::GitCheckout(root));

    fs::remove_dir_all(&base).ok();
}

/// False negative in the other direction: APFS and HFS+ are case-insensitive
/// by default, so `TcrBar.APP` names the same bundle as `TcrBar.app` and must
/// classify the same way.
#[test]
fn find_app_bundle_matches_the_suffix_case_insensitively() {
    let root = scratch("bundle-uppercase");
    let bundle = root.join("TcrBar.APP");
    let exe = bundle.join("Contents").join("MacOS").join("tcr");
    fs::create_dir_all(exe.parent().unwrap()).unwrap();
    fs::write(&exe, b"fake").unwrap();

    assert_eq!(
        find_app_bundle_from(&exe).as_deref(),
        Some(bundle.as_path())
    );
    assert_eq!(classify_install_from(&exe), InstallKind::AppBundle(bundle));

    fs::remove_dir_all(&root).ok();
}

/// A `.app` directory with no `Contents/MacOS` inside it is a name, not a
/// bundle — and with no checkout above it the honest answer is `Installed`.
#[test]
fn app_named_directory_without_contents_macos_is_not_a_bundle() {
    let root = scratch("bundle-shell-only");
    let exe = root.join("Empty.app").join("tcr");
    fs::create_dir_all(exe.parent().unwrap()).unwrap();
    fs::write(&exe, b"fake").unwrap();

    assert_eq!(find_app_bundle_from(&exe), None);
    assert_eq!(classify_install_from(&exe), InstallKind::Installed);

    fs::remove_dir_all(&root).ok();
}

/// Write a minimal XML `Info.plist` declaring `id` into a scratch bundle.
fn bundle_with_identifier(dir: &Path, id: &str) -> PathBuf {
    let bundle = dir.join("Some.app");
    let contents = bundle.join("Contents");
    fs::create_dir_all(contents.join("MacOS")).unwrap();
    fs::write(
        contents.join("Info.plist"),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                 <plist version=\"1.0\"><dict>\n\
                 <key>CFBundleName</key><string>TcrBar</string>\n\
                 <key>CFBundleIdentifier</key><string>{id}</string>\n\
                 </dict></plist>\n"
        ),
    )
    .unwrap();
    bundle
}

/// The `AppBundle` arm must not spawn anything when the bundle cannot be
/// confirmed to be TcrBar — it prints the manual instructions instead.
///
/// The path is a scratch bundle rather than `/Applications/TcrBar.app` on
/// purpose: that path may EXIST on the machine running the suite, and with
/// the Sparkle handoff in place this test would then launch the real app
/// out of `cargo test`.
#[test]
fn run_update_app_bundle_notifies_without_spawning() {
    let base = scratch("handoff-no-spawn");
    // No Info.plist at all → UnreadableIdentity → instructions, no `open`.
    let bare = base.join("Bare.app");
    fs::create_dir_all(bare.join("Contents").join("MacOS")).unwrap();
    let kind = InstallKind::AppBundle(bare);
    assert!(run_update_with(kind.clone(), false).is_ok());
    assert!(run_update_with(kind, true).is_ok());

    // A bundle belonging to some other application likewise gets the
    // instructions, never a request to its updater.
    let foreign = bundle_with_identifier(&base, "com.example.someoneelse");
    assert!(run_update_with(InstallKind::AppBundle(foreign), false).is_ok());

    fs::remove_dir_all(&base).ok();
}

/// The contract with the app, pinned exactly. TcrBar registers this scheme
/// and this bundle id; a typo here is an `open` that silently resolves to
/// nothing (or to some other app) rather than a compile error.
#[test]
fn the_handoff_contract_matches_the_app() {
    assert_eq!(TCRBAR_UPDATE_URL, "tcrbar://check-for-updates");
    assert_eq!(TCRBAR_BUNDLE_ID, "io.github.dhkts1.tcrbar");
    // `-g` first: the request must not steal focus from the terminal.
    assert_eq!(open_handoff_argv(), ["-g", "tcrbar://check-for-updates"]);
}

/// The literal assert above pins the constant against itself — it would
/// pass even if the app shipped under a different id. Bind it to the
/// actual shipped identity instead: the build script's unconditional
/// `bundle_id="..."` assignment (`apps/macos/scripts/build-tcrbar.sh`),
/// which is what lands in `Info.plist` (the plist itself only ever writes
/// `$bundle_id`, a shell variable, so the assignment is the one place the
/// real string lives).
#[test]
fn the_bundle_id_constant_matches_the_shipped_build_script() {
    let script_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("apps")
        .join("macos")
        .join("scripts")
        .join("build-tcrbar.sh");
    let script = fs::read_to_string(&script_path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", script_path.display()));
    let shipped_id = script
        .lines()
        .find_map(|line| {
            let line = line.trim();
            line.strip_prefix("bundle_id=\"")
                .and_then(|rest| rest.strip_suffix('"'))
        })
        .unwrap_or_else(|| {
            panic!(
                "no `bundle_id=\"...\"` assignment found in {}",
                script_path.display()
            )
        });
    assert_eq!(
        shipped_id, TCRBAR_BUNDLE_ID,
        "the build script's shipped bundle id has drifted from TCRBAR_BUNDLE_ID"
    );
}

/// If we print a command, the binary it names has to be there — the same
/// rule `the_recommended_app_installer_script_exists` enforces for the
/// installer path.
#[cfg(target_os = "macos")]
#[test]
fn the_open_binary_the_handoff_spawns_exists() {
    assert!(
        Path::new(OPEN_BIN).is_file(),
        "the handoff spawns {OPEN_BIN}, which does not exist"
    );
}

/// The disk half: a real TcrBar `Info.plist` on disk resolves to the
/// identifier the handoff is gated on, and a bundle that is not there reads
/// as unknown rather than erroring out of the update.
#[cfg(target_os = "macos")]
#[test]
fn read_bundle_identifier_confirms_a_tcrbar_bundle() {
    let base = scratch("bundle-identity-disk");
    let bundle = bundle_with_identifier(&base, TCRBAR_BUNDLE_ID);
    assert_eq!(
        read_bundle_identifier(&bundle).as_deref(),
        Some(TCRBAR_BUNDLE_ID)
    );
    assert_eq!(
        decide_app_bundle_handoff(read_bundle_identifier(&bundle).as_deref()),
        AppBundleAction::HandOff
    );
    assert_eq!(read_bundle_identifier(&base.join("Nope.app")), None);

    fs::remove_dir_all(&base).ok();
}

#[test]
fn parse_bundle_identifier_reads_cfbundleidentifier() {
    let base = scratch("bundle-identity");
    let bundle = bundle_with_identifier(&base, TCRBAR_BUNDLE_ID);
    // A real-shaped Info.plist, read off disk and parsed. `CFBundleName`
    // comes FIRST in it deliberately: a scan that took the first `<string>`
    // in the file would return "TcrBar" here and match nothing.
    let plist = fs::read_to_string(bundle.join("Contents").join("Info.plist")).unwrap();
    assert_eq!(parse_bundle_identifier(&plist), Some(TCRBAR_BUNDLE_ID));
    // Whitespace around the value, as `plutil` and Xcode both emit.
    assert_eq!(
        parse_bundle_identifier(
            "<key>CFBundleIdentifier</key>\n\t<string>\n  com.x.y\n  </string>"
        ),
        Some("com.x.y")
    );
    // A missing key, an empty value, and the BINARY plist format all read
    // as unknown — never as a match we would then hand a URL to.
    assert_eq!(
        parse_bundle_identifier("<key>CFBundleName</key><string>x</string>"),
        None
    );
    assert_eq!(
        parse_bundle_identifier("<key>CFBundleIdentifier</key><string>  </string>"),
        None
    );
    assert_eq!(parse_bundle_identifier("bplist00\u{0}\u{1}"), None);

    fs::remove_dir_all(&base).ok();
}

/// `open` resolves a URL scheme to whatever app claims it, so the handoff
/// is gated on the bundle actually being TcrBar. Every other answer keeps
/// its reason, because a fallback that does not say what it fell back from
/// is indistinguishable from the old notify-only dead end.
#[test]
fn decide_app_bundle_handoff_only_hands_off_to_tcrbar() {
    assert_eq!(
        decide_app_bundle_handoff(Some(TCRBAR_BUNDLE_ID)),
        AppBundleAction::HandOff
    );
    assert_eq!(
        decide_app_bundle_handoff(None),
        AppBundleAction::Instruct(HandoffRefusal::UnreadableIdentity)
    );
    for foreign in [
        // The id TcrBar shipped under before the 2026-08-09 ControlCenter
        // block forced a new identity. A stale copy still installed under it
        // is somebody else's bundle now, and must not receive the handoff.
        "com.github.dhkts1.tcrbar",
        // A suffix extension of the *live* id is the near miss that matters
        // now that the id changed; the `com.` one below guards the old one.
        "io.github.dhkts1.tcrbar.helper",
        "com.github.dhkts1.tcrbar.helper",
        "com.github.dhkts1.TcrBar",
        "com.example.tcrbar",
        "",
    ] {
        assert_eq!(
            decide_app_bundle_handoff(Some(foreign)),
            AppBundleAction::Instruct(HandoffRefusal::ForeignBundle(foreign.to_string())),
            "{foreign:?} is not TcrBar and must not receive the handoff"
        );
    }
    // Both refusals name what was checked, so the printed fallback says why.
    assert!(
        HandoffRefusal::UnreadableIdentity
            .reason()
            .contains(TCRBAR_BUNDLE_ID)
    );
    assert!(
        HandoffRefusal::ForeignBundle("com.example.x".into())
            .reason()
            .contains("com.example.x")
    );
}

/// The whole point of defect #1: with `CARGO_TARGET_DIR` set, cargo writes
/// far outside the checkout, so the built path must come from metadata.
#[test]
fn parse_target_directory_reads_relocated_target_dir() {
    let json = r#"{"packages":[],"workspace_root":"/repo","target_directory":"/elsewhere/cargo-target","version":1}"#;
    assert_eq!(
        parse_target_directory(json).unwrap(),
        PathBuf::from("/elsewhere/cargo-target")
    );
    // …and the printed binary path is target_dir/release/tcr, NOT
    // <root>/target/release/tcr.
    let built = parse_target_directory(json)
        .unwrap()
        .join("release")
        .join("tcr");
    assert_eq!(built, PathBuf::from("/elsewhere/cargo-target/release/tcr"));
    assert!(!built.starts_with("/repo"));
}

#[test]
fn parse_target_directory_errors_are_not_swallowed() {
    // Missing field, wrong type, empty string and non-JSON must all error
    // rather than degrade into a guessed path.
    assert!(parse_target_directory(r#"{"workspace_root":"/repo"}"#).is_err());
    assert!(parse_target_directory(r#"{"target_directory":null}"#).is_err());
    assert!(parse_target_directory(r#"{"target_directory":""}"#).is_err());
    assert!(parse_target_directory("error: no manifest found").is_err());
}

#[test]
fn parse_pull_stdout_already_up_to_date() {
    assert_eq!(
        parse_pull_stdout("Already up to date.\n"),
        PullOutcome::AlreadyUpToDate
    );
}

#[test]
fn parse_pull_stdout_diffstat_is_updated() {
    let body = "Updating a1b2c3d..e4f5g6h\nFast-forward\n src/main.rs | 4 ++--\n 1 file changed\n";
    assert_eq!(parse_pull_stdout(body), PullOutcome::Updated);
}

#[test]
fn argv_shapes_are_stable() {
    assert_eq!(git_pull_argv(), ["pull", "--ff-only"]);
    assert_eq!(cargo_build_argv(), ["build", "--release"]);
    assert_eq!(
        cargo_metadata_argv(),
        ["metadata", "--format-version", "1", "--no-deps"]
    );
}

/// The `AppBundle` arm tells the user to run `apps/macos/scripts/install.sh`.
/// Naming a path that is not there is the same class of defect as printing
/// a built-binary path that does not exist, so the recommendation is
/// checked against the tree rather than trusted to stay true.
#[test]
fn the_recommended_app_installer_script_exists() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("apps")
        .join("macos")
        .join("scripts")
        .join("install.sh");
    assert!(
        script.is_file(),
        "run_update_with(AppBundle) recommends {}, which does not exist",
        script.display()
    );
}

// -- installed-copy update: version comparison ---------------------------

/// The whole point of the check: the common case must cost one API call and
/// no download at all.
#[test]
fn equal_versions_skip_the_download() {
    assert_eq!(
        decide_update("0.1.0", "v0.1.0", false),
        UpdateDecision::AlreadyCurrent
    );
    // The tag is normally `v`-prefixed, but a bare tag must behave the same.
    assert_eq!(
        decide_update("0.1.0", "0.1.0", false),
        UpdateDecision::AlreadyCurrent
    );
    // …and trailing whitespace from a sloppy tag must not read as a change.
    assert_eq!(
        decide_update("0.1.0", " v0.1.0\n", false),
        UpdateDecision::AlreadyCurrent
    );
}

#[test]
fn a_newer_release_is_installed() {
    assert_eq!(
        decide_update("0.1.0", "v0.1.1", false),
        UpdateDecision::Install
    );
    assert_eq!(
        decide_update("0.1.0", "v0.2.0", false),
        UpdateDecision::Install
    );
    assert_eq!(
        decide_update("0.9.9", "v1.0.0", false),
        UpdateDecision::Install
    );
    // Numeric ordering, not lexicographic: "0.10.0" > "0.9.0".
    assert_eq!(
        decide_update("0.9.0", "v0.10.0", false),
        UpdateDecision::Install
    );
}

/// A local build from an unreleased commit must not be silently downgraded
/// to the newest published tag.
#[test]
fn a_build_ahead_of_the_release_is_not_downgraded() {
    assert_eq!(
        decide_update("0.2.0", "v0.1.0", false),
        UpdateDecision::RunningNewer
    );
    assert_eq!(
        decide_update("0.10.0", "v0.9.0", false),
        UpdateDecision::RunningNewer
    );
    assert_eq!(compare_versions("0.2", "0.2.0"), Some(Ordering::Equal));
}

/// Numerically equal but textually different tags are ALREADY CURRENT, not
/// an install. Without the `Equal` arm each of these fell through to
/// `Install`: `0.1.0` would download and execute the installer for a
/// PRERELEASE of its own version — a downgrade in everything but the number
/// — and would do it again on every invocation, which is exactly the
/// download-per-run that `equal_versions_skip_the_download` exists to
/// forbid.
#[test]
fn numerically_equal_tags_are_already_current() {
    for tag in [
        "v0.1.0-rc1",
        "v0.1.0+build7",
        "v0.1.0.0",
        "v0.01.0",
        "0.1.0-alpha.1",
    ] {
        assert_eq!(
            decide_update("0.1.0", tag, false),
            UpdateDecision::AlreadyCurrent,
            "tag {tag} is version 0.1.0, so it must not trigger an install"
        );
    }
    // Zero-padded compare: 0.2 and 0.2.0 are the same version, in both
    // directions.
    assert_eq!(
        decide_update("0.2", "v0.2.0", false),
        UpdateDecision::AlreadyCurrent
    );
    assert_eq!(
        decide_update("0.2.0", "v0.2", false),
        UpdateDecision::AlreadyCurrent
    );
    // A local pre-release of the same number is not an update either.
    assert_eq!(
        decide_update("0.1.0-dev", "v0.1.0", false),
        UpdateDecision::AlreadyCurrent
    );
    // …and none of this weakens --force.
    assert_eq!(
        decide_update("0.1.0", "v0.1.0-rc1", true),
        UpdateDecision::Install
    );
}

/// `--force` is the escape hatch for a version string that lies (a binary
/// rebuilt from a dirty tree still reports the released version), so it must
/// bypass EVERY skip branch, not just the equal one.
#[test]
fn force_always_installs() {
    assert_eq!(
        decide_update("0.1.0", "v0.1.0", true),
        UpdateDecision::Install
    );
    assert_eq!(
        decide_update("0.2.0", "v0.1.0", true),
        UpdateDecision::Install
    );
    assert_eq!(
        decide_update("not-a-version", "v0.1.0", true),
        UpdateDecision::Install
    );
}

/// An unreadable version must NOT be mistaken for a match: failing to parse
/// a string is not evidence that we are up to date.
#[test]
fn unparseable_versions_install_rather_than_skip() {
    assert_eq!(
        decide_update("main", "v0.1.0", false),
        UpdateDecision::Install
    );
    assert_eq!(
        decide_update("0.1.0", "nightly", false),
        UpdateDecision::Install
    );
    assert_eq!(compare_versions("main", "0.1.0"), None);
    assert_eq!(version_parts("0.1.x"), None);
    assert_eq!(version_parts("0.1.0-rc1"), Some(vec![0, 1, 0]));
}

/// The version this binary reports is what gets compared, so it has to be a
/// version at all — the release tags are cut FROM it.
#[test]
fn the_crate_version_is_comparable() {
    assert!(
        version_parts(env!("CARGO_PKG_VERSION")).is_some(),
        "CARGO_PKG_VERSION {} has no numeric core, so decide_update can never \
             report AlreadyCurrent",
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(
        decide_update(
            env!("CARGO_PKG_VERSION"),
            &format!("v{}", env!("CARGO_PKG_VERSION")),
            false
        ),
        UpdateDecision::AlreadyCurrent
    );
}

// -- installed-copy update: URLs and JSON --------------------------------

#[test]
fn asset_and_api_urls_are_built_from_one_repo_constant() {
    assert_eq!(
        latest_release_api_url(),
        "https://api.github.com/repos/dhkts1/tcr/releases/latest"
    );
    assert_eq!(
        installer_url_for_tag("v0.1.0").unwrap(),
        "https://github.com/dhkts1/tcr/releases/download/v0.1.0/tcr-installer.sh"
    );
    // Tag-pinned, NOT /releases/latest/download/…: a release cut between the
    // API query and the download would otherwise install a version we never
    // compared against.
    assert!(
        !installer_url_for_tag("v0.1.0")
            .unwrap()
            .contains("/latest/")
    );
}

/// Mirrors `asset_and_api_urls_are_built_from_one_repo_constant` for the
/// dmg asset: the `v` is dropped from the file name (the release workflow
/// names the dmg `TcrBar-<version>.dmg`) but kept in the tag-pinned path.
#[test]
fn dmg_url_drops_the_v_from_the_filename_not_the_path() {
    assert_eq!(
        dmg_url_for_tag("v0.2.32").unwrap(),
        "https://github.com/dhkts1/tcr/releases/download/v0.2.32/TcrBar-0.2.32.dmg"
    );
    // A tag with no leading `v` names a file equal to itself.
    assert_eq!(
        dmg_url_for_tag("0.2.32").unwrap(),
        "https://github.com/dhkts1/tcr/releases/download/0.2.32/TcrBar-0.2.32.dmg"
    );
}

/// Same guard as `installer_url_for_tag`, same reason: the tag lands in a
/// URL path here too.
#[test]
fn dmg_url_rejects_a_path_escaping_tag() {
    assert!(dmg_url_for_tag("../../../../attacker/evil/releases/download/v1").is_err());
    assert!(dmg_url_for_tag("").is_err());
}

/// The tag is interpolated into a URL PATH, so the repo pin is only real if
/// the tag cannot escape it. `../../../../attacker/evil/releases/download/v1`
/// resolves to an asset on a different account — and that asset is a script
/// we execute.
#[test]
fn a_tag_cannot_escape_the_pinned_repo() {
    for tag in [
        "../../../../attacker/evil/releases/download/v1",
        "..",
        "v1.0.0/../../other",
        "v1.0.0?x=y",
        "v1.0.0#frag",
        "v1.0.0 v2.0.0",
        "-v1.0.0",
        ".v1.0.0",
        "/v1.0.0",
        "v1.0.0%2f..",
        "v1.0.0\nv2",
        "",
    ] {
        assert!(
            installer_url_for_tag(tag).is_err(),
            "tag {tag:?} must be rejected, not turned into a download URL"
        );
        assert!(
            validate_release_tag(tag).is_err(),
            "tag {tag:?} must be rejected"
        );
    }

    // …while every shape a real release tag takes still works.
    for tag in ["v0.1.0", "0.1.0", "v0.1.0-rc1", "v0.1.0+build7", "v1_2"] {
        assert!(
            validate_release_tag(tag).is_ok(),
            "tag {tag:?} is a legitimate release tag and must be accepted"
        );
        assert!(
            installer_url_for_tag(tag)
                .unwrap()
                .starts_with("https://github.com/dhkts1/tcr/releases/download/")
        );
    }

    // The same rejection happens at the API ingress, so a poisoned
    // `tag_name` never even reaches the version comparison.
    assert!(parse_latest_tag(r#"{"tag_name":"../../attacker/evil"}"#).is_err());
}

/// Measured against the published v0.1.0 installer, both directions:
/// `TCR_UNMANAGED_INSTALL=<dir>` installed `<dir>/tcr`, while
/// `CARGO_DIST_FORCE_INSTALL_DIR=<dir>` installed `<dir>/bin/tcr`. Pointing
/// the latter at the running binary's directory would therefore create the
/// second copy this whole path exists to prevent, so the variable name is
/// pinned rather than left to look interchangeable.
#[test]
fn the_install_dir_override_is_the_flat_layout_variable() {
    assert_eq!(UNMANAGED_INSTALL_ENV, "TCR_UNMANAGED_INSTALL");
    assert_ne!(UNMANAGED_INSTALL_ENV, "CARGO_DIST_FORCE_INSTALL_DIR");
}

#[test]
fn parse_latest_tag_reads_the_tag_name() {
    let body = r#"{"tag_name":"v0.1.0","name":"v0.1.0","assets":[{"name":"tcr-installer.sh"}]}"#;
    assert_eq!(parse_latest_tag(body).unwrap(), "v0.1.0");
}

/// The release-query error path: every failure shape must surface as an
/// error, never as a version we then compare against.
#[test]
fn parse_latest_tag_errors_are_not_swallowed() {
    assert!(parse_latest_tag(r#"{"message":"Not Found"}"#).is_err());
    assert!(parse_latest_tag(r#"{"tag_name":null}"#).is_err());
    assert!(parse_latest_tag(r#"{"tag_name":""}"#).is_err());
    assert!(parse_latest_tag(r#"{"tag_name":"   "}"#).is_err());
    assert!(parse_latest_tag(r#"{"tag_name":123}"#).is_err());
    assert!(parse_latest_tag("<html>502 Bad Gateway</html>").is_err());
    assert!(parse_latest_tag("").is_err());
}

// -- installed-copy update: destination ----------------------------------

/// The update must land where the RUNNING binary is. With a symlink on PATH,
/// that is the link's target directory — installing next to the link would
/// leave the link pointing at the old file.
#[test]
fn install_dir_follows_a_symlinked_exe_to_the_real_binary() {
    use std::os::unix::fs::symlink;
    let base = scratch("install-dir");
    let real_dir = base.join("opt").join("tcr-0.1.0");
    fs::create_dir_all(&real_dir).unwrap();
    let real_exe = real_dir.join("tcr");
    fs::write(&real_exe, b"fake").unwrap();

    let bindir = base.join("bin");
    fs::create_dir_all(&bindir).unwrap();
    let link = bindir.join("tcr");
    symlink(&real_exe, &link).unwrap();

    assert_eq!(
        install_dir_for_exe(&link).unwrap(),
        fs::canonicalize(&real_dir).unwrap()
    );
    // A plain copied file (the common install shape) resolves to its own dir.
    assert_eq!(
        install_dir_for_exe(&real_exe).unwrap(),
        fs::canonicalize(&real_dir).unwrap()
    );

    fs::remove_dir_all(&base).ok();
}

/// An exe path that does not exist must error, not degrade into some default
/// install directory that would write a second binary somewhere else.
#[test]
fn install_dir_errors_when_the_exe_cannot_be_resolved() {
    let base = scratch("install-dir-missing");
    assert!(install_dir_for_exe(&base.join("nope").join("tcr")).is_err());
    fs::remove_dir_all(&base).ok();
}

/// The staged installer must be executable and live in a private directory —
/// the system temp root is world-writable.
#[test]
fn stage_installer_writes_a_private_executable_script() {
    use std::os::unix::fs::PermissionsExt as _;
    let staged = stage_installer(b"#!/bin/sh\nexit 0\n").unwrap();

    assert_eq!(fs::read(&staged).unwrap(), b"#!/bin/sh\nexit 0\n");
    assert_eq!(staged.file_name().unwrap(), INSTALLER_ASSET);
    let mode = fs::metadata(&staged).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode & 0o100, 0o100, "installer is not executable: {mode:o}");
    assert_eq!(mode & 0o077, 0, "installer is group/world accessible");

    let dir = staged.parent().unwrap();
    let dir_mode = fs::metadata(dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700, "staging dir is not private: {dir_mode:o}");

    fs::remove_dir_all(dir).ok();
}

/// Two stagings in the same process must not collide — a fixed path in a
/// world-writable temp root is a script-swap window.
#[test]
fn stage_installer_uses_a_fresh_directory_each_time() {
    let a = stage_installer(b"#!/bin/sh\nexit 0\n").unwrap();
    let b = stage_installer(b"#!/bin/sh\nexit 0\n").unwrap();
    assert_ne!(a.parent(), b.parent());
    fs::remove_dir_all(a.parent().unwrap()).ok();
    fs::remove_dir_all(b.parent().unwrap()).ok();
}

// -- installed-copy update: the installer child's environment ------------

/// A stand-in for the published installer that reimplements its install-dir
/// precedence chain (installer.sh:657-666) and records where it decided to
/// write. Enough to make an inherited override actually WIN, which is the
/// failure the scrub exists to stop.
fn fake_installer(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let script = dir.join("fake-installer.sh");
    fs::write(
        &script,
        "#!/bin/sh\n\
             if [ -n \"${TCR_INSTALL_DIR:-}\" ]; then\n\
             \x20 d=\"$TCR_INSTALL_DIR\"\n\
             elif [ -n \"${CARGO_DIST_FORCE_INSTALL_DIR:-}\" ]; then\n\
             \x20 d=\"$CARGO_DIST_FORCE_INSTALL_DIR/bin\"\n\
             elif [ -n \"${TCR_UNMANAGED_INSTALL:-}\" ]; then\n\
             \x20 d=\"$TCR_UNMANAGED_INSTALL\"\n\
             else\n\
             \x20 d=\"$HOME/.local/bin\"\n\
             fi\n\
             mkdir -p \"$d\" && printf 'fake' > \"$d/tcr\"\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    script
}

/// Every variable that can redirect the installer must be REMOVED from the
/// child, not merely left unset — the child inherits our whole environment,
/// so "we did not set it" is not the same as "it is not set".
#[test]
fn the_installer_child_has_every_override_scrubbed() {
    let base = scratch("scrub-envs");
    let cmd = installer_command(&base.join("installer.sh"), &base);

    let envs: Vec<(String, Option<String>)> = cmd
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();

    for key in INSTALLER_ENV_OVERRIDES {
        assert!(
            envs.iter().any(|(k, v)| k == key && v.is_none()),
            "{key} is not scrubbed from the installer child: {envs:?}"
        );
    }
    // The six named in review, spelled out so a rename cannot quietly drop
    // one from the array and still pass the loop above.
    for key in [
        "TCR_INSTALL_DIR",
        "CARGO_DIST_FORCE_INSTALL_DIR",
        "TCR_DOWNLOAD_URL",
        "INSTALLER_DOWNLOAD_URL",
        "TCR_INSTALLER_GHE_BASE_URL",
        "TCR_INSTALLER_GITHUB_BASE_URL",
        // …plus the credential, which is what a poisoned URL would steal.
        "TCR_GITHUB_TOKEN",
    ] {
        assert!(
            envs.iter().any(|(k, v)| k == key && v.is_none()),
            "{key} must be removed from the installer child"
        );
    }
    // …and the one variable we DO set survives the scrub.
    assert!(
        envs.iter()
            .any(|(k, v)| k == UNMANAGED_INSTALL_ENV
                && v.as_deref() == Some(&*base.to_string_lossy()))
    );

    fs::remove_dir_all(&base).ok();
}

/// The scrub proved against a variable that would otherwise have WON.
/// `env_remove` overrides an explicit `env()` on the same key exactly as it
/// overrides inheritance, so setting the poison var on the command is a
/// faithful stand-in for finding it in the user's shell — and the control
/// run shows the poison really does beat our override when nothing scrubs
/// it.
#[test]
fn an_inherited_install_dir_cannot_beat_the_scrub() {
    let base = scratch("scrub-wins");
    let script = fake_installer(&base);
    let intended = base.join("intended");
    let attacker = base.join("attacker");
    fs::create_dir_all(&intended).unwrap();

    // CONTROL: no scrub — the inherited variable wins and the binary lands
    // somewhere we never asked for, while `intended` stays empty.
    let control = Command::new(&script)
        .env("TCR_INSTALL_DIR", &attacker)
        .env(UNMANAGED_INSTALL_ENV, &intended)
        .status()
        .unwrap();
    assert!(control.success());
    assert!(
        attacker.join("tcr").exists(),
        "control is broken: the fake installer never honoured the poison var"
    );
    assert!(
        !intended.join("tcr").exists(),
        "control is broken: the poison var did not actually win"
    );
    fs::remove_dir_all(&attacker).unwrap();

    // PROOF: the same poison var, scrubbed by installer_command — the
    // binary lands in the directory we asked for and nowhere else.
    let mut cmd = Command::new(&script);
    cmd.env("TCR_INSTALL_DIR", &attacker);
    scrub_installer_env(&mut cmd);
    cmd.env(UNMANAGED_INSTALL_ENV, &intended);
    assert!(cmd.status().unwrap().success());
    assert!(
        intended.join("tcr").exists(),
        "the scrubbed run did not install into the intended directory"
    );
    assert!(
        !attacker.join("tcr").exists(),
        "the poison var still redirected the install"
    );

    // Same for the cargo-home-layout variable, which appends /bin.
    let second = base.join("second");
    fs::create_dir_all(&second).unwrap();
    let mut cmd = Command::new(&script);
    cmd.env("CARGO_DIST_FORCE_INSTALL_DIR", &attacker);
    scrub_installer_env(&mut cmd);
    cmd.env(UNMANAGED_INSTALL_ENV, &second);
    assert!(cmd.status().unwrap().success());
    assert!(second.join("tcr").exists());
    assert!(!attacker.join("bin").join("tcr").exists());

    fs::remove_dir_all(&base).ok();
}

/// A zero exit is the installer's claim, not an observation. If the binary
/// is not there, the update failed — printing the directory anyway is the
/// same lie as naming a built binary that does not exist.
#[test]
fn success_is_not_claimed_when_the_binary_is_absent() {
    let base = scratch("verify-installed");
    assert!(
        verify_installed_binary(&base, "v0.1.0").is_err(),
        "an empty install dir must not read as a successful install"
    );

    // A second copy under <dir>/bin — the failure shape an unscrubbed
    // CARGO_DIST_FORCE_INSTALL_DIR produces — is still not an install here.
    fs::create_dir_all(base.join("bin")).unwrap();
    fs::write(base.join("bin").join(BINARY_NAME), b"fake").unwrap();
    assert!(verify_installed_binary(&base, "v0.1.0").is_err());

    // And the real thing is accepted, at the path that gets printed.
    fs::write(base.join(BINARY_NAME), b"fake").unwrap();
    assert_eq!(
        verify_installed_binary(&base, "v0.1.0").unwrap(),
        base.join(BINARY_NAME)
    );

    fs::remove_dir_all(&base).ok();
}

/// A staged installer that cannot be executed at all must not leave its
/// directory behind in the world-writable temp root.
#[test]
fn a_failed_spawn_still_removes_the_staging_dir() {
    use std::os::unix::fs::PermissionsExt as _;
    let base = scratch("staging-cleanup");
    let staged = stage_installer(b"#!/bin/sh\nexit 0\n").unwrap();
    let dir = staged.parent().unwrap().to_path_buf();

    // Not executable → the spawn itself fails, which is the path that used
    // to return before any cleanup ran.
    fs::set_permissions(&staged, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(run_installer(&staged, &base).is_err());
    assert!(
        !dir.exists(),
        "the staging directory {} survived a failed spawn",
        dir.display()
    );

    // The success path cleans up too.
    let staged = stage_installer(b"#!/bin/sh\nexit 0\n").unwrap();
    let dir = staged.parent().unwrap().to_path_buf();
    assert!(run_installer(&staged, &base).unwrap().success());
    assert!(!dir.exists());

    fs::remove_dir_all(&base).ok();
}
