//! `tcr setup shell`, `tcr setup claude-settings` and `tcr setup status`, end to
//! end against the BUILT BINARY and a scratch HOME.
//!
//! # Isolation
//!
//! Every run sets `HOME` to a fresh [`tempfile::TempDir`], removes `ZDOTDIR`
//! and `XDG_CONFIG_HOME` so neither can point a write back at the operator's
//! real files, and passes `--config` at a path inside the scratch tree. The
//! variables are set on the spawned `Command`, never on this test process. No
//! test here reads or writes the real `~/.zshrc`, `~/.bashrc`,
//! `~/.bash_profile`, `~/.config/fish` or `~/.claude/settings.json`, and none
//! starts a server: every verb below is a file edit or a read.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The port the scratch config names: one the kernel just handed out and
/// nothing listens on, never the default, so `tcr doctor` below cannot reach
/// the live proxy and a leaked default would show up as a mismatch.
static PORT: std::sync::LazyLock<u16> = std::sync::LazyLock::new(|| {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a scratch port");
    listener.local_addr().expect("local addr").port()
});

struct Scratch {
    _dir: tempfile::TempDir,
    home: PathBuf,
    config: PathBuf,
}

fn scratch() -> Scratch {
    let dir = tempfile::tempdir().expect("scratch HOME");
    let home = dir.path().to_path_buf();
    let config_dir = home.join(".config");
    std::fs::create_dir_all(&config_dir).expect("create the config directory");
    let config = config_dir.join("teamclaude.json");
    std::fs::write(
        &config,
        format!(
            "{{\n  \"accounts\": [],\n  \"proxy\": {{ \"port\": {} }}\n}}\n",
            *PORT
        ),
    )
    .expect("write the config");
    Scratch {
        _dir: dir,
        home,
        config,
    }
}

