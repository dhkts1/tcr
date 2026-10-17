//! The group settings the manager routes by, held as one value.
//!
//! Every fact here comes from the config file's `groupSettings` and account
//! `groups`, and a hot reload ([`super::Manager::reload_groups_if_changed`])
//! replaces all of them at once. The manager keeps them behind one
//! `RwLock<Arc<GroupPolicy>>`, so a reader clones one `Arc` and sees one
//! version of the file: a `select` can never read `reserved` from one reload
//! and `spillToPool` from the next.

use std::collections::{BTreeMap, HashSet};

use crate::config::Config;

/// One version of the fleet's group settings.
#[derive(Debug, Default, PartialEq)]
pub(super) struct GroupPolicy {
    /// Groups marked `reserved` (`tcr group reserve`): an account carrying
    /// one is off-limits to traffic that asked for no group. See
    /// [`super::Manager::reserved_blocks`].
    pub(super) reserved: HashSet<String>,
    /// Groups marked `parked` (`tcr group park`): every member is out of
    /// rotation. See [`super::Manager::parked_blocks`].
    pub(super) parked: HashSet<String>,
    /// Groups opted in to `allowControlAccount`: an explicit ask for one may
    /// select the control account.
    pub(super) control_allowed: HashSet<String>,
    /// Groups opted in to `spillToPool`: an explicit ask for one falls back to
    /// the whole pool when no member can serve.
    pub(super) spill: HashSet<String>,
    /// Every group on the fleet mapped to its resolved color, in wire order
    /// (`config::Config::group_colors`).
    pub(super) colors: BTreeMap<String, String>,
}

impl GroupPolicy {
    pub(super) fn from_config(config: &Config) -> Self {
        Self {
            reserved: config.reserved_group_names(),
            parked: config.parked_group_names(),
            control_allowed: config.control_allowed_group_names(),
            spill: config.spill_group_names(),
            colors: config.group_colors(),
        }
    }

    /// Whether an explicit ask for `group` is held strictly inside it:
    /// `reserved || !spill`. A reserved group ignores `spillToPool`; see
    /// [`crate::config::GroupSettings::spill_to_pool`].
    pub(super) fn is_strict(&self, group: &str) -> bool {
        self.reserved.contains(group) || !self.spill.contains(group)
    }

    /// One line per setting that differs between `self` and `fresh`, in the
    /// words [`super::Manager::apply_group_reload`] logs. Empty when nothing
    /// changed.
    pub(super) fn changes_to(&self, fresh: &Self) -> Vec<String> {
        let sets = [
            ("reserved groups", &self.reserved, &fresh.reserved),
            ("parked groups", &self.parked, &fresh.parked),
            (
                "control-allowed groups",
                &self.control_allowed,
                &fresh.control_allowed,
            ),
            ("spill-to-pool groups", &self.spill, &fresh.spill),
        ];
        let mut changed: Vec<String> = sets
            .into_iter()
            .filter(|(_, old, new)| old != new)
            .map(|(label, old, new)| {
                format!(
                    "{label}: {:?} -> {:?}",
                    sorted_groups(old),
                    sorted_groups(new)
                )
            })
            .collect();
        if self.colors != fresh.colors {
            changed.push(format!(
                "group colors: {:?} -> {:?}",
                self.colors, fresh.colors
            ));
        }
        changed
    }
}

/// The members of `groups` that are in `set`, sorted: the per-account
/// `reserved_groups` / `parked_groups` / `control_allowed_groups` a snapshot
/// row carries.
pub(super) fn carried_in(groups: &[String], set: &HashSet<String>) -> Vec<String> {
    let mut carried: Vec<String> = groups
        .iter()
        .filter(|g| set.contains(*g))
        .cloned()
        .collect();
    carried.sort();
    carried
}

/// A sorted `Vec` of `set`'s members, for a deterministic (and readable)
/// before/after in the reload's change log: `HashSet`'s own iteration order is
/// unspecified and would make the same reload log differently run to run.
fn sorted_groups(set: &HashSet<String>) -> Vec<String> {
    let mut v: Vec<String> = set.iter().cloned().collect();
    v.sort();
    v
}
