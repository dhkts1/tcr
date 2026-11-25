//! Importing the login the `codex` CLI itself uses (`tcr login --from-codex`):
//! the notices, the route check, the workspace refusals and the name it saves under.

use super::*;

/// The limits of sharing an imported Codex login, printed on every import.
/// Codex refresh tokens rotate and are single-use. tcr checks the recorded auth file before
/// updating it, but Codex does not take tcr's writer lock and can race the replacement.
pub const CODEX_IMPORT_CONSEQUENCE: &str = "this shares the login the `codex` CLI itself uses. \
    Codex refresh tokens are single-use: tcr checks auth.json before writing refreshed tokens \
    and can adopt replacement tokens for the same ChatGPT account. A simultaneous codex refresh \
    can still race this update. Signing in again with `codex login` in that folder revokes \
    this shared login; tcr may then need sign-in. `tcr login --codex` gives tcr a separate login.";

/// A standalone override names no Codex auth.json to synchronize after a refresh.
pub const CODEX_IMPORT_STANDALONE_CONSEQUENCE: &str = "this imports a shared Codex login \
    without automatic file synchronization. Codex refresh tokens are single-use: refreshing \
    here can leave the original store stale, and signing in again with `codex login` there \
    revokes this shared login. `tcr login --codex` gives tcr a separate login.";

/// The compatibility notice printed beside [`CODEX_IMPORT_CONSEQUENCE`] on every import:
/// the config now holds a Codex row, and a tcr built before Codex accounts cannot tell it from a
/// Claude one. What such a build does with the file depends on its age:
///
/// - one with the `credentialVersion` check (0.2.46 onward) refuses the whole file, because the
///   row is written at `config::CODEX_CREDENTIAL_VERSION`, so its server refuses to start and
///   leaves the file as it is;
/// - one from before that check (0.2.45 and older) never reads the key, and serves the Codex
///   login as a Claude account, sending its OpenAI tokens to Anthropic. Nothing in the file can
///   stop that build.
///
/// So the notice names both outcomes and asks for every tcr that may start on this config to be
/// updated before it next starts, the one TcrBar bundles (and runs ahead of `PATH`) included.
pub const CODEX_IMPORT_OLDER_PROXY: &str = "Update every tcr that may start on this config, \
    TcrBar's bundled one included, before it next starts: an older one refuses to start, or reads \
    this login as a Claude account.";

