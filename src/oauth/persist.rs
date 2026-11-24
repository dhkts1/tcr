//! Writing a finished login: into the config file, or through the live proxy's
//! add route, without clobbering what the server or another process wrote.

use super::*;

/// Add or update an OAuth account in `config`, keyed on `(account_uuid, org)`
/// via [`identity::resolve`] — never a loose first-match — so the legacy
/// two-org tie ([`identity::same_identity_strict`]) is broken the same way
/// [`config::save_account`] and [`crate::manager::Manager::add_or_update_account`]
/// already break it, and an unbreakable tie is REFUSED rather than guessed:
/// guessing here means stamping a fresh credential over a DIFFERENT account's
/// single-use refresh token (see [`identity::Resolved::Many`]'s doc-comment).
/// An existing account with the same identity has its tokens refreshed in
/// place and any absent identity field backfilled (never duplicated);
/// otherwise a new account is appended with the next-highest priority and
/// every identity field populated.
///
/// Backward compatible: when the probe and the stored entry both lack identity
/// fields (today's real config), `identity::resolve` falls back to name
/// equality — so a single-org re-login matches its existing entry exactly as
/// before.
/// Takes the whole [`Profile`] rather than its fields one by one: the plan
/// fields joined the identity ones here, and five loose `Option<String>`
/// positional arguments of the same type are a call site where two of them can
/// be transposed with nothing to catch it.
pub fn upsert_account(
    config: &mut Config,
    name: &str,
    tokens: &Tokens,
    profile: &Profile,
) -> anyhow::Result<()> {
    let account_uuid = profile.account_uuid.clone();
    let org_uuid = profile.org_uuid.clone();
    let org_name = profile.org_name.clone();
    // An Anthropic login, so an Anthropic probe: it never resolves to a Codex row
    // that shares its name (`identity::same_identity`), and the row it appends below
    // is Anthropic too.
    let probe = crate::identity::probe(
        crate::provider::Provider::Anthropic,
        name,
        account_uuid.clone(),
        org_uuid.clone(),
        org_name.clone(),
    );

    match crate::identity::resolve(config.accounts.iter().enumerate(), &probe) {
        crate::identity::Resolved::One(index) => {
            let account = &mut config.accounts[index];
            account.account_type = config::AccountType::OAuth;
            account.access_token.clone_from(&tokens.access_token);
            account.refresh_token.clone_from(&tokens.refresh_token);
            account.expires_at = Some(tokens.expires_at_ms);
            // Backfill any identity field the stored entry was missing (e.g. a
            // legacy pre-org entry newly profiled), without overwriting known
            // values.
            if account.account_uuid.is_none() {
                account.account_uuid = account_uuid;
            }
            if account.org_uuid.is_none() {
                account.org_uuid = org_uuid;
            }
            if account.org_name.is_none() {
                account.org_name = org_name;
            }
            // OVERWRITTEN, not backfilled, unlike the identity fields above —
            // an org upgrades its plan, so the first reading must not be pinned
            // forever. Same rule `config::merge_account` applies on disk; see
            // `config::Account::organization_type`. A `None` writes nothing, so
            // a profile fetch that failed never blanks a known plan.
            if profile.organization_type.is_some() {
                account
                    .organization_type
                    .clone_from(&profile.organization_type);
            }
            if profile.rate_limit_tier.is_some() {
                account.rate_limit_tier.clone_from(&profile.rate_limit_tier);
            }
            if profile.seat_tier.is_some() {
                account.seat_tier.clone_from(&profile.seat_tier);
            }
            if profile.plan_display_name.is_some() {
                account
                    .plan_display_name
                    .clone_from(&profile.plan_display_name);
            }
            Ok(())
        }
        crate::identity::Resolved::None => {
            let next_priority = config
                .accounts
                .iter()
                .filter_map(|a| a.priority)
                .max()
                .map_or(0, |max| max + 1);

            config.accounts.push(Account {
                priority: Some(next_priority),
                ..claude_login_row(name, tokens, profile)
            });
            Ok(())
        }
        // Recompute the loose set for its names: `Resolved::Many` itself
        // carries none, and this is exactly the set `resolve` drew from (same
        // predicate, same candidates) — mirrors
        // `Manager::add_or_update_account`'s identical recomputation.
        crate::identity::Resolved::Many => {
            let candidates: Vec<String> = config
                .accounts
                .iter()
                .filter(|a| crate::identity::same_identity(a, &probe))
                .map(|a| a.name.clone())
                .collect();
            bail!("{}", crate::cli::ambiguous_query_message(name, &candidates));
        }
    }
}

