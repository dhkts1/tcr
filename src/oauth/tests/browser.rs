use super::*;

// --- PKCE (via the oauth2 crate) ---------------------------------------

#[test]
fn pkce_challenge_matches_rfc7636_vector() {
    // The canonical RFC 7636 Appendix B S256 example: the oauth2 crate must
    // map this fixed verifier to exactly this base64url (no-pad) challenge.
    let verifier =
        oauth2::PkceCodeVerifier::new("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".to_string());
    let challenge = PkceCodeChallenge::from_code_verifier_sha256(&verifier);
    assert_eq!(
        challenge.as_str(),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

#[test]
fn login_flow_produces_verifier_and_state() {
    // The oauth2-built flow yields a non-empty verifier and CSRF state, and
    // an authorize URL rooted at Claude's endpoint.
    let flow = build_login_flow("http://localhost:12345/callback", None).unwrap();
    assert!(!flow.verifier.is_empty());
    assert!(!flow.state.is_empty());
    assert!(flow.auth_url.starts_with(AUTHORIZE_URL));
    // Claude's authorize endpoint rejects short states ("Invalid request
    // format", observed live 2026-07-17 with the 22-char crate default).
    // 32 random bytes -> 43 base64url chars.
    assert_eq!(
        flow.state.len(),
        43,
        "state must be 32 bytes base64url (43 chars), got {} chars",
        flow.state.len()
    );
}

// --- callback / URL parsing --------------------------------------------

#[test]
fn parse_callback_extracts_code_and_state() {
    let (code, state, error) = parse_oauth_query("code=abc123&state=xyz789");
    assert_eq!(code.as_deref(), Some("abc123"));
    assert_eq!(state.as_deref(), Some("xyz789"));
    assert!(error.is_none());
}

#[test]
fn parse_callback_percent_decodes_values() {
    let (code, _state, _error) = parse_oauth_query("code=a%2Fb%2Bc&state=s");
    assert_eq!(code.as_deref(), Some("a/b+c"));
}

/// The hostile-decode cases the deleted private `percent_decode` helper
/// covered, re-pointed through the public parser now that `form_urlencoded`
/// owns the decoding. Same inputs, same guarantees — coverage is unchanged,
/// only the entry point moved.
#[test]
fn parse_oauth_query_survives_bare_percent_and_multibyte() {
    // Valid %XX still decodes.
    let (code, ..) = parse_oauth_query("code=a%20b");
    assert_eq!(code.as_deref(), Some("a b"));

    // A trailing bare `%` is preserved, never panics.
    let (code, ..) = parse_oauth_query("code=100%");
    assert_eq!(code.as_deref(), Some("100%"));

    // A `%` immediately before a multibyte UTF-8 char (hostile callback)
    // must NOT panic on a char-boundary slice.
    let (code, ..) = parse_oauth_query("code=a%\u{20AC}b");
    let code = code.expect("a % before a multibyte char must still parse");
    assert!(code.contains('a') && code.contains('b'));

    // A lone `%€` (percent right before the euro sign) also survives.
    let _ = parse_oauth_query("code=%\u{20AC}");
}

/// The event sequence, driven for real: `authorize_and_wait` in
/// [`LoginUi::Machine`] emits `browser` then `waiting`, and a fake
/// callback — the only completion path that mode has — hands back the
/// code. The URL in the `browser` event is the one the callback answers,
/// state and all, so this also pins that the app is told to open the URL
/// this very listener is waiting on.
///
/// Watched failing: with the `emit(LoginEvent::Waiting)` line deleted the
/// sequence assertion fails with `["browser"] != ["browser", "waiting"]`;
/// with `LoginUi::Machine`'s arm switched to `open_browser` and no emit,
/// it fails on an empty sequence.
#[tokio::test]
async fn machine_mode_emits_browser_then_waiting_and_completes_on_the_callback() {
    let recorded = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

    // The fake browser: wait for the `browser` event, then GET the
    // loopback callback exactly as Claude's redirect would.
    let watched = Arc::clone(&recorded);
    let browser = tokio::spawn(async move {
        let url = loop {
            let first = watched.lock().expect("sink mutex").first().cloned();
            if let Some(line) = first {
                let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
                break parsed["url"].as_str().unwrap().to_string();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let field = |key: &str| -> String {
            let Some(raw) = url
                .split(&format!("{key}="))
                .nth(1)
                .and_then(|rest| rest.split('&').next())
            else {
                panic!("the authorize URL carries {key}: {url}")
            };
            percent_decode(raw)
        };
        let callback = format!(
            "{}?code=fake-code&state={}",
            field("redirect_uri"),
            field("state")
        );
        crate::http_client::builder()
            .build()
            .unwrap()
            .get(&callback)
            .send()
            .await
            .log_debug("calling the test callback");
    });

    let collected = Arc::clone(&recorded);
    let mut sink = |event: LoginEvent<'_>| {
        collected.lock().expect("sink mutex").push(event.line());
    };
    let authorized = authorize_and_wait(None, LoginUi::Machine, &mut sink)
        .await
        .expect("the fake callback must settle the login");
    browser.await.expect("fake browser task");

    let events = recorded.lock().expect("sink mutex").clone();
    assert_eq!(
        events,
        vec![
            LoginEvent::Browser {
                url: &authorized.flow.auth_url
            }
            .line(),
            LoginEvent::Waiting.line(),
        ],
        "machine mode publishes exactly browser then waiting before the callback"
    );
    assert_eq!(authorized.code, "fake-code");
    assert!(
        authorized.redirect_uri.starts_with("http://localhost:"),
        "the callback is loopback-only: {}",
        authorized.redirect_uri
    );
}

/// Percent-decode just enough for the test above to read the
/// `redirect_uri` back out of an authorize URL.
fn percent_decode(value: &str) -> String {
    form_urlencoded::parse(format!("v={value}").as_bytes())
        .next()
        .map_or_default(|(_, v)| v.into_owned())
}

/// The email-less profile — the one path that prompts — takes the
/// prompt's own default instead of reading a stdin that is a pipe.
#[test]
fn machine_mode_names_an_email_less_account_without_prompting() {
    let empty: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
    let name = mint_login_name(&empty, &all_none_profile(), None, None, LoginUi::Machine).unwrap();
    assert_eq!(name, "unnamed");
}

/// A browser login whose profile fetch came back empty takes its name from
/// the token-exchange response, not from [`unnamed_fallback`]. The fixture
/// is the response shape `docs/design/long-lived-tokens.md` records.
#[test]
fn a_login_with_an_empty_profile_is_named_from_the_exchange_response() {
    let body = r#"{
            "access_token": "at",
            "refresh_token": "rt",
            "expires_in": 31536000,
            "account": {
                "uuid": "11111111-1111-1111-1111-111111111111",
                "email_address": "alice@example.com"
            },
            "organization": {
                "uuid": "22222222-2222-2222-2222-222222222222",
                "name": "Example Org"
            }
        }"#;

    // A config where the fallback would hand out `unnamed-2`, and one
    // where alice's bare email is already taken.
    let with_unnamed: Config = serde_json::from_str(
        r#"{ "accounts": [
                { "name": "unnamed", "type": "oauth", "accessToken": "at-1" }
            ] }"#,
    )
    .unwrap();
    let with_alice: Config = serde_json::from_str(
        r#"{ "accounts": [
                { "name": "alice@example.com", "type": "oauth", "accessToken": "at-1" }
            ] }"#,
    )
    .unwrap();

    for (config, expected) in [
        (&with_unnamed, "alice@example.com"),
        (&with_alice, "alice@example.com/example-org"),
    ] {
        let Exchanged { identity, .. } =
            tokens_from_exchange_body(body).expect("a present refresh_token must return Ok");
        let profile = all_none_profile().or_exchange_identity(identity);
        let name = mint_login_name(config, &profile, None, None, LoginUi::Machine).unwrap();
        assert_eq!(name, expected);
        assert_eq!(
            profile.account_uuid(),
            Some("11111111-1111-1111-1111-111111111111")
        );
        assert_eq!(
            profile.org_uuid.as_deref(),
            Some("22222222-2222-2222-2222-222222222222")
        );
    }
}

