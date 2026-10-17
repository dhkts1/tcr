//! `tcr-run` is `tcr run` under another name, end to end against the built
//! binary.
//!
//! # Isolation
//!
//! A scratch HOME holds a config whose port the kernel just handed out and
//! nothing listens on, so `tcr run` takes its proxy-down path and never
//! connects to the live proxy. `claude` is a stub script (`TCR_CLAUDE_BIN`)
//! that prints its arguments one per line. `PATH` is a scratch `bin` holding
//! a `tcr` symlink to the built binary, so `tcr setup tcr-run` links inside
//! the scratch tree and nowhere else.

use std::path::{Path, PathBuf};
use std::process::Command;

struct Scratch {
    _dir: tempfile::TempDir,
    home: PathBuf,
    bin: PathBuf,
    stub: PathBuf,
}

fn scratch() -> Scratch {
    let dir = tempfile::tempdir().expect("scratch HOME");
    let home = dir.path().to_path_buf();
    let port = tcr::test_support::loopback::unused_port();
    std::fs::create_dir_all(home.join(".config")).expect("config dir");
    std::fs::write(
        home.join(".config/tcr.json"),
        format!("{{\n  \"accounts\": [],\n  \"proxy\": {{ \"port\": {port} }}\n}}\n"),
    )
    .expect("write the config");
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).expect("bin dir");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_tcr"), bin.join("tcr")).expect("link tcr");
    let stub = home.join("claude-stub");
    std::fs::write(
        &stub,
        "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done\n",
    )
    .expect("write the stub");
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    Scratch {
        _dir: dir,
        home,
        bin,
        stub,
    }
}

fn run(s: &Scratch, program: &Path, args: &[&str]) -> (i32, String, String) {
    let mut command = Command::new(program);
    crate::tcr_env::isolate(&mut command, &s.home);
    let output = command
        .args(args)
        .env("PATH", format!("{}:/usr/bin:/bin", s.bin.display()))
        .env("TCR_CLAUDE_BIN", &s.stub)
        .current_dir(&s.home)
        .output()
        .expect("run");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn tcr_run_alias_passes_every_argument_like_tcr_run() {
    let s = scratch();
    let tcr = s.bin.join("tcr");
    let (code, out, err) = run(&s, &tcr, &["setup", "tcr-run"]);
    assert_eq!(code, 0, "{out}{err}");
    let link = s.bin.join("tcr-run");
    assert_eq!(
        std::fs::read_link(&link).expect("a symlink"),
        PathBuf::from("tcr")
    );

    for args in [
        vec!["-p", "hi"],
        vec!["--model", "opus", "-p", "two words"],
        vec!["--", "-p", "hi"],
    ] {
        let through_run: Vec<&str> = std::iter::once("run").chain(args.iter().copied()).collect();
        let (run_code, run_out, run_err) = run(&s, &tcr, &through_run);
        let (alias_code, alias_out, alias_err) = run(&s, &link, &args);
        assert_eq!(run_code, 0, "tcr run {args:?}: {run_err}");
        assert_eq!(alias_code, run_code, "tcr-run {args:?}: {alias_err}");
        assert_eq!(
            alias_out, run_out,
            "tcr-run {args:?} and tcr run {args:?} differ"
        );
    }

    // The flags reach claude untouched, without a `--`.
    let (_, out, _) = run(&s, &link, &["-p", "hi"]);
    assert_eq!(out, "-p\nhi\n");
}

#[test]
fn setup_tcr_run_refuses_a_file_and_removes_only_its_own_link() {
    let s = scratch();
    let tcr = s.bin.join("tcr");
    let link = s.bin.join("tcr-run");
    std::fs::write(&link, "#!/bin/sh\necho mine\n").expect("seed a foreign tcr-run");

    let (code, _, err) = run(&s, &tcr, &["setup", "tcr-run"]);
    assert_ne!(code, 0, "a file is refused: {err}");
    assert_eq!(
        std::fs::read_to_string(&link).expect("still there"),
        "#!/bin/sh\necho mine\n"
    );
    let (code, _, err) = run(&s, &tcr, &["setup", "tcr-run", "--remove"]);
    assert_ne!(code, 0, "remove refuses a file: {err}");
    assert!(link.exists());

    std::fs::remove_file(&link).expect("clear it");
    assert_eq!(run(&s, &tcr, &["setup", "tcr-run"]).0, 0);
    assert_eq!(
        run(&s, &tcr, &["setup", "tcr-run"]).0,
        0,
        "a second link is a no-op"
    );
    assert_eq!(run(&s, &tcr, &["setup", "tcr-run", "--remove"]).0, 0);
    assert!(
        std::fs::symlink_metadata(&link).is_err(),
        "the link is gone"
    );
}
