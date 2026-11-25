//! Browser OAuth login (PKCE). Adds a new account to the config so friends can
//! onboard without hand-editing JSON: the authorize URL, the loopback callback,
//! the code exchange, and `tcr login` itself with the name it saves under.

use super::*;

/// Authorize endpoint.
pub const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
/// OAuth scopes requested at login. Narrowed from six scopes to the three the token endpoint will grant a custom `expires_in`
/// for. `org:create_api_key`, `user:sessions:claude_code` and
/// `user:mcp_servers` are dropped: requesting any of them alongside a
/// custom `expires_in` gets HTTP 400 `Custom expires_in not allowed for
/// scope 'user:mcp_servers'` — measured against the live endpoint,
/// `docs/design/long-lived-tokens.md` ("The finding" / "A reduced scope set
/// loses nothing tcr uses"). `user:profile` is kept because `rate_limit_tier`
/// is not in the token response, so `fetch_profile` still needs it.
pub const OAUTH_SCOPES: &str = "user:inference user:profile user:file_upload";

/// Overall login timeout: the callback server waits two minutes.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(120);

/// PKCE + CSRF material for one login attempt, produced by the `oauth2` crate.
/// Holds the plaintext `verifier` and `state` secrets we replay at the manual
/// token exchange, plus the fully-built authorize URL.
pub(super) struct LoginFlow {
    /// The PKCE `code_verifier` secret, replayed at token exchange.
    pub(super) verifier: String,
    /// The anti-CSRF `state` secret, validated on the callback.
    pub(super) state: String,
    /// The authorize URL to open in the browser.
    pub(super) auth_url: String,
}

/// Build the authorize URL, PKCE challenge and CSRF state via the standard
/// `oauth2` client. PKCE (S256), `state`, and standard params (`response_type`,
/// `client_id`, `redirect_uri`, `scope`, `code_challenge*`) are all produced by
/// the library; only Claude's non-standard `code=true` is added by hand.
///
/// `login_hint`, when present, is passed through to `claude.ai/oauth/authorize`
/// so the login page pre-selects that address — **ergonomics only**, never the
/// source of truth. Measured 2026-08-14 (σ3, N=1, clean incognito session): the
/// endpoint honors it and pre-fills the exact address passed. Untested (σ1):
/// whether it overrides an *already signed-in* browser session — precisely the
/// case `open_browser` produces by handing the URL to the default browser.
/// Because that case is unverified, the caller must never rely on the hint for
/// correctness; the identity assertion after `fetch_profile` returns is what
/// actually enforces the requested account, and depends on nothing external.
pub(super) fn build_login_flow(
    redirect_uri: &str,
    login_hint: Option<&str>,
) -> anyhow::Result<LoginFlow> {
    let client = BasicClient::new(ClientId::new(CLIENT_ID.to_string()))
        .set_auth_uri(AuthUrl::new(AUTHORIZE_URL.to_string()).context("invalid authorize URL")?)
        .set_token_uri(TokenUrl::new(TOKEN_ENDPOINT.to_string()).context("invalid token URL")?)
        .set_redirect_uri(
            RedirectUrl::new(redirect_uri.to_string()).context("invalid redirect URI")?,
        );

    // oauth2 generates a random verifier and its S256 challenge together.
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();

    let mut request = client
        // 32-byte state, NOT the crate-default `new_random()` (16 bytes / 22
        // chars): Claude's authorize endpoint rejects the short state with
        // "Invalid request format" once a logged-in session validates the
        // request. 32 bytes (43 base64url chars) matches the working JS
        // reference (`randomBytes(32).toString('base64url')`).
        .authorize_url(|| CsrfToken::new_random_len(32))
        .add_scope(Scope::new(OAUTH_SCOPES.to_string()))
        .set_pkce_challenge(challenge)
        // Claude's authorize endpoint requires this non-standard flag.
        .add_extra_param("code", "true");
    if let Some(email) = login_hint {
        request = request.add_extra_param("login_hint", email);
    }
    let (auth_url, csrf) = request.url();

    Ok(LoginFlow {
        verifier: verifier.secret().clone(),
        state: csrf.secret().clone(),
        auth_url: auth_url.to_string(),
    })
}