/// A field the profile endpoint DID send is kept; only its gaps are filled.
#[test]
fn exchange_identity_never_overrides_what_the_profile_sent() {
    let body = r#"{
            "access_token": "at",
            "refresh_token": "rt",
            "account": { "uuid": "33333333", "email_address": "bob@example.com" },
            "organization": { "uuid": "44444444", "name": "Other Org" }
        }"#;
    let Exchanged { identity, .. } = tokens_from_exchange_body(body).unwrap();
    let profile = profile_with_identity(
        "alice@example.com",
        Some("11111111"),
        Some("22222222"),
        None,
    )
    .or_exchange_identity(identity);
    assert_eq!(profile.email(), Some("alice@example.com"));
    assert_eq!(profile.account_uuid(), Some("11111111"));
    assert_eq!(profile.org_uuid.as_deref(), Some("22222222"));
    assert_eq!(profile.org_name.as_deref(), Some("Other Org"));
}

// --- token-exchange refresh_token guard --------------------------------

#[test]
fn exchange_body_fails_without_usable_refresh_token() {
    // An exchange response has no prior refresh token to fall back on, so a
    // missing, empty, or whitespace `refresh_token` must FAIL the exchange —
    // never mint a Tokens with an empty refresh_token that dies on first use.
    for body in [
        r#"{"access_token":"at"}"#,                       // absent
        r#"{"access_token":"at","refresh_token":""}"#,    // empty
        r#"{"access_token":"at","refresh_token":"   "}"#, // whitespace
        r#"{"access_token":"at","refresh_token":null}"#,  // explicit null
    ] {
        assert!(
            tokens_from_exchange_body(body).is_err(),
            "expected Err for body: {body}"
        );
    }
}

