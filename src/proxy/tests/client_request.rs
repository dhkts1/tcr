//! What tcr reads off a client request: the headers it drops, the request and session keys it derives, the API-key gate and the body cap.

use super::*;

/// **This Mac's own routing marker never leaves it.**
///
/// `RELAYED_HEADER_NAME` is written onto a relayed request as it goes into
/// this Mac's own proxy, so the dry-fleet terminal knows the request has
/// already crossed one Mac. It describes the mesh, not the request, and
/// upstream has no use for it, so the upstream header build drops it the
/// way it drops the client's own credential.
///
/// The control is an ordinary header in the same map: a strip that took
/// everything would pass this test with its subject deleted.
///
/// Watched red: delete the `RELAYED_HEADER_NAME` arm from
/// `build_upstream_headers` and the first assertion finds it upstream.
#[test]
fn the_relay_marker_is_not_carried_upstream() {
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static(RELAYED_HEADER_NAME),
        HeaderValue::from_static("1"),
    );
    headers.insert(
        HeaderName::from_static("anthropic-version"),
        HeaderValue::from_static("2023-06-01"),
    );
    let upstream = build_upstream_headers(&headers, "not-a-real-token");
    assert!(
        !upstream.contains_key(RELAYED_HEADER_NAME),
        "a header that describes this mesh may not be sent to the API"
    );
    assert_eq!(
        upstream
            .get("anthropic-version")
            .and_then(|value| value.to_str().ok()),
        Some("2023-06-01"),
        "and the client's own headers still cross: this strip is one name, not a filter"
    );
}

/// The request key is this proxy's credential: it reaches neither the pooled upstream
/// nor a passthrough relay, whatever value it carries. Watch it fail by dropping the
/// `crate::request_key::HEADER` arm from either builder.
#[test]
fn the_request_key_header_is_carried_neither_upstream_nor_through() {
    let mut headers = HeaderMap::new();
    headers.insert(
        crate::request_key::HEADER,
        HeaderValue::from_static("any-value-at-all"),
    );
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    let upstream = build_upstream_headers(&headers, "not-a-real-token");
    let passthrough = build_passthrough_headers(
        &headers,
        ProxySecrets {
            api_key: None,
            request_key: Some(REQUEST_KEY),
        },
    );
    for (out, which) in [(&upstream, "upstream"), (&passthrough, "passthrough")] {
        assert!(
            !out.contains_key(crate::request_key::HEADER),
            "{which} carried the request key"
        );
        assert!(out.contains_key("anthropic-version"), "{which}");
    }
}

/// A passthrough relay keeps the client's own `x-api-key` but drops one that IS the
/// request key, as it already dropped one that is `proxy.apiKey`.
#[test]
fn passthrough_drops_an_x_api_key_equal_to_the_request_key() {
    let secrets = ProxySecrets {
        api_key: Some("sk-proxy-secret"),
        request_key: Some(REQUEST_KEY),
    };
    let carried = |value: &'static str| {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static(value));
        build_passthrough_headers(&headers, secrets).contains_key("x-api-key")
    };
    assert!(!carried(REQUEST_KEY), "the request key is ours");
    assert!(!carried("sk-proxy-secret"), "proxy.apiKey is ours");
    assert!(
        carried("sk-ant-client-own-key"),
        "the client's own key crosses"
    );
}

/// An `x-api-key` that is the request key names no client, so session affinity skips it
/// exactly as it skips `proxy.apiKey`: a remote client keys on the body's `user_id`, or
/// on nothing.
#[test]
fn stable_session_key_skips_the_request_key() {
    let secrets = ProxySecrets {
        api_key: None,
        request_key: Some(REQUEST_KEY),
    };
    let remote = |headers: &HeaderMap, body: &[u8]| {
        stable_session_key(headers, body, secrets, &Method::POST, "/v1/messages", false)
    };
    assert_eq!(remote(&headers_with_api_key(REQUEST_KEY), b"{}"), None);
    let body = br#"{"metadata":{"user_id":"user-123"}}"#;
    assert_eq!(
        remote(&headers_with_api_key(REQUEST_KEY), body),
        remote(&HeaderMap::new(), body),
    );
    assert!(
        remote(&headers_with_api_key("sk-team-alice"), b"{}").is_some(),
        "any other key is still an identity"
    );
}

