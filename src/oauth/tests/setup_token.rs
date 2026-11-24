use super::*;

// --- setup-token (`tcr login --token`) ----------------------------------

/// A setup-token-shaped write: [`finish_login`] given a [`Tokens`] with
/// `refresh_token: None` (what [`login_with_token`] always constructs)
/// must write `refreshToken` ABSENT from the JSON — not `null` — and
/// `expiresAt` set to the assumed one-year expiry.
///
/// Watched failing: with `persist_via_file`'s account literal changed to
/// `refresh_token: Some("bogus-rt".to_string())` (a plausible slip if
/// someone "fixed" the E0308 that removing `Some(...)` around the new
/// `Option<String>` field would otherwise cause), this test failed with
/// `refreshToken must be ABSENT ... {"refreshToken":"bogus-rt",...}`.
/// Restored, it passes.
#[tokio::test]
async fn setup_token_account_writes_with_refresh_token_absent_and_expires_at_set() {
    let path = login_file("setup-token-write", r#"{ "accounts": [] }"#);
    let config = load_or_default(&path).unwrap();

    let setup_tokens = Tokens {
        id_token: None,
        access_token: "at-setup".to_string(),
        refresh_token: None,
        expires_at_ms: 1_893_456_000_000,
    };

    let name = finish_login(
        &path,
        WriteRoute::File,
        &config,
        "gina",
        &setup_tokens,
        all_none_profile(),
    )
    .await
    .expect("a setup-token credential with no refresh token must still write");
    assert_eq!(name, "gina");

    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let account = &doc["accounts"][0];
    assert_eq!(account["name"], serde_json::json!("gina"));
    assert!(
        account.get("refreshToken").is_none(),
        "refreshToken must be ABSENT (skip_serializing_if), not written as null or a \
         leftover value: {account}"
    );
    assert_eq!(
        account["expiresAt"],
        serde_json::json!(1_893_456_000_000i64)
    );

    std::fs::remove_file(&path).ok();
}

/// A `tcr mint` clipboard line is `<name>\t<token>`; `--token` must take
/// the token and drop the name, or the name goes upstream inside the bearer.
#[test]
fn parse_setup_token_takes_the_token_from_a_mint_clipboard_line() {
    assert_eq!(
        parse_setup_token("alice@example.com\tfake-token-abc\n").unwrap(),
        "fake-token-abc"
    );
    assert_eq!(
        parse_setup_token("  fake-token-abc  \n").unwrap(),
        "fake-token-abc"
    );
}

/// A token copied out of a terminal that wrapped it carries whitespace
/// in the middle; no token contains any, so it is removed.
#[test]
fn parse_setup_token_removes_whitespace_inside_the_token() {
    assert_eq!(
        parse_setup_token("fake-token \tabc def\n").unwrap(),
        "abcdef"
    );
    assert_eq!(
        parse_setup_token("fake-token abc\n").unwrap(),
        "fake-tokenabc"
    );
}

/// The sign-in page's `code#state` is refused by name before any upstream
/// call, rather than sent and reported as a bad token.
#[test]
fn parse_setup_token_refuses_a_sign_in_code() {
    let err = parse_setup_token("fakecode#fakestate\n").unwrap_err();
    assert!(err.to_string().contains("sign-in code"), "{err}");
}

/// A `tcr mint --group` block holds several accounts; `--token` adds one,
/// so it refuses rather than quietly adding the first.
#[test]
fn parse_setup_token_refuses_a_mint_block_of_several_accounts() {
    let err = parse_setup_token("a@example.com\tfake-one\nb@example.com\tfake-two\n").unwrap_err();
    assert!(err.to_string().contains("more than one account"), "{err}");
}

#[test]
fn parse_setup_token_refuses_empty_input() {
    assert!(parse_setup_token("\n  \n").is_err());
}

/// `--token` combined with `--account` must refuse and write nothing —
/// checked byte-for-byte, not just "still has 2 accounts": an inference-
/// only token carries no identity for `--account` to confirm against.
///
/// Watched failing: with the `account.is_some() || org.is_some()` guard
/// at the top of `login_with_token` deleted, this test hung waiting on a
/// real stdin read (the function fell through past the refusal straight
/// into `read_setup_token`) instead of returning the expected `Err` —
/// the observable failure for a test with no stdin to feed it. Restored,
/// it returns immediately with the refusal and the file is untouched.
#[tokio::test]
async fn login_with_token_refuses_with_account_and_writes_nothing() {
    let path = std::env::temp_dir().join(format!(
        "tcr-oauth-setup-token-refuse-account-{}-{}.json",
        std::process::id(),
        line!()
    ));
    std::fs::write(&path, two_account_seed()).unwrap();
    let before = std::fs::read_to_string(&path).unwrap();

    let err = login_with_token(&path, false, None, Some("alice@example.com"))
        .await
        .expect_err("--token combined with --account must refuse");
    assert!(err.to_string().contains("--account"), "{}", err.to_string());

    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        before, after,
        "the file must be byte-identical — --token+--account must write nothing"
    );

    std::fs::remove_file(&path).ok();
}