/// Extract `(code, state, error)` from a URL query string (`k=v&k2=v2`).
///
/// Decoding is [`form_urlencoded`]'s — the same parser `url`, `axum` and
/// `reqwest` already pull into this tree, so it costs no package — rather than
/// the hand-rolled `%XX`/`+` decoder this replaces. It is lossy on invalid
/// UTF-8, so a hostile callback still cannot panic the CLI.
///
/// The explicit loop is kept deliberately over a `collect()` into a map: it
/// preserves **last-occurrence-wins** on a duplicated key, which is what the
/// hand-rolled version did.
///
/// One disclosed behaviour change: `form_urlencoded` percent-decodes the KEY as
/// well as the value, so `%63ode=x` now matches `code` where it previously did
/// not. That is the more spec-correct reading and is unreachable from a real
/// Claude redirect. A differential run over 4,132 inputs (36 hand-picked edges
/// plus a 4,096-case brute-force sweep) found this as the ONLY divergence —
/// nothing changed for `;` separators, bare keys, `code=a=b`, lone surrogates,
/// `%00`, `%FF`, a trailing `%`, or `+`-vs-`%2B`.
pub(super) fn parse_oauth_query(query: &str) -> (Option<String>, Option<String>, Option<String>) {
    let (mut code, mut state, mut error) = (None, None, None);
    for (k, v) in form_urlencoded::parse(query.as_bytes()) {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            _ => {}
        }
    }
    (code, state, error)
}

/// Resolve a callback into the authorization code, enforcing OAuth security:
/// an `error` fails; `state` MUST be present and equal to ours (mismatch is a
/// CSRF signal and is rejected); a missing code fails.
pub(super) fn resolve_callback_code(
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    expected_state: &str,
) -> anyhow::Result<String> {
    if let Some(err) = error {
        bail!("OAuth error: {err}");
    }
    if state.as_deref() != Some(expected_state) {
        bail!("OAuth state mismatch");
    }
    code.ok_or_else(|| anyhow!("callback missing authorization code"))
}

/// Open `url` in the default browser (best-effort; ignores failure like the JS).
fn open_browser(url: &str) {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "start"
    } else {
        "xdg-open"
    };
    std::process::Command::new(cmd)
        .arg(url)
        .spawn()
        .log_warn("opening the browser for login");
}

/// Minimal local callback server on `listener` (already bound to 127.0.0.1).
/// Accepts connections until a valid `GET /callback?...` arrives, then replies
/// `302` to the success page and returns the code. A callback carrying an
/// `error` or a mismatched `state` fails immediately.
async fn run_callback_server(
    listener: TcpListener,
    expected_state: &str,
) -> anyhow::Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await?;

        // Read one chunk — the request line + headers arrive together; we only
        // need the request line (`GET /callback?query HTTP/1.1`).
        let mut buf = vec![0u8; 8192];
        let n = stream.read(&mut buf).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]);
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("");
        let (path, query) = target.split_once('?').unwrap_or((target, ""));

        if path != "/callback" {
            stream
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nNot found")
                .await
                .log_debug("answering a stray callback request");
            continue;
        }

        let (code, state, error) = parse_oauth_query(query);
        match resolve_callback_code(code, state, error, expected_state) {
            Ok(code) => {
                stream
                    .write_all(
                        b"HTTP/1.1 302 Found\r\nLocation: https://platform.claude.com/oauth/code/success?app=claude-code\r\nContent-Length: 0\r\n\r\n",
                    )
                    .await
                    .log_debug("redirecting the browser after login");
                stream
                    .flush()
                    .await
                    .log_debug("flushing the login redirect");
                return Ok(code);
            }
            Err(err) => {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html><body><h2>Authentication failed</h2><p>You can close this tab.</p></body></html>",
                    )
                    .await
                    .log_debug("telling the browser the login failed");
                stream
                    .flush()
                    .await
                    .log_debug("flushing the login failure page");
                return Err(err);
            }
        }
    }
}

