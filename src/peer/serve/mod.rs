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
//! So the lender forwards [`LENDER_FORWARDED_HEADERS`] and nothing else. An
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
//! [`handle_serve_on`] sends it through the lender's own proxy and therefore
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

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;

use crate::peer::config::PeerStore;

mod borrow;
mod dial;
mod frame;
mod hello;
mod lender;
mod policy;

pub use borrow::{Borrowed, open_serve, open_serve_within};
pub use dial::{
    PeerStream, PeerTransport, Reached, dial_order, dial_peer, dial_peer_reaching,
    dial_peer_reaching_within, dial_peer_with_endpoint, dial_peer_within, forwarders_for,
    has_a_way_back,
};
pub use frame::{
    MAX_BODY_BYTES, MAX_RELAYED_BODY_BYTES, SERVE_FLOW, ServeAck, ServeReply, ServeRequest,
    recv_control, send_control, serve_request_from,
};
pub use hello::say_hello;
pub use lender::{
    NoFleetUtilization, SERVED_LINE, ScopeRestriction, WindowUtilization, handle_serve_on,
    utilization_rise,
};
pub use policy::{
    CLIENT_CREDENTIAL_HEADERS, LENDER_FORWARDED_HEADERS, SERVE_METHOD, lender_forwards_header,
    refused_path_kind, relay_path_is_routable, scrub_client_credentials, serve_is_allowed_for_path,
};

/// Open one SERVE to a lender and return its answer.
///
/// Refuses before opening anything when [`serve_is_allowed_for_path`] says no,
/// and scrubs before writing the first frame. The lease is the borrower's
/// cached copy; the lender's ledger is what actually decides.
///
/// `None` is "not me" in every case a lease could not be spent, no address that
/// answers, `disclose` not granted here, a refusal frame, because the caller's
/// next rung is the honest 429 this proxy already has and that is a better
/// answer than anything invented here. A protocol failure mid-stream IS an
/// error: it is not the same fact as "this lender said no".
///
/// The node key comes from the peers file's own directory, the way
/// `tcr peer id` resolves it (`src/main.rs:868`), so a test that points
/// `--peers` at a temp dir points this at the same one.
///
/// # `headers` is the borrower's own, and it is scrubbed here
///
/// [`Ask`] carries none today, and `crate::fallback`'s module doc says why: the
/// only code that strips a client credential sits 215 lines downstream of the
/// seam an [`Ask`] is built at. So the production caller hands in an empty map,
/// and this parameter exists for the two cases an empty map cannot serve, the
/// day something at the seam does hold headers, and a test that puts a
/// credential in scope so [`scrub_client_credentials`]'s removal is a
/// MEASUREMENT rather than an absence over an empty map. It was
/// `&HeaderMap::new()` inline, which made the scrub on this path untestable and
/// therefore unwatched.
/// How long the whole borrow may take before the client is released: the
/// connect, the handshake, both frames out, the lender's reply and its body.
///
/// **One deadline over the whole exchange, not one per hop.** The review's
/// MEDIUM was that this path had none anywhere, a bare `TcpStream::connect`,
/// an unbounded `dial_handshake` and an unbounded wait for the reply, on the
/// answer path of a live client's request, while the listener's own half
/// ([`crate::peer::listener::HANDSHAKE_TIMEOUT`]) and the blind-egress carry
/// ([`crate::peer::egress::CARRY_SETUP_TIMEOUT`]) each carry one. Per-hop
/// deadlines would sum to a bound no caller can state; the client is waiting on
/// the total, so the total is what is bounded.
///
/// Ten seconds, and deliberately LONGER than the carry's five: a carry runs
/// after the direct path has already failed, while a borrow is this request's
/// first and only attempt at an answer, and a borrow that gave up before a
/// sleeping laptop's radio woke would spend a lease nobody could use.
///
/// # The operator's setting, and this constant's new job
///
/// [`crate::peer::config::PeerFile::borrow_timeout_ms`] is now the value
/// [`open_serve`] reads per call, off the [`PeerStore`] it is already handed,
/// so a lowered timeout takes effect without a restart, exactly like `via`.
/// This constant is what a fresh install starts at:
/// `PeerFile::borrow_timeout_ms`'s own serde default mirrors it, so a peers
/// file with no opinion behaves exactly as it did before that field existed.
pub const BORROW_TIMEOUT: Duration = Duration::from_millis(10_000);

