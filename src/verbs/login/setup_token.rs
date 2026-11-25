//! `tcr login --token`: a pasted `claude setup-token` credential, checked once
//! against upstream before it is written, and the `unnamed` name it falls back to.

use super::*;

/// Our ASSUMPTION about a `claude setup-token` credential's lifetime — one
/// year, matching the `expires_in: 31536000` the Claude CLI requests when it
/// mints one. Not something [`login_with_token`] can read out of the token
/// itself (it never sees the token-endpoint response, only the finished
/// access token pasted or piped in), so this is a best estimate, stamped so
/// `tcr status` shows something truthful rather than "no expiry".
const SETUP_TOKEN_ASSUMED_LIFETIME_MS: i64 = 365 * 24 * 60 * 60 * 1000;

/// Read a `claude setup-token` access token from stdin — never from argv (see
/// `--token`'s doc comment in `main.rs`: an argv value is visible in `ps` and
/// lands in shell history). Prompts on stderr first when stdin is a
/// terminal; reads a bare line straight through when it is piped. Never logs
/// or prints the token back.
fn read_setup_token() -> anyhow::Result<String> {
    if std::io::stdin().is_terminal() {
        eprint!("Paste the token from `claude setup-token`: ");
        std::io::stderr().flush().log_debug("flushing the prompt");
    }
    let mut input = String::new();
    if std::io::stdin().is_terminal() {
        std::io::stdin()
            .read_line(&mut input)
            .context("read setup-token from stdin")?;
    } else {
        // Piped: read it all, so a `tcr mint --group` block of several
        // accounts is refused by name instead of silently using its first.
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)
            .context("read setup-token from stdin")?;
    }
    parse_setup_token(&input)
}

/// Turn what was pasted or piped into the bare access token.
///
/// The token is the first non-empty line. Three shapes of it are handled here,
/// each one a paste that used to reach upstream as a wrong bearer and come back
/// as a 401 blaming the token:
///
/// - a `tcr mint` clipboard line, `<name>\t<token>` (`assemble_clipboard_block`
///   in `src/verbs/mint.rs`): the token is the part after the last tab;
/// - whitespace inside the token, left by copying one a terminal had wrapped:
///   removed, since no token contains any;
/// - the `code#state` the sign-in page shows: refused before any upstream call,
///   because a token never contains `#` and the fix is a different step.
///
/// More than one `tcr mint` line is refused: `--token` adds one account, and
/// picking the first would add it under whatever `--name` said.
pub(super) fn parse_setup_token(input: &str) -> anyhow::Result<String> {
    let mut lines = input.lines().filter(|l| !l.trim().is_empty());
    let Some(first) = lines.next() else {
        bail!("no token read from stdin — pipe or paste the output of `claude setup-token`");
    };
    if first.contains('\t') && lines.any(|l| l.contains('\t')) {
        bail!(
            "this is a `tcr mint` block for more than one account, and --token adds one. \
             Pass one line at a time. Nothing was written."
        );
    }
    let field = first
        .rsplit_once('\t')
        .map_or(first, |(_name, token)| token);
    let token: String = field.chars().filter(|c| !c.is_whitespace()).collect();
    if token.is_empty() {
        bail!("no token read from stdin — pipe or paste the output of `claude setup-token`");
    }
    if token.contains('#') {
        bail!(
            "this is the sign-in code (code#state), not a token. Paste it into the command \
             that printed the sign-in URL (`claude setup-token` or `tcr mint`), then give \
             --token the token that command produces. Nothing was written."
        );
    }
    Ok(token)
}

/// What one authenticated call said about a pasted setup token, before it is
/// written anywhere.
#[derive(Debug, PartialEq, Eq)]
pub enum SetupTokenCheck {
    /// Upstream accepted the credential (2xx, or a 429/5xx — a quota or server
    /// answer is still one that got past auth).
    Accepted,
    /// Upstream refused the credential (401 or 403). Nothing may be written.
    Rejected { status: u16 },
    /// The call never got an upstream answer (DNS, TLS, timeout). Proves
    /// nothing either way.
    Unreachable { message: String },
}