/// One decision per request: a keyed tunnel's request is keyed without its headers, an
/// unkeyed tunnel's request was counted at its CONNECT, and only a plain request is read.
#[test]
fn request_keying_reads_the_tunnel_stamp_before_the_headers() {
    use crate::request_key::{RequestKeying, UaFamily};
    let mut curl = HeaderMap::new();
    curl.insert("user-agent", HeaderValue::from_static("curl/8.7.1"));
    let mut keyed = curl.clone();
    keyed.insert(
        crate::request_key::HEADER,
        HeaderValue::from_static(REQUEST_KEY),
    );

    let plain = axum::http::Extensions::new();
    let mut keyed_tunnel = axum::http::Extensions::new();
    keyed_tunnel.insert(InterceptedHost("api.anthropic.com".to_string()));
    keyed_tunnel.insert(TunnelKeyed);
    let mut unkeyed_tunnel = axum::http::Extensions::new();
    unkeyed_tunnel.insert(InterceptedHost("api.anthropic.com".to_string()));

    assert_eq!(
        request_keying(&plain, &curl, None),
        RequestKeying::NoServerKey
    );
    assert_eq!(
        request_keying(&plain, &curl, Some(REQUEST_KEY)),
        RequestKeying::Unkeyed(UaFamily::Curl)
    );
    assert_eq!(
        request_keying(&plain, &keyed, Some(REQUEST_KEY)),
        RequestKeying::Keyed
    );
    assert_eq!(
        request_keying(&keyed_tunnel, &curl, Some(REQUEST_KEY)),
        RequestKeying::Keyed
    );
    assert_eq!(
        request_keying(&unkeyed_tunnel, &curl, Some(REQUEST_KEY)),
        RequestKeying::CountedAtConnect
    );
}

/// **A peer's own spelling of a hop-by-hop header does not decide whether
/// it is stripped.**
///
/// Two callers of `is_response_skip` are fed arbitrary strings off a peer's
/// wire rather than `HeaderName`s this process built: the SERVE reply
/// rebuild in `peer::serve` and the hand arm in `peer::lease`. The match
/// was exact and lowercase, which held only because every lender in this
/// fleet writes `HeaderName::as_str()`. A peer that spelled one
/// `Content-Length` had it survive into its borrower's client and
/// mis-frame the body, which is the same defect the strip exists to
/// prevent, reintroduced by a peer choosing a capital letter.
///
/// The lowercase leg is here too, so a fix that answered `true` to
/// everything would not pass: `x-request-id` is a header a borrowed reply
/// legitimately carries through.
///
/// Watched red: put the `matches!` back and every capitalized spelling
/// below answers `false`.
#[test]
fn a_hop_by_hop_response_header_is_skipped_whatever_its_case() {
    for name in [
        "content-length",
        "Content-Length",
        "CONTENT-LENGTH",
        "Transfer-Encoding",
        "Connection",
        "Content-Encoding",
    ] {
        assert!(
            is_response_skip(name),
            "{name} frames the peer's connection, not the client's, so it may never \
                 reach the client whatever the peer capitalized"
        );
    }
    for name in ["x-request-id", "X-Request-Id", "content-type"] {
        assert!(
            !is_response_skip(name),
            "{name} is a header a relayed answer legitimately carries through"
        );
    }
}

/// The server login route carries the plan. This is a SEAM, not a
/// formality: `AddAccountRequest` only MIRRORS `config::Account`'s wire
/// shape, so a field added to `Account` alone is silently dropped here —
/// the body still deserializes, the route still answers 200, and the
/// account lands with no plan while the file route has one. Serializing a
/// real `Account` and deserializing it as the request is exactly the hop
/// `post_add_account` performs, so this fails the moment the two shapes
/// drift apart.
#[test]
fn add_account_request_carries_the_plan_fields() {
    let mut account = Account {
        access_token: "at-a".to_string(),
        refresh_token: None,
        expires_at: None,
        priority: None,
        organization_type: Some("claude_team".to_string()),
        rate_limit_tier: Some("default_raven".to_string()),
        seat_tier: Some("team_standard".to_string()),
        ..Account::fixture("alice@example.com")
    };
    let body = serde_json::to_string(&account).expect("serialize the account login sends");
    let parsed: AddAccountRequest =
        serde_json::from_str(&body).expect("the route must accept the body login sends");

    assert_eq!(parsed.organization_type.as_deref(), Some("claude_team"));
    assert_eq!(parsed.rate_limit_tier.as_deref(), Some("default_raven"));
    assert_eq!(parsed.seat_tier.as_deref(), Some("team_standard"));

    // And an older client that sends none of them still parses — the keys
    // are `#[serde(default)]`, so the route did not just become stricter.
    account.organization_type = None;
    account.rate_limit_tier = None;
    account.seat_tier = None;
    let older = serde_json::to_string(&account).expect("serialize a plan-less account");
    let parsed: AddAccountRequest =
        serde_json::from_str(&older).expect("a body with no plan keys must still parse");
    assert_eq!(parsed.organization_type, None);
}

