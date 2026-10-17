//! Whether the running proxy can pool codex, and if not, why: the one decision behind every
//! surface that says so.
//!
//! `tcr run --codex` acts on it (a pooled launch or a direct one), `tcr status` prints it above
//! its header and repeats it on every Codex row of `--json`, and `tcr doctor` and the dashboard
//! report it. They must never word it two ways, which two copies of the decision would sooner or
//! later do, so there is one function, and its reasons are the sentences `tcr run --codex` has
//! always printed.
//!
//! The inputs are the running proxy's own report of its MITM listener (the `mitm` key of its
//! status document, one read) and whether tcr's CA file is readable on disk. Never a TLS load in
//! this process: a load here can re-mint a stale leaf on disk while the running proxy keeps
//! serving the leaf it loaded at boot, and it would report coverage that proxy does not have.
//!
//! The order is fixed, and the first match wins: nothing listening, a status that could not be
//! read, a proxy older than the `mitm` key, MITM off. A leaf tcr minted
//! that intercepts `chatgpt.com` is then not pooled while the pool holds no Codex account (the
//! proxy blind-tunnels `chatgpt.com` without one), else while the CA is unreadable (codex could
//! not trust the leaf), else pooled. A minted leaf that does not intercept `chatgpt.com`, and a
//! leaf this build does not know, are not pooled whatever the accounts or the CA.

use std::path::Path;

use tcr_status_wire::CodexRoute;

use crate::cli::LiveStatusError;
use crate::status::{MitmStatus, StatusPayload};

/// The extra-CA file a launched Codex reads. Preserve the caller's complete bundle;
/// without tcr's certificate, launching on its own login preserves working corporate TLS.
pub fn codex_ca_for_launch(
    ca: &Path,
    inherited: &dyn Fn(&str) -> Option<String>,
) -> Result<std::path::PathBuf, String> {
    for variable in ["CODEX_CA_CERTIFICATE", "SSL_CERT_FILE"] {
        if let Some(bundle) = inherited(variable).filter(|value| !value.is_empty()) {
            let bundle = std::path::PathBuf::from(bundle);
            return if crate::mitm::ca_bundle_contains(&bundle, ca) {
                Ok(bundle)
            } else {
                Err(format!(
                    "{variable} does not include tcr's CA; keep the caller's trust settings and use codex's own login. Add tcr's CA to that bundle before retrying `tcr run --codex` to pool it"
                ))
            };
        }
    }
    Ok(ca.to_path_buf())
}

/// Nothing listens on the proxy's port.
pub const NO_PROXY: &str = "no-proxy";
/// Something listens, but its status could not be read.
pub const STATUS_UNREADABLE: &str = "status-unreadable";
/// The proxy's status has no `mitm` key: it predates Codex interception.
pub const PREDATES: &str = "predates";
/// The proxy runs without MITM: its TLS material failed to load at boot.
pub const MITM_OFF: &str = "mitm-off";
/// The proxy could intercept `chatgpt.com`, but its pool holds no Codex account.
pub const NO_CODEX_ACCOUNT: &str = "no-codex-account";
/// The proxy intercepts `chatgpt.com`, but tcr's CA is not readable here.
pub const NO_CA: &str = "no-ca";
/// The proxy serves a leaf it minted that does not intercept `chatgpt.com`.
pub const LEAF_MISSING_HOST: &str = "leaf-missing-host";
/// The proxy reports a leaf kind this build does not know.
pub const UNKNOWN_LEAF: &str = "unknown-leaf";

/// Can the proxy listening on `port` pool codex, read from its status document (`status`, one
/// read: [`crate::cli::fetch_live_status`]), the CA at `ca` (`ca_readable`) and how many Codex
/// accounts its pool holds (`codex_accounts`, the payload's Codex rows in any state).
///
/// `upstream` is the payload's `codex_upstream`: the proxy names its Codex upstream only when it
/// is not the default. A read that produced no payload names none.
pub fn codex_route(
    status: Result<&StatusPayload, &LiveStatusError>,
    port: u16,
    ca: &Path,
    ca_readable: bool,
    codex_accounts: usize,
) -> CodexRoute {
    let payload = match status {
        Err(LiveStatusError::NoServer) => {
            return not_pooled(
                NO_PROXY,
                format!("no proxy answered on 127.0.0.1:{port}"),
                None,
            );
        }
        Err(err) => {
            return not_pooled(
                STATUS_UNREADABLE,
                format!("could not read the running proxy's status ({})", err.why()),
                None,
            );
        }
        Ok(payload) => payload,
    };
    let upstream = payload.codex_upstream.clone();
    let Some(mitm) = &payload.mitm else {
        return not_pooled(
            PREDATES,
            format!(
                "the proxy on :{port} (build {}) predates Codex interception — restart it on \
                 this build at a quiet moment",
                payload.build.sha
            ),
            upstream,
        );
    };
    decide(
        mitm,
        port,
        ca,
        ca_readable,
        codex_accounts,
        &payload.build.sha,
        upstream,
    )
}

