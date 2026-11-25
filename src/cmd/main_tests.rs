//! Tests of the clap surface `main.rs` defines: flags that must conflict or parse, help text,
//! and every `tcr` command a message names parsing. Declared from `main.rs` by path, so it sits
//! with the binary's other modules rather than widening `src/`.

use super::*;
use tcr::verbs::codex_route::no_codex_account_reason;

/// Every `--config` flag, on every verb, says the default once and the same
/// way.
#[test]
fn every_config_flag_states_the_default_file() {
    fn walk(cmd: &clap::Command, path: &str, out: &mut Vec<(String, String, String)>) {
        for arg in cmd.get_arguments() {
            if arg.get_long() == Some("config") {
                out.push((
                    path.to_string(),
                    arg.get_help().map_or_default(ToString::to_string),
                    arg.get_long_help().map_or_default(ToString::to_string),
                ));
            }
        }
        for sub in cmd.get_subcommands() {
            walk(sub, &format!("{path} {}", sub.get_name()), out);
        }
    }
    let mut flags = Vec::new();
    walk(&cli_command(), "tcr", &mut flags);
    // 31 doc comments carried the default before; a few verbs share one
    // args struct, so the walk sees at least that many flags.
    assert!(
        flags.len() >= 31,
        "found only {} --config flags",
        flags.len()
    );
    let sentence = tcr::base::paths::config_flag_default_help();
    for (verb, help, long_help) in &flags {
        assert!(
            help.ends_with(&format!("{sentence}.")),
            "`{verb} --config` help must end with the default: {help:?}"
        );
        assert_eq!(help.matches("Default:").count(), 1, "`{verb}`: {help:?}");
        // A hand-written "(default: ...)" beside the generated sentence
        // states the default twice (the doctor flag did, once).
        assert_eq!(
            help.to_ascii_lowercase().matches("default:").count(),
            1,
            "`{verb} --config` states its default more than once: {help:?}"
        );
        assert!(
            long_help.is_empty() || long_help.ends_with(&format!("{sentence}.")),
            "`{verb} --config` long help must end with the default: {long_help:?}"
        );
    }
    // And what a user sees on screen.
    let rendered = cli_command()
        .find_subcommand_mut("accounts")
        .expect("an accounts verb")
        .render_help()
        .to_string();
    assert!(
        rendered.contains(&format!("~/.config/{}", tcr::base::paths::CONFIG_FILE)),
        "{rendered}"
    );
}

/// `--no-replace` is documented as a deprecated no-op, and used to be wired as
/// a SILENT VETO over `--replace`: an operator whose launchd plist or alias
/// already carried it, adding `--replace` to force a rebuilt binary onto the
/// port, took over nothing and got exit 0. clap must reject the contradiction
/// by name instead.
#[test]
fn replace_and_no_replace_together_are_a_usage_error() {
    // `let Err(..) else`, not `expect_err`: the Ok side is `Cli`, which does
    // not implement Debug (nor should it — it would print the config path).
    let Err(err) = Cli::try_parse_from(["tcr", "server", "--replace", "--no-replace"]) else {
        panic!("the pair is a contradiction, not a precedence puzzle — clap accepted it");
    };
    assert_eq!(
        err.kind(),
        clap::error::ErrorKind::ArgumentConflict,
        "it must fail as a conflict, not as some other parse error: {err}"
    );
    let rendered = err.to_string();
    assert!(
        rendered.contains("--no-replace") && rendered.contains("--replace"),
        "the message must name BOTH flags so the operator knows what to remove: {rendered}"
    );
}

/// `--token` and `--from-claude-code` are two different credentials from
/// two different places; asking for both is a contradiction, not a
/// precedence puzzle. clap must reject it by name rather than let one
/// silently win.
#[test]
fn token_and_from_claude_code_together_are_a_usage_error() {
    // `let Err(..) else`, not `expect_err`: the Ok side is `Cli`, which
    // does not implement Debug (nor should it — it would print the config
    // path).
    let Err(err) = Cli::try_parse_from(["tcr", "login", "--token", "--from-claude-code"]) else {
        panic!("--token with --from-claude-code must not parse");
    };
    assert_eq!(
        err.kind(),
        clap::error::ErrorKind::ArgumentConflict,
        "it must fail as a conflict, not as some other parse error: {err}"
    );
    let rendered = err.to_string();
    assert!(
        rendered.contains("--token") && rendered.contains("--from-claude-code"),
        "the message must name BOTH flags so the operator knows what to remove: {rendered}"
    );
}

