//! Read the login the `codex` CLI already holds on this machine.
//!
//! Codex keeps its ChatGPT credential at `$CODEX_HOME/auth.json` (default `~/.codex/auth.json`;
//! `openai/codex` f82e365, `login/src/auth/storage.rs:47-73,162-163`): `{ auth_mode,
//! OPENAI_API_KEY, tokens: { id_token, access_token, refresh_token, account_id }, last_refresh }`.
//! The id token is a JWT whose claims name the email, the ChatGPT account id and the plan
//! (`login/src/token_data.rs`, `IdClaims`); the access token's `exp` is the expiry. A keyring store
//! (`"Codex Auth"`, `storage.rs:243`) also exists and is NOT read here yet.
//!
//! Nothing in this module prints, logs or returns a token in an error. The parse takes a `&str`
//! so the tests feed it fake JSON and never touch a real credential.
//!
//! # Why the env override exists
//!
//! [`AUTH_PATH_ENV`] names a file to read instead of `$CODEX_HOME/auth.json`, for the same two
//! callers `TCR_CLAUDE_CODE_CREDENTIALS` serves: a user whose store is elsewhere, and this
//! crate's own tests, which must never import the developer's REAL Codex login into a temp
//! config. A path that does not exist means "no Codex login on this machine".
//!
//! # Where the environment is read
//!
//! Only in [`auth_path`] and [`default_codex_home`], each a thin wrapper that reads its variables
//! and hands them to a function deciding over values already read ([`checked_auth_path_from`],
//! `codex_home_from`). The tests exercise those, so no test mutates this process's environment:
//! the test harness runs tests on parallel threads that share it, and a test that sets a variable
//! another test reads is a race. The binary's own reads are exercised end to end by
//! `tests/codex_import.rs`, which sets them on the child process it spawns.
//!
//! # Logins tcr will not pool
//!
//! tcr sends every pooled Codex request to one origin, `codexUpstream` (`https://chatgpt.com`), so
//! it imports only a login whose traffic belongs there. Two kinds of ChatGPT workspace send theirs
//! elsewhere, or mark it (`openai/codex` f82e365):
//!
//! - **FedRAMP.** The login's token carries `chatgpt_account_is_fedramp`, and codex sends every
//!   request with `X-OpenAI-Fedramp: true` (`login/src/token_data.rs:39-40,98`;
//!   `model-provider/src/bearer_auth_provider.rs:43-45`). [`CodexLogin::is_fedramp`] reads the
//!   claim, offline.
//! - **Routed.** codex asks `GET /backend-api/wham/accounts/check` which backend serves the
//!   login's workspace. When the answer names another origin, all Responses traffic goes there,
//!   and a `us` or `us_cr` residency adds `x-openai-account-routing-override`
//!   (`model-provider/src/workspace_routing.rs:12,39-77`;
//!   `app-server/src/request_processors/account_processor/workspace_routing.rs:312-318,444-485`).
//!   [`check_workspace_routing`] asks the same question with the login's own bearer, and clears
//!   the login only when its workspace's `workspace_backend_origin` is `NO_CONSTRAINT` or
//!   `https://chatgpt.com` and its `account_routing_override` is `NO_CONSTRAINT`. Only those two
//!   keys and that value are relied on, not how the answer nests its entries
//!   ([`judge_accounts_check`]).

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

/// Read this file instead of `$CODEX_HOME/auth.json`. A value naming a file that does not exist
/// means "no Codex login on this machine".
pub const AUTH_PATH_ENV: &str = "TCR_CODEX_AUTH";
/// The directory Codex keeps its state in, `auth.json` included; unset or empty means `~/.codex`
/// (`openai/codex` `utils/home-dir/src/lib.rs:13-18`).
pub const CODEX_HOME_ENV: &str = "CODEX_HOME";

/// The Codex login, as much of it as tcr can use.
///
/// `Debug` is written by hand, so the two tokens print as their length only: a `{:?}`, a `dbg!` or
/// a `tracing::debug!(?login)` added anywhere later must not be how a credential reaches a log.
#[derive(Clone, PartialEq, Eq)]
pub struct CodexLogin {
    pub access_token: String,
    pub refresh_token: String,
    /// The ChatGPT account (workspace) id: `tokens.account_id`, else the id token's claim, else
    /// the access token's.
    pub account_id: Option<String>,
    /// The ChatGPT user: the id token's `chatgpt_user_id` (else its `user_id`), else the access
    /// token's ([`crate::codex_oauth::IdClaims::chatgpt_user_id`]). With [`Self::account_id`] it
    /// is the login's identity: one person in two workspaces, and two seats of one workspace,
    /// are two accounts.
    pub user_id: Option<String>,
    pub email: Option<String>,
    /// `plus`, `pro`, … — shown back to the user on import and stored as the row's plan.
    pub plan_type: Option<String>,
    /// The access token's `exp`, in epoch **milliseconds**, the unit of
    /// `config::Account::expires_at`.
    pub expires_at_ms: Option<i64>,
    /// The login belongs to a FedRAMP workspace: its id token, or else its access token, carries
    /// `chatgpt_account_is_fedramp: true` ([`crate::codex_oauth::IdClaims`]). tcr refuses to
    /// import such a login (`oauth::import_codex_login`).
    pub is_fedramp: bool,
}