/// The file half of a login: a surgical [`config::save_account`] write of the
/// finished `account` row, sharing the exact identity-resolution and backfill
/// semantics [`upsert_account`] used to apply in memory.
///
/// Used to be `upsert_account` (mutating the in-memory `config` this
/// process loaded at the top of `login()`) followed by a whole-file
/// [`config::save`] of that snapshot. That snapshot is taken BEFORE the
/// profile fetch, the browser round-trip and a possibly-unbounded stdin
/// prompt — the exact window `persist_via_account`'s doc-comment names —
/// so a whole-file write here silently reverted anything an out-of-band
/// writer (a live `tcr` server, a concurrent `tcr login`, a hand-edited
/// top-level key such as `sessionAffinity`) added or changed on disk during
/// that window. `config::save_account` re-reads the document fresh from
/// disk immediately before writing and touches only `account`'s own row, so
/// there is nothing left in this path for a stale in-memory snapshot to
/// clobber. It takes no `config` at all, deliberately: mutating the caller's
/// in-memory snapshot here would just resurrect the same stale-snapshot hazard
/// on any FUTURE caller that grew a reason to read it back after this call.
///
/// One case `config::save_account` cannot serve on its own: no file exists
/// at `config_path` yet at all. Its `read_document` is a bare
/// `fs::read_to_string`, which cannot create a missing path — mirrors the
/// same `NotFound` guard [`persist_via_account_or_file`] already carries for
/// exactly this reason. There is nothing on disk in that case for a stale
/// snapshot to clobber, so falling back to a fresh single-account
/// [`config::save`] is safe rather than a reintroduction of the hazard this
/// function exists to remove.
fn persist_via_file(config_path: &Path, account: Account) -> anyhow::Result<String> {
    match config::save_account(config_path, &account) {
        Ok(outcome) => account_write_result(config_path, &account, outcome),
        Err(config::ConfigError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            let name = account.name.clone();
            let mut fresh: Config =
                serde_json::from_str("{}").context("build a default config after login")?;
            let mut first_account = account;
            first_account.priority = Some(0);
            fresh.accounts.push(first_account);
            config::save(config_path, &fresh).context("save config after login")?;
            prose(format_args!(
                "Saved account '{name}' to {}",
                config_path.display()
            ));
            Ok(name)
        }
        Err(err) => Err(err).context("save config after login"),
    }
}

/// The surgical half of a live-route fallback — [`config::save_account`]'s
/// single-account read-modify-write — used when [`probe_add_route`]
/// confirmed the route moments earlier but [`crate::cli::post_add_account`]
/// itself then could not apply it (the route disappeared, or the proxy
/// stopped answering before its 5s deadline). Bridge item A: this replaces
/// [`persist_via_file`] on exactly those two arms of [`finish_login_account`].
///
/// Unlike `persist_via_file`, this never touches any account row but
/// `account`'s own, and it is safe to call even though `account` was built
/// from data gathered before this call's own async window (the profile
/// fetch, the possible stdin prompt, the up-to-5s `post_add_account`
/// timeout): `config::save_account` re-reads the config document FRESH from
/// disk and resolves identity itself, rather than whole-file-saving the
/// (possibly already-stale) in-memory `config` this process is holding — so
/// there is no stale snapshot for it to clobber a concurrent server-side
/// rotation with. This is the same write the server's own persist path uses.
fn persist_via_account(config_path: &Path, account: &Account) -> anyhow::Result<String> {
    let outcome = config::save_account(config_path, account).context("save account after login")?;
    account_write_result(config_path, account, outcome)
}