/// The peers file's own directory, which is also the node-key directory, the
/// same resolution `tcr peer id` uses, so one `--peers` argument points the
/// whole peer surface at a temp dir.
pub(crate) fn node_key_dir(store: &PeerStore) -> PathBuf {
    store.path().parent().map_or_else(
        crate::peer::id::default_config_dir,
        std::path::Path::to_path_buf,
    )
}

/// The runtime-state file that belongs with `peers_path`: the pairing window,
/// the knock queue, the mutes and the bans.
///
/// The DEFAULT peers file lives in the config directory and its state file in
/// the cache directory, a split [`crate::peer::state::default_path`] already
/// owns, so the default is never derived here. Only an explicit `--peers` (or a
/// `--config` that resolves a peers file beside it) moves the state file with
/// it, which is what makes one argument select a whole profile and what stops a
/// test writing the operator's real `peer-state.json`.
///
/// # There is a second copy of this, and it is not deleted
///
/// `src/main.rs`'s private `peer_state_path` is the same six lines, in the part
/// of that file the `ls`, `pair` and `pending` verbs read rather than
/// `lend`/`share`/`allow`. Nothing makes it call this one yet, so the two
/// agree line for line,
/// and this doc is where a reader learns there are two.
pub fn peer_state_path(peers_path: &std::path::Path) -> PathBuf {
    if peers_path == crate::peer::config::default_path() {
        return crate::peer::state::default_path();
    }
    match peers_path.parent() {
        Some(parent) => parent.join("peer-state.json"),
        None => crate::peer::state::default_path(),
    }
}