impl std::fmt::Debug for CodexLogin {
    /// The tokens are deliberately not printed, only their length. See the type docs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexLogin")
            .field(
                "access_token",
                &format_args!("<redacted, {} bytes>", self.access_token.len()),
            )
            .field(
                "refresh_token",
                &format_args!("<redacted, {} bytes>", self.refresh_token.len()),
            )
            .field("account_id", &self.account_id)
            .field("user_id", &self.user_id)
            .field("email", &self.email)
            .field("plan_type", &self.plan_type)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("is_fedramp", &self.is_fedramp)
            .finish()
    }
}

#[derive(Deserialize)]
struct AuthDotJson {
    #[serde(default)]
    auth_mode: Option<String>,
    #[serde(default)]
    tokens: Option<TokensOnDisk>,
}

#[derive(Deserialize)]
struct TokensOnDisk {
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
}

/// `$CODEX_HOME`, else `~/.codex`.
pub fn default_codex_home() -> PathBuf {
    codex_home_from(env_path(CODEX_HOME_ENV), env_path("HOME"))
}

/// The file to read: the env override, else `<codex_home or default>/auth.json` — or a refusal
/// when the override and `codex_home` are BOTH given ([`checked_auth_path_from`]).
pub fn auth_path(codex_home: Option<&Path>) -> anyhow::Result<PathBuf> {
    checked_auth_path_from(env_path(AUTH_PATH_ENV).as_deref(), codex_home)
}

/// [`auth_path_from`], refusing the one combination it would resolve silently wrong: an override
/// AND a `codex_home` (`--codex-home`). The override wins in [`auth_path_from`], so a user who
/// follows the second-account path with [`AUTH_PATH_ENV`] still exported would re-import the FIRST
/// login and be told it succeeded — a personal and a Team workspace can share one email, so the
/// success line would not show the mistake either.
pub fn checked_auth_path_from(
    override_path: Option<&Path>,
    codex_home: Option<&Path>,
) -> anyhow::Result<PathBuf> {
    if let (Some(over), Some(home)) = (override_path, codex_home) {
        anyhow::bail!(
            "both {AUTH_PATH_ENV} ({}) and --codex-home ({}) name a Codex login to import, and \
             {AUTH_PATH_ENV} would win: unset {AUTH_PATH_ENV}, or drop --codex-home. Nothing was \
             written.",
            over.display(),
            home.display()
        );
    }
    Ok(auth_path_from(override_path, codex_home))
}

/// [`auth_path`]'s precedence over an override already read: `override_path` (the value of
/// [`AUTH_PATH_ENV`]) wins outright, then `<codex_home>/auth.json` (`--codex-home`), then
/// `<`[`default_codex_home`]`>/auth.json`.
pub fn auth_path_from(override_path: Option<&Path>, codex_home: Option<&Path>) -> PathBuf {
    if let Some(path) = override_path {
        return path.to_path_buf();
    }
    crate::paths::codex_auth_file(&codex_home.map_or_else(default_codex_home, Path::to_path_buf))
}

/// [`default_codex_home`] over the values of [`CODEX_HOME_ENV`] and `HOME`, already read.
fn codex_home_from(codex_home: Option<PathBuf>, user_home: Option<PathBuf>) -> PathBuf {
    crate::paths::codex_home_in(codex_home, user_home)
}

/// An environment variable as a path, `None` when it is unset or empty: an empty `CODEX_HOME` is
/// no `CODEX_HOME`, which is how Codex reads it too.
fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The login at `path` (from [`auth_path`]): `Ok(None)` when there is no file there; `Err` when
/// there is one we cannot read or parse, or one that is not a ChatGPT login.
pub fn read_codex_login(path: &Path) -> anyhow::Result<Option<CodexLogin>> {
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        anyhow::bail!(
            "refusing to import symlinked Codex login {}: refreshed tokens cannot be synchronized safely. Use a regular auth.json or tcr login --codex. Nothing was written.",
            path.display()
        );
    }
    read_auth_file(path)
}

/// [`read_codex_login`]'s reading.
///
/// A file that exists but is not a ChatGPT login (an API-key login, or one with no refresh token)
/// is an `Err` naming that file, never `Ok(None)`: "no login here" would send a user who already
/// ran `codex login` (in API-key mode), or who mistyped `--codex-home`, to do the wrong thing again.
fn read_auth_file(path: &Path) -> anyhow::Result<Option<CodexLogin>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    match parse_auth_json(&text).with_context(|| format!("parsing {}", path.display()))? {
        Some(login) => Ok(Some(login)),
        None => anyhow::bail!(
            "found {}, but it is not a ChatGPT login (an API-key login, or one with no refresh \
             token): tcr pools ChatGPT logins only. Run `codex login` and sign in with ChatGPT, \
             then re-run. Nothing was written.",
            path.display()
        ),
    }
}

