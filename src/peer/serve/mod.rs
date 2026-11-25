//! The SERVE stream: someone else's account serves my request.
//!
//! Both halves live in this one file on purpose. The borrower's refusal and the
//! lender's backstop are THE SAME refusal, and a copy of a seven-element
//! security list in two files is a fact in two places that will drift; keeping
//! them in one module means a coder editing either reads both.
//!
//! # The disclosure this stream IS
//!
//! The lender sees the borrower's full request and response plaintext, prompts
//! included, because substituting a Bearer on ciphertext is not possible. That
//! is not a leak to be mitigated, it is what borrowing an account means. It
//! needs an explicit act on BOTH machines (`inspect` on the lender, `disclose`
//! on the borrower, both default false), so an unconfigured mesh is blind-only.
//!
//! Through a forwarder, a SERVE is a FRESH end-to-end Noise session inside a
//! TUNNEL: the forwarder holds ciphertext for a session it has no key to, and a
//! forwarder that lies about which node it reached fails the inner handshake
//! against the pinned static.
//!
//! # What the BORROWER does unconditionally, before any grant is consulted
//!
//! 1. **Strips its own credentials**, `authorization`, `x-api-key`,
//!    `proxy-authorization`, `cookie`, plus every hop-by-hop request header,
//!    from every frame before the frame is written. The hop-by-hop half is
//!    `src/proxy.rs`'s own predicate, never a second list here.
//! 2. **Refuses to open a SERVE at all** for the client-credential paths, and
//!    falls through to its own local path instead.
//!
//! Both are on the borrower, not the lender, and the difference is the whole
//! point: a lender-side refusal happens after the bytes are already in the
//! lender's process, its memory and its request log. The lender keeps the
//! identical check as a backstop against an older or malicious borrower, and
//! that is all it is.
//!
//! The scrub cannot be inherited from the existing path, and this is measured
//! rather than assumed: `build_upstream_headers` is the only code in this tree
//! that removes a client's own `authorization` and substitutes a pooled token,
//! its definition is `src/proxy.rs:3455`, its SINGLE call site is `:2526`, and
//! the seam a SERVE is built at is `:2311`, 215 lines upstream. Nothing
//! between them would stop a client credential going out to another host.
//!
//! # The seventh path is the one a coder who reads the array will miss
//!
//! Six prefixes live in `CLIENT_CREDENTIAL_PREFIXES` (`src/proxy.rs:820-827`)
//! and the seventh is a SEPARATE constant, `CLIENT_TOKEN_REFRESH_PATH`
//! (`:839`), `/v1/oauth/token`, whose own doc records the live defect that
//! omitting it caused: an exact compare let the trailing-slash spelling fall
//! through to the pooled path, putting our Bearer on a client's token
//! exchange. That path targets `platform.claude.com`, which a blind tunnel
//! carries for free and a SERVE must never touch, and it is matched with
//! `path_is_under` (the whole path or that path plus `/`, never a longer
//! identifier) rather than `starts_with`.
//!
//! Both constants went `pub(crate)` in `src/proxy.rs`, a visibility change in
//! `proxy.rs` and nowhere else, taken rather than the fallback (a peer-side
//! constant plus an element-for-element contract test), because one list with
//! two readers cannot drift and two lists with a test can. [`path_is_under`]
//! went `pub(crate)` with them, since a peer-side re-spelling of the matcher
//! would reintroduce exactly the `starts_with` defect the matcher exists to
//! fix. Never a silent copy.
//!
//! # What the LENDER forwards, which is an ALLOWLIST and not a filter
//!
//! The lender re-sends the relayed request through its own proxy, and it
//! once replayed **every** header the borrower supplied onto that
//! request. That is a hole with a measured shape rather than a theoretical one:
//! `build_upstream_headers` (`src/proxy.rs:3502`) removes `authorization`,
//! `x-api-key`, `accept-encoding` and the hop-by-hop names, and `cookie` is
//! none of those, so a borrower-supplied session cookie went out to Anthropic
//! on the lender's own TLS session, as would any header a borrower invented.
//!
//! So the lender forwards [`LENDER_FORWARDED_HEADERS`](crate::peer::serve::LENDER_FORWARDED_HEADERS) and nothing else. An
//! allowlist rather than a longer denylist, because the two fail in opposite
//! directions: a name nobody thought of is dropped by the first and forwarded
//! by the second, and the set of names a borrower can invent is not
//! enumerable.
//!
//! This is the house pattern rather than a new idea, and the precedent is the
//! closest relative there is: `build_raw_relay_headers`
//! (`src/proxy.rs:3554`) carries `content-type`, `accept`, `user-agent` "and
//! nothing else" for the one other request in this tree built out of a client's
//! headers for a host whose credential is not the client's. This list is that
//! one plus `anthropic-version` and `anthropic-beta`, which a `/v1/messages`
//! relay needs and a token exchange does not.
//!
//! # The lender's per-org bucket, which is not a new mechanism
//!
//! A borrowed request counts against the LENDER's GCRA bucket for free, because
//! [`handle_serve_on`](crate::peer::serve::handle_serve_on) sends it through the lender's own proxy and therefore
//! through the lender's own picker and its own
//! `Manager::throttle_send` call (`src/proxy.rs:2593`), which keys the
//! per-organization bucket through `Manager::throttle_bucket_key`
//! (`src/manager/throttle.rs:98`) and `bucket_key_for`
//! (`src/manager/throttle.rs:38`) off the SERVING account's `org_uuid`, the
//! lender's. There is no second bucket, no peer-keyed bucket and nothing to add:
//! one sender per org is preserved by construction, and the way to break it
//! would be to serve a relayed request anywhere other than through the lender's
//! own path. `tests/peer_lease.rs` measures it against a tightened per-org
//! bucket rather than asserting it in prose.