/// Fallback path: accept a pasted callback URL or raw code from stdin. Empty
/// lines keep waiting; a full URL has its `state` validated (reject on
/// mismatch) exactly like the callback. On a non-TTY / EOF stdin, this never
/// settles so the browser callback or the timeout wins (mirrors the JS
/// `isTTY` guard).
async fn read_stdin_code(expected_state: &str) -> anyhow::Result<String> {
    eprint!("Paste authorization code here (or wait for browser callback): ");
    std::io::stderr().flush().log_debug("flushing the prompt");

    let mut reader = BufReader::new(tokio::io::stdin());
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            // EOF (piped/closed stdin): never settle from here.
            std::future::pending::<()>().await;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // A pasted callback URL: validate state, extract code.
        if let Some((_, query)) = trimmed.split_once('?') {
            let (code, state, _error) = parse_oauth_query(query);
            if let Some(code) = code {
                // Only reject when a state was supplied and does not match —
                // a bare code without state stays acceptable (JS parity).
                if let Some(state) = state
                    && state != expected_state
                {
                    bail!("OAuth state mismatch");
                }
                return Ok(code);
            }
        }
        // Otherwise treat the whole line as the raw authorization code.
        return Ok(trimmed.to_string());
    }
}

/// Wait for the authorization code from whichever source arrives first — the
/// browser callback or a manual stdin paste — bounded by the 2-minute timeout.
/// In [`LoginUi::Machine`] the stdin branch is not merely inert, it is not
/// constructed: a GUI parent's child process has a pipe (or `/dev/null`) on
/// stdin, and `read_stdin_code`'s first act is an `eprint!` prompt to a human
/// who is not there. The callback and the timeout are the only two ways that
/// mode can settle.
async fn wait_for_code(
    listener: TcpListener,
    expected_state: &str,
    ui: LoginUi,
) -> anyhow::Result<String> {
    if !ui.reads_stdin() {
        return tokio::select! {
            result = run_callback_server(listener, expected_state) => result,
            () = tokio::time::sleep(LOGIN_TIMEOUT) => Err(anyhow!("Login timed out after 2 minutes")),
        };
    }
    tokio::select! {
        result = run_callback_server(listener, expected_state) => result,
        result = read_stdin_code(expected_state) => result,
        () = tokio::time::sleep(LOGIN_TIMEOUT) => Err(anyhow!("Login timed out after 2 minutes")),
    }
}

/// Lifetime (seconds) requested for the access token minted at login, sent
/// as a client-supplied `expires_in` in the exchange body. The token endpoint
/// is CLIENT-DRIVEN here, not server-fixed: measured against the live
/// endpoint, the same request with no `expires_in` was granted `28800` (8h);
/// with `expires_in: 31536000` it was granted `31536000` (365d) verbatim —
/// `docs/design/long-lived-tokens.md` ("The finding"). This only works
/// because [`OAUTH_SCOPES`] excludes the scopes the endpoint refuses a custom
/// `expires_in` for; see that constant's doc comment.
pub const LOGIN_TOKEN_LIFETIME_SECS: u64 = 31_536_000;

/// Build the JSON body for [`exchange_code`]'s `POST {TOKEN_ENDPOINT}`, as a
/// pure function so the request shape can be asserted without the network —
/// mirrors `warm_request_spec` in `src/clients/warmer.rs`.
pub(super) fn exchange_request_body(
    code: &str,
    verifier: &str,
    state: &str,
    redirect_uri: &str,
) -> serde_json::Value {
    serde_json::json!({
        "code": code,
        "state": state,
        "grant_type": "authorization_code",
        "client_id": CLIENT_ID,
        "redirect_uri": redirect_uri,
        "code_verifier": verifier,
        "expires_in": LOGIN_TOKEN_LIFETIME_SECS,
    })
}

/// Exchange the authorization code for tokens (`POST {TOKEN_ENDPOINT}`).
///
/// This is the ONE step kept as a manual `reqwest` JSON POST rather than
/// oauth2's `exchange_code`: Claude's token endpoint is non-standard — it wants
/// a JSON body with `Content-Type: application/json`, not the RFC 6749 form-encoding oauth2 emits, and it echoes `state`
/// in the body. Uses a `.no_proxy()` client so an ambient `HTTPS_PROXY` cannot
/// swallow the request.
async fn exchange_code(
    code: &str,
    verifier: &str,
    state: &str,
    redirect_uri: &str,
) -> anyhow::Result<Exchanged> {
    let client = crate::base::http_client::builder()
        .no_proxy()
        .build()
        .context("build OAuth token-exchange client")?;

    let response = client
        .post(TOKEN_ENDPOINT)
        .header("Content-Type", "application/json")
        .header("Accept", OAUTH_ACCEPT)
        .header("User-Agent", OAUTH_USER_AGENT)
        .json(&exchange_request_body(code, verifier, state, redirect_uri))
        .send()
        .await
        .context("token exchange request failed")?;

    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("Token exchange failed ({}): {text}", status.as_u16());
    }

    tokens_from_exchange_body(&text)
}