/// Each alone still parses, and `--from-claude-code` combines with the
/// flags a browser login combines with — `--account` especially, which
/// `--token` has to refuse (an inference-only credential has no identity
/// to confirm; this one does).
#[test]
fn from_claude_code_parses_alone_and_with_account_and_name() {
    for args in [
        vec!["tcr", "login", "--from-claude-code"],
        vec!["tcr", "login", "--from-claude-code", "--account", "work"],
        vec!["tcr", "login", "--from-claude-code", "--name", "work"],
        vec!["tcr", "login", "--from-claude-code", "--force"],
    ] {
        Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("{args:?} must parse: {e}"));
    }
}

/// `--from-codex` is a third credential from a third place; asking for it
/// with either of the other two is a contradiction clap must reject by name,
/// exactly as it rejects `--token` with `--from-claude-code`.
#[test]
fn from_codex_with_token_or_from_claude_code_is_a_usage_error() {
    for other in ["--token", "--from-claude-code"] {
        let Err(err) = Cli::try_parse_from(["tcr", "login", "--from-codex", other]) else {
            panic!("--from-codex with {other} must not parse");
        };
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::ArgumentConflict,
            "it must fail as a conflict, not as some other parse error: {err}"
        );
        let rendered = err.to_string();
        assert!(
            rendered.contains("--from-codex") && rendered.contains(other),
            "the message must name BOTH flags so the operator knows what to remove: {rendered}"
        );
    }
}

/// `--codex-home` means something only to the Codex import. Accepted alone,
/// every other login would ignore it in silence, so clap refuses it.
#[test]
fn codex_home_without_from_codex_is_a_usage_error() {
    let Err(err) = Cli::try_parse_from(["tcr", "login", "--codex-home", "/tmp/x"]) else {
        panic!("--codex-home without --from-codex must not parse");
    };
    assert_eq!(
        err.kind(),
        clap::error::ErrorKind::MissingRequiredArgument,
        "it must fail as the missing --from-codex: {err}"
    );
}

#[test]
fn from_codex_parses_alone_and_with_codex_home_name_and_account() {
    for args in [
        vec!["tcr", "login", "--from-codex"],
        vec![
            "tcr",
            "login",
            "--from-codex",
            "--codex-home",
            "/tmp/x",
            "--name",
            "alice",
        ],
        vec![
            "tcr",
            "login",
            "--from-codex",
            "--account",
            "alice@example.com",
        ],
    ] {
        Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("{args:?} must parse: {e}"));
    }
}

/// Every `tcr` command the "proxy too old" warnings tell the user to run
/// must be one this binary accepts. All four named a `restart` subcommand,
/// which clap rejected with exit 2: the remedy in the warning was itself an
/// error.
#[test]
fn restart_hint_names_only_commands_that_parse() {
    let commands: Vec<&str> = tcr::proxy::live_client::RESTART_HINT
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|span| span.starts_with("tcr "))
        .collect();
    assert!(
        !commands.is_empty(),
        "the hint must name a tcr command to run: {}",
        tcr::proxy::live_client::RESTART_HINT
    );
    for command in commands {
        let argv: Vec<&str> = command.split_whitespace().collect();
        if let Err(e) = Cli::try_parse_from(&argv) {
            panic!("the warning tells the user to run `{command}`, which does not parse: {e}");
        }
    }
}

