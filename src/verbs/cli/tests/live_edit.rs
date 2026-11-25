//! Tests for `cli/live_edit.rs`: each live verb's words, and where a live edit may write
//! the file.

use super::*;
/// Every verb that asks the running proxy first says what it said when each
/// verb spelled out its own refusals and warnings, word for word. The one
/// planned difference is `tcr priority`'s "too old" warning, which named a
/// `tcr restart` subcommand that does not exist and now ends in
/// [`RESTART_HINT`] like the rest.
#[test]
fn each_live_verb_keeps_its_own_refusal_and_warning_words() {
    let mut config = load_from(TWO_ACCOUNTS);
    config.proxy.port = 3456;
    let path = Path::new("/x/config.json");
    let refusal = |words: &'static LiveWords, error: LiveControlError| {
        let site = LiveSite {
            config: &config,
            config_path: path,
            words,
        };
        match site.decide::<()>(Err(error)) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a rejected key or a refused query must not reach the file"),
        }
    };
    let warning = |words: &'static LiveWords, stale: Stale, keeps: Option<&str>| {
        LiveSite {
            config: &config,
            config_path: path,
            words,
        }
        .warning(&stale, keeps)
    };
    let message = "no account matches 'zed'.";
    let why = "HTTP 500 Internal Server Error";

    struct Case {
        words: &'static LiveWords,
        keeps: Option<&'static str>,
        unauthorized: &'static str,
        rejected: String,
        too_old: String,
        failed: String,
    }
    let cases = [
        Case {
            words: &REMOVE_WORDS,
            keeps: Some("routing to 'alice'"),
            unauthorized: "the proxy on :3456 rejected the api-key in /x/config.json — the config was NOT changed, because deleting it would leave the running proxy still rotating this account. Fix `proxy.apiKey` and retry.",
            rejected: format!(
                "the proxy running on :3456 refused this: {message} Nothing was changed."
            ),
            too_old: format!(
                "[tcr] WARNING: the proxy running on :3456 is too old to accept live account control (no /_tcr/accounts/disabled route), so only the config file was changed. It will KEEP routing to 'alice' until it restarts. {RESTART_HINT}"
            ),
            failed: format!(
                "[tcr] WARNING: could not apply this to the proxy running on :3456 ({why}), so only the config file was changed. It may KEEP routing to 'alice' until it restarts."
            ),
        },
        Case {
            words: &PRIORITY_WORDS,
            keeps: Some("the old order"),
            unauthorized: "the proxy on :3456 rejected the api-key in /x/config.json, so the config was NOT changed, because writing it would leave the running proxy on the old order. Fix `proxy.apiKey` and retry.",
            rejected: format!(
                "the proxy running on :3456 refused this: {message} Nothing was changed."
            ),
            too_old: format!(
                "[tcr] WARNING: the proxy running on :3456 is too old to accept a live priority change (no /_tcr/accounts/priority route), so only the config file was changed. It will KEEP the old order until it restarts. {RESTART_HINT}"
            ),
            failed: format!(
                "[tcr] WARNING: could not apply this to the proxy running on :3456 ({why}), so only the config file was changed. It may KEEP the old order until it restarts."
            ),
        },
        Case {
            words: &CONFIG_SET_WORDS,
            keeps: None,
            unauthorized: "the proxy on :3456 rejected the api-key in /x/config.json, so nothing was changed: writing the file beside a running proxy can lose the change to its own next write. Fix `proxy.apiKey` and retry.",
            rejected: format!("the proxy running on :3456 refused this: {message}"),
            too_old: format!(
                "[tcr] WARNING: the proxy running on :3456 is too old to save settings itself (no /_tcr/config route), so this process wrote the file beside it. {RESTART_HINT}"
            ),
            failed: format!(
                "[tcr] WARNING: could not hand this to the proxy running on :3456 ({why}), so this process wrote the file beside it."
            ),
        },
        Case {
            words: &ENABLE_WORDS,
            keeps: Some("benching 'alice'"),
            unauthorized: "the proxy on :3456 rejected the api-key in /x/config.json — the config was NOT changed, because writing it would leave the running proxy still rotating this account. Fix `proxy.apiKey` and retry.",
            rejected: format!(
                "the proxy running on :3456 refused this: {message} Nothing was changed."
            ),
            too_old: format!(
                "[tcr] WARNING: the proxy running on :3456 is too old to accept live account control (no /_tcr/accounts/disabled route), so only the config file was changed. It will KEEP benching 'alice' until it restarts. {RESTART_HINT}"
            ),
            failed: format!(
                "[tcr] WARNING: could not apply this to the proxy running on :3456 ({why}), so only the config file was changed. It may KEEP benching 'alice' until it restarts."
            ),
        },
        Case {
            words: &CONTROL_WORDS,
            keeps: Some("its OLD control account"),
            unauthorized: "the proxy on :3456 rejected the api-key in /x/config.json — the config was NOT changed, because writing it would leave the running proxy still resolving identity traffic against its old control account. Fix `proxy.apiKey` and retry.",
            rejected: format!(
                "the proxy running on :3456 refused this: {message} Nothing was changed."
            ),
            too_old: format!(
                "[tcr] WARNING: the proxy running on :3456 is too old to accept live account control (no /_tcr/accounts/control route), so only the config file was changed. It will KEEP its OLD control account until it restarts. {RESTART_HINT}"
            ),
            failed: format!(
                "[tcr] WARNING: could not apply this to the proxy running on :3456 ({why}), so only the config file was changed. It may KEEP its OLD control account until it restarts."
            ),
        },
    ];
    for case in cases {
        let route = case.words.route_path;
        assert_eq!(
            refusal(case.words, LiveControlError::Unauthorized),
            case.unauthorized,
            "{route}"
        );
        assert_eq!(
            refusal(case.words, LiveControlError::Rejected(message.to_string())),
            case.rejected,
            "{route}"
        );
        assert_eq!(
            warning(case.words, Stale::NoRoute, case.keeps),
            case.too_old,
            "{route}"
        );
        assert_eq!(
            warning(case.words, Stale::Failed(why.to_string()), case.keeps),
            case.failed,
            "{route}"
        );
    }
}

/// The decision itself: nothing listening is a quiet file write, a missing
/// route and an unusable answer are a warned one, and an answer is applied.
#[test]
fn the_live_decision_writes_the_file_only_where_it_may() {
    let config = load_from(TWO_ACCOUNTS);
    let site = LiveSite {
        config: &config,
        config_path: Path::new("/x/config.json"),
        words: &ENABLE_WORDS,
    };
    assert!(matches!(site.decide(Ok(7)), Ok(Live::Applied(7))));
    assert!(matches!(
        site.decide::<()>(Err(LiveControlError::NoServer)),
        Ok(Live::FileOnly(None))
    ));
    assert!(matches!(
        site.decide::<()>(Err(LiveControlError::NoRoute)),
        Ok(Live::FileOnly(Some(Stale::NoRoute)))
    ));
    assert!(matches!(
        site.decide::<()>(Err(LiveControlError::NoAnswer("late".into()))),
        Ok(Live::FileOnly(Some(Stale::Failed(why)))) if why == "late"
    ));
    assert!(matches!(
        site.decide::<()>(Err(LiveControlError::Unusable("HTTP 500".into()))),
        Ok(Live::FileOnly(Some(Stale::Failed(why)))) if why == "HTTP 500"
    ));
    assert!(
        site.decide::<()>(Err(LiveControlError::Unauthorized))
            .is_err()
    );
    assert!(
        site.decide::<()>(Err(LiveControlError::Rejected("no".into())))
            .is_err()
    );
}