/// Turn a [`config::AccountWrite`] outcome into `persist_via_account`'s return
/// value, shared with [`persist_via_account_or_file`] so the two callers agree
/// on the exact same success/refusal wording rather than drifting apart.
fn account_write_result(
    config_path: &Path,
    account: &Account,
    outcome: config::AccountWrite,
) -> anyhow::Result<String> {
    match outcome {
        config::AccountWrite::Added | config::AccountWrite::Updated => {
            prose(format_args!(
                "Saved account '{}' to {}",
                account.name,
                config_path.display()
            ));
            Ok(account.name.clone())
        }
        config::AccountWrite::Ambiguous => bail!(
            "'{}''s identity matches more than one account already in {} — give each row its own \
             accountUuid/orgUuid and retry. Nothing was changed.",
            account.name,
            config_path.display()
        ),
        config::AccountWrite::Unwritable => bail!(
            "the accounts key in {} is not a JSON array — refusing to write. Nothing was \
             changed.",
            config_path.display()
        ),
    }
}

/// [`persist_via_account`], but falling back to [`persist_via_file`] when
/// there is no file yet for `save_account`'s `read_document` to read —
/// `fs::read_to_string` cannot create a missing file, and a first-ever login
/// must still be able to. Used by `finish_login_account`'s `NoServer` arm, which
/// otherwise carries the exact same clobber risk `NoRoute` and the
/// timeout/`Unusable` arm already write surgically around: the caller's config
/// snapshot was taken before the profile fetch, the possible
/// stdin prompt, and the live-add round-trip, so a whole-file write here
/// would revert any rotation a server that exited moments ago made to any
/// OTHER account inside that window.
fn persist_via_account_or_file(config_path: &Path, account: &Account) -> anyhow::Result<String> {
    match config::save_account(config_path, account) {
        Err(config::ConfigError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            persist_via_file(config_path, account.clone())
        }
        result => account_write_result(
            config_path,
            account,
            result.context("save account after login")?,
        ),
    }
}

