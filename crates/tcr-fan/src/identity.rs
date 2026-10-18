//! Who may talk to whom: the root fan helper's label, both ends' signing
//! identifiers, and the code-signing requirement each end sets on its XPC
//! connection before the connection is activated.
//!
//! The helper accepts only the proxy (`tcr`, signed `io.github.dhkts1.tcr`);
//! the proxy accepts only the helper (signed with the helper's label). Both
//! requirements carry the same Developer ID team clause as the socket
//! handoff's peer check in the main crate (`handoff::PEER_REQUIREMENT`), and a
//! test there holds the two equal.
//!
//! A dev build (`TCRBAR_DEV_BUILD=1`) signs both binaries with `.dev`
//! identifiers and registers the helper under a `.dev` label, so a release
//! proxy can never reach a dev helper or the reverse.
//!
//! Each binary decides which identity it is from its own signature, never from
//! an environment variable or an argument: [`FanIdentity::of_this_process`]. A
//! binary that matches neither (ad-hoc, standalone, another team) has no fan
//! identity at all, so it never listens (the helper) or never dials (the
//! proxy). That is how an unsigned build fails closed.

use std::fmt;

/// The Developer ID team clause, spelled as a literal so both requirement
/// strings can be built with `concat!` and stay `&'static str`.
///
/// The two marker OIDs are load-bearing: without them the clause accepts an
/// Apple Development certificate of the same team (`handoff.rs` says why).
macro_rules! team_clause {
    () => {
        concat!(
            " and certificate 1[field.1.2.840.113635.100.6.2.6]",
            " and certificate leaf[field.1.2.840.113635.100.6.1.13]",
            " and certificate leaf[subject.OU] = \"UJQ3GQF56Y\"",
        )
    };
}

/// The team clause on its own, as the main crate's test compares it.
pub const TEAM: &str = team_clause!();

/// Which build a binary belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanIdentity {
    /// A shipping build, signed with Developer ID.
    Release,
    /// A `TCRBAR_DEV_BUILD=1` build: `.dev` identifiers and label.
    Dev,
}

/// Which end of the connection a binary is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The proxy, `tcr`: dials the helper.
    Client,
    /// The root helper, `tcr-fan-helper`: listens.
    Helper,
}

impl FanIdentity {
    /// Release first: a binary that satisfies both (it cannot, the identifiers
    /// differ) would be treated as the shipping one.
    pub const ALL: [Self; 2] = [Self::Release, Self::Dev];

    /// The launchd label, which is also the Mach service name, the plist's
    /// file stem and the helper's signing identifier.
    pub const fn helper_label(self) -> &'static str {
        match self {
            Self::Release => "io.github.dhkts1.tcrbar.fanhelper",
            Self::Dev => "io.github.dhkts1.tcrbar.fanhelper.dev",
        }
    }

    /// The signing identifier of the bundled `tcr`, as `build-tcrbar.sh` and
    /// `release-tcrbar.sh` set it.
    pub const fn client_identifier(self) -> &'static str {
        match self {
            Self::Release => "io.github.dhkts1.tcr",
            Self::Dev => "io.github.dhkts1.tcr.dev",
        }
    }

    /// What the helper's listener demands of every caller: the proxy of the
    /// same build.
    pub const fn client_requirement(self) -> &'static str {
        match self {
            Self::Release => concat!(
                "anchor apple generic and identifier \"io.github.dhkts1.tcr\"",
                team_clause!()
            ),
            Self::Dev => concat!(
                "anchor apple generic and identifier \"io.github.dhkts1.tcr.dev\"",
                team_clause!()
            ),
        }
    }

    /// What the proxy's connection demands of the helper that answers: the
    /// helper of the same build.
    pub const fn helper_requirement(self) -> &'static str {
        match self {
            Self::Release => concat!(
                "anchor apple generic and identifier \"io.github.dhkts1.tcrbar.fanhelper\"",
                team_clause!()
            ),
            Self::Dev => concat!(
                "anchor apple generic and identifier \"io.github.dhkts1.tcrbar.fanhelper.dev\"",
                team_clause!()
            ),
        }
    }

    /// The requirement a binary in `role` must itself satisfy to be this
    /// identity: the one the OTHER end will demand of it. One string decides
    /// both sides, so they cannot drift.
    pub const fn own_requirement(self, role: Role) -> &'static str {
        match role {
            Role::Client => self.client_requirement(),
            Role::Helper => self.helper_requirement(),
        }
    }

    /// The identity whose helper listens on `label`, for the probe's
    /// `--label`. `None` for any other name.
    pub fn of_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|i| i.helper_label() == label)
    }
}

impl fmt::Display for FanIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Release => write!(f, "release"),
            Self::Dev => write!(f, "dev"),
        }
    }
}