/// `tcr login --from-codex` — import the login this machine's `codex` CLI
/// already holds, read by [`crate::codex_login::read_codex_login`]. One of the
/// two Codex logins: `tcr login --codex` ([`crate::codex_cli_login::login`])
/// signs in one of tcr's own instead. A second account imported this way is
/// logged in with `CODEX_HOME=<dir> codex login` and imported from that dir
/// (`codex_home`).
///
/// This is the READ: which file ([`crate::codex_login::auth_path`], which
/// refuses `TCR_CODEX_AUTH` and `--codex-home` together), and what is in it —
/// no file, or a file that is not a ChatGPT login, each named by its path.
/// Everything after it is `import_codex_login`, split out so a test can hand
/// it a login without this process's environment deciding which one.
/// [`CODEX_IMPORT_CONSEQUENCE`] and [`CODEX_IMPORT_OLDER_PROXY`] print on every
/// import.
///
/// An auth.json import records its absolute parent (`codexHome`) for guarded token sharing
/// and TcrBar's re-import. A differently named override file is imported without a sync folder:
/// its parent does not identify the auth.json that Codex writes. `ui` decides how the
/// import speaks: the terminal gets its lines on stderr, then [`CODEX_IMPORT_RUN_LINE`], then
/// each notice that holds ([`crate::codex_local::codex_notices`]); [`LoginUi::Machine`] gets the
/// import's cost as one `notice` event, then those notices as events, then `saved` with the
/// provider and the plan. Nothing is said before the import is written, so a refusal is the
/// only line a refused import prints.
pub async fn login_from_codex(
    config_path: &Path,
    force: bool,
    name: Option<&str>,
    account: Option<&str>,
    codex_home: Option<&Path>,
    ui: LoginUi,
) -> anyhow::Result<String> {
    ui.install();
    let path = crate::codex_login::auth_path(codex_home)?;
    let Some(login) = crate::codex_login::read_codex_login(&path)? else {
        bail!("{}", no_codex_login_message(&path));
    };
    let folder = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let folder = (path.file_name() == Some(std::ffi::OsStr::new(crate::paths::CODEX_AUTH_FILE)))
        .then(|| {
            std::path::absolute(folder)
                .with_context(|| format!("making {} absolute", folder.display()))
        })
        .transpose()?;
    let plan = tcr_status_wire::codex_plan_label(login.plan_type.as_deref());
    let first = holds_no_codex_row(config_path)?;
    let saved =
        import_codex_login(config_path, force, name, account, login, folder.as_deref()).await?;
    let first_account = if first {
        Some(read_first_codex_account_notice(config_path).await)
    } else {
        None
    };
    let notices = crate::codex_local::codex_notices(first_account);
    let consequence = if folder.is_some() {
        CODEX_IMPORT_CONSEQUENCE
    } else {
        CODEX_IMPORT_STANDALONE_CONSEQUENCE
    };
    match ui {
        LoginUi::Terminal => {
            // The plan as `tcr status` names it (`ChatGPT Pro`), not the raw claim (`pro`).
            eprintln!(
                "[tcr] imported as '{saved}'{}; {consequence} {CODEX_IMPORT_OLDER_PROXY}",
                plan.as_deref()
                    .map_or_else(String::new, |label| format!(" ({label})"))
            );
            eprintln!("{CODEX_IMPORT_RUN_LINE}");
            say_codex_notices(ui, &notices);
        }
        LoginUi::Machine => {
            let consequence = format!("{consequence} {CODEX_IMPORT_OLDER_PROXY}");
            ui.emit(LoginEvent::Notice { text: &consequence });
            say_codex_notices(ui, &notices);
            ui.emit(LoginEvent::SavedCodex {
                account: &saved,
                plan: plan.as_deref(),
            });
        }
    }
    Ok(saved)
}

/// What `tcr login --from-codex` says when there is no login at `path`: how to make one, and,
/// last, that a login codex keeps in the system keyring (its `keyring` and `auto` stores) has no
/// file tcr can read, which `tcr login --codex` does not need.
pub fn no_codex_login_message(path: &Path) -> String {
    format!(
        "no Codex login found at {} — nothing was written. Log in with `codex login` first (for \
         a second account: `mkdir -p <dir>`, `CODEX_HOME=<dir> codex login`, then `tcr login \
         --from-codex --codex-home <dir>`). If codex keeps this login in the system keyring, \
         which tcr does not read, use tcr login --codex instead, which needs no file.",
        path.display()
    )
}

/// Whether the config at `config_path` holds no Codex row in any state: enabled, disabled or
/// needing a re-login all count. Read before a Codex save, it decides
/// [`CODEX_FIRST_ACCOUNT_NOTICE`].
pub(crate) fn holds_no_codex_row(config_path: &Path) -> anyhow::Result<bool> {
    Ok(!load_or_default(config_path)?.holds_provider(crate::provider::Provider::Codex))
}

/// What a Codex save says when the config held no Codex row before it: whether tcr pools codex
/// from now on. [`CODEX_FIRST_ACCOUNT_NOTICE`] only when the proxy's leaf takes `chatgpt.com`:
/// the running proxy reports a leaf it minted that intercepts it, or nothing listens on `port`
/// and the next boot mints such a leaf. Otherwise `codex is not pooled: {reason}`, the sentence `tcr status`
/// prints after `codexVerdict:`, with [`crate::codex_route::codex_route`]'s reason over the same
/// read, so it never claims what an unread proxy does.
///
/// Keyed on the leaf, not on the route's `pooled`: a running server may not have admitted the
/// new row yet, and the route over its payload would then say it holds no Codex account.
pub fn first_codex_account_notice(
    live: &Result<crate::status::StatusPayload, crate::live_client::LiveStatusError>,
    port: u16,
    ca: &Path,
    ca_readable: bool,
) -> String {
    let leaf_takes_chatgpt = match live {
        Ok(payload) => payload.minted_leaf_intercepts(crate::provider::CODEX_HOST),
        Err(crate::live_client::LiveStatusError::NoServer) => true,
        Err(_) => false,
    };
    let codex_rows = live
        .as_ref()
        .map_or(0, crate::codex_route::listed_codex_accounts);
    let route = crate::codex_route::codex_route(live.as_ref(), port, ca, ca_readable, codex_rows);
    match route.reason {
        Some(reason) if !leaf_takes_chatgpt => format!("codex is not pooled: {reason}"),
        _ => CODEX_FIRST_ACCOUNT_NOTICE.to_string(),
    }
}