/// Persist a finished login's `account` row per `route`, and return the name
/// it was saved under: through the live proxy when it is [`WriteRoute::Live`],
/// via [`persist_via_file`] when it is [`WriteRoute::File`]. Split out of
/// [`login`] so WHERE the credential lands is testable without driving a real
/// browser.
///
/// Provider-agnostic: the Anthropic flow builds its row from a profile fetch
/// ([`finish_login`]), the Codex import from id-token claims
/// ([`import_codex_login`]); everything from the row onward is the same. The
/// row's `provider` rides in the live route's request body (the serialized
/// `config::Account`), but it reaches the running server's rotation only when
/// that server knows the field: a tcr that predates it drops the key and stores
/// the row as a Claude account. So a non-Anthropic row takes the live route only
/// after the server ADVERTISED its provider ([`crate::proxy::PROVIDERS_HEADER`],
/// checked by [`import_codex_login`]), and the server's answer is checked here
/// as well: the `provider` it echoes must be the row's, or this is a hard error
/// telling the user to remove the row it stored.
///
/// On the live path the running server owns the durable write — this must
/// NOT also whole-file [`config::save`] the (possibly already-stale) `config`
/// this process is holding; that write is exactly the clobber this unit
/// exists to remove. The quiet [`crate::cli::LiveControlError::NoServer`]
/// case carries that SAME clobber risk, not a lesser one: the proxy the
/// earlier capability probe confirmed alive can have exited any time in the
/// profile-fetch / stdin-prompt / round-trip window since, so any OTHER
/// account it rotated in that window is still on disk waiting to be
/// reverted. It therefore routes through [`persist_via_account_or_file`]'s
/// surgical write exactly like `NoRoute`/`Unusable` below, never straight to
/// a whole-file one. A server that answered and REFUSED us (`Unauthorized`,
/// `Rejected`) must surface, never silently write the file instead, because
/// the two halves would then disagree about what the account's credentials
/// are.
pub(super) async fn finish_login_account(
    config_path: &Path,
    route: WriteRoute,
    config: &Config,
    account: Account,
) -> anyhow::Result<String> {
    match route {
        WriteRoute::File => persist_via_file(config_path, account),
        WriteRoute::Live => {
            match crate::cli::post_add_account(config, &account).await {
                // Behind the provider gate, and checked anyway: a non-Anthropic
                // row the server did not confirm storing AS that provider is a
                // row it stored as a Claude account, whose tokens go to Anthropic
                // from now on. Anthropic rows need no echo (`None` is any server
                // older than the field, which stored them exactly right).
                Ok(applied)
                    if !account.provider.is_anthropic()
                        && applied.provider != Some(account.provider) =>
                {
                    bail!(
                        "{}",
                        misstored_provider_refusal(config.proxy.port, &applied, account.provider)
                    )
                }
                Ok(applied) => {
                    prose(format_args!(
                        "Saved account '{}' to the running proxy on :{}",
                        applied.name, config.proxy.port
                    ));
                    if let Some(warning) = &applied.warning {
                        eprintln!("[tcr] warning: {warning}");
                    }
                    if !applied.persisted {
                        eprintln!(
                            "[tcr] warning: this account is live but NOT saved to {} — it will not survive a restart.",
                            config_path.display()
                        );
                    }
                    Ok(applied.name)
                }
                // Nothing is listening any more (it exited between the probe
                // and here). NOT the safe "offline the whole time" case
                // `persist_via_file` is for: a proxy WAS confirmed alive
                // moments ago, and it can have rotated any OTHER account's
                // tokens on disk any time in the profile-fetch / stdin-prompt
                // / round-trip window since. Same surgical write as
                // `NoRoute`/`Unusable` below, falling back to a whole-file
                // write only when there is no file yet at all.
                Err(crate::cli::LiveControlError::NoServer) => {
                    persist_via_account_or_file(config_path, &account)
                }
                // The proxy rejected our api-key. Writing the file here would
                // be the clobber this unit exists to remove, in a new place:
                // the running server keeps whatever it already had while the
                // file quietly disagrees. Change nothing, exit non-zero.
                Err(crate::cli::LiveControlError::Unauthorized) => bail!(
                    "the proxy on :{} rejected the api-key in {} — the config was NOT changed, \
                     because writing it would leave the running proxy unaware of this login. \
                     Fix `proxy.apiKey` and retry.",
                    config.proxy.port,
                    config_path.display()
                ),
                // The route ran and refused the submission itself (e.g. the
                // identity matched more than one live account). Do not fall
                // back — the file's own resolution could land on a different
                // account than the one the server was talking about.
                Err(crate::cli::LiveControlError::Rejected(message)) => bail!(
                    "the proxy running on :{} refused this: {message} Nothing was changed.",
                    config.proxy.port
                ),
                // The route is missing: the capability probe said it was
                // there and it no longer is (a race, or a downgrade mid-run).
                // Bridge item A: this is reached with a CONFIRMED-live server
                // on the other end of the port, seconds after the config
                // snapshot above was loaded (a profile fetch, possibly an
                // unbounded stdin prompt, then this call itself came between)
                // — so a whole-file `persist_via_file` here would revert any
                // edit the live server made to any OTHER account in that
                // window. `persist_via_account` writes only this one row,
                // re-reading the file fresh instead of trusting the stale
                // snapshot. It is still HALF a login — say so loudly,
                // mirroring `set_enabled`'s NoRoute arm.
                Err(crate::cli::LiveControlError::NoRoute) => {
                    let saved = persist_via_account(config_path, &account)?;
                    eprintln!(
                        "[tcr] WARNING: the proxy running on :{} is too old to accept a live login (no {} route), so the account was written to {} while the proxy is running. It will not see this account until it restarts. {}",
                        config.proxy.port,
                        crate::proxy::ADD_ACCOUNT_PATH,
                        config_path.display(),
                        crate::cli::RESTART_HINT,
                    );
                    Ok(saved)
                }
                // It answered something unusable, or did not answer in time
                // (the wedged-port shape `cli::probe_incumbent`'s own tests
                // document as measured and real — the probe confirmed the
                // route seconds ago and `post_add_account` then hit its own
                // 5s deadline). Same shape of consequence as `NoRoute`, same
                // surgical write for the same reason, different cause, and
                // equally never silent.
                Err(other) => {
                    let why = other.why();
                    let saved = persist_via_account(config_path, &account)?;
                    eprintln!(
                        "[tcr] WARNING: could not apply this login to the proxy running on :{} ({why}), so the account was written to {} while the proxy is running — it may not see this account until it restarts.",
                        config.proxy.port,
                        config_path.display(),
                    );
                    Ok(saved)
                }
            }
        }
    }
}