/// The server login route carries the provider, for the plan fields' SEAM
/// reason above: a Codex import posts a serialized `Account` carrying
/// `"provider": "codex"`, and a request struct without the field drops it
/// while the route still answers 200, filing a Codex credential as an
/// Anthropic account. An Anthropic row serializes no `provider` key at all,
/// and still parses as Anthropic.
#[test]
fn add_account_request_carries_the_provider() {
    let mut account = Account {
        provider: Provider::Codex,
        account_uuid: Some("11111111-1111-4111-8111-111111111111".to_string()),
        access_token: "at-codex".to_string(),
        refresh_token: Some("rt-codex".to_string()),
        expires_at: None,
        priority: None,
        organization_type: Some("pro".to_string()),
        ..Account::fixture("alice@example.com/codex")
    };
    let body = serde_json::to_string(&account).expect("serialize the account an import sends");
    let parsed: AddAccountRequest =
        serde_json::from_str(&body).expect("the route must accept the body an import sends");
    assert_eq!(parsed.provider, Provider::Codex);

    account.provider = Provider::Anthropic;
    let body = serde_json::to_string(&account).expect("serialize an Anthropic account");
    assert!(!body.contains("provider"), "{body}");
    let parsed: AddAccountRequest =
        serde_json::from_str(&body).expect("a body with no provider key must still parse");
    assert_eq!(parsed.provider, Provider::Anthropic);
}

fn headers_with_api_key(value: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("x-api-key", HeaderValue::from_str(value).unwrap());
    h
}

/// The scope tier 3 requires: a loopback `POST /v1/messages`. Tiers 1/2
/// don't care about method/path/loopback, so tests exercising ONLY those
/// tiers call `stable_session_key` through this helper rather than
/// re-stating the tier-3 gate at every call site; tests of the gate itself
/// call `stable_session_key` directly with a different method/path/loopback.
/// [`super::stable_session_key`] on raw body bytes, parsed the way the handler parses
/// them (without the ttl rewrite, which these bodies never trigger), so the tier tests
/// below state their bodies as bytes.
fn stable_session_key(
    headers: &HeaderMap,
    body: &[u8],
    secrets: ProxySecrets<'_>,
    method: &Method,
    path: &str,
    client_is_loopback: bool,
) -> Option<(u64, SessionKind)> {
    let parsed =
        crate::records::request_body::RequestBody::parse(Bytes::copy_from_slice(body), false);
    super::stable_session_key(headers, &parsed, secrets, method, path, client_is_loopback)
}

fn messages_key(
    headers: &HeaderMap,
    body: &[u8],
    proxy_key: Option<&str>,
) -> Option<(u64, SessionKind)> {
    stable_session_key(
        headers,
        body,
        ProxySecrets {
            api_key: proxy_key,
            request_key: None,
        },
        &Method::POST,
        "/v1/messages",
        true,
    )
}

#[test]
fn stable_session_key_is_deterministic_for_api_key() {
    let h = headers_with_api_key("sk-team-alice");
    let a = messages_key(&h, b"{}", None);
    let b = messages_key(&h, b"{}", None);
    assert_eq!(a, b, "same x-api-key must survive a reconnect");
    assert!(a.is_some());
}

#[test]
fn stable_session_key_is_deterministic_for_user_id() {
    let body = br#"{"metadata":{"user_id":"user-123"},"messages":[]}"#;
    let h = HeaderMap::new();
    let a = messages_key(&h, body, None);
    let b = messages_key(&h, body, None);
    assert_eq!(a, b, "same user_id must survive a reconnect");
    assert!(a.is_some());
}

#[test]
fn stable_session_key_prefers_api_key_over_user_id_for_a_remote_client() {
    let body = br#"{"metadata":{"user_id":"user-123"}}"#;
    let remote = |h: &HeaderMap, b: &[u8]| {
        stable_session_key(
            h,
            b,
            ProxySecrets::NONE,
            &Method::POST,
            "/v1/messages",
            false,
        )
    };
    let with_key = remote(&headers_with_api_key("the-key"), body);
    let key_only = remote(&headers_with_api_key("the-key"), b"{}");
    assert_eq!(
        with_key, key_only,
        "a remote client's x-api-key is its identity and wins over user_id"
    );
}

