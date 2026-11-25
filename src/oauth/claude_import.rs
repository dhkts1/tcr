//! Importing the login the `claude` CLI itself uses: `tcr login --from-claude-code`,
//! the first-run auto-import and `tcr import`.

use super::*;

/// What importing the `claude` CLI's own login costs the user, in the words
/// they are told it in — printed by BOTH the explicit `--from-claude-code`
/// verb and the first-run auto-import, from this one constant so the two can
/// never drift into telling the user two different things.
///
/// It is a real cost, and it is stated rather than hidden: refresh tokens
/// rotate on every use (measured 2026-09-12, `docs/design/long-lived-tokens.md`),
/// so the first time tcr refreshes the imported credential, the copy the
/// `claude` CLI is still holding becomes dead and `claude` asks for a browser
/// login once.
pub const CLAUDE_CODE_IMPORT_CONSEQUENCE: &str = "this copies the login the `claude` CLI itself uses, and each refresh token is single-use, \
     so the next time tcr refreshes it, `claude` will ask you to log in again in the browser, \
     once. Sessions started with `tcr run` are not affected.";

/// `" (max)"` for a known subscription, `""` for none — the plan is shown back
/// to the user as confirmation of WHICH login was taken, never acted on.
pub(super) fn plan_suffix(subscription_type: Option<&str>) -> String {
    match subscription_type {
        Some(plan) if !plan.trim().is_empty() => format!(" ({plan})"),
        _ => String::new(),
    }
}

/// The Claude Code credential as a [`Tokens`], the shape every write path in
/// this module already takes.
///
/// A credential with no `expiresAt` is stamped as expiring NOW, so the very
/// first use refreshes it — correct here, because this credential carries a
/// refresh token. Deliberately NOT the year-long assumption
/// [`login_with_token`] stamps: that one has no refresh token and no way to
/// find out when it dies, and this one has both.
pub(super) fn tokens_from_claude_code_login(
    login: crate::claude_code_login::ClaudeCodeLogin,
) -> Tokens {
    Tokens {
        id_token: None,
        access_token: login.access_token,
        refresh_token: login.refresh_token,
        expires_at_ms: login
            .expires_at
            .map_or_else(crate::now_ms, normalize_expires_at),
    }
}

/// `tcr login --from-claude-code` — import the login this machine's `claude`
/// CLI already holds, instead of opening a browser.
///
/// The explicit redo of what [`auto_import_claude_code_login`] does by itself
/// on a first run: same read, same write path, and it is the way back in after
/// a deliberate `tcr remove` (the auto path refuses to re-import a fleet the
/// user emptied on purpose).
///
/// `ui` decides how it speaks, as it does for [`login`]: the terminal gets the import line on
/// stderr, and [`LoginUi::Machine`] gets the import's cost as one `notice` event and then
/// `saved`, each exactly once, with no prose on stdout.
pub async fn login_from_claude_code(
    config_path: &Path,
    force: bool,
    name: Option<&str>,
    account: Option<&str>,
    ui: LoginUi,
) -> anyhow::Result<String> {
    ui.install();
    let Some(login) = crate::claude_code_login::read_claude_code_login()? else {
        bail!(
            "no Claude Code login found on this machine — nothing was written. Log in with the \
             `claude` CLI first, or use `tcr login` for the browser flow."
        );
    };
    let subscription_type = login.subscription_type.clone();
    let saved = import_claude_code_login(
        config_path,
        ImportInto::AnyFleet,
        force,
        name,
        account,
        login,
        ui,
    )
    .await?
    .ok_or_else(|| anyhow::anyhow!("the import skipped a non-empty fleet it was told to accept"))?;
    match ui {
        LoginUi::Terminal => eprintln!(
            "[tcr] imported as '{saved}'{}; {CLAUDE_CODE_IMPORT_CONSEQUENCE}",
            plan_suffix(subscription_type.as_deref())
        ),
        LoginUi::Machine => {
            ui.emit(LoginEvent::Notice {
                text: CLAUDE_CODE_IMPORT_CONSEQUENCE,
            });
            ui.emit(LoginEvent::Saved { account: &saved });
        }
    }
    Ok(saved)
}

/// Import this machine's Claude Code login on a first run, if there is one.
///
/// Called by the verbs that read the config when the fleet is EMPTY — the
/// config was just created, or it exists with no accounts in it. Returns the
/// account name on success and `None` every other way, and it is `None` that
/// makes the caller print the ordinary `no accounts configured` hint.
///
/// Best-effort by construction: a machine with no Claude Code login, a
/// Keychain read that fails, a profile fetch that fails, a live proxy that
/// refuses the add route — each is ONE warning line on stderr and the verb
/// carries on with zero accounts. None of them is an error worth failing
/// `tcr status` over, which TcrBar renders as a failed poll. One attempt per
/// process, no retry loop.
///
/// The caller is responsible for only calling this on an empty fleet: an
/// import must never run over a config that already has accounts, or a user
/// who removed their account on purpose would find it back. `--from-claude-code`
/// is the explicit way to redo it.
pub async fn auto_import_claude_code_login(config_path: &Path) -> Option<String> {
    let login = match crate::claude_code_login::read_claude_code_login() {
        Ok(Some(login)) => login,
        Ok(None) => return None,
        Err(err) => {
            eprintln!(
                "[tcr] warning: could not read this machine's Claude Code login ({err:#}) — \
                 continuing with no accounts"
            );
            return None;
        }
    };
    let subscription_type = login.subscription_type.clone();
    // `LoginUi::Machine`: no prompt (nobody typed `tcr login` here) and no
    // prose on STDOUT — `tcr status --json` and `tcr accounts --json` are piped
    // into `jq` and decoded by TcrBar, so everything this path says goes to
    // stderr.
    match import_claude_code_login(
        config_path,
        ImportInto::EmptyFleetOnly,
        false,
        None,
        None,
        login,
        LoginUi::Machine,
    )
    .await
    {
        // Another `tcr` process (the server booting while `tcr status` polls
        // it, typically) imported first. Its row is the import; adding ours
        // too would be the `unnamed` + `unnamed-2` duplicate.
        Ok(None) => None,
        Ok(Some(saved)) => {
            eprintln!(
                "[tcr] imported '{saved}'{} from your Claude Code login; \
                 {CLAUDE_CODE_IMPORT_CONSEQUENCE}",
                plan_suffix(subscription_type.as_deref())
            );
            Some(saved)
        }
        Err(err) => {
            eprintln!(
                "[tcr] warning: could not import this machine's Claude Code login ({err:#}) — \
                 continuing with no accounts"
            );
            None
        }
    }
}