#[test]
fn exchange_body_ok_with_refresh_token() {
    // A response carrying a real refresh_token yields Ok with that token
    // preserved verbatim.
    let tokens = tokens_from_exchange_body(
        r#"{"access_token":"at","refresh_token":"rt","expires_at":1700000000000}"#,
    )
    .expect("a present refresh_token must return Ok")
    .tokens;
    assert_eq!(tokens.access_token, "at");
    assert_eq!(tokens.refresh_token.as_deref(), Some("rt"));
    assert_eq!(tokens.expires_at_ms, 1_700_000_000_000);
}

/// A response carrying `expires_in: 31536000` (one year) must land as an
/// `expires_at_ms` about 365 days from now — `tokens_from_exchange_body`
/// derives it via `expires_at_from`, which just adds `expires_in * 1000`
/// to `now_ms()`, so this pins that the year-long value survives the
/// round trip rather than being silently capped or defaulted.
#[test]
fn exchange_body_with_year_long_expires_in_yields_year_out_expiry() {
    let before = crate::now_ms();
    let tokens = tokens_from_exchange_body(
        r#"{"access_token":"at","refresh_token":"rt","expires_in":31536000}"#,
    )
    .expect("a present refresh_token must return Ok")
    .tokens;
    let after = crate::now_ms();

    let year_ms: i64 = 31_536_000 * 1000;
    assert!(
        tokens.expires_at_ms >= before + year_ms && tokens.expires_at_ms <= after + year_ms,
        "expected expires_at_ms about one year out, got {} (now was {before}..{after})",
        tokens.expires_at_ms
    );
}

/// The serialized exchange body must request the year-long lifetime as a
/// client-supplied `expires_in`, mirroring `warm_request_spec`'s
/// no-network assertion pattern in `src/warmer.rs`.
#[test]
fn exchange_request_body_carries_year_long_expires_in() {
    let body = exchange_request_body("a-code", "a-verifier", "a-state", "https://cb");
    assert_eq!(body["expires_in"], LOGIN_TOKEN_LIFETIME_SECS);
    assert_eq!(body["grant_type"], "authorization_code");
    assert_eq!(body["code"], "a-code");
}

/// The three scopes the token endpoint refuses a custom `expires_in` for
/// must never come back: assert their absence explicitly so a future
/// re-add (e.g. restoring MCP or Claude Code session scopes) fails this
/// test loudly instead of silently reintroducing the HTTP 400.
#[test]
fn oauth_scopes_excludes_the_custom_expires_in_incompatible_scopes() {
    for forbidden in [
        "org:create_api_key",
        "user:sessions:claude_code",
        "user:mcp_servers",
    ] {
        assert!(
            !OAUTH_SCOPES.contains(forbidden),
            "OAUTH_SCOPES must not contain {forbidden:?}, got {OAUTH_SCOPES:?}"
        );
    }
}

