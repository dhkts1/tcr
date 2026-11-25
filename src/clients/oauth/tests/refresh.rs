use super::*;

#[test]
fn normalize_treats_small_values_as_seconds() {
    // 1_700_000_000 s → ms.
    assert_eq!(normalize_expires_at(1_700_000_000), 1_700_000_000_000);
}

#[test]
fn normalize_leaves_millisecond_values_unchanged() {
    assert_eq!(normalize_expires_at(1_700_000_000_000), 1_700_000_000_000);
}

#[test]
fn is_expired_true_when_now_past_expiry() {
    assert!(is_expired(Some(1000), 2000));
}

#[test]
fn is_expired_false_for_none() {
    assert!(!is_expired(None, 2000));
}

#[test]
fn is_expiring_soon_within_window() {
    // Expiry 1 minute out is inside the 5-minute window.
    let now = 10_000_000;
    assert!(is_expiring_soon(Some(now + 60_000), now));
    assert!(!is_expiring_soon(Some(now + 10 * 60_000), now));
}

#[test]
fn expires_at_from_saturates_on_huge_expires_in() {
    // A hostile/huge expires_in must saturate, never overflow into a
    // negative (past) expiry that would force a refresh storm.
    let now = crate::now_ms();
    let saturated = expires_at_from(now, Some(i64::MAX));
    assert_eq!(saturated, i64::MAX);
    assert!(
        saturated >= now,
        "expiry must be in the future, not negative"
    );
    // Normal values are unchanged: default 3600s and an explicit value.
    assert_eq!(expires_at_from(0, None), 3_600 * 1000);
    assert_eq!(expires_at_from(1000, Some(60)), 1000 + 60 * 1000);
}

/// THE TRAP (bridge §"THE TRAP"): a refresh whose response omits a
/// rotated `refresh_token` must fall back to the token it was called
/// with — never `None`. `apply_refresh` (`manager/refresh.rs`) and
/// `persist_tokens` (`manager/mod.rs`) both write `tokens.refresh_token`
/// straight onto an existing account's row with no other check, so a
/// bare `None` here would silently make a healthy account unrefreshable
/// on its very next rotation, with no error and no log line.
///
/// Watched failing: with the `unwrap_or_else` fallback in
/// `refresh_access_token_at` replaced by a bare `data.refresh_token`
/// (still type-correct — `Option<String>` on both sides, so this is a
/// silent behavioural regression, not a compile error), this test failed
/// with `left: None, right: Some("rt-original")`. Restored, it passes.
#[tokio::test]
async fn refresh_access_token_at_preserves_stored_refresh_token_when_response_omits_one() {
    let app = axum::Router::new().route(
        "/",
        axum::routing::post(|| async {
            // A real rotation response that omits `refresh_token` —
            // exactly the shape Anthropic's endpoint sends when it
            // does not rotate the refresh token on this call.
            axum::Json(serde_json::json!({
                "access_token": "at-fresh",
                "expires_at": 1_893_456_000_000i64
            }))
        }),
    );
    let endpoint = format!("{}/", crate::test_support::stub_upstream::http(app).url());

    let client = crate::base::http_client::builder().build().unwrap();
    let tokens = refresh_access_token_at(&client, "rt-original", &endpoint)
        .await
        .expect("a refresh with no rotated refresh_token must still succeed");
    assert_eq!(
        tokens.refresh_token.as_deref(),
        Some("rt-original"),
        "must fall back to the refresh token it was called with, never None"
    );
}

