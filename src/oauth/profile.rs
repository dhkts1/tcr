//! The account and org identity read from Anthropic's profile endpoint.

use super::*;

/// Profile endpoint used to name the account.
pub const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
/// The account+org identity fetched from the profile endpoint. Every field is
/// optional so a partial or failed fetch degrades gracefully: `email` names the
/// account (falls back to a prompt when absent) and `account_uuid`/`org_uuid`/
/// `org_name` key the account's identity so multi-org logins stay distinct.
pub struct Profile {
    pub(super) email: Option<String>,
    pub(super) account_uuid: Option<String>,
    pub(super) org_uuid: Option<String>,
    pub(super) org_name: Option<String>,
    /// `organization.organization_type`, verbatim — the account's PLAN. See
    /// [`crate::config::Account::organization_type`] for why this field and not
    /// `account.has_claude_max`.
    pub organization_type: Option<String>,
    /// `organization.rate_limit_tier`, verbatim — what separates Max 20x from
    /// Max 5x.
    pub rate_limit_tier: Option<String>,
    /// `organization.seat_tier`, verbatim — which seat this row holds on a
    /// seated org (`team_standard`, `team_tier_1`). `None` on Max and Pro,
    /// which have no seats.
    pub seat_tier: Option<String>,
    /// `organization.plan_display_name`, verbatim ("Ultra"): the plan's own name,
    /// which wins the label when present. Absent on orgs whose profile has no such key.
    pub plan_display_name: Option<String>,
}

impl Profile {
    /// `account.email`, or `None` when the fetch failed or the endpoint sent none.
    pub fn email(&self) -> Option<&str> {
        self.email.as_deref()
    }

    /// `account.uuid`, or `None` when the fetch failed or the endpoint sent none.
    pub fn account_uuid(&self) -> Option<&str> {
        self.account_uuid.as_deref()
    }

    /// This profile, with each identity field the profile endpoint left empty
    /// taken from the token-exchange response instead. A field the profile
    /// endpoint did send is kept. Both answers describe the one token the
    /// exchange just minted, so they cannot name two different accounts.
    pub(super) fn or_exchange_identity(self, identity: ExchangeIdentity) -> Self {
        let (account_uuid, email) = identity
            .account
            .map_or((None, None), |a| (a.uuid, a.email_address));
        let (org_uuid, org_name) = identity
            .organization
            .map_or((None, None), |o| (o.uuid, o.name));
        Self {
            email: self.email.or(email),
            account_uuid: self.account_uuid.or(account_uuid),
            org_uuid: self.org_uuid.or(org_uuid),
            org_name: self.org_name.or(org_name),
            ..self
        }
    }

    /// This profile, with each identity field the profile endpoint left empty taken from
    /// what the tool the login was imported from stored beside it. The endpoint describes the
    /// token itself, so a field it did send wins.
    pub(super) fn or_stored_identity(self, stored: &StoredClaudeIdentity) -> Self {
        Self {
            email: self.email.or_else(|| stored.email.clone()),
            account_uuid: self.account_uuid.or_else(|| stored.account_uuid.clone()),
            org_uuid: self.org_uuid.or_else(|| stored.org_uuid.clone()),
            org_name: self.org_name.or_else(|| stored.org_name.clone()),
            ..self
        }
    }
}

/// The identity another tool stores beside a Claude login it holds: claude-swap's
/// `sequence.json` entry, a `CLIProxyAPI` auth file's own fields, Claude Code's
/// `~/.claude.json` `oauthAccount`. `tcr import` compares it against the fleet without a
/// network call, and the import falls back to it field by field when the profile fetch
/// answers nothing ([`Profile::or_stored_identity`]). Empty strings are stored as `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoredClaudeIdentity {
    pub email: Option<String>,
    pub account_uuid: Option<String>,
    pub org_uuid: Option<String>,
    pub org_name: Option<String>,
}

impl StoredClaudeIdentity {
    /// The four fields, each `None` when absent, empty or blank.
    pub fn new(
        email: Option<String>,
        account_uuid: Option<String>,
        org_uuid: Option<String>,
        org_name: Option<String>,
    ) -> Self {
        let kept = |value: Option<String>| value.filter(|v| !v.trim().is_empty());
        Self {
            email: kept(email),
            account_uuid: kept(account_uuid),
            org_uuid: kept(org_uuid),
            org_name: kept(org_name),
        }
    }

    /// The row of `accounts` this identity already is, by the rule the write resolves its row
    /// by ([`crate::identity::same_identity`]: account UUID, then the org when both sides know
    /// it), and by email when either side has no account UUID: a Claude row whose name's email
    /// part ([`crate::identity::email_of`]) is this email, ignoring case.
    ///
    /// Deliberately at least as loose as the write's own resolution, so a login this calls new
    /// can never resolve onto an existing row at write time and replace its token.
    pub fn existing_row<'a>(&self, accounts: &'a [Account]) -> Option<&'a Account> {
        let probe = crate::identity::probe(
            crate::provider::Provider::Anthropic,
            self.email.as_deref().unwrap_or_default(),
            self.account_uuid.clone(),
            self.org_uuid.clone(),
            self.org_name.clone(),
        );
        accounts
            .iter()
            .filter(|row| row.provider == crate::provider::Provider::Anthropic)
            .find(|row| {
                if self.account_uuid.is_some() && row.account_uuid.is_some() {
                    crate::identity::same_identity(row, &probe)
                } else {
                    self.email.as_deref().is_some_and(|email| {
                        crate::identity::email_of(&row.name).eq_ignore_ascii_case(email)
                    })
                }
            })
    }
}