/// A canned upstream that answers every connection with `status_line` and an
/// empty body, for [`verify_setup_token`]. Returns its base URL.
async fn canned_upstream(status_line: &'static str) -> String {
    let (listener, port) = crate::test_support::loopback::listener().await;
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                sock.read(&mut buf)
                    .await
                    .log_debug("reading a test request");
                sock.write_all(
                    format!(
                        "HTTP/1.1 {status_line}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .log_debug("writing a test response");
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// The bug this guards: a pasted token upstream refuses with 401 must be
/// classified `Rejected`, so `login_with_token` writes nothing. Before the
/// check existed the account was stored and reported `active` while every
/// request it served came back 401.
#[tokio::test]
async fn verify_setup_token_classifies_401_as_rejected() {
    let upstream = canned_upstream("401 Unauthorized").await;
    assert_eq!(
        verify_setup_token(&upstream, "bad-token").await,
        SetupTokenCheck::Rejected { status: 401 }
    );
}

#[tokio::test]
async fn verify_setup_token_classifies_403_as_rejected() {
    let upstream = canned_upstream("403 Forbidden").await;
    assert_eq!(
        verify_setup_token(&upstream, "refused-token").await,
        SetupTokenCheck::Rejected { status: 403 }
    );
}

/// A 2xx is acceptance, and so is a 429: a quota answer is one that got past
/// auth, and refusing a valid-but-exhausted token would block the operator
/// from adding an account they mean to use once its window resets.
#[tokio::test]
async fn verify_setup_token_accepts_2xx_and_429() {
    let ok = canned_upstream("200 OK").await;
    assert_eq!(
        verify_setup_token(&ok, "good-token").await,
        SetupTokenCheck::Accepted
    );
    let exhausted = canned_upstream("429 Too Many Requests").await;
    assert_eq!(
        verify_setup_token(&exhausted, "exhausted-token").await,
        SetupTokenCheck::Accepted
    );
}

/// No upstream answer at all proves nothing about the token — the caller
/// stores it with a warning rather than refusing an offline operator.
#[tokio::test]
async fn verify_setup_token_reports_dead_upstream_as_unreachable() {
    assert!(matches!(
        verify_setup_token(&format!("http://127.0.0.1:{DEAD_PORT}"), "any-token").await,
        SetupTokenCheck::Unreachable { .. }
    ));
}

// --- the `unnamed` fallback ---------------------------------------------

/// An `Account` carrying nothing but a name — the only field
/// [`unnamed_fallback`] reads.
fn named_account(name: &str) -> Account {
    Account {
        access_token: "at-placeholder".to_string(),
        refresh_token: None,
        expires_at: None,
        priority: None,
        ..Account::fixture(name)
    }
}

#[test]
fn unnamed_fallback_is_plain_unnamed_when_free() {
    assert_eq!(unnamed_fallback(&[]), "unnamed");
    assert_eq!(
        unnamed_fallback(&[named_account("alice@example.com")]),
        "unnamed"
    );
}

/// The collision case, and the reason this function exists at all: an
/// inference-only account has no email and no uuid, so `identity::resolve`
/// can only match it by NAME. Handing out `unnamed` twice would resolve the
/// second login onto the first row and overwrite its token instead of
/// adding an account.
#[test]
fn unnamed_fallback_skips_taken_names() {
    assert_eq!(unnamed_fallback(&[named_account("unnamed")]), "unnamed-2");
    assert_eq!(
        unnamed_fallback(&[named_account("unnamed"), named_account("unnamed-2")]),
        "unnamed-3"
    );
}

/// Gaps are filled rather than skipped past — the name is chosen by
/// scanning for the first FREE suffix, never by counting rows, so removing
/// `unnamed-2` and adding an account reuses that slot instead of colliding
/// with `unnamed-3`. A count-derived fallback gets this wrong.
#[test]
fn unnamed_fallback_fills_a_gap_left_by_a_removed_account() {
    let accounts = [named_account("unnamed"), named_account("unnamed-3")];
    assert_eq!(unnamed_fallback(&accounts), "unnamed-2");
}