/// Run `tcr <args…>` with the scratch HOME and `SHELL` set to `shell`.
fn tcr(scratch: &Scratch, shell: &str, args: &[&str]) -> (i32, String, String) {
    let mut command = crate::tcr_env::tcr(&scratch.home);
    command
        .args(args)
        .env("SHELL", shell)
        .current_dir(&scratch.home);
    let output = command.output().expect("run the built tcr");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn shell_setup(scratch: &Scratch, shell: &str, remove: bool) -> (i32, String, String) {
    let mut args = vec!["setup", "shell"];
    if remove {
        args.push("--remove");
    }
    tcr(scratch, shell, &args)
}

fn settings_setup(scratch: &Scratch, extra: &[&str]) -> (i32, String, String) {
    let config = scratch.config.to_string_lossy().into_owned();
    let mut args = vec!["setup", "claude-settings", "--config", config.as_str()];
    args.extend_from_slice(extra);
    tcr(scratch, "/bin/zsh", &args)
}

fn status_json(scratch: &Scratch, shell: &str) -> serde_json::Value {
    let config = scratch.config.to_string_lossy().into_owned();
    let (code, out, err) = tcr(
        scratch,
        shell,
        &["setup", "status", "--json", "--config", config.as_str()],
    );
    assert_eq!(code, 0, "status exits 0: {err}");
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("status --json is JSON ({e}): {out}"))
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

const ZSHRC: &str = "export PATH=\"$HOME/bin:$PATH\"\nsetopt autocd\n";

#[test]
fn zsh_add_is_idempotent_and_remove_restores_the_exact_bytes() {
    let s = scratch();
    let rc = s.home.join(".zshrc");
    std::fs::write(&rc, ZSHRC).expect("seed .zshrc");

    let (code, out, err) = shell_setup(&s, "/bin/zsh", false);
    assert_eq!(code, 0, "add exits 0: {out}{err}");
    let added = read(&rc);
    assert!(
        added.starts_with(ZSHRC),
        "the user's lines stay first: {added}"
    );
    assert!(
        added.contains("# >>> tcr >>>  (added by tcr; remove with: tcr setup shell --remove)"),
        "the start marker is written: {added}"
    );
    assert!(
        added.contains("claude() {"),
        "the function is written: {added}"
    );
    assert!(
        added.contains("# <<< tcr <<<"),
        "the end marker is written: {added}"
    );

    let (code, _, err) = shell_setup(&s, "/bin/zsh", false);
    assert_eq!(code, 0, "re-add exits 0: {err}");
    assert_eq!(read(&rc), added, "a second add changes nothing");

    let (code, _, err) = shell_setup(&s, "/bin/zsh", true);
    assert_eq!(code, 0, "remove exits 0: {err}");
    assert_eq!(read(&rc), ZSHRC, "remove leaves the file byte-identical");
}

#[test]
fn remove_restores_a_file_that_had_no_final_newline() {
    let s = scratch();
    let rc = s.home.join(".zshrc");
    let original = "alias ll='ls -l'";
    std::fs::write(&rc, original).expect("seed .zshrc");

    assert_eq!(shell_setup(&s, "/bin/zsh", false).0, 0);
    assert_ne!(read(&rc), original);
    assert_eq!(shell_setup(&s, "/bin/zsh", true).0, 0);
    assert_eq!(
        read(&rc),
        original,
        "the missing final newline stays missing"
    );
}

#[test]
fn an_alias_outside_the_markers_is_refused_and_named_with_its_line() {
    let s = scratch();
    let rc = s.home.join(".zshrc");
    let original = "setopt autocd\n# a comment about claude\nalias claude='claude --verbose'\n";
    std::fs::write(&rc, original).expect("seed .zshrc");

    let (code, out, err) = shell_setup(&s, "/bin/zsh", false);
    assert_ne!(code, 0, "an alias is refused: {out}{err}");
    let expected = format!("{}:3", rc.display());
    assert!(err.contains(&expected), "names file:line {expected}: {err}");
    assert!(
        err.contains("alias claude='claude --verbose'"),
        "prints the line found: {err}"
    );
    assert_eq!(read(&rc), original, "a refused add writes nothing");
}

#[test]
fn a_function_in_another_startup_file_is_refused() {
    let s = scratch();
    std::fs::write(s.home.join(".zshrc"), ZSHRC).expect("seed .zshrc");
    let zshenv = s.home.join(".zshenv");
    std::fs::write(&zshenv, "function claude {\n  echo mine\n}\n").expect("seed .zshenv");

    let (code, _, err) = shell_setup(&s, "/bin/zsh", false);
    assert_ne!(code, 0, "a function is refused: {err}");
    assert!(err.contains(&format!("{}:1", zshenv.display())), "{err}");
    assert_eq!(read(&s.home.join(".zshrc")), ZSHRC);
}

#[test]
fn bash_writes_bashrc_and_the_login_file_unless_it_already_sources_bashrc() {
    let s = scratch();
    let bashrc = s.home.join(".bashrc");
    let profile = s.home.join(".bash_profile");
    std::fs::write(&bashrc, "export EDITOR=vi\n").expect("seed .bashrc");
    std::fs::write(&profile, "[ -f ~/.bashrc ] && . ~/.bashrc\n").expect("seed .bash_profile");

    assert_eq!(shell_setup(&s, "/bin/bash", false).0, 0);
    assert!(read(&bashrc).contains("# >>> tcr >>>"));
    assert_eq!(
        read(&profile),
        "[ -f ~/.bashrc ] && . ~/.bashrc\n",
        "a login file that sources .bashrc is left alone"
    );
    assert_eq!(shell_setup(&s, "/bin/bash", true).0, 0);
    assert_eq!(read(&bashrc), "export EDITOR=vi\n");
}

#[test]
fn fish_gets_its_own_function_file_and_remove_deletes_it() {
    let s = scratch();
    let file = s.home.join(".config/fish/functions/claude.fish");

    assert_eq!(shell_setup(&s, "/usr/local/bin/fish", false).0, 0);
    let text = read(&file);
    assert!(text.contains("function claude"), "{text}");
    assert!(
        text.contains("tcr run -- $argv") || text.contains("run -- $argv"),
        "{text}"
    );

    assert_eq!(shell_setup(&s, "/usr/local/bin/fish", true).0, 0);
    assert!(!file.exists(), "remove deletes the file tcr wrote");
}

#[test]
fn a_fish_function_file_tcr_did_not_write_is_refused() {
    let s = scratch();
    let dir = s.home.join(".config/fish/functions");
    std::fs::create_dir_all(&dir).expect("create fish functions dir");
    let file = dir.join("claude.fish");
    std::fs::write(&file, "function claude\n    echo mine\nend\n").expect("seed claude.fish");

    let (code, _, err) = shell_setup(&s, "/usr/local/bin/fish", false);
    assert_ne!(code, 0, "{err}");
    assert!(err.contains(&file.display().to_string()), "{err}");
    assert_eq!(read(&file), "function claude\n    echo mine\nend\n");
}

#[test]
fn a_scratch_zsh_resolves_claude_to_the_function() {
    if !Path::new("/bin/zsh").exists() {
        eprintln!("no /bin/zsh on this machine; skipping the end-to-end check");
        return;
    }
    let s = scratch();
    let rc = s.home.join(".zshrc");
    std::fs::write(&rc, ZSHRC).expect("seed .zshrc");
    assert_eq!(shell_setup(&s, "/bin/zsh", false).0, 0);

    let output = Command::new("/bin/zsh")
        .arg("-f")
        .arg("-c")
        .arg(format!("source '{}'; whence -w claude", rc.display()))
        .env("HOME", &s.home)
        .env_remove("ZDOTDIR")
        .output()
        .expect("run a scratch zsh");
    let out = String::from_utf8_lossy(&output.stdout);
    assert_eq!(out.trim(), "claude: function", "{out}");
}

#[test]
fn doctor_counts_the_shell_route_as_on_route() {
    let s = scratch();
    std::fs::write(s.home.join(".zshrc"), ZSHRC).expect("seed .zshrc");
    let config = s.config.to_string_lossy().into_owned();
    let doctor = ["doctor", "--config", config.as_str()];

    let (code, out, _) = tcr(&s, "/bin/zsh", &doctor);
    assert_eq!(code, 2, "no route yet: {out}");
    assert!(out.contains("shellRoute: not installed"), "{out}");

    assert_eq!(shell_setup(&s, "/bin/zsh", false).0, 0);
    let (code, out, _) = tcr(&s, "/bin/zsh", &doctor);
    // 3, not 0: the route reaches this port and no proxy answers on it.
    assert_eq!(code, 3, "{out}");
    assert!(out.contains("goes through tcr run"), "{out}");
}

const SETTINGS: &str = "{\n  \"model\": \"opus\",\n  \"permissions\": {\n    \"allow\": [\"Bash(ls)\"]\n  },\n  \"env\": {\n    \"FOO\": \"bar\"\n  }\n}\n";

#[test]
fn claude_settings_merges_one_key_and_remove_restores_the_exact_bytes() {
    let s = scratch();
    let dir = s.home.join(".claude");
    std::fs::create_dir_all(&dir).expect("create .claude");
    let path = dir.join("settings.json");
    std::fs::write(&path, SETTINGS).expect("seed settings.json");

    let (code, out, err) = settings_setup(&s, &[]);
    assert_eq!(code, 0, "{out}{err}");
    let written: serde_json::Value = serde_json::from_str(&read(&path)).expect("still JSON");
    assert_eq!(
        written["env"]["ANTHROPIC_BASE_URL"],
        format!("http://127.0.0.1:{}", *PORT),
        "the port comes from the tcr config"
    );
    assert_eq!(written["env"]["FOO"], "bar", "other env keys stay");
    assert_eq!(written["model"], "opus", "other keys stay");
    assert_eq!(written["permissions"]["allow"][0], "Bash(ls)");

    let again = read(&path);
    assert_eq!(settings_setup(&s, &[]).0, 0);
    assert_eq!(read(&path), again, "a second add changes nothing");

    assert_eq!(settings_setup(&s, &["--remove"]).0, 0);
    assert_eq!(read(&path), SETTINGS, "remove takes out only tcr's key");
}

#[test]
fn claude_settings_without_an_env_block_round_trips() {
    let s = scratch();
    let dir = s.home.join(".claude");
    std::fs::create_dir_all(&dir).expect("create .claude");
    let path = dir.join("settings.json");
    let original = "{\n  \"model\": \"opus\"\n}\n";
    std::fs::write(&path, original).expect("seed settings.json");

    assert_eq!(settings_setup(&s, &[]).0, 0);
    assert!(read(&path).contains("ANTHROPIC_BASE_URL"));
    assert_eq!(settings_setup(&s, &["--remove"]).0, 0);
    assert_eq!(read(&path), original);
}

#[test]
fn a_foreign_base_url_is_refused_unless_forced() {
    let s = scratch();
    let dir = s.home.join(".claude");
    std::fs::create_dir_all(&dir).expect("create .claude");
    let path = dir.join("settings.json");
    let original =
        "{\n  \"env\": {\n    \"ANTHROPIC_BASE_URL\": \"https://gateway.example:9443\"\n  }\n}\n";
    std::fs::write(&path, original).expect("seed settings.json");

    let (code, _, err) = settings_setup(&s, &[]);
    assert_ne!(code, 0, "{err}");
    assert!(
        err.contains("https://gateway.example:9443"),
        "names the value: {err}"
    );
    assert_eq!(read(&path), original, "a refused add writes nothing");

    let (code, _, err) = settings_setup(&s, &["--remove"]);
    assert_ne!(code, 0, "remove refuses a value tcr did not write: {err}");
    assert_eq!(read(&path), original);

    assert_eq!(settings_setup(&s, &["--force"]).0, 0);
    let written: serde_json::Value = serde_json::from_str(&read(&path)).expect("still JSON");
    assert_eq!(
        written["env"]["ANTHROPIC_BASE_URL"],
        format!("http://127.0.0.1:{}", *PORT)
    );
}

#[test]
fn status_reports_each_route_and_the_conflicts() {
    let s = scratch();
    std::fs::write(s.home.join(".zshrc"), ZSHRC).expect("seed .zshrc");

    let status = status_json(&s, "/bin/zsh");
    assert_eq!(status["route"], "off", "{status}");
    assert_eq!(status["shell"], "zsh");
    assert_eq!(status["shellRoute"]["installed"], false);
    assert_eq!(status["claudeSettings"]["installed"], false);
    assert_eq!(status["conflicts"].as_array().map(Vec::len), Some(0));

    assert_eq!(shell_setup(&s, "/bin/zsh", false).0, 0);
    let status = status_json(&s, "/bin/zsh");
    assert_eq!(status["route"], "shell", "{status}");
    assert_eq!(status["shellRoute"]["installed"], true);

    assert_eq!(settings_setup(&s, &[]).0, 0);
    let status = status_json(&s, "/bin/zsh");
    assert_eq!(status["route"], "both", "{status}");
    assert_eq!(status["claudeSettings"]["installed"], true);

    assert_eq!(shell_setup(&s, "/bin/zsh", true).0, 0);
    let status = status_json(&s, "/bin/zsh");
    assert_eq!(status["route"], "everyLaunch", "{status}");
}

#[test]
fn status_names_a_conflict_without_writing_anything() {
    let s = scratch();
    let rc = s.home.join(".zshrc");
    let original = "alias claude=\"cmux-claude\"\n";
    std::fs::write(&rc, original).expect("seed .zshrc");
    let dir = s.home.join(".claude");
    std::fs::create_dir_all(&dir).expect("create .claude");
    std::fs::write(
        dir.join("settings.json"),
        "{\"env\":{\"ANTHROPIC_BASE_URL\":\"https://gateway.example:9443\"}}",
    )
    .expect("seed settings.json");

    let status = status_json(&s, "/bin/zsh");
    let conflicts = status["conflicts"].as_array().expect("conflicts array");
    assert_eq!(conflicts.len(), 2, "{status}");
    assert!(
        conflicts
            .iter()
            .any(|c| c["kind"] == "alias" && c["line"] == 1 && c["route"] == "shell"),
        "{status}"
    );
    assert!(
        conflicts
            .iter()
            .any(|c| c["kind"] == "baseUrl" && c["text"] == "https://gateway.example:9443"),
        "{status}"
    );
    assert_eq!(read(&rc), original);
}