/// `Ok(None)` for a file that is not a ChatGPT login (API-key mode, or no tokens).
///
/// A document of the wrong shape is an `Err` carrying serde's error class and position, never
/// serde's own message: that message quotes the value it could not use, and the values in this
/// document are tokens (a `tokens` that is a string rather than an object would be echoed whole).
/// The same rule `codex_oauth::refresh_access_token_at` applies to a refresh response.
pub fn parse_auth_json(text: &str) -> anyhow::Result<Option<CodexLogin>> {
    let doc: AuthDotJson = serde_json::from_str(text).map_err(|err| {
        anyhow::anyhow!(
            "auth.json is not the expected JSON shape ({:?} error at line {} column {})",
            err.classify(),
            err.line(),
            err.column()
        )
    })?;
    if doc.auth_mode.as_deref().is_some_and(|m| m != "chatgpt") {
        return Ok(None);
    }
    let Some(tokens) = doc.tokens else {
        return Ok(None);
    };
    let (Some(access_token), Some(refresh_token)) = (
        tokens.access_token.filter(|t| !t.is_empty()),
        tokens.refresh_token.filter(|t| !t.is_empty()),
    ) else {
        return Ok(None);
    };
    Ok(Some(login_from_tokens(
        tokens.id_token.as_deref(),
        access_token,
        refresh_token,
        tokens.account_id,
    )))
}

/// A [`CodexLogin`] from its token pair, its id token and a stored account id, read the way
/// codex reads them: the account id is the stored one, else the id token's claim, else the
/// access token's; the user, the plan and the FedRAMP mark come from the id token, else the
/// access token; the email from the id token only; the expiry is the access token's `exp`.
///
/// Every reader of a stored Codex login builds it here, so `auth.json` and another tool's copy
/// of the same login ([`crate::import`]) can never disagree on who it is.
pub(crate) fn login_from_tokens(
    id_token: Option<&str>,
    access_token: String,
    refresh_token: String,
    account_id: Option<String>,
) -> CodexLogin {
    let id = id_token
        .and_then(crate::codex_oauth::id_claims)
        .unwrap_or_default();
    let access = crate::codex_oauth::id_claims(&access_token).unwrap_or_default();
    let is_fedramp = id.chatgpt_account_is_fedramp || access.chatgpt_account_is_fedramp;
    CodexLogin {
        expires_at_ms: crate::codex_oauth::jwt_exp_ms(&access_token),
        account_id: account_id
            .filter(|a| !a.is_empty())
            .or(id.chatgpt_account_id)
            .or(access.chatgpt_account_id),
        user_id: id.chatgpt_user_id.or(access.chatgpt_user_id),
        email: id.email,
        plan_type: id.chatgpt_plan_type.or(access.chatgpt_plan_type),
        access_token,
        refresh_token,
        is_fedramp,
    }
}

/// Whether `text` is a Codex `auth.json` holding an API-key login rather than a ChatGPT one:
/// its `auth_mode` is `apikey`, or it has no tokens and carries an `OPENAI_API_KEY`. Reads
/// nothing but those two keys and never returns the key.
pub fn auth_json_is_api_key(text: &str) -> bool {
    #[derive(Deserialize)]
    struct Shape {
        #[serde(default)]
        auth_mode: Option<String>,
        #[serde(default, rename = "OPENAI_API_KEY")]
        api_key: Option<String>,
        #[serde(default)]
        tokens: Option<serde_json::Value>,
    }
    serde_json::from_str::<Shape>(text).is_ok_and(|doc| {
        doc.auth_mode.as_deref() == Some("apikey")
            || (doc.tokens.is_none() && doc.api_key.is_some_and(|key| !key.is_empty()))
    })
}

/// Where codex asks which backend a ChatGPT login's workspace is served from, joined to
/// `codexUpstream` the way the usage probe joins `/backend-api/wham/usage`.
pub const ACCOUNTS_CHECK_PATH: &str = "/backend-api/wham/accounts/check";
/// An entry's key naming the backend origin its workspace's traffic must go to.
const BACKEND_ORIGIN_KEY: &str = "workspace_backend_origin";
/// An entry's key naming a residency the workspace's requests must be routed by.
const ROUTING_OVERRIDE_KEY: &str = "account_routing_override";
/// The value either key carries for a workspace with no constraint.
const NO_CONSTRAINT: &str = "NO_CONSTRAINT";

/// Why `GET /backend-api/wham/accounts/check` does not clear a login for the pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceRefusal {
    /// The answer says, or fails to say, that the login's workspace is served at
    /// `https://chatgpt.com` with no routing override: what it said.
    NotPoolable(String),
    /// No usable answer: why.
    Unconfirmed(String),
}