/// [`first_codex_account_notice`] over one read of the proxy the config at `config_path` names,
/// and whether tcr's CA exists (an existence check, never a read of the file).
///
/// It follows a save that already happened, so it cannot fail the login: a config it cannot read
/// is read as the default one, by falling back to the serde default.
pub(crate) async fn read_first_codex_account_notice(config_path: &Path) -> String {
    let Ok(config) = config::load(config_path).or_else(|_| serde_json::from_str::<Config>("{}"))
    else {
        return CODEX_FIRST_ACCOUNT_NOTICE.to_string();
    };
    let live = crate::live_client::fetch_live_status(&config).await;
    let ca = crate::tls::minted_ca_path();
    first_codex_account_notice(&live, config.proxy.port, &ca, ca.is_file())
}

/// Say each of `notices`, in order: a `notice` event apiece in [`LoginUi::Machine`]; a stderr
/// line apiece in the terminal, on the `[tcr] ` rung or, for a warning, `[tcr] warning: `.
pub(crate) fn say_codex_notices(ui: LoginUi, notices: &[crate::codex_local::CodexNotice]) {
    for notice in notices {
        match ui {
            LoginUi::Machine => ui.emit(LoginEvent::Notice { text: &notice.text }),
            LoginUi::Terminal if notice.warning => eprintln!("[tcr] warning: {}", notice.text),
            LoginUi::Terminal => eprintln!("[tcr] {}", notice.text),
        }
    }
}

/// The route check `tcr login --codex` runs before codex does, so a proxy that cannot take the
/// login refuses it before the person spends a browser sign-in on it: the route, as every login
/// checks it, and on the live route the provider gate [`import_codex_login`] applies, naming
/// `tcr login --codex` as the command to run again.
pub(crate) async fn codex_login_route_check(config_path: &Path, force: bool) -> anyhow::Result<()> {
    admit_login(config_path, force, LoginCommand::Codex)
        .await?
        .require_codex(LoginCommand::Codex)
}