/// The 2026-09-22 fleet collapse: every `tcr run` coder on the box exports
/// the same placeholder `ANTHROPIC_API_KEY`, so on loopback the header is
/// not an identity. The body's per-conversation `user_id` must key instead,
/// so two coders sharing one placeholder get two pins; a loopback client
/// that sends no `user_id` still keys on the header as before.
#[test]
fn stable_session_key_prefers_user_id_over_a_shared_key_on_loopback() {
    let h = headers_with_api_key("via-tcr");
    let coder_a = messages_key(&h, br#"{"metadata":{"user_id":"coder-a"}}"#, None);
    let coder_b = messages_key(&h, br#"{"metadata":{"user_id":"coder-b"}}"#, None);
    assert!(coder_a.is_some() && coder_b.is_some());
    assert_ne!(
        coder_a, coder_b,
        "two loopback clients sharing one placeholder key must not share a pin"
    );
    let via_uid = messages_key(
        &HeaderMap::new(),
        br#"{"metadata":{"user_id":"coder-a"}}"#,
        None,
    );
    assert_eq!(
        coder_a, via_uid,
        "on loopback the user_id is the discriminator, the header is ignored"
    );
    let key_only = messages_key(&h, b"{}", None);
    assert!(
        key_only.is_some(),
        "a loopback client with a key and no user_id still keys on the header"
    );
    assert_ne!(key_only, coder_a);
}

#[test]
fn stable_session_key_namespaces_key_vs_uid() {
    // An x-api-key "abc" and a user_id "abc" must NOT collide.
    let from_key = messages_key(&headers_with_api_key("abc"), b"{}", None);
    let from_uid = messages_key(
        &HeaderMap::new(),
        br#"{"metadata":{"user_id":"abc"}}"#,
        None,
    );
    assert_ne!(from_key, from_uid, "prefixes must isolate the two spaces");
}

#[test]
fn stable_session_key_none_without_identity() {
    // No x-api-key, no top-level metadata.user_id, no system/tools → None
    // even ON the endpoint and origin tier 3 is scoped to.
    assert_eq!(
        messages_key(&HeaderMap::new(), br#"{"messages":[]}"#, None),
        None
    );
}

#[test]
fn stable_session_key_ignores_nested_user_id() {
    // A user_id nested in message content is NOT top-level metadata.
    let body = br#"{"messages":[{"role":"user","content":{"metadata":{"user_id":"nested"}}}]}"#;
    assert_eq!(messages_key(&HeaderMap::new(), body, None), None);
}

#[test]
fn stable_session_key_distinguishes_different_api_keys() {
    let a = messages_key(&headers_with_api_key("key-a"), b"{}", None);
    let b = messages_key(&headers_with_api_key("key-b"), b"{}", None);
    assert_ne!(a, b, "distinct team keys → distinct accounts");
}

#[test]
fn stable_session_key_skips_shared_proxy_key() {
    // When the x-api-key IS the configured proxy secret, it is NOT a client
    // identity: fall through to user_id, else None. Otherwise every remote
    // client (all forced to send that one key) would collapse onto one
    // account, defeating rotation.
    let shared = "sk-proxy-secret";
    // No user_id → falls through to None (per-connection key at the caller).
    assert_eq!(
        messages_key(&headers_with_api_key(shared), b"{}", Some(shared)),
        None,
        "the shared proxy key must not be used as an affinity discriminator"
    );
    // With a body user_id, it falls through to that instead of the shared key.
    let body = br#"{"metadata":{"user_id":"user-123"}}"#;
    let via_shared = messages_key(&headers_with_api_key(shared), body, Some(shared));
    let via_uid = messages_key(&HeaderMap::new(), body, None);
    assert_eq!(
        via_shared, via_uid,
        "with the shared key skipped, the user_id is the discriminator"
    );
    assert!(via_shared.is_some());
    // A DIFFERENT (genuine team) key with the same proxy_key configured is
    // still used — only the exact shared secret is skipped.
    let team = messages_key(&headers_with_api_key("sk-team-alice"), b"{}", Some(shared));
    assert!(
        team.is_some(),
        "a distinct team key is a real identity and must still key"
    );
}

#[test]
fn stable_session_key_falls_back_to_prefix_hash_of_system_and_tools() {
    // No x-api-key, no metadata.user_id, but a system + tools prefix on an
    // in-scope (loopback POST /v1/messages) request — tier 3 pins on a hash
    // of that prefix instead of routing unpinned.
    let body = br#"{"system":"You are a helpful assistant.","tools":[{"name":"bash"}]}"#;
    let a = messages_key(&HeaderMap::new(), body, None);
    let b = messages_key(&HeaderMap::new(), body, None);
    assert_eq!(a, b, "same prefix must hash the same every time");
    assert_eq!(
        a.map(|(_, kind)| kind),
        Some(SessionKind::Prefix),
        "tier 3 must record SessionKind::Prefix, not Stable"
    );
}

#[test]
fn stable_session_key_prefix_hash_accepts_system_or_tools_alone() {
    // Either field alone is a cacheable prefix — both need not be present.
    let system_only = messages_key(&HeaderMap::new(), br#"{"system":"hi"}"#, None);
    let tools_only = messages_key(&HeaderMap::new(), br#"{"tools":[{"name":"x"}]}"#, None);
    assert!(system_only.is_some(), "system alone must pin");
    assert!(tools_only.is_some(), "tools alone must pin");
    assert_ne!(
        system_only, tools_only,
        "a system-only and a tools-only prefix are different prefixes"
    );
}

#[test]
fn stable_session_key_prefix_hash_distinguishes_different_prefixes() {
    let a = messages_key(&HeaderMap::new(), br#"{"system":"prompt A"}"#, None);
    let b = messages_key(&HeaderMap::new(), br#"{"system":"prompt B"}"#, None);
    assert_ne!(
        a, b,
        "distinct prefixes must spread across the fleet, not collide"
    );
}

#[test]
fn stable_session_key_prefix_hash_is_byte_exact_not_canonicalized() {
    // Same fields, different KEY ORDER inside the `system` value. Anthropic's
    // own cache is byte-exact, so these are two DIFFERENT cache entries —
    // canonicalizing (sorting keys) before hashing would merge them onto one
    // account for zero cache benefit and only concentrate load.
    let a = messages_key(&HeaderMap::new(), br#"{"system":{"a":1,"b":2}}"#, None);
    let b = messages_key(&HeaderMap::new(), br#"{"system":{"b":2,"a":1}}"#, None);
    assert_ne!(
        a, b,
        "raw bytes must be hashed verbatim — key order must not be canonicalized"
    );
}

#[test]
fn stable_session_key_prefix_hash_distinguishes_field_identity() {
    // The SAME raw text "ab", attached to a DIFFERENT field. A naive
    // implementation that concatenated system+tools into one string before
    // hashing (e.g. `format!("{system}{tools}")`) would treat
    // `{"system":"ab"}` (tools absent, so "" via unwrap_or) and
    // `{"tools":"ab"}` (system absent) as the identical string "ab" and hash
    // them the same. Hashing each field as its own `Option<&str>` must not.
    let system_ab = messages_key(&HeaderMap::new(), br#"{"system":"ab"}"#, None);
    let tools_ab = messages_key(&HeaderMap::new(), br#"{"tools":"ab"}"#, None);
    assert_ne!(
        system_ab, tools_ab,
        "system:\"ab\" and tools:\"ab\" must not collide"
    );
}

#[test]
fn stable_session_key_prefix_hash_resists_boundary_shift() {
    // Bare JSON NUMBERS, not quoted strings: a quoted string's own `"`
    // delimiters would accidentally break a naive concatenation apart, which
    // defeats the point of this test. Numbers have no such delimiter, so a
    // concatenating implementation genuinely collides here: `system:12` +
    // `tools:3` and `system:1` + `tools:23` both concatenate their raw text
    // to "123". `str`'s own `Hash` impl appends a sentinel byte after each
    // value specifically to prevent this; hashing system and tools as two
    // separate `.hash()` calls relies on it.
    let a = messages_key(&HeaderMap::new(), br#"{"system":12,"tools":3}"#, None);
    let b = messages_key(&HeaderMap::new(), br#"{"system":1,"tools":23}"#, None);
    assert_ne!(a, b, "a field boundary shift must not collide");
}

#[test]
fn stable_session_key_prefix_none_without_system_or_tools() {
    // Neither system nor tools → no cacheable prefix → stay unpinned even
    // in scope. This is the guard that stops every trivial anonymous
    // request from piling onto one account.
    assert_eq!(
        messages_key(&HeaderMap::new(), br#"{"messages":[]}"#, None),
        None
    );
}

#[test]
fn stable_session_key_prefix_is_the_last_resort_after_identity_tiers() {
    // x-api-key and metadata.user_id both outrank the prefix hash even when a
    // cacheable prefix is also present.
    let body = br#"{"system":"hi","metadata":{"user_id":"user-123"}}"#;
    let with_key = messages_key(&headers_with_api_key("the-key"), body, None).map(|(_, k)| k);
    assert_eq!(with_key, Some(SessionKind::Stable), "x-api-key still wins");

    let with_uid = messages_key(&HeaderMap::new(), body, None).map(|(_, k)| k);
    assert_eq!(
        with_uid,
        Some(SessionKind::Stable),
        "user_id still wins over the prefix hash"
    );
}

/// The cacheable-prefix body used by every tier-3 scope-gating test below —
/// no x-api-key, no `user_id`, so tier 3 is the only tier that could fire.
const PREFIX_ONLY_BODY: &[u8] = br#"{"system":"You are a helpful assistant."}"#;

#[test]
fn stable_session_key_prefix_requires_exact_v1_messages_path() {
    // In scope: exact match.
    assert!(
        stable_session_key(
            &HeaderMap::new(),
            PREFIX_ONLY_BODY,
            ProxySecrets::NONE,
            &Method::POST,
            "/v1/messages",
            true,
        )
        .is_some(),
        "an exact /v1/messages match must pin"
    );
    // Out of scope: a LONGER path must not match as a prefix — this is the
    // exact trap the scope gate exists to close. /v1/messages/count_tokens
    // never uses prompt caching (Anthropic's own docs), so pinning it would
    // concentrate load for zero cache benefit.
    assert_eq!(
        stable_session_key(
            &HeaderMap::new(),
            PREFIX_ONLY_BODY,
            ProxySecrets::NONE,
            &Method::POST,
            "/v1/messages/count_tokens",
            true,
        ),
        None,
        "/v1/messages/count_tokens must NOT prefix-match /v1/messages"
    );
    // Defense in depth: an unstripped query string must not match either,
    // even though the real call site (`handle`) always passes an
    // already-stripped path — this guards the match itself, not just the
    // caller's discipline.
    assert_eq!(
        stable_session_key(
            &HeaderMap::new(),
            PREFIX_ONLY_BODY,
            ProxySecrets::NONE,
            &Method::POST,
            "/v1/messages?beta=true",
            true,
        ),
        None,
        "a path carrying its query string must not match /v1/messages"
    );
}

#[test]
fn stable_session_key_prefix_requires_post() {
    assert_eq!(
        stable_session_key(
            &HeaderMap::new(),
            PREFIX_ONLY_BODY,
            ProxySecrets::NONE,
            &Method::GET,
            "/v1/messages",
            true,
        ),
        None,
        "a GET must not pin on the cacheable prefix"
    );
}

#[test]
fn stable_session_key_prefix_requires_loopback() {
    // Tier 1 already refuses to key on x-api-key when it equals the shared
    // proxy secret, precisely so remote clients sharing that secret don't
    // collapse onto one account (`stable_session_key_skips_shared_proxy_key`
    // above). Tier 3 has no secret to check, so without a loopback gate N
    // remote callers sharing one system prompt would collapse through the
    // same back door.
    assert_eq!(
        stable_session_key(
            &HeaderMap::new(),
            PREFIX_ONLY_BODY,
            ProxySecrets::NONE,
            &Method::POST,
            "/v1/messages",
            false,
        ),
        None,
        "a non-loopback caller must not pin on the cacheable prefix"
    );
}

#[test]
fn codex_session_id_header_keys_the_session() {
    let mut h = HeaderMap::new();
    h.insert(
        "session-id",
        HeaderValue::from_static("0f0f0f0f-aaaa-4bbb-8ccc-dddddddddddd"),
    );
    let body = b"\x28\xb5\x2f\xfd not json"; // a compressed body: the peek must never be needed
    let key = stable_session_key(
        &h,
        body,
        ProxySecrets::NONE,
        &Method::POST,
        "/backend-api/codex/responses",
        true,
    );
    assert_eq!(
        key,
        Some((
            stable_hash("sid:", "0f0f0f0f-aaaa-4bbb-8ccc-dddddddddddd"),
            SessionKind::Stable
        ))
    );
    // A second request from the same session, different body, same key.
    let again = stable_session_key(
        &h,
        b"other",
        ProxySecrets::NONE,
        &Method::POST,
        "/backend-api/codex/responses",
        true,
    );
    assert_eq!(key, again);
}

#[test]
fn codex_session_id_outranks_a_shared_api_key_on_loopback_and_remote() {
    let mut h = HeaderMap::new();
    h.insert("x-api-key", HeaderValue::from_static("shared-by-everyone"));
    h.insert("session-id", HeaderValue::from_static("s-1"));
    let loopback = stable_session_key(
        &h,
        b"{}",
        ProxySecrets::NONE,
        &Method::POST,
        "/backend-api/codex/responses",
        true,
    );
    let remote = stable_session_key(
        &h,
        b"{}",
        ProxySecrets::NONE,
        &Method::POST,
        "/backend-api/codex/responses",
        false,
    );
    assert_eq!(
        loopback,
        Some((stable_hash("sid:", "s-1"), SessionKind::Stable))
    );
    assert_eq!(
        remote, loopback,
        "the session id is per conversation; the key is per box"
    );
}

#[test]
fn anthropic_requests_ignore_a_stray_session_id_header() {
    let mut h = HeaderMap::new();
    h.insert("session-id", HeaderValue::from_static("s-1"));
    let key = stable_session_key(
        &h,
        b"{}",
        ProxySecrets::NONE,
        &Method::POST,
        "/v1/messages",
        true,
    );
    assert_eq!(key, None);
}

/// An empty or whitespace-only `session-id` is no session id: a Codex request
/// carrying one falls through to the tiers below, here tier 1's `x-api-key`
/// for a remote client, rather than keying every such session onto the one
/// `sid:` hash of an empty string, which would pin them all together.
///
/// Watched red: drop the `.filter(|s| !s.is_empty())` from tier 0 and both
/// blank headers key on `stable_hash("sid:", "")`.
#[test]
fn a_blank_codex_session_id_falls_through_to_the_other_tiers() {
    for blank in ["", "   "] {
        let mut h = HeaderMap::new();
        h.insert("x-api-key", HeaderValue::from_static("client-own-key"));
        h.insert("session-id", HeaderValue::from_static(blank));
        let key = stable_session_key(
            &h,
            b"{}",
            ProxySecrets::NONE,
            &Method::POST,
            "/backend-api/codex/responses",
            false,
        );
        assert_eq!(
            key,
            Some((stable_hash("key:", "client-own-key"), SessionKind::Stable)),
            "session-id {blank:?} must fall through to the x-api-key tier"
        );
    }
}

/// End-to-end through the REAL router (`app()`, not the unit-tested
/// `stable_session_key` directly): a loopback `POST /v1/messages?beta=true`
/// — the exact shape live traffic sends, query string included — with no
/// `x-api-key` and no `metadata.user_id`, but a `system` field, must record
/// as `SessionKind::Prefix` in the manager's own session snapshot.
///
/// This is the one seam the unit tests above cannot cover: they call
/// `stable_session_key` with a hand-fed `path`, which proves the MATCH
/// logic but nothing about which variable the real call site passes it.
/// `handle` has two candidates in scope — `path` (query-stripped) and
/// `path_and_query` (raw) — and passing the wrong one would silently
/// unpin 100% of real `/v1/messages?beta=true` traffic while every one of
/// those unit tests kept passing, because they never touch `handle` at
/// all. Driving a real request through `app()` with the query string
/// attached is what actually pins the wiring, not just the function.
#[tokio::test]
async fn real_request_through_app_pins_on_prefix_for_loopback_v1_messages() {
    use tower::ServiceExt as _;

    struct NoRefresh;
    impl crate::clients::oauth::TokenRefresher for NoRefresh {
        fn refresh(&self, _provider: Provider, _t: String) -> crate::clients::oauth::RefreshFuture {
            Box::pin(async {
                Err(crate::clients::oauth::OAuthError::Transient(
                    "unused".into(),
                ))
            })
        }
    }

    // Fake upstream: one 200 with a minimal usage body (the test sends one request).
    let body = r#"{"usage":{"input_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}"#;
    let up_addr = stub::canned(stub::reply("200 OK", "application/json", body));

    let config = dummy_config(None, &format!("http://{up_addr}"));
    let manager = Manager::new(
        config,
        Arc::new(NoRefresh),
        Arc::new(crate::records::probe::LiveUsageProber::new()),
        Arc::new(crate::clients::warmer::LiveWarmer::new()),
        None,
    );

    // No x-api-key, no metadata.user_id — only a `system` field. Real
    // `/v1/messages` calls always carry the query string tacked on by
    // live clients (`?beta=true`); this is not incidental to the test, it
    // is the exact thing D1 exists to pin.
    let body = serde_json::to_vec(&serde_json::json!({
        "system": "You are a helpful assistant.",
        "messages": [],
    }))
    .unwrap();

    let mut req = Request::builder()
        .method(Method::POST)
        .uri("/v1/messages?beta=true")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("build request");
    // Session affinity's feature flag (see `SessionKey`'s doc-comment) and
    // the loopback proof `handle`'s auth gate and tier 3 both read — both
    // injected by the hybrid listener per real connection; `app()` alone
    // injects neither, so a request driven straight at it needs both by
    // hand.
    req.extensions_mut().insert(SessionKey(0xF00D));
    req.extensions_mut().insert(ClientAddr(loopback_peer()));

    let response = app(Arc::clone(&manager))
        .oneshot(req)
        .await
        .expect("response");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the request must actually reach and clear the fake upstream"
    );

    let snap = manager.snapshot(OffsetDateTime::now_utc());
    // Exactly one request was ever served by this manager, so its one
    // session is the whole list — no need to reach for the private
    // `short_session_id` helper `manager/mod.rs` uses to derive `s.id`.
    let session = snap
        .sessions
        .first()
        .expect("the request must have been tracked as a session");
    assert_eq!(
        session.kind,
        SessionKind::Prefix,
        "a loopback POST /v1/messages?beta=true with a system field and no \
             identity must pin via tier 3 through the REAL call site, not just \
             the unit-tested function"
    );
}

#[test]
fn strips_client_authorization_and_sets_bearer() {
    let mut req = HeaderMap::new();
    req.insert(
        "authorization",
        HeaderValue::from_static("Bearer client-secret"),
    );
    req.insert("x-api-key", HeaderValue::from_static("proxy-key"));
    req.insert("accept-encoding", HeaderValue::from_static("gzip"));
    req.insert(
        "anthropic-beta",
        HeaderValue::from_static("oauth-2025-04-20"),
    );
    let out = build_upstream_headers(&req, "fresh-access-token");
    assert_eq!(
        out.get("authorization").unwrap().to_str().unwrap(),
        "Bearer fresh-access-token"
    );
    assert!(
        out.get("x-api-key").is_none(),
        "client key must not leak upstream"
    );
    assert!(
        out.get("accept-encoding").is_none(),
        "encoding stripped for SSE parse"
    );
    assert_eq!(
        out.get("anthropic-beta").unwrap().to_str().unwrap(),
        "oauth-2025-04-20",
        "unrelated headers pass through"
    );
}

#[test]
fn quota_rejected_detects_any_window() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "anthropic-ratelimit-unified-7d-status",
        HeaderValue::from_static("rejected"),
    );
    assert!(is_quota_rejected(&headers));

    let mut allowed = HeaderMap::new();
    allowed.insert(
        "anthropic-ratelimit-unified-status",
        HeaderValue::from_static("allowed_warning"),
    );
    assert!(!is_quota_rejected(&allowed));

    let mut fable_only = HeaderMap::new();
    for (name, value) in [
        ("anthropic-ratelimit-unified-status", "rejected"),
        ("anthropic-ratelimit-unified-5h-status", "allowed"),
        ("anthropic-ratelimit-unified-7d-status", "allowed_warning"),
        ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
    ] {
        fable_only.insert(name, HeaderValue::from_str(value).unwrap());
    }
    assert_eq!(
        quota_rejections(&fable_only),
        vec![UnifiedRejectionKind::FableWeekly]
    );

    let mut five_hour_only = HeaderMap::new();
    for (name, value) in [
        ("anthropic-ratelimit-unified-status", "rejected"),
        ("anthropic-ratelimit-unified-5h-status", "rejected"),
        ("anthropic-ratelimit-unified-7d-status", "allowed"),
    ] {
        five_hour_only.insert(name, HeaderValue::from_str(value).unwrap());
    }
    assert_eq!(
        quota_rejections(&five_hour_only),
        vec![UnifiedRejectionKind::FiveHour],
        "a known scope must not also create a duplicate overall rejection"
    );
}

/// Boot smoke test, driven at `app()` in-process (as plain `axum::serve` would
/// serve it), so no `ClientAddr` extension is injected and the client is
/// treated as remote (fail-closed) — a keyless request is 401, a keyed one
/// proceeds upstream. The loopback exemption (the production path via
/// `mitm::serve`) is covered separately by `loopback_client_is_exempt`.
/// The upstream points at a dead local port so the request never reaches the
/// real Anthropic API — it fails transport at connect and returns 503 (safe
/// to retry, see [`bad_gateway`]).
#[tokio::test]
async fn auth_rejects_without_key_and_attempts_upstream_with_key() {
    // 127.0.0.1:1 is reliably connection-refused, so the upstream forward
    // fails fast without any network egress to Anthropic.
    let manager =
        Manager::with_live_refresher(dummy_config(Some("test-key"), "http://127.0.0.1:1"), None);
    let router = app(manager);
    let no_key = send_to_proxy(router.clone(), client_request(Method::GET, "/", "")).await;
    assert_eq!(
        no_key.status().as_u16(),
        401,
        "keyless request must be rejected"
    );

    let mut keyed = client_request(Method::GET, "/", "");
    keyed
        .headers_mut()
        .insert("x-api-key", HeaderValue::from_static("test-key"));
    let with_key = send_to_proxy(router, keyed).await;
    assert_eq!(
        with_key.status().as_u16(),
        503,
        "authenticated request proceeds upstream (dead port → 503), never 401"
    );
}

/// Loopback exemption: served through the production hybrid listener
/// (`mitm::serve`, which injects `ClientAddr`), a KEYLESS request from a
/// loopback client must NOT be 401 even though a proxy key is configured — it
/// proceeds upstream (dead port → 503). This is what lets `claude` (which
/// sends no proxy key, only its own OAuth) talk to the local proxy.
#[tokio::test]
async fn loopback_client_is_exempt() {
    let manager = Manager::with_live_refresher(
        dummy_config(Some("secret-proxy-key"), "http://127.0.0.1:1"),
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // The hybrid server injects ClientAddr; base-URL (non-CONNECT) needs no TLS.
    tokio::spawn(async move { mitm::serve(listener, manager, None).await });

    let client = crate::test_support::loopback::client();
    // NO x-api-key — but the client is loopback, so it must be exempt.
    let resp = client.get(format!("http://{addr}/")).send().await.unwrap();
    assert_ne!(
        resp.status().as_u16(),
        401,
        "a loopback client must be exempt from the api-key gate"
    );
    assert_eq!(
        resp.status().as_u16(),
        503,
        "the exempt request proceeds upstream (dead port → 503)"
    );
}

/// The response-body cap rejects an over-cap stream (`BAD_GATEWAY` path) while
/// buffering a within-cap body verbatim and surfacing transport errors.
#[tokio::test]
async fn read_capped_body_rejects_oversized() {
    let chunks = vec![
        Ok::<Bytes, Infallible>(Bytes::from_static(b"hello ")),
        Ok::<Bytes, Infallible>(Bytes::from_static(b"world!")),
    ];
    // 12 bytes total against an 8-byte cap → TooLarge (drives BAD_GATEWAY).
    let result = read_capped_body(futures::stream::iter(chunks), 8).await;
    assert!(matches!(result, Err(BodyReadError::TooLarge)));
}

#[tokio::test]
async fn read_capped_body_accepts_within_and_at_cap() {
    let within = vec![
        Ok::<Bytes, Infallible>(Bytes::from_static(b"hello ")),
        Ok::<Bytes, Infallible>(Bytes::from_static(b"world")),
    ];
    let bytes = read_capped_body(futures::stream::iter(within), 1024)
        .await
        .ok()
        .expect("within-cap body buffers");
    assert_eq!(bytes, Bytes::from_static(b"hello world"));

    // A body whose total exactly equals the cap is accepted (only > rejects).
    let at_cap = vec![Ok::<Bytes, Infallible>(Bytes::from_static(b"12345678"))];
    assert!(
        read_capped_body(futures::stream::iter(at_cap), 8)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn read_capped_body_surfaces_transport_error() {
    let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
        Ok(Bytes::from_static(b"partial")),
        Err(std::io::Error::other("boom")),
    ];
    let result = read_capped_body(futures::stream::iter(chunks), 1024).await;
    assert!(matches!(result, Err(BodyReadError::Transport)));
}
