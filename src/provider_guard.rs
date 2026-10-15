//! The verbs a Codex account cannot take, and every sentence that says so.
//!
//! Three things in tcr are Claude-only, and each for a reason of its own:
//!
//! - **A long-lived token.** `tcr mint` asks Anthropic's authorize endpoint for a 365-day
//!   token. ChatGPT has no such grant, so a Codex row has nothing to mint.
//! - **Leaving through a peer.** A Codex request always leaves from this Mac: no Mac lends,
//!   serves or carries one in this version (`Manager::lend_candidates` skips every Codex
//!   account, and selection skips a strictly pinned one). So a Codex account cannot be
//!   pinned to exit from another Mac, and it cannot be lent.
//! - **Being reached through a group.** A group is asked for with `tcr run --group`, which
//!   sets a header that only a Claude session sends. No Codex request carries a group, so a
//!   group labels a Codex account and can park it, but never routes to it, and while the
//!   group is reserved its Codex members serve nothing at all.
//!
//! Every refusal and note about those three facts is written here once, so the CLI's
//! refusals, its warnings and the docs cannot drift apart. Each follows the house order:
//! what happened, why, what to do, then the safety statement. A refusal leaves the config
//! byte-identical, and its last sentence says so.
//!
//! A fleet with no Codex account never reaches any of these: every caller gates on the
//! row's provider, or on the fleet holding a Codex row, so a Claude-only fleet prints what
//! it printed before.

use crate::config::Account;
use crate::provider::Provider;

/// A verb only a Claude account can take. [`provider_guard`] answers whether a row may.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeOnlyVerb {
    /// `tcr mint`: minting a long-lived (365-day) token.
    Mint,
    /// `tcr peer account --exits-from <peer>` and `--must`: pinning the account to leave
    /// from another Mac.
    ExitFromPeer,
    /// `tcr peer lend` and `tcr peer share on`, with a scope that names the account:
    /// lending it to another Mac.
    Lend,
}

/// Whether `row` may take `verb`: `Ok(())` for a Claude row, and for a Codex row the
/// refusal to print, which names the row and ends by saying nothing was changed.
pub fn provider_guard(row: &Account, verb: ClaudeOnlyVerb) -> Result<(), String> {
    let name = &row.name;
    match row.provider {
        Provider::Anthropic => Ok(()),
        Provider::Codex => Err(match verb {
            ClaudeOnlyVerb::Mint => format!(
                "'{name}' is a Codex account, and only a Claude account can mint a long-lived \
                 token. Nothing was changed."
            ),
            ClaudeOnlyVerb::ExitFromPeer => format!(
                "'{name}' is a Codex account, and a Codex request always leaves from this Mac, \
                 so it cannot exit from another Mac. Nothing was changed."
            ),
            ClaudeOnlyVerb::Lend => format!(
                "'{name}' is a Codex account, and Codex accounts stay on this Mac: only a Claude \
                 account can be lent. Nothing was changed."
            ),
        }),
    }
}

/// The refusal when reserving `group` would leave no enabled, unreserved Claude account.
///
/// `codex_left` is how many enabled Codex accounts stay unreserved; they are named because
/// they are what an operator sees still serving, and they cannot serve a Claude session.
/// `advice` is the remedy without its full stop.
pub fn reserve_leaves_no_claude(group: &str, codex_left: usize, advice: &str) -> String {
    format!(
        "reserving '{group}' would leave 0 unreserved enabled Claude accounts — Claude could \
         serve no unrequested traffic, and the {codex_left} Codex account(s) left cannot serve \
         it. {advice}. Nothing changed."
    )
}

/// The refusal when `group` holds Codex accounts and `--force` was not given.
///
/// `names` is every Codex account carrying the group, in config order, and is never empty:
/// the caller builds this only when there is one. The command names the first of them.
pub fn reserve_strands_codex(group: &str, names: &[&str]) -> String {
    let first = names.first().copied().unwrap_or_default();
    format!(
        "reserving '{group}' strands its Codex account(s) {}: a Codex session cannot ask for a \
         group, so while '{group}' is reserved they serve nothing. Remove them first (`tcr group \
         rm {group} {first}`), or pass --force to reserve it anyway. Nothing changed.",
        names.join(", ")
    )
}

/// The warning `tcr group reserve --force` prints once the reservation is written, when
/// `group` holds the Codex accounts `names` (never empty; see [`reserve_strands_codex`]).
pub fn reserve_force_warning(group: &str, names: &[&str]) -> String {
    let first = names.first().copied().unwrap_or_default();
    format!(
        "warning: '{group}' holds the Codex account(s) {}. A Codex session cannot ask for a \
         group, so while '{group}' is reserved they serve nothing — `tcr group rm {group} \
         {first}` returns one to the pool.",
        names.join(", ")
    )
}