/// [`codex_route`] inside the serving process, which reads its own listener (`mitm`,
/// [`crate::manager::Manager::mitm_status`]) rather than a status document. So it never says
/// `no-proxy`, `status-unreadable` or `predates`: this process is the proxy, it answered, and it
/// has the key.
///
/// `codex_accounts` is the pool's live Codex accounts:
/// [`crate::manager::Manager::provider_account_count`] for `Provider::Codex`, which counts them in
/// any state but skips a slot deleted while the process runs. Those are the accounts a status
/// document lists, and the ones the listener counts on every CONNECT to decide whether it
/// intercepts `chatgpt.com`. `codex_upstream` is the process's Codex upstream, named in the
/// route only when it is not the default.
pub fn codex_route_in_process(
    mitm: &MitmStatus,
    port: u16,
    ca: &Path,
    ca_readable: bool,
    codex_accounts: usize,
    codex_upstream: &str,
) -> CodexRoute {
    let upstream = crate::config::non_default_codex_upstream(codex_upstream);
    decide(
        mitm,
        port,
        ca,
        ca_readable,
        codex_accounts,
        &crate::build_info::BuildInfo::current().sha,
        upstream,
    )
}

/// The decision over a listener's report, in `tcr run --codex`'s order: the leaf, then the Codex
/// account count, then the CA. `build` names the proxy's build in the unknown-leaf reason.
fn decide(
    mitm: &MitmStatus,
    port: u16,
    ca: &Path,
    ca_readable: bool,
    codex_accounts: usize,
    build: &str,
    upstream: Option<String>,
) -> CodexRoute {
    let host = crate::provider::CODEX_HOST;
    let (code, reason) = match mitm.leaf.as_str() {
        MitmStatus::LEAF_NONE => (
            MITM_OFF,
            format!(
                "the proxy on :{port} is running without MITM: its TLS material failed to load \
                 at boot — see its log"
            ),
        ),
        // The proxy terminates `chatgpt.com` only while its pool holds a Codex account
        // (`mitm::handle_connect`), so with none it blind-tunnels the launch whatever its leaf
        // covers.
        MitmStatus::LEAF_MINTED if mitm.intercepts(host) => {
            if codex_accounts == 0 {
                (NO_CODEX_ACCOUNT, no_codex_account_reason(port, host))
            } else if ca_readable {
                return CodexRoute {
                    pooled: true,
                    code: None,
                    reason: None,
                    upstream,
                };
            } else {
                (NO_CA, format!("no readable CA at {}", ca.display()))
            }
        }
        MitmStatus::LEAF_MINTED => (
            LEAF_MISSING_HOST,
            format!(
                "the proxy on :{port} serves a leaf it minted but does not intercept {host} (it \
                 intercepts [{}])",
                mitm.intercepted_hosts.join(", ")
            ),
        ),
        // A leaf kind this build does not know is a different build's, and nothing it reports
        // can be read as tcr's own minted leaf.
        leaf => (
            UNKNOWN_LEAF,
            format!(
                "the proxy on :{port} (build {build}) reports a leaf this build does not know \
                 ({leaf:?}) — run tcr run --codex from the same build as the proxy"
            ),
        ),
    };
    not_pooled(code, reason, upstream)
}

/// How many Codex accounts a status document lists, in any state: the count [`codex_route`] takes
/// from a live read. A deleted account is not listed, and the proxy does not count it either
/// ([`crate::manager::Manager::provider_account_count`]).
pub fn listed_codex_accounts(payload: &StatusPayload) -> usize {
    payload
        .accounts
        .iter()
        .filter(|row| row.provider == crate::provider::Provider::Codex)
        .count()
}

/// Why codex is not pooled when the proxy holds no Codex account, and the command that adds one:
/// a login of tcr's own, which leaves the codex CLI's login alone.
pub fn no_codex_account_reason(port: u16, host: &str) -> String {
    format!(
        "the proxy on :{port} holds no Codex account, so it blind-tunnels {host} and codex keeps \
         its own login — add one with `tcr login --codex`"
    )
}