/// What one code-for-token exchange produced: the credential, and the identity
/// the token endpoint sent beside it.
pub(super) struct Exchanged {
    pub(super) tokens: Tokens,
    pub(super) identity: ExchangeIdentity,
}

/// The token-exchange response: the refresh fields plus the identity. A
/// separate struct from [`RefreshResponse`], because a refresh never needs
/// the identity.
#[derive(Deserialize)]
struct ExchangeResponse {
    #[serde(flatten)]
    refresh: RefreshResponse,
    #[serde(flatten)]
    identity: ExchangeIdentity,
}

/// Turn a raw token-endpoint response body into [`Tokens`], failing the exchange
/// when no usable `refresh_token` is present.
///
/// Unlike the *refresh* path — which has the prior refresh token to fall back on
/// (`refresh_token.unwrap_or_else(|| refresh_token.to_string())`) — a code→token
/// *exchange* has no prior token. So an absent or empty/whitespace `refresh_token`
/// here would mint an account whose stored token is `""`; that account dies with
/// `AuthRejected` on its first refresh (the refresh POST sends the empty token).
/// Fail the exchange instead of persisting a self-destructing account.
pub(super) fn tokens_from_exchange_body(text: &str) -> anyhow::Result<Exchanged> {
    let ExchangeResponse {
        refresh: data,
        identity,
    } = serde_json::from_str(text).context("parse token-exchange response")?;

    let refresh_token = match data.refresh_token {
        Some(token) if !token.trim().is_empty() => token,
        _ => bail!("token exchange returned no refresh_token"),
    };

    let expires_at_ms = data.expires_at.map_or_else(
        || expires_at_from(crate::now_ms(), data.expires_in),
        normalize_expires_at,
    );

    Ok(Exchanged {
        tokens: Tokens {
            id_token: None,
            access_token: data.access_token,
            refresh_token: Some(refresh_token),
            expires_at_ms,
        },
        identity,
    })
}

/// Prompt for an account name on stdin, falling back to `fallback` on empty
/// input or unreadable stdin.
fn prompt_account_name(fallback: &str) -> String {
    eprint!("Name this account [{fallback}]: ");
    std::io::stderr().flush().log_debug("flushing the prompt");
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(_) => {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                fallback.to_string()
            } else {
                trimmed.to_string()
            }
        }
        Err(_) => fallback.to_string(),
    }
}

/// What the browser half of a login produced: the PKCE/CSRF material, the
/// redirect URI it was bound to, and the authorization code that came back.
/// All three are needed together by [`exchange_code`], so they travel as one
/// value rather than as three locals a caller could pair wrongly.
pub(super) struct Authorized {
    pub(super) flow: LoginFlow,
    pub(super) redirect_uri: String,
    pub(super) code: String,
}