/// Ask `codex_upstream`'s accounts/check, with the login's own bearer and account id, where the
/// login's workspace is served, and clear it for the pool only when [`judge_accounts_check`] does.
/// The request is bounded by [`crate::probe::PROBE_TIMEOUT`]; every way it can fail is an
/// [`WorkspaceRefusal::Unconfirmed`] naming why, and none of them echoes a token.
///
/// `client` must follow no redirect. Across origins reqwest drops the bearer but keeps the
/// `chatgpt-account-id` header, so a 3xx would carry the login's account id to another origin,
/// whose answer would then decide the import. The import's own client is built that way.
pub async fn check_workspace_routing(
    client: &reqwest::Client,
    codex_upstream: &str,
    login: &CodexLogin,
) -> Result<(), WorkspaceRefusal> {
    let url = format!(
        "{}{ACCOUNTS_CHECK_PATH}",
        codex_upstream.trim_end_matches('/')
    );
    let mut request = client
        .get(&url)
        .header(
            reqwest::header::USER_AGENT,
            concat!("tcr/", env!("CARGO_PKG_VERSION")),
        )
        .bearer_auth(&login.access_token)
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(crate::probe::PROBE_TIMEOUT);
    if let Some(id) = login.account_id.as_deref() {
        request = request.header(crate::provider::CODEX_ACCOUNT_HEADER, id);
    }
    let response = request.send().await.map_err(|err| {
        WorkspaceRefusal::Unconfirmed(if err.is_timeout() {
            format!(
                "GET {url} timed out after {} seconds",
                crate::probe::PROBE_TIMEOUT.as_secs()
            )
        } else if err.is_connect() {
            format!("GET {url} could not connect")
        } else {
            format!("GET {url} failed")
        })
    })?;
    let status = response.status();
    if !status.is_success() {
        let hint = match status.as_u16() {
            401 => {
                " (the access token was refused: run codex once to refresh its login, then retry this import)"
            }
            403 => " (check Codex's connection and workspace access, then retry this import)",
            _ => "",
        };
        return Err(WorkspaceRefusal::Unconfirmed(format!(
            "GET {url} answered HTTP {}{hint}",
            status.as_u16()
        )));
    }
    let body = response.bytes().await.map_err(|_| {
        WorkspaceRefusal::Unconfirmed(format!(
            "GET {url} answered, but its body could not be read"
        ))
    })?;
    let answer: serde_json::Value = serde_json::from_slice(&body).map_err(|_| {
        WorkspaceRefusal::Unconfirmed(format!("GET {url} answered something that is not JSON"))
    })?;
    judge_accounts_check(&answer, login.account_id.as_deref())
}

/// Judge the answer to `GET /backend-api/wham/accounts/check` for the login whose ChatGPT
/// account id is `account_id`.
///
/// An entry is any object in the answer that carries `workspace_backend_origin` or
/// `account_routing_override`, wherever it sits: the answer's layout is not part of what tcr
/// relies on. The login's own entry is one keyed by its account id, or carrying it as a member
/// of its own. When the answer names it, that entry decides, so another workspace the same
/// person belongs to never refuses this login. When it does not (no account id, or one the answer
/// never names), every entry must pass. An entry passes when its origin is `NO_CONSTRAINT` or
/// `https://chatgpt.com` and its override is `NO_CONSTRAINT`; a value left out does not pass.
/// An answer with no entry at all confirms nothing.
pub fn judge_accounts_check(
    answer: &serde_json::Value,
    account_id: Option<&str>,
) -> Result<(), WorkspaceRefusal> {
    let mut entries = Vec::new();
    collect_routing_entries(answer, None, &mut entries);
    if entries.is_empty() {
        return Err(WorkspaceRefusal::Unconfirmed(format!(
            "its answer names no {BACKEND_ORIGIN_KEY} or {ROUTING_OVERRIDE_KEY}"
        )));
    }
    let own: Vec<&RoutingEntry<'_>> = account_id.map_or_else(Vec::new, |id| {
        entries.iter().filter(|entry| entry.names(id)).collect()
    });
    let judged = if own.is_empty() {
        entries.iter().collect()
    } else {
        own
    };
    let problems: Vec<String> = judged.iter().filter_map(|entry| entry.problem()).collect();
    if problems.is_empty() {
        Ok(())
    } else {
        Err(WorkspaceRefusal::NotPoolable(problems.join("; ")))
    }
}

/// One workspace's routing in an accounts/check answer: the object carrying it, and the member
/// name it sits under when its parent is an object (an answer keyed by account id).
struct RoutingEntry<'a> {
    key: Option<&'a str>,
    fields: &'a serde_json::Map<String, serde_json::Value>,
}

impl RoutingEntry<'_> {
    /// Keyed by `account_id`, or carrying it as a member of its own.
    fn names(&self, account_id: &str) -> bool {
        self.key == Some(account_id)
            || self
                .fields
                .values()
                .any(|value| value.as_str() == Some(account_id))
    }

    /// What keeps this workspace out of the pool, or `None` when nothing does.
    fn problem(&self) -> Option<String> {
        let origin = self.fields.get(BACKEND_ORIGIN_KEY);
        let routing = self.fields.get(ROUTING_OVERRIDE_KEY);
        let origin_passes = origin
            .and_then(serde_json::Value::as_str)
            .is_some_and(|o| o == NO_CONSTRAINT || o == crate::provider::CODEX_UPSTREAM);
        let routing_passes = routing.and_then(serde_json::Value::as_str) == Some(NO_CONSTRAINT);
        let mut said = Vec::new();
        if !origin_passes {
            said.push(format!("{BACKEND_ORIGIN_KEY} is {}", shown(origin)));
        }
        if !routing_passes {
            said.push(format!("{ROUTING_OVERRIDE_KEY} is {}", shown(routing)));
        }
        (!said.is_empty()).then(|| said.join(", "))
    }
}

