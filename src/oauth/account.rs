//! `--account`: which row a re-login targets, the refusals when that row is the
//! wrong provider, and the check that the login that came back is that row.

use super::*;

/// What `tcr login --account` says to a row that is not a Claude account, before it does
/// anything else ([`refuse_a_non_claude_row`]): the row, why `tcr login` cannot sign it in, the
/// two commands that can (a login of tcr's own, or a re-import), and that nothing was written.
pub fn codex_account_login_refusal(name: &str) -> String {
    format!(
        "'{name}' is a Codex account; `tcr login` signs in Claude accounts only. Sign it in again \
         with `tcr login --codex --account {name}`, or re-import it with `tcr login --from-codex \
         --account {name}`. Nothing was written."
    )
}

/// Refuse a `--account` row that a Claude sign-in can never confirm: anything but a Claude
/// account. Both a browser login and a Claude Code import come back with an Anthropic identity,
/// which no Codex row matches ([`assert_requested_identity`]'s probe), so going on would end in
/// a mismatch that tells the user to sign out and retry the same command. Called before the
/// browser opens, and again by the identity check itself.
pub(super) fn refuse_a_non_claude_row(account: &Account) -> anyhow::Result<()> {
    match account.provider {
        crate::provider::Provider::Anthropic => Ok(()),
        crate::provider::Provider::Codex => Err(LoginRefusal {
            code: LoginRefusal::ACCOUNT_IS_CODEX,
            reason: codex_account_login_refusal(&account.name),
        }
        .into()),
    }
}

/// What `tcr login --codex --account` says to a Claude row: a ChatGPT sign-in can never confirm
/// it, so it is refused before codex runs, naming the command that can.
pub fn claude_account_codex_login_refusal(name: &str) -> String {
    format!(
        "'{name}' is a Claude account; `tcr login --codex` signs in Codex accounts only. Sign it \
         in again with `tcr login --account {name}`. Nothing was written."
    )
}

/// What `tcr login --from-codex --account` says to a Claude row, before the route probe and any
/// write: a Codex import can never re-sign it.
pub fn claude_account_codex_import_refusal(name: &str) -> String {
    format!(
        "'{name}' is a Claude account; `tcr login --from-codex` imports Codex accounts only. Sign \
         it in again with `tcr login --account {name}`. Nothing was written."
    )
}

/// The row `tcr login --codex --account <query>` re-signs: `query` resolved by the rule every
/// verb's `--account` uses ([`crate::cli::resolve_account`], an exact row name), refused with
/// [`LoginRefusal::ACCOUNT_IS_CLAUDE`] when it names a Claude row. Runs before anything else the
/// login does, so a refusal spawns nothing and writes nothing.
pub fn codex_relogin_target(config: &Config, query: &str) -> anyhow::Result<usize> {
    let idx = crate::cli::resolve_account(&config.accounts, query)?;
    let row = &config.accounts[idx];
    match row.provider {
        crate::provider::Provider::Codex => Ok(idx),
        crate::provider::Provider::Anthropic => Err(LoginRefusal {
            code: LoginRefusal::ACCOUNT_IS_CLAUDE,
            reason: claude_account_codex_login_refusal(&row.name),
        }
        .into()),
    }
}

/// The refusal of a `tcr login --codex --account <name>` whose ChatGPT login came back as
/// someone else: `got` is that login's email (or a stand-in when it has none). The browser
/// is the usual cause, already signed in to chatgpt.com as another person, which is the remedy
/// it names.
pub fn codex_identity_refusal(got: &str, name: &str) -> LoginRefusal {
    LoginRefusal {
        code: LoginRefusal::CODEX_IDENTITY,
        reason: format!(
            "the ChatGPT login that came back is {got}, not the one '{name}' holds. Sign out of \
             chatgpt.com in the browser first, or use a private window, then retry `tcr login \
             --codex --account {name}`. Nothing was written."
        ),
    }
}

/// The refusal of a `tcr login --codex --account <name>` whose login is the row's own ChatGPT
/// user in another workspace: the same person, so both workspaces are known and differ, and the
/// remedy is to pick the row's workspace when ChatGPT asks.
///
/// `email` is the login's (or `None`); `got` and `want` are the login's and the row's workspace
/// ids, shown as their first 8 hex digits; each plan is a raw plan word, labelled the way
/// `tcr status` labels it, and its parenthesis is dropped when it is unknown.
pub fn codex_relogin_workspace_refusal(
    email: Option<&str>,
    got: &str,
    got_plan: Option<&str>,
    name: &str,
    want: &str,
    want_plan: Option<&str>,
) -> LoginRefusal {
    let short =
        |id: &str| -> String { id.chars().filter(char::is_ascii_hexdigit).take(8).collect() };
    let plan = |raw: Option<&str>| {
        tcr_status_wire::codex_plan_label(raw)
            .map_or_else(String::new, |label| format!(" ({label})"))
    };
    LoginRefusal {
        code: LoginRefusal::CODEX_IDENTITY,
        reason: format!(
            "the ChatGPT login that came back is {} in ChatGPT workspace {}{}, but '{name}' holds \
             workspace {}{}. Retry `tcr login --codex --account {name}` and choose that workspace \
             when ChatGPT asks which one to use. Nothing was written.",
            email.unwrap_or("the same ChatGPT user"),
            short(got),
            plan(got_plan),
            short(want),
            plan(want_plan),
        ),
    }
}