#[test]
fn resolve_callback_accepts_matching_state() {
    let code = resolve_callback_code(
        Some("the-code".into()),
        Some("expected".into()),
        None,
        "expected",
    )
    .expect("matching state should resolve");
    assert_eq!(code, "the-code");
}

#[test]
fn resolve_callback_rejects_state_mismatch() {
    let result = resolve_callback_code(
        Some("the-code".into()),
        Some("attacker".into()),
        None,
        "expected",
    );
    assert!(result.is_err(), "mismatched state must be rejected");
}

#[test]
fn resolve_callback_rejects_missing_state() {
    // A callback with no state at all is a CSRF signal → reject.
    let result = resolve_callback_code(Some("the-code".into()), None, None, "expected");
    assert!(result.is_err());
}

#[test]
fn resolve_callback_rejects_oauth_error() {
    let result = resolve_callback_code(
        None,
        Some("expected".into()),
        Some("access_denied".into()),
        "expected",
    );
    assert!(result.is_err());
}

// --- authorize URL ------------------------------------------------------

#[test]
fn authorize_url_carries_required_params() {
    // The oauth2-built authorize URL must carry every required OAuth param
    // plus Claude's non-standard `code=true`.
    let url = build_login_flow("http://localhost:12345/callback", None)
        .unwrap()
        .auth_url;
    assert!(url.starts_with(AUTHORIZE_URL));
    assert!(url.contains("code=true"));
    assert!(url.contains(&format!("client_id={CLIENT_ID}")));
    assert!(url.contains("response_type=code"));
    assert!(url.contains("code_challenge="));
    assert!(url.contains("code_challenge_method=S256"));
    assert!(url.contains("state="));
    // redirect_uri is percent-encoded by oauth2.
    assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A12345%2Fcallback"));
    // No --account given: no login_hint at all (add-a-new-account path).
    assert!(!url.contains("login_hint"));
}

#[test]
fn authorize_url_carries_login_hint_when_given() {
    let url = build_login_flow("http://localhost:12345/callback", Some("alice@example.com"))
        .unwrap()
        .auth_url;
    assert!(url.contains("login_hint=alice%40example.com"));
}

// --- `login_hint` value validation --------------------------------------

#[test]
fn looks_like_email_accepts_and_rejects() {
    assert!(looks_like_email("alice@example.com"));
    assert!(!looks_like_email("work"), "a bare display name, no '@'");
    assert!(!looks_like_email("personal-max"), "no '@'");
    assert!(!looks_like_email("@example.com"), "empty local part");
    assert!(!looks_like_email("alice@"), "empty domain");
    assert!(!looks_like_email("alice @example.com"), "whitespace");
    assert!(!looks_like_email("alice@b@c"), "two '@'s");
}

/// A login mints its own name, and never prompts on the happy path: the
/// bare email when nothing carries it, `email/<org-slug>` when something
/// does. This is the whole mechanism that stops a second org of the same
/// person from landing on the first one's row.
#[test]
fn a_login_mints_the_bare_email_when_free_and_qualifies_it_when_not() {
    let empty: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
    let profile = profile_with_identity(
        "me@example.com",
        Some("uuid-person"),
        Some("22222222-2222"),
        Some("Henry Token"),
    );
    assert_eq!(
        mint_login_name(&empty, &profile, None, None, LoginUi::Terminal).unwrap(),
        "me@example.com"
    );

    let occupied: Config = serde_json::from_str(
        r#"{ "accounts": [
                { "name": "me@example.com", "type": "oauth", "accessToken": "at-1" }
            ] }"#,
    )
    .unwrap();
    assert_eq!(
        mint_login_name(&occupied, &profile, None, None, LoginUi::Terminal).unwrap(),
        "me@example.com/henry-token"
    );

    // No org name on the profile: the org uuid's first eight characters.
    let no_org_name = profile_with_identity(
        "me@example.com",
        Some("uuid-person"),
        Some("22222222-2222"),
        None,
    );
    assert_eq!(
        mint_login_name(&occupied, &no_org_name, None, None, LoginUi::Terminal).unwrap(),
        "me@example.com/22222222"
    );
}