/// What [`finish_login_account`] says when a live server did not confirm storing
/// a `wanted`-provider row as one: the row is live and on disk as a Claude
/// account, so the only safe next step is removing it, then a fresh `codex
/// login` (its token may already have reached Anthropic).
fn misstored_provider_refusal(
    port: u16,
    applied: &crate::proxy::AddAccountResponse,
    wanted: crate::provider::Provider,
) -> String {
    let noun = |provider: crate::provider::Provider| match provider {
        crate::provider::Provider::Anthropic => "Claude",
        crate::provider::Provider::Codex => "Codex",
    };
    let said = match applied.provider {
        Some(stored) => format!("stored '{}' as a {} account", applied.name, noun(stored)),
        None => format!(
            "did not say how it stored '{}' — only a proxy older than Codex accounts answers that way",
            applied.name
        ),
    };
    format!(
        "the proxy on :{port} {said}, not as a {} account. That row is now live, and on disk, as \
         a Claude account, and its token may already have reached Anthropic: run `tcr remove \
         '{}'` now, then `codex login` again.",
        noun(wanted),
        applied.name
    )
}

/// [`finish_login_account`] for an Anthropic login: the row is the profile the
/// login authenticated as, plus the tokens it produced.
pub(super) async fn finish_login(
    config_path: &Path,
    route: WriteRoute,
    config: &Config,
    name: &str,
    tokens: &Tokens,
    profile: Profile,
) -> anyhow::Result<String> {
    let account = claude_login_row(name, tokens, &profile);
    finish_login_account(config_path, route, config, account).await
}

/// The Anthropic row a login becomes: the profile it authenticated as, plus the
/// tokens it produced, with no priority (the caller sets one or leaves the
/// server to).
///
/// The plan fields ride the SERVER route too, and for free: the request body
/// `finish_login_account` posts is a serialized `config::Account`, so the two
/// serde fields put the plan on the wire without a second payload shape to
/// keep in step. Pinned by `add_account_request_carries_the_plan_fields`.
fn claude_login_row(name: &str, tokens: &Tokens, profile: &Profile) -> Account {
    Account {
        account_uuid: profile.account_uuid.clone(),
        org_uuid: profile.org_uuid.clone(),
        org_name: profile.org_name.clone(),
        refresh_token: tokens.refresh_token.clone(),
        expires_at: Some(tokens.expires_at_ms),
        organization_type: profile.organization_type.clone(),
        rate_limit_tier: profile.rate_limit_tier.clone(),
        seat_tier: profile.seat_tier.clone(),
        plan_display_name: profile.plan_display_name.clone(),
        ..Account::new(
            name,
            crate::provider::Provider::Anthropic,
            tokens.access_token.clone(),
        )
    }
}

/// Load the config for a login, creating the file with defaults when it does
/// not exist yet (the very first login on a fresh box) while surfacing genuine
/// parse/permission errors so a corrupt file is never overwritten.
///
/// Shares [`config::load_or_init`] with every other verb rather than keeping a
/// second copy of the missing-file rule here.
pub(crate) fn load_or_default(config_path: &Path) -> anyhow::Result<Config> {
    let (config, _created) = config::load_or_init(config_path).context("load config for login")?;
    Ok(config)
}