/// The write half of `tcr login --from-codex`: everything after the read, given
/// the login. The Codex twin of [`import_claude_code_login`]: the same route
/// probe and persist path every other login takes ([`admit_login`],
/// [`finish_login_account`]), so a running proxy, `--force` and a corrupt config
/// behave as they do for `tcr login`. The row is [`codex_account_row`]'s, with
/// `provider: codex`.
///
/// In order, and nothing is written before the last step:
///
/// 0. **A FedRAMP login is refused**, before anything else and offline: its token
///    carries `chatgpt_account_is_fedramp` ([`crate::codex_login::CodexLogin::is_fedramp`]),
///    so codex marks its requests `X-OpenAI-Fedramp` and may route them away from
///    chatgpt.com, where tcr would send them.
/// 1. `account` (`--account`) — a guard against importing the wrong `CODEX_HOME`.
///    A name that is a row, exactly as `tcr accounts` prints it, must be a Codex
///    row this login is ([`crate::identity::same_identity`] against the row
///    [`codex_account_row`] would write); a Claude row is refused with
///    [`LoginRefusal::ACCOUNT_IS_CLAUDE`], since no Codex login re-signs it. A
///    name that is no row must be the login's email. All of it before the route
///    probe, so a refusal asks nothing of anyone.
/// 2. The route.
/// 3. **The provider gate.** On the LIVE route the server must have named
///    `codex` among the providers it can store ([`crate::proxy::PROVIDERS_HEADER`],
///    read off the probe's own answer). A server that did not predates Codex
///    accounts: it would drop the row's `provider`, store this login as a Claude
///    account and from then on send its OpenAI tokens to Anthropic — a row a later
///    restart does not heal. Refused, and `--force` does not get past it: a confirmed
///    live route is never traded for the file ([`login_guard_refusal_for`]), and the file
///    would not help either — at its next start that server refuses it if it has the
///    `credentialVersion` check, and reads the login as a Claude account if it predates it.
/// 4. **The row this login already is** ([`existing_codex_row`]), if any. A
///    re-import updates that row and renames nothing, so it keeps the row's name
///    and refuses a different `--name`. And it is refused outright when tcr holds a
///    NEWER credential for it than the import. A stale file can remain when guarded
///    write-back could not complete; writing it over tcr's live pair would kill the account
///    at its next refresh.
/// 5. Otherwise the name [`mint_codex_login_name`] picks.
/// 6. **The workspace check**, last before the write because it is the one step that
///    sends the login's bearer anywhere: `GET {codexUpstream}/backend-api/wham/accounts/check`
///    ([`crate::codex_login::check_workspace_routing`]). A workspace routed to another
///    backend origin or by a residency override is refused, and so is any check that
///    cannot complete: tcr sends every pooled Codex request to `codexUpstream`, so it pools
///    only a login served there. Then the write.
///
/// `codex_home` is the folder the login was read from, recorded on the row as `codexHome`;
/// `None` for a login of tcr's own, which leaves no folder to re-import from.
pub(crate) async fn import_codex_login(
    config_path: &Path,
    force: bool,
    name: Option<&str>,
    account: Option<&str>,
    login: crate::codex_login::CodexLogin,
    codex_home: Option<&Path>,
) -> anyhow::Result<String> {
    import_codex_login_as(
        LoginCommand::FromCodex,
        config_path,
        force,
        name,
        account,
        login,
        codex_home,
    )
    .await
}

/// [`import_codex_login`], for the login `command` names, which a refusal tells the user to run
/// again: `tcr login --codex` writes through here too.
pub(crate) async fn import_codex_login_as(
    command: LoginCommand,
    config_path: &Path,
    force: bool,
    name: Option<&str>,
    account: Option<&str>,
    login: crate::codex_login::CodexLogin,
    codex_home: Option<&Path>,
) -> anyhow::Result<String> {
    if login.is_fedramp {
        return Err(LoginRefusal {
            code: LoginRefusal::CODEX_FEDRAMP,
            reason: CODEX_FEDRAMP_REFUSAL.to_string(),
        }
        .into());
    }
    if let Some(wanted) = account {
        let config = load_or_default(config_path)?;
        let is_this_login = match config.resolve_account_or_none(wanted)? {
            Some(row) => match row.provider {
                crate::provider::Provider::Anthropic => {
                    return Err(LoginRefusal {
                        code: LoginRefusal::ACCOUNT_IS_CLAUDE,
                        reason: claude_account_codex_import_refusal(&row.name),
                    }
                    .into());
                }
                crate::provider::Provider::Codex => codex_login_is_row(row, &login),
            },
            None => login.email.as_deref() == Some(wanted),
        };
        if !is_this_login {
            bail!("the Codex login on this machine is not '{wanted}'; nothing was written.");
        }
    }
    let admitted = admit_login(config_path, force, command).await?;
    admitted.require_codex(command)?;
    let route = admitted.route;
    let config = load_or_default(config_path)?;
    let resolved_name = match existing_codex_row(&config, &login) {
        Some(row) => {
            if let Some(explicit) = name.filter(|explicit| *explicit != row.name) {
                bail!(
                    "this login is already saved as '{}' — a re-import updates that row and \
                     renames nothing, so --name '{explicit}' cannot apply. Drop --name. Nothing \
                     was written.",
                    row.name
                );
            }
            if holds_newer_credential(row.expires_at, login.expires_at_ms) {
                bail!(
                    "tcr already holds a newer credential for this account than `codex` does; \
                     run `codex login` first if you meant to replace it; nothing was written."
                );
            }
            row.name.clone()
        }
        None => mint_codex_login_name(
            &config,
            login.email.as_deref(),
            login.user_id.as_deref(),
            login.account_id.as_deref(),
            name,
        )?,
    };
    // The check carries the login's bearer and account id, so it follows no redirect: a 3xx
    // would carry the account id (reqwest drops only the bearer across origins) to an origin
    // whose answer would then decide the import. A redirect is refused as an unconfirmed check.
    let client = crate::http_client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building the client for the Codex workspace check")?;
    if let Err(refusal) =
        crate::codex_login::check_workspace_routing(&client, &config.codex_upstream, &login).await
    {
        return Err(codex_workspace_refusal(refusal).into());
    }
    finish_login_account(
        config_path,
        route,
        &config,
        codex_account_row(resolved_name, login, codex_home),
    )
    .await
}