/// The write both Claude Code import paths share: the SAME route probe, name
/// mint and finishing path a browser login uses ([`login`]), so account-name
/// collision, the live-proxy add route and `--force` behave identically. The
/// only thing that differs is where the credential came from.
///
/// `--account` is honoured, unlike [`login_with_token`]'s refusal of it: this
/// credential carries the full `user:profile` scope, so `fetch_profile` returns
/// a real identity for [`assert_requested_identity`] to confirm the requested
/// row against. There is nothing here that cannot be evaluated.
///
/// Returns `Ok(None)` only under [`ImportInto::EmptyFleetOnly`], when the
/// config it loads already has an account in it.
pub(super) async fn import_claude_code_login(
    config_path: &Path,
    into: ImportInto,
    force: bool,
    name: Option<&str>,
    account: Option<&str>,
    login: crate::claude_code_login::ClaudeCodeLogin,
    ui: LoginUi,
) -> anyhow::Result<Option<String>> {
    let imported = import_claude_login(
        config_path,
        into,
        force,
        name,
        account,
        login,
        None,
        LoginCommand::FromClaudeCode,
        ui,
    )
    .await?;
    match imported {
        ClaudeImported::Saved(saved) => Ok(Some(saved)),
        ClaudeImported::FleetNotEmpty => Ok(None),
        ClaudeImported::AlreadyAs(row) => {
            bail!("this login is already saved as '{row}'; nothing was written.")
        }
    }
}

/// What [`import_claude_login`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClaudeImported {
    /// Saved, under this name.
    Saved(String),
    /// [`ImportInto::EmptyFleetOnly`], and the config it loaded already had an account.
    FleetNotEmpty,
    /// [`ImportInto::NewRowOnly`], and the login is already this row: nothing was written.
    AlreadyAs(String),
}

/// [`import_claude_code_login`] for a Claude login from any source: `stored` is the identity
/// the source keeps beside the login, which fills each field the profile fetch leaves empty
/// ([`Profile::or_stored_identity`]), and `command` is the login a refusal names.
///
/// Under [`ImportInto::NewRowOnly`] the login's identity, after the profile fetch, is checked
/// against the config the write is based on, and a login that is already a row is
/// [`ClaudeImported::AlreadyAs`] with nothing written: the write would otherwise update that
/// row and replace tcr's own working token with another tool's copy.
#[expect(
    clippy::too_many_arguments,
    reason = "the login, where it came from, and the options `tcr login` parses for it"
)]
pub(crate) async fn import_claude_login(
    config_path: &Path,
    into: ImportInto,
    force: bool,
    name: Option<&str>,
    account: Option<&str>,
    login: crate::claude_code_login::ClaudeCodeLogin,
    stored: Option<&StoredClaudeIdentity>,
    command: LoginCommand,
    ui: LoginUi,
) -> anyhow::Result<ClaudeImported> {
    ui.install();
    let route = admit_login(config_path, force, command).await?.route;

    let tokens = tokens_from_claude_code_login(login);

    let config = load_or_default(config_path)?;
    // The caller's "fleet is empty" check ran on an earlier read, and the route
    // probe above can take a while against a server that is still booting.
    // Another `tcr` process may have imported in that gap, and minting a name
    // against its row would add the same login a second time.
    // A fleet emptied by `tcr remove` says so in the file; refilling it here
    // would undo that delete on the next poll.
    if into == ImportInto::EmptyFleetOnly
        && (!config.accounts.is_empty() || config.refuses_claude_code_import())
    {
        return Ok(ClaudeImported::FleetNotEmpty);
    }
    let profile = fetch_profile(&tokens.access_token).await;
    let profile = match stored {
        Some(stored) => profile.or_stored_identity(stored),
        None => profile,
    };
    if into == ImportInto::NewRowOnly {
        let identity = StoredClaudeIdentity::new(
            profile.email.clone(),
            profile.account_uuid.clone(),
            profile.org_uuid.clone(),
            profile.org_name.clone(),
        );
        if let Some(row) = identity.existing_row(&config) {
            return Ok(ClaudeImported::AlreadyAs(row.name.clone()));
        }
    }
    let resolved_name = mint_login_name(&config, &profile, account, name, ui)?;

    finish_login_checked(
        config_path,
        route,
        &config,
        &resolved_name,
        &tokens,
        profile,
        account,
    )
    .await
    .map(ClaudeImported::Saved)
}

/// Which configs [`import_claude_login`] may write a Claude login into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImportInto {
    /// `tcr login --from-claude-code`: the user asked, so any config.
    AnyFleet,
    /// The automatic first-run import: only a config with no accounts, checked
    /// again on the read the write is based on.
    EmptyFleetOnly,
    /// `tcr import --yes`: any config, but only as a new row. A login that is already a row
    /// is left alone ([`ClaudeImported::AlreadyAs`]).
    NewRowOnly,
}