/// The offline half of [`login`]: bind the loopback callback, build the
/// authorize URL, tell whoever is listening about it, and wait for the code.
///
/// Split out of `login` so the event sequence is testable at all. Everything
/// after this point talks to Anthropic's token and profile endpoints, so an
/// end-to-end test of `login` is not possible without the network; a test can
/// drive THIS with a fake callback and assert both the emitted lines and the
/// code that comes back. `emit` is the sink for that reason — the shipping
/// caller hands it [`LoginUi::emit`] (stdout), a test hands it a `Vec`.
///
/// The two modes differ in exactly two places, both here: who opens the
/// browser, and whether the progress is prose or JSON.
pub(super) async fn authorize_and_wait(
    hint: Option<&str>,
    ui: LoginUi,
    emit: &mut dyn FnMut(LoginEvent<'_>),
) -> anyhow::Result<Authorized> {
    // Bind the callback server on a random loopback port (127.0.0.1 only).
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("bind local OAuth callback server")?;
    let callback_port = listener.local_addr()?.port();
    let redirect_uri = format!("http://localhost:{callback_port}/callback");

    // PKCE + CSRF state + authorize URL, built by the oauth2 crate.
    let flow = build_login_flow(&redirect_uri, hint)?;
    match ui {
        LoginUi::Terminal => {
            println!("Opening browser for authentication...");
            println!("If it doesn't open, visit:\n  {}\n", flow.auth_url);
            open_browser(&flow.auth_url);
        }
        LoginUi::Machine => emit(LoginEvent::Browser {
            url: &flow.auth_url,
        }),
    }
    emit(LoginEvent::Waiting);

    let code = wait_for_code(listener, &flow.state, ui).await?;
    Ok(Authorized {
        flow,
        redirect_uri,
        code,
    })
}

/// Run the full browser OAuth login and persist the account. Returns the
/// account name on success. Never logs or prints the tokens.
///
/// Refuses (unless `force`) when a live proxy server already holds the
/// configured port AND that proxy has no live account-add route: an older
/// tcr reads config only at boot, and its next `persist_tokens` writes its
/// boot-time TOKENS back over the file, silently clobbering the fresh ones
/// this login writes (observed live 2026-07-19 — a server refresh overwrote a
/// re-login within seconds). When the live route IS there, this routes the
/// finished credential through the server instead ([`finish_login`]) — the
/// server owns the write with its own current state, so the window above
/// does not exist on that path at all. Detection is read-only; the server is
/// never signalled.
pub async fn login(
    config_path: &Path,
    force: bool,
    account: Option<&str>,
    name_override: Option<&str>,
    ui: LoginUi,
) -> anyhow::Result<String> {
    ui.install();
    let route = admit_login(config_path, force, LoginCommand::Browser)
        .await?
        .route;

    // Resolve --account early, ONLY to compute the login_hint (ergonomics —
    // see build_login_flow's doc-comment). Fail fast here on a typo'd or
    // ambiguous name, and on a row this sign-in can never confirm (a Codex
    // account, `refuse_a_non_claude_row`), rather than after a full browser
    // round trip. The SAME resolution runs again below, against a freshly
    // loaded config, where it is load-bearing rather than a convenience.
    let hint = match account {
        Some(query) => {
            let probe_config = load_or_default(config_path)?;
            let row = probe_config.resolve_account(query)?;
            refuse_a_non_claude_row(row)?;
            let email = crate::base::identity::email_of(&row.name);
            // Only ever send something address-shaped: a non-email display
            // name (e.g. an account named "work") produces `login_hint=work`
            // otherwise — an unvalidated non-address value on an endpoint
            // this crate already knows is picky about param shape (the
            // 32-byte-state note above). Skipping it there costs nothing:
            // the hint is ergonomics only (see this function's doc-comment),
            // never the source of truth.
            if looks_like_email(email) {
                Some(email.to_string())
            } else {
                None
            }
        }
        None => None,
    };

    let authorized = authorize_and_wait(hint.as_deref(), ui, &mut |event| ui.emit(event)).await?;
    let Authorized {
        flow,
        redirect_uri,
        code,
    } = authorized;

    ui.say("Exchanging authorization code for tokens...");
    let Exchanged { tokens, identity } =
        exchange_code(&code, &flow.verifier, &flow.state, &redirect_uri).await?;

    // Load the existing config once (missing file → empty default; a corrupt
    // file surfaces as an error so login never clobbers it with defaults).
    let config = load_or_default(config_path)?;

    // Fetch the account+org identity, then MINT a name for it. A failed
    // profile fetch still names the row from the exchange response.
    let profile = fetch_profile(&tokens.access_token)
        .await
        .or_exchange_identity(identity);
    let name = mint_login_name(&config, &profile, account, name_override, ui)?;

    let saved = finish_login_checked(
        config_path,
        route,
        &config,
        &name,
        &tokens,
        profile,
        account,
    )
    .await?;
    ui.emit(LoginEvent::Saved { account: &saved });
    Ok(saved)
}

/// The name a browser login writes under.
///
/// Nothing is prompted for on the happy path. The profile's email is minted into
/// a free name by [`crate::base::identity::mint_name`] — the bare email when no other
/// row carries it, `email/<org-slug>` when one does — so a second org of the same
/// person lands beside the first instead of on top of it.
///
/// Three things override or replace that, in order:
///
/// * `name_override` (`--name`) wins outright, and is REFUSED when some other
///   account already holds it. Taking a name off an existing row is how a login
///   overwrites the wrong credential, and it is the one case a name collision
///   would be silent rather than migrated.
/// * A RE-login (`--account`) keeps the name the row already has. Renaming a
///   row on re-login would break the pins, groups and control-account key that
///   name is the identity for; the row is confirmed to be the same account by
///   [`assert_requested_identity`] a moment later.
/// * A profile with no email at all is the only path left that prompts. A
///   browser login reaches it only when the token-exchange response carried no
///   email either ([`Profile::or_exchange_identity`]); a setup-token login,
///   which never sees an exchange response, reaches it every time. The prompt is
///   [`prompt_account_name`], and the name it comes back with is minted for
///   uniqueness too. Under [`LoginUi::Machine`] there is
///   nobody to prompt, so that path takes [`unnamed_fallback`] directly — the
///   same answer an empty reply to the prompt gives.
pub(super) fn mint_login_name(
    config: &Config,
    profile: &Profile,
    requested_account: Option<&str>,
    name_override: Option<&str>,
    ui: LoginUi,
) -> anyhow::Result<String> {
    let taken = config.taken_names();

    if let Some(explicit) = name_override {
        if taken.contains(explicit) {
            bail!(
                "'{explicit}' is already the name of another account — account names are unique. \
                 Pick a different --name, or re-login that account with `tcr login --account \
                 {explicit}`. Nothing was changed."
            );
        }
        return Ok(explicit.to_string());
    }

    // A re-login keeps the row's own name: it IS the identity every pin, group
    // and `controlAccount` key already points at.
    if let Some(query) = requested_account {
        return Ok(config.resolve_account(query)?.name.clone());
    }

    let base = match (&profile.email, ui) {
        (Some(email), _) => email.clone(),
        // No email and no human: take the same fallback the prompt itself
        // defaults to on an empty answer, rather than prompting into a pipe
        // and reading EOF. A machine caller that wants a specific name passes
        // `--name`.
        (None, LoginUi::Machine) => unnamed_fallback(&taken),
        (None, LoginUi::Terminal) => prompt_account_name(&unnamed_fallback(&taken)),
    };
    Ok(crate::base::identity::mint_name(
        &base,
        profile.org_name.as_deref(),
        profile.org_uuid.as_deref(),
        &taken,
    ))
}

/// The name a setup-token account falls back to when the operator declines to
/// type one: `unnamed`, or the first free `unnamed-N` when that is taken.
///
/// Deliberately NOT the browser flow's `account-{len+1}` fallback. That one is
/// derived from the account COUNT, so removing a row and adding another hands
/// out a name already in use — harmless for a credential that carries an email
/// and a uuid to be resolved by, and not harmless here. A setup-token account
/// has neither (the scope is inference-only), so
/// [`crate::base::identity::resolve`] can only match it on its NAME: two accounts
/// sharing one would resolve onto the same row, and the second login would
/// overwrite the first one's token instead of adding an account.
///
/// Scanning for the first free suffix rather than counting is what makes that
/// structurally impossible instead of merely unlikely.
pub(super) fn unnamed_fallback(taken: &std::collections::HashSet<String>) -> String {
    const BASE: &str = "unnamed";
    if !taken.contains(BASE) {
        return BASE.to_string();
    }
    // Start at 2: `unnamed` itself is the first, so the next one a human reads
    // should be `unnamed-2`, not `unnamed-1`.
    let mut n: u64 = 2;
    loop {
        let candidate = format!("{BASE}-{n}");
        if !taken.contains(&candidate) {
            return candidate;
        }
        n = n.saturating_add(1);
    }
}

/// Whether `s` is plausibly an email address — no whitespace, and exactly one
/// `@` with at least one character on each side. Deliberately not a full RFC
/// 5322 validator: this only ever gates whether a value is safe to hand to
/// `login_hint` (see `build_login_flow`'s doc-comment), where the failure mode
/// of being too strict is "no pre-fill" and the failure mode of being too
/// loose is an unvalidated string on an endpoint already known to reject
/// malformed params outright.
pub(super) fn looks_like_email(s: &str) -> bool {
    if s.contains(char::is_whitespace) {
        return false;
    }
    match s.split_once('@') {
        Some((local, domain)) => !local.is_empty() && !domain.is_empty() && !domain.contains('@'),
        None => false,
    }
}