/// Every `tcr` command a Codex login or launch tells someone to run parses: each production
/// string that names one in backticks is split on its backticks, and each span starting
/// `tcr ` goes through the real CLI parser, as does each command a refusal over a running
/// proxy echoes. A remedy naming a flag clap refuses sends its reader into a usage error.
///
/// Watched red: one flag misspelled in one production string panics naming that span.
#[test]
fn codex_remedies_name_only_commands_that_parse() {
    let workspace = login::codex_relogin_workspace_refusal(
        Some("erin@example.com"),
        "99999999-9999-4999-8999-999999999999",
        Some("business"),
        "work",
        "88888888-8888-4888-8888-888888888888",
        Some("pro"),
    );
    let strings = [
        login::codex_account_login_refusal("bob@example.com/codex"),
        login::claude_account_codex_login_refusal("alice@example.com"),
        login::claude_account_codex_import_refusal("alice@example.com"),
        login::codex_identity_refusal("carol@example.com", "erin@example.com/codex").reason,
        workspace.reason,
        no_codex_account_reason(3999, "chatgpt.com"),
        tcr::clients::oauth::NO_REFRESH_CODEX_MESSAGE.to_string(),
        login::no_codex_login_message(Path::new("/Users/example/.codex/auth.json")),
        login::codex_cli::codex_login_saved_line("bob@example.com", Some("ChatGPT Pro")),
        login::CODEX_IMPORT_RUN_LINE.to_string(),
        login::CODEX_FIRST_ACCOUNT_NOTICE.to_string(),
    ];
    let mut commands: Vec<String> = Vec::new();
    for text in &strings {
        let spans: Vec<&str> = text
            .split('`')
            .skip(1)
            .step_by(2)
            .filter(|span| span.starts_with("tcr "))
            .collect();
        assert!(!spans.is_empty(), "names no tcr command: {text}");
        commands.extend(spans.iter().map(ToString::to_string));
    }
    commands.extend(
        [
            login::LOGIN_COMMAND,
            login::LOGIN_FROM_CODEX_COMMAND,
            login::LOGIN_FROM_CLAUDE_CODE_COMMAND,
            login::LOGIN_CODEX_COMMAND,
            login::IMPORT_COMMAND,
        ]
        .map(str::to_string),
    );
    for command in commands {
        let argv: Vec<&str> = command.split_whitespace().collect();
        if let Err(e) = Cli::try_parse_from(&argv) {
            panic!("a Codex remedy tells the user to run `{command}`, which does not parse: {e}");
        }
    }
}

/// The help says what `--non-interactive` does under `--codex`, where `codex login` opens
/// the browser itself and nothing times the sign-in out: the `browser` line's
/// `"opened":true` tells the caller to open nothing, and SIGTERM is how it ends an abandoned
/// sign-in. And `tcr peer ls --help` names the Codex exit pins its text form reads from the
/// config.
///
/// Watched red: the old help says the caller opens the URL and the same 2-minute timeout
/// applies, and `peer ls` takes nothing from the config but the account labels.
#[test]
fn the_help_tells_the_truth_about_codex_logins_and_pins() {
    use clap::CommandFactory as _;
    let cli = Cli::command();
    let help = |path: &[&str], id: &str| -> String {
        let mut command = &cli;
        for name in path {
            command = command
                .find_subcommand(name)
                .unwrap_or_else(|| panic!("no {name} subcommand"));
        }
        let arg = command
            .get_arguments()
            .find(|arg| arg.get_id() == id)
            .unwrap_or_else(|| panic!("no {id} argument"));
        arg.get_long_help()
            .or_else(|| arg.get_help())
            .map_or_default(ToString::to_string)
    };
    let login = help(&["login"], "non_interactive");
    for fact in [r#""opened":true"#, "`--codex` has no timeout", "SIGTERM"] {
        assert!(
            login.contains(fact),
            "`tcr login --help` must say {fact}:\n{login}"
        );
    }
    let import = help(&["login"], "from_codex");
    for fact in ["auth.json", "only while", "same ChatGPT account"] {
        assert!(import.contains(fact), "{import}");
    }
    // `tcr import --yes` states the take before anyone runs it: the other tool's copy dies,
    // and tcr's own rows are never overwritten. Watched red with the help naming neither.
    let take = help(&["import"], "yes");
    for fact in ["only lists", "single-use", "never written", "non-zero"] {
        assert!(
            take.contains(fact),
            "`tcr import --help` must say {fact}:\n{take}"
        );
    }
    assert!(!import.contains("asks you to log in again"), "{import}");
    let ls = help(&["peer", "ls"], "config");
    assert!(
        ls.contains("Codex exit pins"),
        "`tcr peer ls --help` must name the Codex exit pins it reads:\n{ls}"
    );
    assert!(!ls.contains("Nothing else is taken from it"), "{ls}");
}