/// `LiveRefresher` sends each provider's refresh token to that provider's
/// own endpoint, in that provider's grant. A refresh token is only good
/// where it was issued, so a Codex token posted to Anthropic is not a slow
/// refresh but a rejected one — and a rejection sidelines the account as
/// dead until it is logged in again.
///
/// Both endpoints point at one loopback stub — the Anthropic one by setting
/// the private field, since it has no builder seam — so a broken dispatch
/// fails here instead of posting a fake token to a real token endpoint. The
/// client ids pin the pairing of grant to endpoint as well as the route.
#[tokio::test]
async fn live_refresher_sends_each_provider_to_its_own_token_endpoint() {
    type Seen = Arc<std::sync::Mutex<Vec<(&'static str, serde_json::Value)>>>;
    let seen: Seen = Seen::default();
    let recorder = |route: &'static str, seen: Seen| {
        move |axum::Json(body): axum::Json<serde_json::Value>| {
            let seen = Arc::clone(&seen);
            async move {
                seen.lock().unwrap().push((route, body));
                axum::Json(serde_json::json!({
                    "access_token": format!("at-from-{route}"),
                    "refresh_token": "rt-rotated",
                    "expires_in": 3600
                }))
            }
        }
    };
    let app = axum::Router::new()
        .route(
            "/anthropic",
            axum::routing::post(recorder("anthropic", Arc::clone(&seen))),
        )
        .route(
            "/codex",
            axum::routing::post(recorder("codex", Arc::clone(&seen))),
        );
    let addr = crate::test_support::stub_upstream::http(app);

    let mut refresher = LiveRefresher::new().with_codex_endpoint(format!("http://{addr}/codex"));
    refresher.endpoint = format!("http://{addr}/anthropic");

    let codex = refresher
        .refresh(
            crate::base::provider::Provider::Codex,
            "rt-codex".to_string(),
        )
        .await
        .expect("the Codex refresh must succeed against the stub");
    let anthropic = refresher
        .refresh(
            crate::base::provider::Provider::Anthropic,
            "rt-anthropic".to_string(),
        )
        .await
        .expect("the Anthropic refresh must succeed against the stub");

    assert_eq!(
        codex.access_token, "at-from-codex",
        "a Codex refresh must reach the Codex token endpoint"
    );
    assert_eq!(
        anthropic.access_token, "at-from-anthropic",
        "an Anthropic refresh must reach the Anthropic token endpoint"
    );
    let seen = seen.lock().unwrap();
    let body_at = |route: &str| {
        let (_, body) = seen
            .iter()
            .find(|(hit, _)| *hit == route)
            .unwrap_or_else(|| panic!("the {route} endpoint was never called: {seen:?}"));
        body.clone()
    };
    assert_eq!(body_at("codex")["client_id"], codex::CLIENT_ID);
    assert_eq!(body_at("codex")["refresh_token"], "rt-codex");
    assert_eq!(body_at("anthropic")["client_id"], CLIENT_ID);
    assert_eq!(body_at("anthropic")["refresh_token"], "rt-anthropic");
}

#[tokio::test]
async fn no_refresh_never_hits_the_endpoint_and_fails_transiently() {
    // The whole point of Fix 1: an offline snapshot's refresher must fail
    // locally (no OAuth token-endpoint call) so a `tcr status` beside a live
    // server can never rotate/revoke its single-use refresh token. Transient,
    // never AuthRejected — it must NOT falsely mark the credential dead.
    let err = NoRefresh
        .refresh(
            crate::base::provider::Provider::Anthropic,
            "rt-should-never-be-sent".to_string(),
        )
        .await
        .expect_err("NoRefresh must always fail — it never contacts the endpoint");
    assert!(matches!(err, OAuthError::Transient(_)));
    assert_eq!(
        err.to_string(),
        format!("token refresh failed transiently: {NO_REFRESH_MESSAGE}")
    );
}

/// An offline snapshot never refreshes; for a Codex row its remedy names the command that
/// signs a Codex account in again, and the Anthropic remedy is the one it always was.
///
/// Watched red: one message for both providers names `tcr login` for a Codex row.
#[tokio::test]
async fn the_offline_expired_token_remedy_names_tcr_login_codex() {
    let err = NoRefresh
        .refresh(
            crate::base::provider::Provider::Codex,
            "rt-never-sent".to_string(),
        )
        .await
        .expect_err("NoRefresh never refreshes");
    std::assert_matches!(err, OAuthError::Transient(_));
    assert_eq!(
        err.to_string(),
        format!("token refresh failed transiently: {NO_REFRESH_CODEX_MESSAGE}")
    );
    assert!(
        NO_REFRESH_CODEX_MESSAGE.contains("re-run after `tcr login --codex --account <name>`"),
        "{NO_REFRESH_CODEX_MESSAGE}"
    );
    let claude = NoRefresh
        .refresh(
            crate::base::provider::Provider::Anthropic,
            "rt-never-sent".to_string(),
        )
        .await
        .expect_err("NoRefresh never refreshes");
    assert_eq!(
        claude.to_string(),
        format!("token refresh failed transiently: {NO_REFRESH_MESSAGE}")
    );
}