/// Every [`RoutingEntry`] in `value`, depth first.
fn collect_routing_entries<'a>(
    value: &'a serde_json::Value,
    key: Option<&'a str>,
    out: &mut Vec<RoutingEntry<'a>>,
) {
    match value {
        serde_json::Value::Object(fields) => {
            if fields.contains_key(BACKEND_ORIGIN_KEY) || fields.contains_key(ROUTING_OVERRIDE_KEY)
            {
                out.push(RoutingEntry { key, fields });
            }
            for (name, child) in fields {
                collect_routing_entries(child, Some(name), out);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_routing_entries(item, None, out);
            }
        }
        _ => {}
    }
}

/// A routing value as a refusal names it: `absent`, or its JSON, cut short past 80 characters.
fn shown(value: Option<&serde_json::Value>) -> String {
    let Some(value) = value else {
        return "absent".to_string();
    };
    let text = value.to_string();
    match text.char_indices().nth(80) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const ACCOUNT_A: &str = "11111111-1111-4111-8111-111111111111";
    const ACCOUNT_B: &str = "22222222-2222-4222-8222-222222222222";

    /// One workspace's entry in an accounts/check answer, with these two routing values.
    fn workspace(origin: &str, routing: &str) -> serde_json::Value {
        serde_json::json!({
            "workspace_backend_origin": origin,
            "account_routing_override": routing,
            "plan_type": "pro",
            "structure": "personal",
        })
    }

    /// An answer keyed by account id, the way `account_ordering` beside it names them.
    fn keyed(entries: &[(&str, serde_json::Value)]) -> serde_json::Value {
        let accounts: serde_json::Map<String, serde_json::Value> = entries
            .iter()
            .map(|(id, entry)| ((*id).to_string(), entry.clone()))
            .collect();
        let ordering: Vec<&str> = entries.iter().map(|(id, _)| *id).collect();
        serde_json::json!({ "accounts": accounts, "account_ordering": ordering })
    }

    /// A workspace served at chatgpt.com passes: an origin of `NO_CONSTRAINT` or
    /// `https://chatgpt.com`, and an override of `NO_CONSTRAINT`, in an answer keyed by account
    /// id or in a list of entries that carry it.
    ///
    /// Watched red against a judge that refuses every answer.
    #[test]
    fn a_workspace_served_at_chatgpt_com_passes_the_routing_check() {
        for origin in ["NO_CONSTRAINT", "https://chatgpt.com"] {
            let answer = keyed(&[(ACCOUNT_A, workspace(origin, "NO_CONSTRAINT"))]);
            assert_eq!(
                judge_accounts_check(&answer, Some(ACCOUNT_A)),
                Ok(()),
                "{answer}"
            );
            let mut listed = workspace(origin, "NO_CONSTRAINT");
            listed["id"] = serde_json::json!(ACCOUNT_A);
            let answer = serde_json::json!({ "accounts": [listed] });
            assert_eq!(
                judge_accounts_check(&answer, Some(ACCOUNT_A)),
                Ok(()),
                "{answer}"
            );
        }
    }

    /// A workspace routed anywhere else is refused, naming what the answer said: another backend
    /// origin, a residency override, or a routing value the answer leaves out.
    ///
    /// Watched red against a judge that clears every answer.
    #[test]
    fn a_routed_workspace_fails_the_routing_check_naming_its_routing() {
        for (entry, named) in [
            (
                workspace("https://gov.chatgpt.com", "NO_CONSTRAINT"),
                "https://gov.chatgpt.com",
            ),
            (workspace("NO_CONSTRAINT", "us"), "\"us\""),
            (workspace("https://chatgpt.com", "us_cr"), "\"us_cr\""),
            (
                serde_json::json!({ "account_routing_override": "NO_CONSTRAINT" }),
                "workspace_backend_origin is absent",
            ),
            (
                serde_json::json!({ "workspace_backend_origin": "NO_CONSTRAINT" }),
                "account_routing_override is absent",
            ),
        ] {
            let answer = keyed(&[(ACCOUNT_A, entry)]);
            match judge_accounts_check(&answer, Some(ACCOUNT_A)) {
                Err(WorkspaceRefusal::NotPoolable(why)) => {
                    assert!(why.contains(named), "{named}: {why}");
                }
                other => panic!("{answer} must be refused as not poolable, got {other:?}"),
            }
        }
    }

    /// The login's own workspace decides when the answer shows which one it is: keyed by its
    /// account id, or carrying that id. Another workspace the same person belongs to does not
    /// refuse it. When the answer does not show it (no account id, or one the answer never
    /// names), every workspace in it must pass.
    ///
    /// Watched red against a judge that clears every answer.
    #[test]
    fn the_routing_check_judges_the_logins_own_workspace_when_the_answer_names_it() {
        let answer = keyed(&[
            (ACCOUNT_A, workspace("NO_CONSTRAINT", "NO_CONSTRAINT")),
            (
                ACCOUNT_B,
                workspace("https://gov.chatgpt.com", "NO_CONSTRAINT"),
            ),
        ]);
        assert_eq!(judge_accounts_check(&answer, Some(ACCOUNT_A)), Ok(()));
        for account_id in [
            Some(ACCOUNT_B),
            None,
            Some("33333333-3333-4333-8333-333333333333"),
        ] {
            assert!(
                matches!(
                    judge_accounts_check(&answer, account_id),
                    Err(WorkspaceRefusal::NotPoolable(_))
                ),
                "{account_id:?} must be refused"
            );
        }

        // A list whose entries carry their account id as a member of their own.
        let mut own = workspace("NO_CONSTRAINT", "NO_CONSTRAINT");
        own["account_id"] = serde_json::json!(ACCOUNT_A);
        let mut other = workspace("NO_CONSTRAINT", "us");
        other["id"] = serde_json::json!(ACCOUNT_B);
        let listed = serde_json::json!({ "accounts": [own, other.clone()] });
        assert_eq!(judge_accounts_check(&listed, Some(ACCOUNT_A)), Ok(()));

        // An id one level further down does not name the entry it sits under, so nothing names
        // the login's workspace and every entry is judged.
        let mut buried = workspace("NO_CONSTRAINT", "NO_CONSTRAINT");
        buried["account"] = serde_json::json!({ "id": ACCOUNT_A });
        let listed = serde_json::json!({ "accounts": [buried, other] });
        assert!(
            matches!(
                judge_accounts_check(&listed, Some(ACCOUNT_A)),
                Err(WorkspaceRefusal::NotPoolable(_))
            ),
            "{listed}"
        );
    }

    /// An answer with no routing in it at all is no answer: the check could not confirm where
    /// the workspace's traffic goes.
    ///
    /// Watched red against a judge that clears every answer.
    #[test]
    fn an_answer_that_names_no_routing_leaves_the_workspace_unconfirmed() {
        for answer in [
            serde_json::json!({}),
            serde_json::json!({ "accounts": {} }),
            serde_json::json!({ "accounts": [] }),
            serde_json::json!([]),
            serde_json::json!({ "accounts": { ACCOUNT_A: { "plan_type": "pro" } } }),
        ] {
            assert!(
                matches!(
                    judge_accounts_check(&answer, Some(ACCOUNT_A)),
                    Err(WorkspaceRefusal::Unconfirmed(_))
                ),
                "{answer}"
            );
        }
    }

    const JWT_HEADER: &str = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0";
    const ACCESS_PAYLOAD: &str = "eyJleHAiOjE4MDAwMDAwMDAsImh0dHBzOi8vYXBpLm9wZW5haS5jb20vYXV0aCI6eyJjaGF0Z3B0X2FjY291bnRfaWQiOiIxMTExMTExMS0xMTExLTQxMTEtODExMS0xMTExMTExMTExMTEiLCJjaGF0Z3B0X3BsYW5fdHlwZSI6InBybyJ9fQ";
    const ID_PAYLOAD: &str = "eyJleHAiOjE4MDAwMDAwMDAsImVtYWlsIjoiYWxpY2VAZXhhbXBsZS5jb20iLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiMTExMTExMTEtMTExMS00MTExLTgxMTEtMTExMTExMTExMTExIiwiY2hhdGdwdF9wbGFuX3R5cGUiOiJwcm8iLCJjaGF0Z3B0X3VzZXJfaWQiOiJ1c2VyLWZha2UifX0";

    async fn codex_header_stub(status: u16) -> (String, Arc<std::sync::Mutex<Option<String>>>) {
        let captured = Arc::new(std::sync::Mutex::new(None));
        let seen = Arc::clone(&captured);
        let answer = keyed(&[(ACCOUNT_A, workspace("NO_CONSTRAINT", "NO_CONSTRAINT"))]);
        let app = axum::Router::new().fallback(move |headers: axum::http::HeaderMap| {
            let seen = Arc::clone(&seen);
            let answer = answer.clone();
            async move {
                *seen.lock().unwrap() = headers
                    .get("user-agent")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                (
                    axum::http::StatusCode::from_u16(status).unwrap(),
                    axum::Json(answer),
                )
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}"), captured)
    }

    #[tokio::test]
    async fn codex_workspace_import_check_identifies_tcr_in_the_user_agent() {
        let (base, captured) = codex_header_stub(200).await;
        let login = parse_auth_json(&fake_auth_json()).unwrap().unwrap();
        let client = crate::http_client::builder().no_proxy().build().unwrap();
        check_workspace_routing(&client, &base, &login)
            .await
            .unwrap();
        assert_eq!(
            captured.lock().unwrap().as_deref(),
            Some(concat!("tcr/", env!("CARGO_PKG_VERSION")))
        );
    }

    #[tokio::test]
    async fn codex_expired_import_suggests_refreshing_without_revoking_the_login() {
        let (base, _) = codex_header_stub(401).await;
        let mut login = parse_auth_json(&fake_auth_json()).unwrap().unwrap();
        login.expires_at_ms = Some(0);
        let client = crate::http_client::builder().no_proxy().build().unwrap();
        let Err(WorkspaceRefusal::Unconfirmed(why)) =
            check_workspace_routing(&client, &base, &login).await
        else {
            panic!("the refused access token must leave the workspace unconfirmed")
        };
        assert!(why.contains("run codex once"), "{why}");
        assert!(!why.contains("run `codex login`"), "{why}");
    }

    #[test]
    fn codex_import_refuses_a_symlink_before_it_can_promise_token_sharing() {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("stored-login.json");
        let path = crate::paths::codex_auth_file(directory.path());
        std::fs::write(&original, fake_auth_json()).unwrap();
        std::os::unix::fs::symlink(&original, &path).unwrap();
        let error = read_codex_login(&path).expect_err("a symlink cannot be synchronized safely");
        assert!(error.to_string().contains("symlink"));
        assert_eq!(std::fs::read_to_string(original).unwrap(), fake_auth_json());
        assert!(
            std::fs::symlink_metadata(path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    fn fake_auth_json() -> String {
        format!(
            r#"{{"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{{"id_token":"{JWT_HEADER}.{ID_PAYLOAD}.sig","access_token":"{JWT_HEADER}.{ACCESS_PAYLOAD}.sig","refresh_token":"rt-fake-codex","account_id":"11111111-1111-4111-8111-111111111111"}},"last_refresh":"2026-10-03T06:44:00.000000Z"}}"#
        )
    }

    #[test]
    fn a_chatgpt_login_parses_with_identity_plan_and_expiry() {
        let login = parse_auth_json(&fake_auth_json())
            .unwrap()
            .expect("a login");
        assert_eq!(login.email.as_deref(), Some("alice@example.com"));
        assert_eq!(
            login.account_id.as_deref(),
            Some("11111111-1111-4111-8111-111111111111")
        );
        assert_eq!(login.plan_type.as_deref(), Some("pro"));
        assert_eq!(login.expires_at_ms, Some(1_800_000_000_000));
        assert_eq!(login.refresh_token, "rt-fake-codex");
        assert!(login.access_token.starts_with(JWT_HEADER));
    }

    #[test]
    fn an_api_key_login_or_missing_tokens_is_not_a_login() {
        assert!(
            parse_auth_json(r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-fake"}"#)
                .unwrap()
                .is_none()
        );
        assert!(
            parse_auth_json(&fake_auth_json().replace(r#""chatgpt""#, r#""apikey""#))
                .unwrap()
                .is_none(),
            "an API-key login is not one tcr can pool, whatever tokens ride along"
        );
        assert!(
            parse_auth_json(r#"{"auth_mode":"chatgpt"}"#)
                .unwrap()
                .is_none()
        );
        assert!(
            parse_auth_json(r#"{"auth_mode":"chatgpt","tokens":{"access_token":"x"}}"#)
                .unwrap()
                .is_none(),
            "no refresh token is no login"
        );
        assert!(parse_auth_json("not json").is_err());
    }

    /// `tcr import` names an API-key `auth.json` by what it is: `auth_mode: apikey`, or a key and
    /// no tokens. A ChatGPT login, a file with neither, and text that is not JSON are not one.
    ///
    /// Watched red against a check that reads `auth_mode` alone.
    #[test]
    fn an_api_key_auth_json_is_told_from_a_chatgpt_login() {
        assert!(auth_json_is_api_key(
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-fake"}"#
        ));
        assert!(auth_json_is_api_key(r#"{"OPENAI_API_KEY":"sk-fake"}"#));
        assert!(!auth_json_is_api_key(&fake_auth_json()));
        assert!(!auth_json_is_api_key(r#"{"OPENAI_API_KEY":null}"#));
        assert!(!auth_json_is_api_key("not json"));
    }

    /// A login whose tokens carry `chatgpt_account_is_fedramp: true` parses as a FedRAMP login:
    /// from the id token, where codex reads the claim, and from the access token too, which the
    /// account id and plan already fall back to. The fixture login carries no such claim. The
    /// payloads are `{"exp":1800000000,"email":"alice@example.com",
    /// "https://api.openai.com/auth":{"chatgpt_account_id":"11111111-1111-4111-8111-111111111111",
    /// "chatgpt_plan_type":"enterprise","chatgpt_account_is_fedramp":true}}` and the same without
    /// the email.
    #[test]
    fn a_login_whose_token_claims_fedramp_is_marked_as_one() {
        const FEDRAMP_ID_PAYLOAD: &str = "eyJleHAiOjE4MDAwMDAwMDAsImVtYWlsIjoiYWxpY2VAZXhhbXBsZS5jb20iLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiMTExMTExMTEtMTExMS00MTExLTgxMTEtMTExMTExMTExMTExIiwiY2hhdGdwdF9wbGFuX3R5cGUiOiJlbnRlcnByaXNlIiwiY2hhdGdwdF9hY2NvdW50X2lzX2ZlZHJhbXAiOnRydWV9fQ";
        const FEDRAMP_ACCESS_PAYLOAD: &str = "eyJleHAiOjE4MDAwMDAwMDAsImh0dHBzOi8vYXBpLm9wZW5haS5jb20vYXV0aCI6eyJjaGF0Z3B0X2FjY291bnRfaWQiOiIxMTExMTExMS0xMTExLTQxMTEtODExMS0xMTExMTExMTExMTEiLCJjaGF0Z3B0X3BsYW5fdHlwZSI6ImVudGVycHJpc2UiLCJjaGF0Z3B0X2FjY291bnRfaXNfZmVkcmFtcCI6dHJ1ZX19";
        let auth_json = |id: &str, access: &str| {
            fake_auth_json()
                .replace(ID_PAYLOAD, id)
                .replace(ACCESS_PAYLOAD, access)
        };
        let by_id = parse_auth_json(&auth_json(FEDRAMP_ID_PAYLOAD, ACCESS_PAYLOAD))
            .unwrap()
            .expect("a login");
        assert!(by_id.is_fedramp, "the id token's claim: {by_id:?}");
        let by_access = parse_auth_json(&auth_json(ID_PAYLOAD, FEDRAMP_ACCESS_PAYLOAD))
            .unwrap()
            .expect("a login");
        assert!(
            by_access.is_fedramp,
            "the access token's claim: {by_access:?}"
        );
        let plain = parse_auth_json(&fake_auth_json())
            .unwrap()
            .expect("a login");
        assert!(!plain.is_fedramp, "no claim, no FedRAMP: {plain:?}");
    }

    #[test]
    fn account_id_falls_back_to_the_id_token_claim() {
        let without = fake_auth_json().replace(
            r#","account_id":"11111111-1111-4111-8111-111111111111""#,
            "",
        );
        let login = parse_auth_json(&without).unwrap().unwrap();
        assert_eq!(
            login.account_id.as_deref(),
            Some("11111111-1111-4111-8111-111111111111")
        );
    }

    /// A document of the wrong shape names serde's class and position, never serde's message:
    /// for a `tokens` that is a string, that message quotes the string, and here it is a token.
    #[test]
    fn a_document_of_the_wrong_shape_never_echoes_its_tokens() {
        let err = parse_auth_json(r#"{"auth_mode":"chatgpt","tokens":"rt-must-not-echo"}"#)
            .expect_err("a string where the tokens object belongs must not parse");
        let text = format!("{err:#}");
        assert!(!text.contains("rt-must-not-echo"), "{text}");
        assert!(text.contains("Data error at line 1"), "{text}");
    }

    #[test]
    fn the_override_path_wins_over_codex_home() {
        let over = Path::new("/tmp/x/override.json");
        let home = Path::new("/tmp/x/second-login");
        assert_eq!(auth_path_from(Some(over), Some(home)), over);
        assert_eq!(auth_path_from(Some(over), None), over);
        assert_eq!(
            auth_path_from(None, Some(home)),
            Path::new("/tmp/x/second-login/auth.json")
        );
    }

    #[test]
    fn codex_home_is_its_env_value_else_dot_codex_under_home() {
        assert_eq!(
            codex_home_from(
                Some(PathBuf::from("/tmp/x/codex")),
                Some(PathBuf::from("/tmp/x"))
            ),
            Path::new("/tmp/x/codex")
        );
        assert_eq!(
            codex_home_from(None, Some(PathBuf::from("/tmp/x"))),
            Path::new("/tmp/x/.codex")
        );
    }

    #[test]
    fn a_missing_file_is_no_login_and_a_present_one_parses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nowhere.json");
        assert!(read_auth_file(&path).unwrap().is_none());
        std::fs::write(&path, fake_auth_json()).unwrap();
        assert!(read_auth_file(&path).unwrap().is_some());
    }

    /// The override and `--codex-home` together are refused, naming both — the
    /// override would win and silently re-import the FIRST login. Either alone resolves
    /// as [`auth_path_from`] always has.
    #[test]
    fn the_override_and_codex_home_together_are_refused() {
        let over = Path::new("/tmp/x/override.json");
        let home = Path::new("/tmp/x/second-login");
        let err = checked_auth_path_from(Some(over), Some(home))
            .expect_err("both given is a conflict, not a precedence");
        let text = err.to_string();
        assert!(
            text.contains(AUTH_PATH_ENV)
                && text.contains("--codex-home")
                && text.contains("/tmp/x/override.json")
                && text.contains("/tmp/x/second-login"),
            "the refusal names both inputs: {text}"
        );
        assert_eq!(checked_auth_path_from(Some(over), None).unwrap(), over);
        assert_eq!(
            checked_auth_path_from(None, Some(home)).unwrap(),
            Path::new("/tmp/x/second-login/auth.json")
        );
    }

    /// A file that is there but is not a ChatGPT login is named, with what is wrong
    /// with it — never reported as "no Codex login", which sends an API-key user back to
    /// the `codex login` they already ran.
    #[test]
    fn a_file_that_is_not_a_chatgpt_login_is_named_not_reported_missing() {
        let dir = tempfile::tempdir().unwrap();
        for (label, text) in [
            (
                "an API-key login",
                r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-fake"}"#.to_string(),
            ),
            (
                "a login with no refresh token",
                r#"{"auth_mode":"chatgpt","tokens":{"access_token":"at-fake"}}"#.to_string(),
            ),
        ] {
            let path = dir.path().join("auth.json");
            std::fs::write(&path, text).unwrap();
            let err = read_auth_file(&path).expect_err("there IS a file: an error, not None");
            let message = err.to_string();
            assert!(
                message.contains(&format!(
                    "found {}, but it is not a ChatGPT login",
                    path.display()
                )),
                "{label}: {message}"
            );
            assert!(!message.contains("sk-fake"), "{label}: {message}");
        }
    }

    /// `{:?}` of a login prints its identity, never a token — only the tokens' length.
    #[test]
    fn debug_prints_no_token() {
        let login = parse_auth_json(&fake_auth_json()).unwrap().unwrap();
        let text = format!("{login:?}");
        assert!(
            !text.contains("rt-fake-codex") && !text.contains(ACCESS_PAYLOAD),
            "{text}"
        );
        assert!(
            text.contains("<redacted") && text.contains("alice@example.com"),
            "{text}"
        );
    }
}