/// Every command the `login` row of docs/troubleshooting.md's gate table tells a user to run
/// parses, and its `--from-codex` re-import names the folder to read: without
/// `--codex-home`, `--from-codex` reads `$CODEX_HOME` or `~/.codex`, never the folder the
/// row records, so an account imported from another folder would be refused.
///
/// Watched red: the row said `tcr login --from-codex --account <name>` imports the login
/// again "from the folder it came from".
#[test]
fn the_troubleshooting_login_remedies_parse_and_name_the_folder() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/troubleshooting.md");
    let doc =
        std::fs::read_to_string(path).unwrap_or_else(|err| panic!("cannot read {path}: {err}"));
    let row = doc
        .lines()
        .find(|line| line.starts_with("| `login` |"))
        .expect("docs/troubleshooting.md has a `login` row");
    let commands: Vec<String> = row
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|span| span.starts_with("tcr "))
        .map(|span| {
            span.replace("<name>", "bob@example.com/codex")
                .replace("<folder>", "/Users/example/.codex-second")
        })
        .collect();
    assert!(
        commands.iter().any(|command| command.contains("--codex ")),
        "positive control, the row's `--codex` remedy: {commands:?}"
    );
    for command in &commands {
        let argv: Vec<&str> = command.split_whitespace().collect();
        if let Err(e) = Cli::try_parse_from(&argv) {
            panic!("the login row tells the user to run `{command}`, which does not parse: {e}");
        }
        if command.contains("--from-codex") {
            assert!(
                command.contains("--codex-home "),
                "`{command}` reads `$CODEX_HOME` or ~/.codex, not the folder the row records"
            );
        }
    }
    assert!(
        commands
            .iter()
            .any(|command| command.contains("--from-codex")),
        "the row names no `--from-codex` re-import: {commands:?}"
    );
}

/// The command `tcr doctor`'s pooled Codex verdict names parses, in the sentence for one
/// request and in the one for several.
#[test]
fn the_pooled_codex_verdict_names_a_command_that_parses() {
    for requests in [1, 2] {
        let verdict = tcr::verbs::doctor::CodexReport {
            port: 3999,
            rows: Vec::new(),
            route: tcr_status_wire::CodexRoute {
                pooled: true,
                code: None,
                reason: None,
                upstream: None,
            },
            ca: PathBuf::from("/Users/example/.config/tcr-ca.pem"),
            ca_readable: true,
            upstream: "https://chatgpt.com".to_string(),
            upstream_is_default: true,
            bin: Ok((PathBuf::from("/opt/homebrew/bin/codex"), "PATH".to_string())),
            env: tcr::verbs::doctor::CodexEnv::default(),
            login: PathBuf::from("/Users/example/.codex/auth.json"),
            login_exists: true,
            requests_10m: Some(requests),
            traffic: None,
        }
        .verdict();
        let commands: Vec<&str> = verdict
            .split('`')
            .skip(1)
            .step_by(2)
            .filter(|span| span.starts_with("tcr "))
            .collect();
        assert!(!commands.is_empty(), "names no tcr command: {verdict}");
        for command in commands {
            let argv: Vec<&str> = command.split_whitespace().collect();
            if let Err(e) = Cli::try_parse_from(&argv) {
                panic!("the verdict names `{command}`, which does not parse: {e}");
            }
        }
    }
}

/// `tcr login --help` carries `--codex` as a whole token, beside the tokens TcrBar already
/// probes for: `LoginCapability` decides between the in-app Codex sign-in and Terminal by
/// finding it, so a help that renamed it would silently send every Codex sign-in to
/// Terminal.
#[test]
fn login_help_lists_codex_as_a_whole_token() {
    use clap::CommandFactory as _;
    let mut cli = Cli::command();
    let login = cli
        .find_subcommand_mut("login")
        .expect("tcr has a login subcommand");
    let help = login.render_long_help().to_string();
    for token in [
        "--codex",
        "--non-interactive",
        "--from-codex",
        "--codex-home",
    ] {
        assert!(
            tcr::clients::codex_local::help_has_whole_token(&help, token),
            "`tcr login --help` must carry {token} as a whole token:\n{help}"
        );
    }
}