/// Fetch the account+org identity from the profile endpoint. Returns an
/// all-`None` [`Profile`] on any failure (network, non-2xx, or malformed body)
/// so the caller can still prompt for a name and login without org info. Serde
/// ignores unknown fields, so extra profile keys are harmless.
pub async fn fetch_profile(access_token: &str) -> Profile {
    fetch_profile_at(PROFILE_URL, access_token).await
}

/// [`fetch_profile`] against an explicit profile URL — the proxy's client-identity
/// check resolves a client bearer against its configured upstream, which in tests
/// is a local fake rather than [`PROFILE_URL`].
pub async fn fetch_profile_at(url: &str, access_token: &str) -> Profile {
    try_fetch_profile_at(url, access_token)
        .await
        .unwrap_or(Profile {
            email: None,
            account_uuid: None,
            org_uuid: None,
            org_name: None,
            organization_type: None,
            rate_limit_tier: None,
            seat_tier: None,
            plan_display_name: None,
        })
}

/// Why a profile fetch produced no [`Profile`]. The proxy's client-identity check
/// treats the two arms differently: a status is upstream's verdict on the bearer
/// and can be remembered for a while, a transport failure says nothing about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileFetchError {
    /// The request never completed, or the body did not parse.
    Transport,
    /// Upstream answered, with this non-2xx status.
    Status(u16),
}

/// [`fetch_profile_at`], keeping the reason for a miss.
pub async fn try_fetch_profile_at(
    url: &str,
    access_token: &str,
) -> Result<Profile, ProfileFetchError> {
    #[derive(Deserialize)]
    struct ProfileResponse {
        account: Option<ProfileAccount>,
        organization: Option<ProfileOrg>,
    }
    #[derive(Deserialize)]
    struct ProfileAccount {
        uuid: Option<String>,
        email: Option<String>,
    }
    #[derive(Deserialize)]
    struct ProfileOrg {
        uuid: Option<String>,
        name: Option<String>,
        organization_type: Option<String>,
        rate_limit_tier: Option<String>,
        seat_tier: Option<String>,
        plan_display_name: Option<String>,
    }

    async fn inner(url: &str, access_token: &str) -> Result<ProfileResponse, ProfileFetchError> {
        let client = crate::http_client::builder()
            .no_proxy()
            .build()
            .map_err(|_| ProfileFetchError::Transport)?;
        let response = client
            .get(url)
            .header("Authorization", format!("Bearer {access_token}"))
            .send()
            .await
            .map_err(|_| ProfileFetchError::Transport)?;
        let status = response.status();
        if !status.is_success() {
            return Err(ProfileFetchError::Status(status.as_u16()));
        }
        response
            .json()
            .await
            .map_err(|_| ProfileFetchError::Transport)
    }

    let non_empty = |s: Option<String>| s.filter(|v| !v.is_empty());
    inner(url, access_token).await.map(|profile| {
        let account = profile.account;
        let organization = profile.organization;
        Profile {
            email: non_empty(account.as_ref().and_then(|a| a.email.clone())),
            account_uuid: non_empty(account.and_then(|a| a.uuid)),
            org_uuid: non_empty(organization.as_ref().and_then(|o| o.uuid.clone())),
            org_name: non_empty(organization.as_ref().and_then(|o| o.name.clone())),
            organization_type: non_empty(
                organization
                    .as_ref()
                    .and_then(|o| o.organization_type.clone()),
            ),
            rate_limit_tier: non_empty(
                organization
                    .as_ref()
                    .and_then(|o| o.rate_limit_tier.clone()),
            ),
            seat_tier: non_empty(organization.as_ref().and_then(|o| o.seat_tier.clone())),
            plan_display_name: non_empty(organization.and_then(|o| o.plan_display_name)),
        }
    })
}

/// The `account` and `organization` objects of a token-exchange response.
///
/// The token endpoint sends these on every exchange, whatever the scope
/// (`docs/design/long-lived-tokens.md`, "Identity arrives free at exchange
/// time"). [`login`] still asks the profile endpoint first, because only it
/// carries the plan fields; this is what names the row when that fetch comes
/// back empty, instead of [`unnamed_fallback`].
#[derive(Debug, Default, Deserialize)]
pub(super) struct ExchangeIdentity {
    #[serde(default)]
    account: Option<ExchangeAccount>,
    #[serde(default)]
    organization: Option<ExchangeOrg>,
}

#[derive(Debug, Deserialize)]
struct ExchangeAccount {
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    email_address: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExchangeOrg {
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    name: Option<String>,
}