/// 128 opaque bits for a request id, from the same source
/// [`crate::peer::lease::Ledger::grant`] mints a lease id with.
fn random_u128() -> Result<u128> {
    crate::peer::lease::random_id()
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::time::Instant;

    use axum::http::{HeaderMap, HeaderValue};
    use tcr_peer_wire::{PROTO_VERSION, PeerId, Window};

    use super::dial::{CONNECTION_ATTEMPT_DELAY, clear_cooldowns_for_fetch, race_candidates};
    use super::lender::{RelaySlot, serve_on_own_account};
    use super::*;
    use crate::peer::config::{Endpoint, Locator, PeerRow};
    use crate::peer::lease::Ledger;
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

    /// **The stagger, through the private race itself, with no sockets at
    /// all.**
    ///
    /// Two scripted candidates: the first takes 600 ms to answer, the second
    /// only 5 ms. The second is started [`CONNECTION_ATTEMPT_DELAY`] after
    /// the first, so at 255 ms it is already ahead of the first candidate's
    /// own 600 ms clock, and must win. A stagger short enough to let the
    /// slow candidate finish before the fast one even starts (anything at or
    /// above 595 ms) would prove nothing but the order of two futures; 600 ms
    /// leaves headroom above the real 250 ms delay while still failing loudly
    /// if the delay were ever raised past it.
    ///
    /// Watched red: raise [`CONNECTION_ATTEMPT_DELAY`] to 10 seconds. The
    /// second candidate then starts long after the first's own 600 ms clock
    /// has already answered, so the first (slow) candidate wins instead:
    ///
    /// ```text
    /// assertion `left == right` failed: the second candidate started
    /// CONNECTION_ATTEMPT_DELAY after the first and still answers first: it
    /// must win
    ///   left: Some(600ms)
    ///  right: Some(5ms)
    /// ```
    #[tokio::test]
    async fn the_race_starts_its_next_candidate_a_stagger_after_the_previous_one() {
        let slow = Duration::from_millis(600);
        let fast = Duration::from_millis(5);
        let candidates = vec![slow, fast];

        let started = Instant::now();
        let outcome = race_candidates(
            candidates,
            CONNECTION_ATTEMPT_DELAY,
            |delay: Duration| async move {
                tokio::time::sleep(delay).await;
                Some(delay)
            },
        )
        .await;
        let elapsed = started.elapsed();

        assert_eq!(
            outcome.won.map(|(_, value)| value),
            Some(fast),
            "the second candidate started CONNECTION_ATTEMPT_DELAY after the first and still \
             answers first: it must win"
        );
        assert!(
            outcome.failed.is_empty(),
            "the slow candidate was still running when the fast one won; it was cancelled, \
             not refused, so it must not be in the failed list: {:?}",
            outcome.failed
        );
        assert!(
            elapsed < slow,
            "the race must not wait for the cancelled candidate's own clock: took {elapsed:?}"
        );
    }

    /// **`clear_cooldowns_for_fetch` clears exactly the locators it is
    /// handed, and no other.**
    ///
    /// Two locators on one peer, both cooling. Only one is passed as
    /// `written`. Read back through [`crate::peer::probe::dial_order`], the
    /// same public function the dead-drop retry calls: if the cleared
    /// locator alone comes back, the clear was scoped; if the untouched
    /// locator ALSO comes back, either nothing was cleared (so
    /// `dial_order`'s own "every path is cooling, try them all anyway"
    /// fallback fired) or the clear was a blanket one.
    ///
    /// Watched red: comment out the `clear_cooldown` call inside
    /// `clear_cooldowns_for_fetch`. The untouched locator's cooldown then
    /// never lifts, both locators stay cooling, `dial_order`'s fallback
    /// returns BOTH, and this test's `assert!(!order.iter().any(...))` for
    /// the untouched locator fails: `left: true` where the assertion
    /// expects it absent.
    #[test]
    fn clear_cooldowns_for_fetch_only_clears_the_locators_it_is_given() {
        let peer = PeerId([0x60; 32]);
        let cleared_addr: SocketAddr = "127.0.0.1:1".parse().expect("a socket address");
        let untouched_addr: SocketAddr = "127.0.0.1:2".parse().expect("a socket address");
        let cleared_locator = Locator::Direct { addr: cleared_addr };
        let untouched_locator = Locator::Direct {
            addr: untouched_addr,
        };

        let until_ms = crate::now_ms() + 60_000;
        crate::peer::probe::with_table(|table| {
            table.cool_down(peer, cleared_locator, until_ms);
            table.cool_down(peer, untouched_locator, until_ms);
        })
        .expect("the process-local path table is not poisoned");

        let written = vec![Endpoint::direct(
            cleared_addr,
            crate::now_ms(),
            crate::peer::config::EndpointSource::Drop,
        )];
        clear_cooldowns_for_fetch(peer, &written);

        let mut row = PeerRow {
            node: peer,
            label: "clear-cooldowns-for-fetch-test".to_string(),
            endpoints: Vec::new(),
            added_at: 0,
            allow: crate::peer::config::Allow::default(),
            lend: Vec::new(),
            rendezvous_secret: None,
            sees_us_at: None,
        };
        row.observe_endpoint(Endpoint::direct(
            cleared_addr,
            1_000,
            crate::peer::config::EndpointSource::Drop,
        ));
        row.observe_endpoint(Endpoint::direct(
            untouched_addr,
            1_000,
            crate::peer::config::EndpointSource::Drop,
        ));

        let order = dial_order(&row);
        assert!(
            order
                .iter()
                .any(|endpoint| endpoint.locator == cleared_locator),
            "the locator the fetch wrote must be dialable again: {order:?}"
        );
        assert!(
            !order
                .iter()
                .any(|endpoint| endpoint.locator == untouched_locator),
            "a locator the fetch did NOT write must stay excluded, not swept in by a blanket \
             clear or by dial_order's own cooling-fallback: {order:?}"
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
        let codex = crate::provider::CODEX_RESPONSES_PATH;
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