mod borrow;
pub mod fallback;
mod frame;
mod hello;
mod lender;
mod policy;
mod provider;

pub use borrow::{Borrowed, open_serve, open_serve_within};
pub use frame::{
    MAX_BODY_BYTES, MAX_RELAYED_BODY_BYTES, SERVE_FLOW, ServeAck, ServeReply, ServeRequest,
    serve_request_from,
};
pub use hello::{NodeFacts, hello_for_peer, say_hello};
pub use lender::{
    LeaseServing, NoFleetUtilization, SERVED_LINE, handle_serve_on, utilization_rise,
};
pub use policy::{
    CLIENT_CREDENTIAL_HEADERS, LENDER_FORWARDED_HEADERS, SERVE_METHOD, lender_forwards_header,
    refused_path_kind, relay_path_is_routable, scrub_client_credentials, serve_is_allowed_for_path,
};
pub use provider::{PeerLeaseProvider, serve_on_handed_bearer};

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue};
    use tcr_peer_wire::{PROTO_VERSION, PeerId, Window};

    use super::lender::{RelaySlot, serve_on_own_account};
    use super::*;
    use crate::peer::lease::{Ledger, ScopeRestriction};
    use tcr_peer_wire::{Lease, LeaseUnit};

    /// The request key is a credential for this proxy alone, so the borrower's scrub removes
    /// it before a frame leaves for another Mac. Watch it fail by dropping it from
    /// `CLIENT_CREDENTIAL_HEADERS`.
    #[test]
    fn the_scrub_removes_the_request_key() {
        let mut headers = HeaderMap::new();
        headers.insert(
            crate::request_key::HEADER,
            HeaderValue::from_static("not-a-real-request-key"),
        );
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        assert_eq!(scrub_client_credentials(&mut headers), 1);
        assert!(headers.get(crate::request_key::HEADER).is_none());
        assert!(headers.get("content-type").is_some());
    }

    /// The head of the one request `serve_on_own_account` sends to a stand-in for
    /// this Mac's own proxy, which answers `200 {}`.
    async fn own_proxy_request_head(request_key: Option<&str>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the stand-in proxy");
        let base = format!("http://{}", listener.local_addr().expect("its address"));
        let accepted = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("one request");
            let mut head = Vec::new();
            let mut chunk = [0_u8; 4096];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                let read = socket.read(&mut chunk).await.expect("read the head");
                assert!(read > 0, "the request ended before its head did");
                head.extend_from_slice(&chunk[..read]);
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}")
                .await
                .expect("answer");
            String::from_utf8_lossy_owned(head)
        });
        let request = ServeRequest {
            lease_id: 1,
            request_id: 2,
            method: "POST".to_string(),
            path: "/v1/messages".to_string(),
            query: None,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body_bytes: 2,
            proto: PROTO_VERSION,
            flow: SERVE_FLOW,
        };
        let served = serve_on_own_account(
            &base,
            &request,
            b"{}".to_vec(),
            &ScopeRestriction::Unrestricted,
            request_key,
        )
        .await
        .expect("served");
        assert_eq!(served.status, 200);
        accepted.await.expect("the stand-in finished")
    }

    /// (h): the lender's relay into its OWN proxy carries that proxy's request
    /// key on `x-tcr-key`, so it is not counted as an unkeyed caller; a proxy
    /// that holds no key gets no header.
    #[tokio::test]
    async fn the_lenders_own_proxy_request_carries_the_request_key() {
        let key = "not-a-real-request-key";
        let head = own_proxy_request_head(Some(key)).await.to_ascii_lowercase();
        assert!(
            head.contains(&format!("\r\n{}: {key}\r\n", crate::request_key::HEADER)),
            "{head}"
        );
        let head = own_proxy_request_head(None).await.to_ascii_lowercase();
        assert!(
            head.starts_with("post /v1/messages "),
            "control: the stand-in read the relay's head: {head}"
        );
        assert!(!head.contains(crate::request_key::HEADER), "{head}");
    }

    /// A live lease this Mac has granted, with room for one relay.
    fn lent(ledger: &mut Ledger, lease_id: u128, grantee: PeerId) {
        let now = crate::now_ms();
        ledger.record_scoped(
            Lease {
                lease_id,
                window: Window::SevenDay,
                unit: LeaseUnit::Fraction(0.50),
                granted_at_ms: now,
                expires_at_ms: now + 300_000,
                spent: 0.0,
                max_inflight: 1,
                until: None,
            },
            grantee,
            tcr_peer_wire::LendScope::All,
        );
        ledger.note_owner_headroom(Window::SevenDay, 0.30);
    }

    /// **The in-flight slot comes back on every way out, including the two that
    /// used to leak it.**
    ///
    /// `enter_relay` takes a slot and it used to be given back by three
    /// hand-placed calls, with the `?` on the ack write sitting between two of
    /// them: a borrower that hung up while the ack was being written, or a
    /// poisoned ledger lock on either later release, left the slot held and the
    /// lease answered `InFlightFull` for the rest of its TTL.
    ///
    /// Both legs, because the poisoned one is the half a guard could easily get
    /// wrong: a `lock()` that returns `Err` and is quietly ignored leaks
    /// exactly what this replaces.
    ///
    /// Watched red: delete the `impl Drop for RelaySlot` body and the first
    /// assertion reads 1; make the poisoned arm return instead of calling
    /// `into_inner` and the second reads 1.
    #[test]
    fn a_relay_slot_is_given_back_when_its_guard_goes_out_of_scope() {
        let grantee = PeerId([11_u8; 32]);
        let lease_id = 0x5107_u128;
        let ledger = std::sync::Mutex::new(Ledger::new());
        {
            let mut held = ledger.lock().expect("ledger lock");
            lent(&mut held, lease_id, grantee);
            held.enter_relay(lease_id, &grantee, 1, crate::now_ms())
                .expect("the first relay is admitted");
            assert_eq!(held.inflight(lease_id), 1, "the slot is taken");
        }
        {
            let _slot = RelaySlot {
                ledger: &ledger,
                lease_id,
                returned: false,
            };
        }
        assert_eq!(
            ledger.lock().expect("ledger lock").inflight(lease_id),
            0,
            "a guard that goes out of scope gives the slot back, whatever path took it there"
        );

        // AND THROUGH A POISONED LOCK. The ledger is the only thing that can
        // put the slot back, so a guard that gives up on a poisoned lock leaks
        // exactly what it exists to stop.
        let ledger = std::sync::Arc::new(std::sync::Mutex::new(Ledger::new()));
        {
            let mut held = ledger.lock().expect("ledger lock");
            lent(&mut held, lease_id, grantee);
            held.enter_relay(lease_id, &grantee, 1, crate::now_ms())
                .expect("the first relay is admitted");
        }
        let poisoner = std::sync::Arc::clone(&ledger);
        let panicked = std::thread::spawn(move || {
            let _held = poisoner.lock().expect("ledger lock");
            panic!("a thread that dies holding the ledger");
        })
        .join();
        assert!(panicked.is_err(), "the fixture really did poison the lock");
        assert!(
            ledger.lock().is_err(),
            "positive control: the lock reads poisoned, or the leg below measures nothing"
        );
        {
            let _slot = RelaySlot {
                ledger: &ledger,
                lease_id,
                returned: false,
            };
        }
        let held = match ledger.lock() {
            Ok(held) => held,
            Err(poisoned) => poisoned.into_inner(),
        };
        assert_eq!(
            held.inflight(lease_id),
            0,
            "a poisoned ledger lock may not strand the lease's in-flight count for its whole \
             life"
        );
    }

    /// A slot the caller already gave back is not given back twice.
    ///
    /// The debit path releases under the same lock it debits on, so nothing
    /// between the two can move the lease. `leave_relay` saturates at zero, so
    /// a double release would be silent rather than loud, which is why this is
    /// asserted with a SECOND relay in flight: the second slot is what a double
    /// release would take away.
    ///
    /// Watched red: delete the `returned` early return from `Drop`.
    #[test]
    fn a_slot_the_caller_returned_is_not_returned_again() {
        let grantee = PeerId([12_u8; 32]);
        let lease_id = 0x5108_u128;
        let ledger = std::sync::Mutex::new(Ledger::new());
        {
            let mut held = ledger.lock().expect("ledger lock");
            let now = crate::now_ms();
            held.record_scoped(
                Lease {
                    lease_id,
                    window: Window::SevenDay,
                    unit: LeaseUnit::Fraction(0.50),
                    granted_at_ms: now,
                    expires_at_ms: now + 300_000,
                    spent: 0.0,
                    max_inflight: 4,
                    until: None,
                },
                grantee,
                tcr_peer_wire::LendScope::All,
            );
            held.note_owner_headroom(Window::SevenDay, 0.30);
            held.enter_relay(lease_id, &grantee, 1, now)
                .expect("the first relay is admitted");
            held.enter_relay(lease_id, &grantee, 2, now)
                .expect("the second relay is admitted");
            // The first request's own release, where the debit path writes it.
            held.leave_relay(lease_id);
        }
        {
            let _slot = RelaySlot {
                ledger: &ledger,
                lease_id,
                returned: true,
            };
        }
        assert_eq!(
            ledger.lock().expect("ledger lock").inflight(lease_id),
            1,
            "the second relay is still in flight: a guard must not give back a slot its \
             caller already returned"
        );
    }

    /// A refused Codex path is logged as the Codex refusal, never as a
    /// credential one, at both sites that log it (the lender's backstop in
    /// `handle_serve_on` and the borrower's `PeerLeaseProvider::try_serve`);
    /// every other refused path keeps the credential wording.
    ///
    /// Watched red: drop the Codex arm from `refused_path_kind` and the Codex
    /// path is described as a client-credential path.
    #[test]
    fn refused_path_kind_names_the_codex_refusal_for_a_codex_path() {
        let codex = crate::base::provider::CODEX_RESPONSES_PATH;
        assert!(!serve_is_allowed_for_path(codex), "precondition: refused");
        let kind = refused_path_kind(codex);
        assert!(
            kind.contains("Codex") && !kind.contains("client-credential"),
            "{kind}"
        );

        let credential = "/v1/sessions";
        assert!(
            !serve_is_allowed_for_path(credential),
            "precondition: refused"
        );
        assert_eq!(
            refused_path_kind(credential),
            "a client-credential or local-control path"
        );
    }

    /// A path refused FIRST for being unroutable is logged as that refusal, at
    /// both sites, in the order `serve_is_allowed_for_path` asks: never as the
    /// credential refusal, and never as the Codex one even under the Codex
    /// prefix. The three are a dot-segment escape onto this Mac's own control
    /// surface, an authority smuggled in as userinfo, and a Codex-prefixed path
    /// that is unroutable too.
    ///
    /// Watched red: drop the unroutable arm from `refused_path_kind` and the
    /// dot-segment escape is described as a client-credential path.
    #[test]
    fn refused_path_kind_names_the_unroutable_refusal_before_any_other() {
        for path in [
            "/x/../_tcr/accounts/control",
            "@169.254.169.254/latest/meta-data",
            "/backend-api/codex/../../_tcr/accounts/control",
        ] {
            assert!(
                !relay_path_is_routable(path),
                "precondition: {path} is unroutable"
            );
            assert!(
                !serve_is_allowed_for_path(path),
                "precondition: {path} is refused"
            );
            assert_eq!(
                refused_path_kind(path),
                "an unroutable path (one the upstream URL parser would read differently than \
                 it is spelled)",
                "{path}"
            );
        }
    }
}