/// The clause that ends both the reserved line and the already-reserved line, in a fleet
/// holding a Codex account: the enabled accounts left unreserved, per provider.
pub fn reserve_remaining(claude: usize, codex: usize) -> String {
    format!("{claude} unreserved enabled Claude account(s) and {codex} Codex account(s) remain.")
}

/// The low-count warning after a reservation, in a fleet holding a Codex account, when
/// `claude` (fewer than three) enabled Claude accounts are left unreserved.
pub fn reserve_few_claude_left(claude: usize) -> String {
    format!(
        "warning: only {claude} unreserved enabled Claude account(s) remain — ordinary \
         (unrequested) Claude traffic may starve."
    )
}

/// The refusal of `tcr run --group <group>` when no Claude account carries the group: every
/// member is a Codex account, and a Claude session cannot be served by one.
pub fn group_has_no_claude(group: &str) -> String {
    format!(
        "group '{group}' has no Claude account (its members are Codex accounts, which a Claude \
         session cannot use) — nothing launched."
    )
}

/// What `tcr priority --first` or `--last` prints for a Codex row: the move stays inside
/// the Codex rotation, so the line says which end of it the account now holds.
pub fn codex_priority_line(name: &str, value: i64, first: bool) -> String {
    let end = if first { "front" } else { "back" };
    format!("Set priority of '{name}' to {value}, the {end} of the Codex rotation.")
}

/// Appended by `tcr peer ls` to a Codex account's exit pin that is not strict: the pin is
/// on the row, and it changes nothing, because a Codex request always leaves from this Mac.
pub const INERT_EXIT_SUFFIX: &str = " (inert: Codex accounts always leave from this Mac)";

/// Appended by `tcr peer ls` to a STRICT Codex exit pin naming a peer. That pin is not
/// inert: selection skips the account, because a Codex account cannot exit from another
/// Mac and a strict pin forbids leaving from this one. So it names the command that clears
/// the pin.
pub fn refused_exit_suffix(name: &str) -> String {
    format!(
        " (refused: a Codex account cannot exit from another Mac, so tcr skips this account; \
         clear it with `tcr peer account {name} --exits-from local --no-must`)"
    )
}

/// The warning `tcr group add` prints on stderr when the account it labels is a Codex
/// account. The label is still written: it is how a group parks the account, and TcrBar
/// shows the stderr of a successful group add.
pub fn codex_group_add_warning(name: &str, group: &str) -> String {
    format!(
        "warning: '{name}' is a Codex account. A Codex session cannot ask for a group, so \
         '{group}' labels it and can park it, but never routes to it; while '{group}' is \
         reserved, it serves nothing."
    )
}

/// The reason column of `tcr mint --group`'s `REFUSED` line for a Codex member, which the
/// group mint skips without opening a URL.
pub const MINT_SKIP_REASON: &str = "Codex account (tcr mint is Claude-only)";

#[cfg(test)]
mod tests {
    use super::*;

    /// One account row as the config file holds it, Claude when `provider` is `None`.
    fn row(name: &str, provider: Option<&str>) -> Account {
        let mut value = serde_json::json!({
            "name": name,
            "accessToken": "t",
            "refreshToken": "r",
        });
        if let Some(provider) = provider {
            value["provider"] = serde_json::json!(provider);
        }
        serde_json::from_value(value).unwrap()
    }

    /// Each Claude-only verb refuses a Codex row with its own sentence, which names the row
    /// and says nothing was changed.
    #[test]
    fn a_codex_row_is_refused_for_each_claude_only_verb() {
        let bob = row("bob@example.com/codex", Some("codex"));
        assert_eq!(
            provider_guard(&bob, ClaudeOnlyVerb::Mint),
            Err("'bob@example.com/codex' is a Codex account, and only a Claude account can mint a long-lived token. Nothing was changed.".to_string())
        );
        assert_eq!(
            provider_guard(&bob, ClaudeOnlyVerb::ExitFromPeer),
            Err("'bob@example.com/codex' is a Codex account, and a Codex request always leaves from this Mac, so it cannot exit from another Mac. Nothing was changed.".to_string())
        );
        assert_eq!(
            provider_guard(&bob, ClaudeOnlyVerb::Lend),
            Err("'bob@example.com/codex' is a Codex account, and Codex accounts stay on this Mac: only a Claude account can be lent. Nothing was changed.".to_string())
        );
    }

    /// A Claude row, which is every row of a fleet that never held a Codex account, is never
    /// refused by any of them.
    #[test]
    fn a_claude_row_is_never_refused() {
        let alice = row("alice@example.com", None);
        for verb in [
            ClaudeOnlyVerb::Mint,
            ClaudeOnlyVerb::ExitFromPeer,
            ClaudeOnlyVerb::Lend,
        ] {
            assert_eq!(provider_guard(&alice, verb), Ok(()), "{verb:?}");
        }
    }
}
