//! `tcr fan probe [--label <label>]`: send the root fan helper its version op,
//! straight from this CLI process, and print what came back. Hidden: it is a
//! diagnostic for installing the helper, not a feature.
//!
//! Version only. The probe writes no fan and sends no other op, so it is safe
//! to run against a helper that holds the fans for a running proxy.
//!
//! Which helper it dials: the one this binary's own signature belongs to
//! (`FanIdentity::of_this_process`), so an unsigned `tcr` fails closed here
//! exactly as the proxy will, without dialling. `--label` overrides only the
//! service dialled (one of the two helper labels), so a test can point an
//! unsigned build at a real helper and watch the helper refuse it: the helper's
//! own requirement is still the gate. The proxy has no such override.
//!
//! The XPC call blocks on a channel with a deadline, so it runs on tokio's
//! blocking pool, never on a runtime worker.

use std::time::Duration;

use tcr_fan::identity::{FanIdentity, Role};

/// How long the probe waits for the reply. A first call cold-starts the
/// helper through launchd, so this is generous.
pub const PROBE_DEADLINE: Duration = Duration::from_secs(10);

/// Which helper to dial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub identity: FanIdentity,
    /// Chosen by `--label` rather than by this binary's signature.
    pub overridden: bool,
}

/// The helper this probe dials: `--label`'s when given (it must be one of the
/// two helper labels), else the one this binary's signature belongs to.
pub fn target(label: Option<&str>) -> anyhow::Result<Target> {
    match label {
        Some(label) => FanIdentity::of_label(label)
            .map(|identity| Target {
                identity,
                overridden: true,
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{label} is not a fan helper label (expected {} or {})",
                    FanIdentity::Release.helper_label(),
                    FanIdentity::Dev.helper_label()
                )
            }),
        None => FanIdentity::of_this_process(Role::Client)
            .map(|identity| Target {
                identity,
                overridden: false,
            })
            .map_err(|e| {
                anyhow::anyhow!(
                    "not dialling: {e}. Fan control needs the tcr bundled in TcrBar.app; \
                     --label dials a helper anyway, to watch it refuse this binary"
                )
            }),
    }
}

/// Run the probe and print its outcome. Exits non-zero (through the error)
/// on anything but a protocol-1 version reply.
pub async fn run(label: Option<String>) -> anyhow::Result<()> {
    let target = target(label.as_deref())?;
    let label = target.identity.helper_label();
    println!(
        "dialling {label} ({} helper{})",
        target.identity,
        if target.overridden {
            ", chosen by --label"
        } else {
            ", from this binary's signature"
        }
    );
    let identity = target.identity;
    let line = tokio::task::spawn_blocking(move || version(identity)).await??;
    println!("{line}");
    Ok(())
}

#[cfg(target_os = "macos")]
fn version(identity: FanIdentity) -> anyhow::Result<String> {
    use tcr_fan::wire::{PROTOCOL_VERSION, Reply, Request};
    use tcr_fan::xpc::Client;

    let client = Client::connect(
        identity.helper_label(),
        identity.helper_requirement(),
        |_connection_error| {
            // The reply handler already carries the same error for this one
            // message; nothing else is in flight.
        },
    )?;
    match client.send(&Request::Version, PROBE_DEADLINE)? {
        Reply::Version { protocol, build } if protocol == PROTOCOL_VERSION => Ok(format!(
            "helper answered: protocol {protocol}, build {build}"
        )),
        Reply::Version { protocol, build } => anyhow::bail!(
            "helper answered protocol {protocol} (build {build}); this tcr speaks {PROTOCOL_VERSION}"
        ),
        Reply::Refused(error) => anyhow::bail!("helper refused the version op: {error}"),
        other => anyhow::bail!("helper answered a version op with {other:?}"),
    }
}

#[cfg(not(target_os = "macos"))]
fn version(_identity: FanIdentity) -> anyhow::Result<String> {
    anyhow::bail!("the fan helper exists only on macOS")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_override_takes_only_helper_labels() {
        assert_eq!(
            target(Some("io.github.dhkts1.tcrbar.fanhelper.dev")).unwrap(),
            Target {
                identity: FanIdentity::Dev,
                overridden: true
            }
        );
        assert!(target(Some("com.apple.something")).is_err());
    }

    /// A test binary is unsigned, so without `--label` the probe refuses to
    /// dial rather than guess a helper.
    #[test]
    fn an_unsigned_tcr_does_not_dial() {
        let error = target(None).unwrap_err().to_string();
        assert!(error.starts_with("not dialling:"), "{error}");
    }
}