/// The `tcr group` argv TcrBar sends still parses, unchanged: the panel shells
/// out to these shapes and never passes `--force`, which is additive. A
/// reserve without it is not forced; with it, it is.
#[test]
fn the_group_argv_tcrbar_sends_is_unchanged() {
    for argv in [
        &["tcr", "group", "add", "orchard", "alice@example.com"][..],
        &["tcr", "group", "rm", "orchard", "alice@example.com"],
        &["tcr", "group", "rm", "orchard", "--all"],
        &["tcr", "group", "park", "orchard"],
        &["tcr", "group", "unpark", "orchard"],
    ] {
        if let Err(e) = Cli::try_parse_from(argv) {
            panic!("TcrBar sends {argv:?}, which no longer parses: {e}");
        }
    }

    let reserve = |argv: &[&str]| -> GroupReserveArgs {
        match Cli::try_parse_from(argv) {
            Ok(Cli {
                command:
                    Some(Command::Group(GroupArgs {
                        action: GroupAction::Reserve(args),
                    })),
                ..
            }) => args,
            Ok(_) => panic!("{argv:?} parsed to something other than a reserve"),
            Err(e) => panic!("{argv:?} does not parse: {e}"),
        }
    };
    let plain = reserve(&["tcr", "group", "reserve", "orchard"]);
    assert_eq!(plain.group, "orchard");
    assert!(!plain.force, "TcrBar's reserve is never forced");
    let forced = reserve(&["tcr", "group", "reserve", "orchard", "--force"]);
    assert_eq!(forced.group, "orchard");
    assert!(forced.force);
}

/// Every `tcr` command the Codex refusals and notes tell an operator to run
/// is one this binary accepts, read out of the sentences themselves as they
/// are printed: a remedy that clap rejects is itself an error, which is how
/// the restart hint above once went wrong.
#[test]
fn every_command_a_codex_refusal_names_parses() {
    use tcr::verbs::provider_guard;
    let sentences = [
        provider_guard::reserve_strands_codex(
            "orchard",
            &["bob@example.com/codex", "dana@example.com/codex"],
        ),
        provider_guard::reserve_force_warning("orchard", &["bob@example.com/codex"]),
        provider_guard::refused_exit_suffix("bob@example.com/codex"),
    ];
    let mut named: Vec<Vec<String>> = Vec::new();
    for sentence in &sentences {
        let commands: Vec<&str> = sentence
            .split('`')
            .skip(1)
            .step_by(2)
            .filter(|span| span.starts_with("tcr "))
            .collect();
        assert_eq!(
            commands.len(),
            1,
            "each sentence names one command to run: {sentence}"
        );
        for command in commands {
            let argv: Vec<&str> = command.split_whitespace().collect();
            if let Err(e) = Cli::try_parse_from(&argv) {
                panic!("`{sentence}` tells the user to run `{command}`, which does not parse: {e}");
            }
            named.push(argv.iter().map(ToString::to_string).collect());
        }
    }

    // Both reserve sentences name the remove that takes the first Codex
    // member out of the group, and it parses as exactly that.
    for argv in &named[..2] {
        assert_eq!(
            argv,
            &["tcr", "group", "rm", "orchard", "bob@example.com/codex"]
        );
        match Cli::try_parse_from(argv) {
            Ok(Cli {
                command:
                    Some(Command::Group(GroupArgs {
                        action: GroupAction::Rm(rm),
                    })),
                ..
            }) => {
                assert_eq!(rm.group, "orchard");
                assert_eq!(rm.account.as_deref(), Some("bob@example.com/codex"));
                assert!(!rm.all);
            }
            _ => panic!("{argv:?} is not a group rm"),
        }
    }
    // The strict exit pin's remedy clears it: back to local, and not strict.
    assert_eq!(
        named[2],
        [
            "tcr",
            "peer",
            "account",
            "bob@example.com/codex",
            "--exits-from",
            "local",
            "--no-must"
        ]
    );
    match Cli::try_parse_from(&named[2]) {
        Ok(Cli {
            command:
                Some(Command::Peer(peer_cli::PeerArgs {
                    action: peer_cli::PeerAction::Account(account),
                })),
            ..
        }) => {
            assert_eq!(account.account, "bob@example.com/codex");
            assert_eq!(account.exits_from.as_deref(), Some("local"));
            assert!(account.no_must && !account.must);
        }
        _ => panic!("{:?} is not a peer account", named[2]),
    }
}

/// Each flag alone still parses — the deprecated one is accepted, as promised
/// to the scripts and launch agents that already pass it.
#[test]
fn each_replace_flag_alone_still_parses() {
    for args in [
        vec!["tcr", "server", "--replace"],
        vec!["tcr", "server", "--no-replace"],
        vec!["tcr", "--no-replace"],
        vec!["tcr", "server"],
    ] {
        Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("{args:?} must parse: {e}"));
    }
}