/// [`import_codex_login`]'s refusal of a FedRAMP login, before it does anything else.
const CODEX_FEDRAMP_REFUSAL: &str = "this Codex login belongs to a FedRAMP ChatGPT workspace (its \
     token carries chatgpt_account_is_fedramp): codex marks such a workspace's requests \
     X-OpenAI-Fedramp and may route them to the workspace's own backend, and tcr pools a Codex \
     login only when it is served at https://chatgpt.com with no routing of its own. Nothing was \
     written.";

/// [`import_codex_login`]'s refusal of a login the workspace check did not clear
/// ([`crate::codex_login::check_workspace_routing`]), coded by which way it failed: a
/// workspace served elsewhere, or a check that could not complete.
fn codex_workspace_refusal(refusal: crate::codex_login::WorkspaceRefusal) -> LoginRefusal {
    match refusal {
        crate::codex_login::WorkspaceRefusal::NotPoolable(said) => LoginRefusal {
            code: LoginRefusal::CODEX_ROUTED,
            reason: format!(
                "this Codex login's ChatGPT workspace is not served at https://chatgpt.com with no \
                 routing of its own ({said}): codex sends that workspace's traffic to its own \
                 backend, or routes it by residency, and tcr pools a Codex login only at \
                 https://chatgpt.com. Nothing was written."
            ),
        },
        crate::codex_login::WorkspaceRefusal::Unconfirmed(why) => LoginRefusal {
            code: LoginRefusal::CODEX_UNCONFIRMED,
            reason: format!(
                "tcr could not confirm that this Codex login's ChatGPT workspace is served at \
                 https://chatgpt.com ({why}), and imports a Codex login only once that check \
                 answers: a FedRAMP or data-residency workspace must not be pooled. Nothing was \
                 written."
            ),
        },
    }
}

/// The row of `config` this Codex login already is, resolved the way the write
/// resolves its row: [`crate::identity::resolve`] over a Codex probe carrying the
/// login's email, its ChatGPT user and its workspace, so only a Codex row of that
/// user in that workspace can match. `None` when no row is this login, and when
/// two are — a tie the write refuses itself.
pub(crate) fn existing_codex_row<'a>(
    config: &'a Config,
    login: &crate::codex_login::CodexLogin,
) -> Option<&'a Account> {
    let this_login = crate::identity::probe(
        crate::provider::Provider::Codex,
        login.email.as_deref().unwrap_or_default(),
        login.user_id.clone(),
        login.account_id.clone(),
        None,
    );
    config.identity_row(&this_login)
}

/// Whether the credential tcr already stores (`stored`, its `expiresAt`) is
/// NEWER than the one being imported (`import`). Only a provable "newer" counts:
/// an expiry missing on either side is no evidence, and an equal one is the same
/// credential imported twice, which is harmless.
const fn holds_newer_credential(stored: Option<i64>, import: Option<i64>) -> bool {
    matches!((stored, import), (Some(stored), Some(import)) if stored > import)
}