/// This binary's own signature satisfies neither identity's requirement for
/// its role: ad-hoc, standalone, or another signer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotSignedForFans {
    /// What the Security framework said about the last requirement tried, or
    /// why no check could run.
    pub detail: String,
}

impl fmt::Display for NotSignedForFans {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "this binary is not signed as either fan identity: {}",
            self.detail
        )
    }
}

impl std::error::Error for NotSignedForFans {}

#[cfg(target_os = "macos")]
impl FanIdentity {
    /// Check this process's own signature against each identity's
    /// requirement for `role`, release first.
    pub fn of_this_process(role: Role) -> Result<Self, NotSignedForFans> {
        use std::str::FromStr;

        use security_framework::os::macos::code_signing::{Flags, SecCode, SecRequirement};

        let me = SecCode::for_self(Flags::NONE).map_err(|e| NotSignedForFans {
            detail: format!("SecCodeCopySelf: {e}"),
        })?;
        let mut last = String::from("no requirement tried");
        for identity in Self::ALL {
            let requirement =
                SecRequirement::from_str(identity.own_requirement(role)).map_err(|e| {
                    NotSignedForFans {
                        detail: format!("{identity} requirement does not parse: {e}"),
                    }
                })?;
            match me.check_validity(Flags::NONE, &requirement) {
                Ok(()) => return Ok(identity),
                Err(e) => last = format!("{identity}: {e}"),
            }
        }
        Err(NotSignedForFans { detail: last })
    }
}

#[cfg(not(target_os = "macos"))]
impl FanIdentity {
    /// No code signing off macOS: never a fan identity.
    pub fn of_this_process(_role: Role) -> Result<Self, NotSignedForFans> {
        Err(NotSignedForFans {
            detail: "code signing exists only on macOS".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_requirement() -> Vec<&'static str> {
        FanIdentity::ALL
            .into_iter()
            .flat_map(|i| [i.client_requirement(), i.helper_requirement()])
            .collect()
    }

    /// Every requirement pins one identifier, the anchor and the whole team
    /// clause; the client's names the binary that actually calls (the proxy),
    /// the helper's names the helper's label.
    #[test]
    fn requirements_pin_identifier_and_team() {
        for identity in FanIdentity::ALL {
            assert_eq!(
                identity.client_requirement(),
                format!(
                    "anchor apple generic and identifier \"{}\"{TEAM}",
                    identity.client_identifier()
                )
            );
            assert_eq!(
                identity.helper_requirement(),
                format!(
                    "anchor apple generic and identifier \"{}\"{TEAM}",
                    identity.helper_label()
                )
            );
        }
        assert!(TEAM.contains("1.2.840.113635.100.6.2.6"));
        assert!(TEAM.contains("1.2.840.113635.100.6.1.13"));
        assert!(TEAM.contains("subject.OU] = \"UJQ3GQF56Y\""));
    }

    #[test]
    fn dev_and_release_never_share_a_name() {
        let release = FanIdentity::Release;
        let dev = FanIdentity::Dev;
        assert_eq!(
            dev.helper_label(),
            format!("{}.dev", release.helper_label())
        );
        assert_eq!(
            dev.client_identifier(),
            format!("{}.dev", release.client_identifier())
        );
        assert_eq!(FanIdentity::of_label(release.helper_label()), Some(release));
        assert_eq!(FanIdentity::of_label(dev.helper_label()), Some(dev));
        assert_eq!(FanIdentity::of_label("io.github.dhkts1.tcrbar"), None);
    }

    /// The four strings parse, so no end can be left with a requirement the
    /// framework refuses at activation. On macOS through the Security
    /// framework's own parser.
    #[cfg(target_os = "macos")]
    #[test]
    fn identity_strings_parse() {
        use std::str::FromStr;

        use security_framework::os::macos::code_signing::SecRequirement;

        for requirement in every_requirement() {
            if let Err(e) = SecRequirement::from_str(requirement) {
                panic!("{requirement:?} does not parse: {e}");
            }
        }
        // A positive control for the parser itself: a broken string is refused.
        assert!(SecRequirement::from_str("anchor apple generic and identifier").is_err());
    }

    /// Off macOS there is no parser; hold the shape a parser needs instead.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn identity_strings_parse() {
        for requirement in every_requirement() {
            assert!(requirement.starts_with("anchor apple generic and identifier \""));
            assert_eq!(requirement.matches('"').count(), 4, "{requirement}");
        }
    }

    /// A test binary is ad-hoc signed, so it is neither identity in either
    /// role: the fail-closed path, measured.
    #[test]
    fn an_unsigned_binary_has_no_fan_identity() {
        for role in [Role::Client, Role::Helper] {
            assert!(FanIdentity::of_this_process(role).is_err(), "{role:?}");
        }
    }
}