/// The help renders no fleet, so every fleet reads the same help: it names both providers,
/// and says which verbs, flags and peer features are Claude's alone. clap drops a
/// doc-comment's closing period from the text it shows. The tokens TcrBar probes the help
/// for stay whole tokens.
#[test]
fn help_names_both_providers() {
    use clap::CommandFactory as _;
    let cli = Cli::command();
    let text =
        |styled: Option<&clap::builder::StyledStr>| styled.map_or_default(ToString::to_string);
    let sub = |path: &[&str]| -> clap::Command {
        let mut command = cli.clone();
        for name in path {
            command = command
                .find_subcommand(name)
                .unwrap_or_else(|| panic!("tcr has no {path:?} subcommand"))
                .clone();
        }
        command
    };
    let arg_help = |command: &clap::Command, id: &str| {
        text(
            command
                .get_arguments()
                .find(|arg| arg.get_id() == id)
                .unwrap_or_else(|| panic!("no {id} argument"))
                .get_help(),
        )
    };

    assert_eq!(
        text(cli.get_about()),
        "Lean single-user rotating proxy for Claude and Codex (ChatGPT) accounts, with a \
         live TUI"
    );
    assert_eq!(
        text(sub(&["login"]).get_about()),
        "Add an account: Claude through the browser, or Codex (ChatGPT) with --codex or \
         --from-codex"
    );
    assert_eq!(
        text(sub(&["mint"]).get_about()),
        "Mint a long-lived (365-day) token for an existing Claude account, or every Claude \
         account in a group, and put the result on the clipboard. Nothing is stored — \
         export only (Claude accounts only)"
    );
    assert_eq!(
        text(sub(&["control"]).get_about()),
        "Set, clear, or show the identity-bound control account (Claude accounts only)"
    );
    assert_eq!(
        text(sub(&["doctor"]).get_about()),
        "Say whether Claude Code is actually reaching this proxy, and which file decides \
         that; and whether codex is pooled, when the fleet holds a Codex account or with \
         --codex. Exits 0 when it is, 2 when another base URL owns the route, 3 when the \
         route points here and no proxy answers"
    );
    assert_eq!(
        arg_help(&sub(&["doctor"]), "codex"),
        "Report only whether codex is pooled, and exit with that verdict's code"
    );
    assert!(
        text(sub(&["list"]).get_about()).starts_with(
            "Print every Claude account as claude-swap's `list --json` document, for \
             CodexBar."
        ),
        "{}",
        text(sub(&["list"]).get_about())
    );
    let account = text(sub(&["peer", "account"]).get_about());
    assert!(
        account.ends_with(". Codex accounts always leave from this Mac"),
        "{account}"
    );
    for verb in ["share", "via", "lend"] {
        let about = text(sub(&["peer", verb]).get_about());
        assert!(
            about.ends_with(
                ". Codex requests always leave from this Mac: no Mac lends, serves or \
                 carries them in this version"
            ),
            "peer {verb}: {about}"
        );
    }
    let group = arg_help(&sub(&["run"]), "group");
    assert!(
        group.ends_with(". Claude sessions only; a Codex session cannot ask for a group"),
        "{group}"
    );

    for (path, tokens) in [
        (&["login"][..], &["--non-interactive", "--codex"][..]),
        (&["server"], &["--replace", "--no-replace"]),
        (&["doctor"], &["--codex"]),
    ] {
        let help = sub(path).render_long_help().to_string();
        for token in tokens {
            assert!(
                tcr::clients::codex_local::help_has_whole_token(&help, token),
                "`tcr {} --help` must carry {token} as a whole token:\n{help}",
                path.join(" ")
            );
        }
    }
}

mod routing_cli_tests {
    use super::*;

    /// **`tcr routing` speaks JSON only**: without `--json` it is a usage error rather than
    /// a text form a launcher could mistake for the document.
    #[test]
    fn routing_takes_only_json() {
        let Err(err) = Cli::try_parse_from(["tcr", "routing"]) else {
            panic!("`tcr routing` without --json must be refused");
        };
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::MissingRequiredArgument,
            "{err}"
        );
        assert!(matches!(
            Cli::try_parse_from(["tcr", "routing", "--json"]).map(|cli| cli.command),
            Ok(Some(Command::Routing(_)))
        ));
    }
}