/// The config row a Codex login becomes: `provider: codex`, its identity a ChatGPT
/// user plus a workspace — the user (`chatgpt_user_id`) as `accountUuid` and the
/// workspace (the ChatGPT account id, which tcr sends as `chatgpt-account-id`) as
/// `orgUuid`, the pair [`crate::identity::same_identity`] compares within one
/// provider — and the plan as its `organizationType`. So one person in two
/// workspaces is two accounts, and so are two seats of one workspace: each pair
/// holds its own token and its own quota. No org name: a workspace is named by its
/// id alone. `codex_home` is the folder the login was imported from, when it was.
pub(super) fn codex_account_row(
    name: String,
    login: crate::codex_login::CodexLogin,
    codex_home: Option<&Path>,
) -> Account {
    Account {
        account_uuid: login.user_id,
        org_uuid: login.account_id,
        refresh_token: Some(login.refresh_token),
        expires_at: login.expires_at_ms,
        organization_type: login.plan_type,
        codex_home: codex_home.map(|folder| folder.display().to_string()),
        ..Account::new(name, crate::provider::Provider::Codex, login.access_token)
    }
}

/// Whether `login` is the account `row` holds, by the rule the write resolves its row by:
/// [`crate::identity::same_identity`] against the row [`codex_account_row`] would write, so a
/// Codex row of the same ChatGPT user in the same workspace.
///
/// A login that does not name both its user and its workspace is never the row's. Without
/// them `same_identity` falls back to comparing names, and the probe row here carries `row`'s
/// own name, so any such login would pass.
pub(crate) fn codex_login_is_row(row: &Account, login: &crate::codex_login::CodexLogin) -> bool {
    login.user_id.is_some()
        && login.account_id.is_some()
        && crate::identity::same_identity(
            row,
            &codex_account_row(row.name.clone(), login.clone(), None),
        )
}

/// The name a Codex import writes under.
///
/// `--name` wins, and must be free. Else the email; if an existing row of ANY
/// provider already holds it, `<email>/codex`, mirroring the `email/<org-slug>`
/// rule for duplicated Anthropic emails, then `-2`, `-3`, … as
/// [`crate::identity::mint_name`] numbers its own. Names are unique across the
/// file (`Config::migrate_duplicate_names`).
///
/// Free means free FOR THIS LOGIN: no row holds the name, or the row holding it
/// is this same Codex account by [`crate::identity::same_identity`] (its ChatGPT
/// user, `user_id`, in its workspace, `workspace_id`), the rule the write itself
/// resolves its row by. So a re-import of the same login keeps its name and
/// updates its row in place, while the same email in a second workspace (a
/// personal and a Team workspace) gets a name of its own: the write would ADD
/// that row, and two rows of one name is the state `Config::migrate_duplicate_names`
/// exists to repair. `--force` does not waive this — it overrides a running
/// proxy's refusal, and no route makes a shared name safe.
pub(super) fn mint_codex_login_name(
    config: &Config,
    email: Option<&str>,
    user_id: Option<&str>,
    workspace_id: Option<&str>,
    name: Option<&str>,
) -> anyhow::Result<String> {
    let free = |candidate: &str| {
        let this_login = crate::identity::probe(
            crate::provider::Provider::Codex,
            candidate,
            user_id.map(str::to_string),
            workspace_id.map(str::to_string),
            None,
        );
        config.name_free_for(&this_login)
    };
    if let Some(explicit) = name {
        if !free(explicit) {
            bail!(
                "'{explicit}' is already the name of another account — account names are unique. \
                 Pick a different --name. Nothing was written."
            );
        }
        return Ok(explicit.to_string());
    }
    let Some(email) = email.filter(|e| !e.is_empty()) else {
        bail!("the Codex id token carries no email; pass --name <name>.");
    };
    if free(email) {
        return Ok(email.to_string());
    }
    let base = format!("{email}{}codex", crate::identity::ORG_SEPARATOR);
    if free(&base) {
        return Ok(base);
    }
    let mut n: u64 = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if free(&candidate) {
            return Ok(candidate);
        }
        n = n.saturating_add(1);
    }
}