/// Make ONE authenticated call with `access_token` against `upstream` — the
/// same 1-token `POST /v1/messages` the warmer sends — and classify the answer.
///
/// This exists because a `claude setup-token` credential is the one kind of
/// account nothing else ever validates: it has no refresh token, so the prober
/// skips it, a refresh has nothing to send, and the proxy's 401 arm can only
/// sideline it after the first real request has already failed. A bad paste
/// therefore used to be accepted silently and reported `active` for as long
/// as the config held it. The Messages endpoint is used rather than the
/// profile endpoint because the profile fetch usually fails for an
/// inference-only scope whether the token is good or not, so it cannot tell
/// the two apart. The call is a real 1-token completion: it spends a trivial
/// amount of quota and starts the account's 5h window, which a login is about
/// to do anyway.
pub async fn verify_setup_token(upstream: &str, access_token: &str) -> SetupTokenCheck {
    use crate::clients::warmer::AccountWarmer as _;
    let warmer = crate::clients::warmer::LiveWarmer::new();
    match warmer
        .warm(access_token.to_string(), upstream.to_string())
        .await
    {
        Ok(_) => SetupTokenCheck::Accepted,
        Err(crate::clients::warmer::WarmError {
            status: Some(status @ (401 | 403)),
            ..
        }) => SetupTokenCheck::Rejected { status },
        Err(crate::clients::warmer::WarmError {
            status: Some(_), ..
        }) => SetupTokenCheck::Accepted,
        Err(crate::clients::warmer::WarmError {
            status: None,
            message,
        }) => SetupTokenCheck::Unreachable { message },
    }
}

/// Add an account from a `claude setup-token` credential instead of the
/// browser flow. Reuses every downstream layer [`login`] does — the same
/// [`admit_login`] guard against a live server, the same [`fetch_profile`] /
/// [`finish_login`] pipeline — and replaces only the browser half: instead of
/// a PKCE round trip, the access token is read from stdin
/// ([`read_setup_token`]) and wrapped in a [`Tokens`] with `refresh_token:
/// None` — a setup-token mint carries only the `user:inference` scope, and
/// the Claude CLI that produces it never surfaces its refresh token to the
/// caller, so there is genuinely nothing to store.
///
/// `--account` is refused up front (before the port probe or the
/// stdin read): an inference-only token carries no identity for
/// [`assert_requested_identity`] to confirm, and an assertion that cannot be
/// evaluated must fail closed rather than pass — so this calls [`finish_login`]
/// directly, never `finish_login_checked`.
///
/// Because the scope is inference-only, [`fetch_profile`] will very likely
/// come back all-`None` (no email, no org) — expected, not an error. The
/// account name falls back to `--name`, then a prompt, exactly like the
/// browser flow's own fallback ([`prompt_account_name`]).
///
/// Always prints [`crate::proxy::NO_REFRESH_TOKEN_WARNING`] on success: this
/// account serves until its (assumed) one-year expiry and then goes dead,
/// with nothing to renew it, and — unlike the live-add wire route — this
/// offline path has no other caller that would say so.
pub async fn login_with_token(
    config_path: &Path,
    force: bool,
    name: Option<&str>,
    account: Option<&str>,
) -> anyhow::Result<String> {
    if account.is_some() {
        bail!(
            "--token cannot be combined with --account: a `claude setup-token` \
             credential carries only the user:inference scope, so it has no email and no \
             account id for that flag to confirm against — an identity assertion that \
             cannot be evaluated must fail closed rather than pass. Log in with --token alone \
             (use --name to choose the account name), or drop --token and use the browser flow \
             with --account instead. Nothing was written."
        );
    }

    let route = admit_login(config_path, force, LoginCommand::Browser)
        .await?
        .route;

    let access_token = read_setup_token()?;
    let tokens = Tokens {
        id_token: None,
        access_token,
        refresh_token: None,
        expires_at_ms: crate::now_ms() + SETUP_TOKEN_ASSUMED_LIFETIME_MS,
    };

    let config = load_or_default(config_path)?;

    // One authenticated call BEFORE anything is written: a rejected token is
    // refused here, because nothing downstream would ever catch it (see
    // `verify_setup_token`).
    match verify_setup_token(&config.upstream, &tokens.access_token).await {
        SetupTokenCheck::Accepted => {}
        SetupTokenCheck::Rejected { status } => bail!(
            "upstream refused this token (HTTP {status}) — it is expired, revoked or mistyped. \
             A token pasted at the prompt is read up to its first line break, so one that \
             wrapped over two lines arrives cut short: pipe it in instead. Otherwise mint a \
             fresh one with `claude setup-token` and try again. Nothing was written."
        ),
        SetupTokenCheck::Unreachable { message } => {
            eprintln!(
                "[tcr] warning: could not reach {} to verify the token ({message}) — \
                 storing it unverified; a bad token will be marked `error` on its first request",
                config.upstream
            );
        }
    }

    let profile = fetch_profile(&tokens.access_token).await;
    // Always `Terminal`: `--token` reads the credential from stdin itself, so
    // it is refused in combination with `--non-interactive` (see `LoginArgs`
    // in `src/main.rs`) and this path never runs under a GUI parent.
    let resolved_name = mint_login_name(&config, &profile, None, name, LoginUi::Terminal)?;

    let result = finish_login(
        config_path,
        route,
        &config,
        &resolved_name,
        &tokens,
        profile,
    )
    .await;
    if result.is_ok() {
        eprintln!("[tcr] warning: {}", crate::proxy::NO_REFRESH_TOKEN_WARNING);
    }
    result
}