fn not_pooled(code: &str, reason: String, upstream: Option<String>) -> CodexRoute {
    CodexRoute {
        pooled: false,
        code: Some(code.to_string()),
        reason: Some(reason),
        upstream,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORT: u16 = 3999;

    fn ca() -> &'static Path {
        Path::new("/tmp/tcr-run-test/tcr-ca.pem")
    }

    /// The running proxy's status as `cli::fetch_live_status` hands it over: parsed from the
    /// wire, so the `mitm` key is read the way a real answer is. `None` leaves the key out, as a
    /// server older than it does. The pool lists `codex_rows` Codex accounts.
    fn payload(
        mitm: Option<serde_json::Value>,
        codex_rows: usize,
        codex_upstream: Option<&str>,
    ) -> StatusPayload {
        let mut wire = crate::test_support::status_wire::codex_status_wire(mitm, codex_rows);
        if let Some(upstream) = codex_upstream {
            wire["codex_upstream"] = serde_json::json!(upstream);
        }
        serde_json::from_value(wire).expect("a status payload")
    }

    fn intercepting_chatgpt() -> serde_json::Value {
        serde_json::json!({
            "interceptedHosts": ["api.anthropic.com", "platform.claude.com", "chatgpt.com"],
            "leaf": "minted",
        })
    }

    fn mitm(value: serde_json::Value) -> MitmStatus {
        serde_json::from_value(value).expect("a mitm status")
    }

    /// Every code, with the sentence `tcr run --codex` prints for it, word for word: the codes
    /// are what a script or TcrBar keys on, and the reasons are what a person reads, on every
    /// surface alike. Only `no-proxy`'s reads as the doctor's verdict does.
    ///
    /// Mismatches are collected, so a regression names every row it breaks.
    #[test]
    fn every_code_carries_the_launchers_reason_verbatim() {
        let no_server = LiveStatusError::NoServer;
        let no_answer = LiveStatusError::NoAnswer("the server did not answer within 5s".into());
        let predates = payload(None, 1, None);
        let js = payload(
            Some(serde_json::json!({"interceptedHosts": ["api.anthropic.com"], "leaf": "js"})),
            1,
            None,
        );
        let off = payload(
            Some(serde_json::json!({"interceptedHosts": [], "leaf": "none"})),
            1,
            None,
        );
        let no_codex = payload(Some(intercepting_chatgpt()), 0, None);
        let pooling = payload(Some(intercepting_chatgpt()), 1, None);
        let missing_host = payload(
            Some(serde_json::json!({
                "interceptedHosts": ["api.anthropic.com", "platform.claude.com"],
                "leaf": "minted",
            })),
            1,
            None,
        );
        let quantum = payload(
            Some(serde_json::json!({"interceptedHosts": ["chatgpt.com"], "leaf": "quantum"})),
            1,
            None,
        );
        // (the read, whether the CA is readable, the Codex accounts, the code, the reason)
        type Row<'a> = (
            Result<&'a StatusPayload, &'a LiveStatusError>,
            bool,
            usize,
            &'a str,
            &'a str,
        );
        let rows: Vec<Row> = vec![
            (
                Err(&no_server),
                true,
                1,
                NO_PROXY,
                "no proxy answered on 127.0.0.1:3999",
            ),
            (
                Err(&no_answer),
                true,
                1,
                STATUS_UNREADABLE,
                "could not read the running proxy's status (the server did not answer within 5s)",
            ),
            (
                Ok(&predates),
                true,
                1,
                PREDATES,
                "the proxy on :3999 (build abc1234) predates Codex interception — restart it on \
                 this build at a quiet moment",
            ),
            // A leaf kind no build serves any more reads as any other unknown leaf.
            (
                Ok(&js),
                true,
                1,
                UNKNOWN_LEAF,
                "the proxy on :3999 (build abc1234) reports a leaf this build does not know \
                 (\"js\") — run tcr run --codex from the same build as the proxy",
            ),
            (
                Ok(&off),
                true,
                1,
                MITM_OFF,
                "the proxy on :3999 is running without MITM: its TLS material failed to load at \
                 boot — see its log",
            ),
            (
                Ok(&no_codex),
                true,
                0,
                NO_CODEX_ACCOUNT,
                "the proxy on :3999 holds no Codex account, so it blind-tunnels chatgpt.com and \
                 codex keeps its own login — add one with `tcr login --codex`",
            ),
            (
                Ok(&pooling),
                false,
                1,
                NO_CA,
                "no readable CA at /tmp/tcr-run-test/tcr-ca.pem",
            ),
            (
                Ok(&missing_host),
                true,
                1,
                LEAF_MISSING_HOST,
                "the proxy on :3999 serves a leaf it minted but does not intercept chatgpt.com \
                 (it intercepts [api.anthropic.com, platform.claude.com])",
            ),
            (
                Ok(&quantum),
                true,
                1,
                UNKNOWN_LEAF,
                "the proxy on :3999 (build abc1234) reports a leaf this build does not know \
                 (\"quantum\") — run tcr run --codex from the same build as the proxy",
            ),
        ];
        let mut wrong = Vec::new();
        for (read, ca_readable, codex_accounts, code, reason) in rows {
            let got = codex_route(read, PORT, ca(), ca_readable, codex_accounts);
            let want = CodexRoute {
                pooled: false,
                code: Some(code.to_string()),
                reason: Some(reason.to_string()),
                upstream: None,
            };
            if got != want {
                wrong.push(format!("{code}:\n  got  {got:?}\n  want {want:?}"));
            }
        }
        assert!(
            wrong.is_empty(),
            "{} row(s) wrong:\n{}",
            wrong.len(),
            wrong.join("\n")
        );
    }

    /// A pooled route says only that: no code, no reason, and on the wire nothing but
    /// `"pooled": true`, so a reader never finds a reason beside a route that works.
    #[test]
    fn pooled_carries_no_code_and_no_reason() {
        let pooling = payload(Some(intercepting_chatgpt()), 1, None);
        let route = codex_route(Ok(&pooling), PORT, ca(), true, 1);
        assert_eq!(
            route,
            CodexRoute {
                pooled: true,
                code: None,
                reason: None,
                upstream: None,
            }
        );
        assert_eq!(
            serde_json::to_string(&route).expect("serializes"),
            r#"{"pooled":true}"#
        );
    }

    /// A proxy whose Codex upstream is not the default says so on the route, pooled or not:
    /// requests that look pooled may be going somewhere a reader does not expect.
    #[test]
    fn a_non_default_upstream_is_reported() {
        let pooling = payload(Some(intercepting_chatgpt()), 1, Some("http://127.0.0.1:9"));
        let off = payload(
            Some(serde_json::json!({"interceptedHosts": [], "leaf": "none"})),
            1,
            Some("http://127.0.0.1:9"),
        );
        let pooled = codex_route(Ok(&pooling), PORT, ca(), true, 1);
        assert!(pooled.pooled, "{pooled:?}");
        assert_eq!(pooled.upstream.as_deref(), Some("http://127.0.0.1:9"));
        let not_pooled = codex_route(Ok(&off), PORT, ca(), true, 1);
        assert_eq!(not_pooled.code.as_deref(), Some(MITM_OFF));
        assert_eq!(not_pooled.upstream.as_deref(), Some("http://127.0.0.1:9"));
    }

    /// The serving process reads its own listener, so it is never absent, unreadable or older
    /// than itself: every leaf gives one of the codes a running proxy can have, and an unknown
    /// leaf names this very build. The default upstream is not named.
    #[test]
    fn the_in_process_route_never_says_no_proxy() {
        let default = crate::config::default_codex_upstream();
        let cases: Vec<(serde_json::Value, bool, usize, Option<&str>)> = vec![
            (intercepting_chatgpt(), true, 1, None),
            (intercepting_chatgpt(), true, 0, Some(NO_CODEX_ACCOUNT)),
            (intercepting_chatgpt(), false, 1, Some(NO_CA)),
            (
                serde_json::json!({"interceptedHosts": [], "leaf": "none"}),
                true,
                1,
                Some(MITM_OFF),
            ),
            (
                serde_json::json!({
                    "interceptedHosts": ["api.anthropic.com", "platform.claude.com"],
                    "leaf": "minted",
                }),
                true,
                1,
                Some(LEAF_MISSING_HOST),
            ),
            (
                serde_json::json!({"interceptedHosts": ["chatgpt.com"], "leaf": "quantum"}),
                true,
                1,
                Some(UNKNOWN_LEAF),
            ),
        ];
        for (listener, ca_readable, codex_accounts, want) in cases {
            let route = codex_route_in_process(
                &mitm(listener.clone()),
                PORT,
                ca(),
                ca_readable,
                codex_accounts,
                &default,
            );
            assert_eq!(route.code.as_deref(), want, "{listener}");
            assert_eq!(route.pooled, want.is_none(), "{listener}");
            assert!(
                ![NO_PROXY, STATUS_UNREADABLE, PREDATES]
                    .contains(&route.code.as_deref().unwrap_or("")),
                "{route:?}"
            );
            assert_eq!(route.upstream, None, "the default upstream is not named");
            if want == Some(UNKNOWN_LEAF) {
                let reason = route.reason.expect("a reason");
                assert!(
                    reason.contains(&format!(
                        "(build {})",
                        crate::build_info::BuildInfo::current().sha
                    )),
                    "{reason}"
                );
            }
        }
        let elsewhere = codex_route_in_process(
            &mitm(intercepting_chatgpt()),
            PORT,
            ca(),
            true,
            1,
            "http://127.0.0.1:9",
        );
        assert_eq!(elsewhere.upstream.as_deref(), Some("http://127.0.0.1:9"));
    }
}
