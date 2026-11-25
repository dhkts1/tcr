use super::*;

// --- remedies that name the right command ------------------------------------------

/// A refusal over a running proxy names the login that was run as the one to run again,
/// for both kinds of incumbent; plain `tcr login` still says `run 'tcr login'`.
///
/// Watched red: with `'tcr login'` written into the message, the import's refusal sends
/// its user to the browser flow.
#[test]
fn the_running_proxy_refusal_echoes_the_command_that_was_run() {
    for kind in [singleton::ProxyKind::Tcr, singleton::ProxyKind::TcrEmbedded] {
        let msg = expect_refuse(login_guard_refusal_for(
            incumbent(kind),
            false,
            3456,
            false,
            "tcr login --from-codex",
        ));
        assert!(
            msg.contains("run 'tcr login --from-codex'"),
            "{kind:?}: {msg}"
        );
        let plain = expect_refuse(login_guard_refusal(incumbent(kind), false, 3456, false));
        assert!(plain.contains("run 'tcr login',"), "{kind:?}: {plain}");
    }
    let live = codex_live_route_refusal(3456, Some(4242), LOGIN_CODEX_COMMAND);
    assert!(
        live.contains("then re-run `tcr login --codex`. --force does not get past this"),
        "{live}"
    );
}

/// With no login file, `--from-codex` says how to make one, and, last, that a login codex
/// keeps in the system keyring has no file to import, which `tcr login --codex` does not need.
///
/// Watched red: the message without its last sentence.
#[test]
fn a_keyring_held_codex_login_names_the_keyring() {
    assert_eq!(
        no_codex_login_message(Path::new("/Users/example/.codex/auth.json")),
        "no Codex login found at /Users/example/.codex/auth.json — nothing was written. Log \
         in with `codex login` first (for a second account: `mkdir -p <dir>`, \
         `CODEX_HOME=<dir> codex login`, then `tcr login --from-codex --codex-home <dir>`). If \
         codex keeps this login in the system keyring, which tcr does not read, use tcr login \
         --codex instead, which needs no file."
    );
}

/// The same ChatGPT user in another workspace names both workspaces by their first 8 hex,
/// each with its plan in parentheses, and drops a parenthesis whose plan is unknown.
#[test]
fn codex_relogin_workspace_refusal_drops_unknown_plans() {
    let named = codex_relogin_workspace_refusal(
        Some("erin@example.com"),
        "99999999-9999-4999-8999-999999999999",
        Some("business"),
        "work",
        "88888888-8888-4888-8888-888888888888",
        Some("pro"),
    );
    assert_eq!(named.code, "codex-identity");
    assert_eq!(
        named.reason,
        "the ChatGPT login that came back is erin@example.com in ChatGPT workspace 99999999 \
         (ChatGPT Business), but 'work' holds workspace 88888888 (ChatGPT Pro). Retry `tcr \
         login --codex --account work` and choose that workspace when ChatGPT asks which one \
         to use. Nothing was written."
    );
    let unknown = codex_relogin_workspace_refusal(
        None,
        "99999999-9999-4999-8999-999999999999",
        None,
        "work",
        "88888888-8888-4888-8888-888888888888",
        None,
    );
    assert_eq!(
        unknown.reason,
        "the ChatGPT login that came back is the same ChatGPT user in ChatGPT workspace \
         99999999, but 'work' holds workspace 88888888. Retry `tcr login --codex --account \
         work` and choose that workspace when ChatGPT asks which one to use. Nothing was \
         written."
    );
}

/// A running proxy's status read, as `live_client::fetch_live_status` hands it over: parsed from the
/// wire, listing no Codex row (the server may not have admitted the new one yet), with the
/// listener `mitm` reports.
fn live_proxy_reporting(
    mitm: serde_json::Value,
) -> Result<crate::status::StatusPayload, crate::proxy::live_client::LiveStatusError> {
    Ok(serde_json::from_value(serde_json::json!({
        "kind": crate::status::STATUS_KIND,
        "accounts": [],
        "build": {"sha": "abc1234"},
        "mitm": mitm,
    }))
    .expect("a status payload"))
}

const NOTICE_PORT: u16 = 3999;

fn notice_ca() -> &'static Path {
    Path::new("/tmp/tcr-run-test/tcr-ca.pem")
}

/// A proxy whose leaf cannot take `chatgpt.com` cannot terminate marked Codex connections,
/// so the first account is told codex is not pooled, and why.
///
/// Watched red: with the first-account notice keyed on the config's count alone, such a
/// proxy gets a notice claiming marked Codex connections are pooled.
#[test]
fn a_proxy_without_mitm_gets_the_not_pooled_notice() {
    let off = live_proxy_reporting(serde_json::json!({"interceptedHosts": [], "leaf": "none"}));
    assert_eq!(
        first_codex_account_notice(&off, NOTICE_PORT, notice_ca(), true),
        "codex is not pooled: the proxy on :3999 is running without MITM: its TLS material \
         failed to load at boot — see its log"
    );
    assert_eq!(
        first_codex_account_notice(
            &Err(crate::proxy::live_client::LiveStatusError::NoAnswer(
                "the server did not answer within 5s".to_string()
            )),
            NOTICE_PORT,
            notice_ca(),
            true,
        ),
        "codex is not pooled: could not read the running proxy's status (the server did not \
         answer within 5s)",
        "a proxy that could not be read is not claimed to intercept anything"
    );
}

/// A proxy whose minted leaf covers chatgpt.com can take marked Codex traffic
/// from now on, though its status, read right after the save, may not list the new row yet:
/// the notice keys on the leaf, not on the route. So does a released port, where the next
/// boot mints such a leaf.
#[test]
fn a_minted_chatgpt_leaf_gets_the_first_account_notice() {
    let minted = live_proxy_reporting(serde_json::json!({
        "interceptedHosts": ["api.anthropic.com", "platform.claude.com", "chatgpt.com"],
        "leaf": "minted",
    }));
    assert_eq!(
        first_codex_account_notice(&minted, NOTICE_PORT, notice_ca(), true),
        "tcr pools marked Codex connections to chatgpt.com: use `tcr run --codex`; keep \
         the marker scoped to that launch, since unmarked connections keep their own login"
    );
    assert_eq!(
        first_codex_account_notice(
            &Err(crate::proxy::live_client::LiveStatusError::NoServer),
            NOTICE_PORT,
            notice_ca(),
            false,
        ),
        CODEX_FIRST_ACCOUNT_NOTICE
    );
}