/// `--name` wins over the minted name, and is REFUSED when another account
/// already holds it. Taking a name off an existing row is how a login
/// overwrites the wrong credential, and it is the one collision the loader's
/// migration would never see.
#[test]
fn a_name_override_wins_but_never_takes_a_name_already_in_use() {
    let occupied: Config = serde_json::from_str(
        r#"{ "accounts": [
                { "name": "alice@example.com", "type": "oauth", "accessToken": "at-1" }
            ] }"#,
    )
    .unwrap();
    let profile = profile_named("bob@example.com");

    assert_eq!(
        mint_login_name(
            &occupied,
            &profile,
            None,
            Some("bob-work"),
            LoginUi::Terminal
        )
        .unwrap(),
        "bob-work"
    );

    let err = mint_login_name(
        &occupied,
        &profile,
        None,
        Some("alice@example.com"),
        LoginUi::Terminal,
    )
    .expect_err("a --name already in use must refuse");
    assert!(
        err.to_string()
            .contains("already the name of another account"),
        "{err}"
    );
}

/// A re-login keeps the row's own name rather than re-minting one: that
/// name is what every pin, group label and `controlAccount` key points at.
#[test]
fn a_re_login_keeps_the_rows_existing_name() {
    let config: Config = serde_json::from_str(
        r#"{ "accounts": [
                { "name": "me@example.com/corp", "type": "oauth", "accessToken": "at-1" }
            ] }"#,
    )
    .unwrap();
    let profile = profile_named("me@example.com");
    assert_eq!(
        mint_login_name(
            &config,
            &profile,
            Some("me@example.com/corp"),
            None,
            LoginUi::Terminal
        )
        .unwrap(),
        "me@example.com/corp"
    );
}

/// An all-`None` profile plus `--name` must produce that exact name,
/// without ever reaching the stdin prompt — the `--name` branch is taken
/// instead of falling through to `prompt_account_name`. This is the
/// setup-token case: an inference-only credential's profile fetch normally
/// comes back empty, so `--name` is the only name there is.
#[test]
fn a_name_override_skips_the_prompt_on_an_empty_profile() {
    let empty: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
    let name = mint_login_name(
        &empty,
        &all_none_profile(),
        None,
        Some("bob-work"),
        LoginUi::Terminal,
    )
    .unwrap();
    assert_eq!(name, "bob-work");
}

/// `--name` wins over the profile's email too. It used to be the other way
/// round, which made the flag unusable exactly when a person wanted to name
/// their second org's row something they would recognise.
#[test]
fn a_name_override_wins_over_the_profile_email() {
    let empty: Config = serde_json::from_str(r#"{ "accounts": [] }"#).unwrap();
    let name = mint_login_name(
        &empty,
        &profile_named("carol@example.com"),
        None,
        Some("carol-personal"),
        LoginUi::Terminal,
    )
    .unwrap();
    assert_eq!(name, "carol-personal");
}

// --- the `unnamed` fallback ---------------------------------------------

/// The names already taken, the only thing [`unnamed_fallback`] reads.
fn taken(names: &[&str]) -> std::collections::HashSet<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

#[test]
fn unnamed_fallback_is_plain_unnamed_when_free() {
    assert_eq!(unnamed_fallback(&taken(&[])), "unnamed");
    assert_eq!(unnamed_fallback(&taken(&["alice@example.com"])), "unnamed");
}

/// The collision case, and the reason this function exists at all: an
/// inference-only account has no email and no uuid, so `identity::resolve`
/// can only match it by NAME. Handing out `unnamed` twice would resolve the
/// second login onto the first row and overwrite its token instead of
/// adding an account.
#[test]
fn unnamed_fallback_skips_taken_names() {
    assert_eq!(unnamed_fallback(&taken(&["unnamed"])), "unnamed-2");
    assert_eq!(
        unnamed_fallback(&taken(&["unnamed", "unnamed-2"])),
        "unnamed-3"
    );
}

/// Gaps are filled rather than skipped past — the name is chosen by
/// scanning for the first FREE suffix, never by counting rows, so removing
/// `unnamed-2` and adding an account reuses that slot instead of colliding
/// with `unnamed-3`. A count-derived fallback gets this wrong.
#[test]
fn unnamed_fallback_fills_a_gap_left_by_a_removed_account() {
    assert_eq!(
        unnamed_fallback(&taken(&["unnamed", "unnamed-3"])),
        "unnamed-2"
    );
}