/// [`finish_login`], but first asserting the requested `--account` identity
/// when one was given. Split out so the assertion + write sequence `login()`
/// runs is directly testable without a real browser or network round trip —
/// exactly the reason [`finish_login`] itself was split from [`login`].
pub(super) async fn finish_login_checked(
    config_path: &Path,
    route: WriteRoute,
    config: &Config,
    name: &str,
    tokens: &Tokens,
    profile: Profile,
    requested_account: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(query) = requested_account {
        assert_requested_identity(config, query, &profile)?;
    }
    finish_login(config_path, route, config, name, tokens, profile).await
}

/// The load-bearing half of `tcr login --account <query>`:
/// resolve `query` against `config` via [`crate::cli::resolve_account`]
/// — the exact rule `tcr enable`/`tcr disable` and TcrBar's row buttons
/// already rely on, not a second copy of it — then refuse to proceed unless
/// the identity that came back from `fetch_profile` resolves, through
/// [`crate::identity::resolve`], to that SAME row.
///
/// Deliberately reuses `identity::resolve`/`identity::probe` — the exact
/// functions [`upsert_account`] writes through — rather than a hand-rolled
/// comparison: an account UUID alone identifies the *person*, not the
/// account. The same person routinely holds more than one organization (a
/// corporate Pro org and a personal Max org), each with its own token and
/// quota (`src/identity.rs:3-8`), so a `uuid`-only check cannot tell those
/// apart — it would happily confirm a re-login intended for the Corp row
/// against a browser session signed into Personal, because both carry the
/// same `uuid`. `identity::resolve` folds the org discriminator in on top of
/// the uuid, which is why it — not a second copy of only half its logic — is
/// what both the check and the write need to agree on.
///
/// Called strictly BEFORE any write path
/// ([`finish_login`]/[`persist_via_file`]/[`persist_via_account`]): on
/// mismatch, on an unresolvable/ambiguous requested account, on a requested
/// row that is not a Claude account ([`refuse_a_non_claude_row`]), or on a profile
/// that carries neither an email nor a uuid (`fetch_profile`'s all-`None`
/// failure return, in `fetch_profile_at`, `src/oauth/profile.rs` — an assertion that cannot be
/// evaluated must fail closed, not pass by default), nothing is written.
pub(super) fn assert_requested_identity(
    config: &Config,
    query: &str,
    profile: &Profile,
) -> anyhow::Result<()> {
    let idx = crate::cli::resolve_account(&config.accounts, query)?;
    let requested = &config.accounts[idx];
    refuse_a_non_claude_row(requested)?;

    if profile.email.is_none() && profile.account_uuid.is_none() {
        bail!(
            "could not confirm the identity that just authenticated (the profile carried no \
             email and no account id) — refusing to touch '{}'. Nothing was changed.",
            requested.name
        );
    }

    // The identity a fresh login resolves to, computed the SAME way
    // `upsert_account` resolves one for a write: build a probe carrying only
    // what the browser told us, and resolve it against the WHOLE fleet, not
    // just `requested`. Comparing against the whole fleet (rather than just
    // asking "does `requested` match the probe?") is what makes the org
    // discriminator load-bearing: a uuid-only local comparison against
    // `requested` alone would still pass when Corp was requested and the
    // browser returned Personal, because it never looks at whether that
    // identity actually belongs to a DIFFERENT row.
    let probe_name = profile.email.as_deref().unwrap_or(&requested.name);
    // The browser just authenticated an ANTHROPIC account, so this probe is one: a
    // requested Codex row is never confirmed by it (`identity::same_identity`).
    let probe = crate::identity::probe(
        crate::provider::Provider::Anthropic,
        probe_name,
        profile.account_uuid.clone(),
        profile.org_uuid.clone(),
        profile.org_name.clone(),
    );
    let resolved = crate::identity::resolve(config.accounts.iter().enumerate(), &probe);
    let resolved_idx = match resolved {
        crate::identity::Resolved::One(i) => Some(i),
        crate::identity::Resolved::None | crate::identity::Resolved::Many => None,
    };

    if resolved_idx != Some(idx) {
        let got = match resolved_idx {
            // The identity resolved cleanly, just to a DIFFERENT existing
            // row — name that row, it is the actionable fact.
            Some(other) => config.accounts[other].name.clone(),
            None if requested.account_uuid.is_none() && !requested.name.contains('@') => {
                // `requested` has no stored account id, and its own display
                // name isn't email-shaped (e.g. an account named "work"), so
                // there is no stored fact to compare the browser's identity
                // against at all — this is not "wrong browser session", it
                // is "nothing on file to check against". Telling the
                // operator to sign out cannot help here; say so honestly.
                bail!(
                    "'{}' has no stored account id, and its name is not an email address, so \
                     `--account` cannot confirm the browser authenticated as the right person — \
                     there is nothing on file to compare against. Nothing was written. Run `tcr \
                     login` once WITHOUT --account, confirm from the printed email that it is the \
                     account you intended, then either rename this entry to that email or use \
                     that email with --account from now on.",
                    requested.name
                );
            }
            None => {
                let email = profile
                    .email
                    .as_deref()
                    .unwrap_or("an account with no email in its profile");
                match profile.org_name.as_deref().or(profile.org_uuid.as_deref()) {
                    Some(org) => format!("{email} (org {org})"),
                    None => email.to_string(),
                }
            }
        };
        bail!(
            "requested re-login for '{}', but the browser authenticated as '{got}' instead — \
             nothing was written. Sign out of that account in the browser first, or use a \
             private/incognito window, then retry `tcr login --account {}`.",
            requested.name,
            query
        );
    }

    Ok(())
}
