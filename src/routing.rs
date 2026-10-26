#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed instant on a whole second, in ms.
    const AT: i64 = 1_800_000_000_000;

    /// **`count10m` counts the last error's kind in the ten minutes up to now, per
    /// provider, and only those.**
    #[test]
    fn count10m_counts_the_last_kind_in_the_ten_minutes_up_to_now() {
        let mut seen = Observations::default();
        // Exactly ten minutes before `AT` is outside `(now - 600s, now]`.
        seen.record_error(Provider::Anthropic, ErrorKind::Quota, AT - 600_000);
        seen.record_error(Provider::Anthropic, ErrorKind::Quota, AT - 599_000);
        seen.record_error(Provider::Anthropic, ErrorKind::Transport, AT - 1_000);
        seen.record_error(Provider::Anthropic, ErrorKind::Quota, AT - 500);
        seen.record_error(Provider::Codex, ErrorKind::Quota, AT - 400);
        assert_eq!(
            seen.last_error(Provider::Anthropic, AT),
            Some((ErrorKind::Quota, AT - 500, 2)),
            "the last error is quota, and two quota errors fell inside the window"
        );
        assert_eq!(
            seen.last_error(Provider::Codex, AT),
            Some((ErrorKind::Quota, AT - 400, 1)),
            "each provider keeps its own errors"
        );
        assert_eq!(
            seen.last_error(Provider::Anthropic, AT + 600_000),
            Some((ErrorKind::Quota, AT - 500, 0)),
            "ten minutes on, the last error is still named and counts nothing"
        );
        assert_eq!(
            Observations::default().last_error(Provider::Codex, AT),
            None
        );
    }

    /// **The window is fixed memory**: a burst over far longer than ten minutes keeps one
    /// slot per second, and an error a whole window older than its slot is not counted.
    #[test]
    fn the_error_window_is_bounded_and_forgets_what_it_overwrites() {
        let mut log = ErrorLog::default();
        for second in 0..3_000 {
            log.record(ErrorKind::Overloaded, AT + second * 1_000);
        }
        assert_eq!(log.slots.len(), 600);
        assert_eq!(log.count(ErrorKind::Overloaded, AT + 2_999_000), 600);
        log.record(ErrorKind::Overloaded, AT);
        assert_eq!(
            log.count(ErrorKind::Overloaded, AT + 2_999_000),
            600,
            "a late error a window old lands on a slot a later second holds"
        );
        assert_eq!(log.last, Some((ErrorKind::Overloaded, AT + 2_999_000)));
    }

    /// **A status alone names auth, quota and overloaded, and nothing else.** A 403 is
    /// recorded only where it refuses the credential.
    #[test]
    fn a_status_names_only_the_kinds_it_can() {
        assert_eq!(ErrorKind::of_status(401), Some(ErrorKind::Auth));
        assert_eq!(ErrorKind::of_status(429), Some(ErrorKind::Quota));
        assert_eq!(ErrorKind::of_status(529), Some(ErrorKind::Overloaded));
        assert_eq!(ErrorKind::of_status(503), Some(ErrorKind::Overloaded));
        for status in [200, 400, 403, 404, 500, 502] {
            assert_eq!(ErrorKind::of_status(status), None, "{status}");
        }
    }

    /// **A failed stream names transport when it proved truncated and overloaded when it
    /// failed in-band as overloaded; any other in-band error names none.**
    #[test]
    fn a_stream_names_transport_or_overloaded_and_nothing_else() {
        assert_eq!(
            ErrorKind::of_stream("truncated"),
            Some(ErrorKind::Transport)
        );
        assert_eq!(
            ErrorKind::of_stream("overloaded_error"),
            Some(ErrorKind::Overloaded)
        );
        assert_eq!(
            ErrorKind::of_stream("server_is_overloaded"),
            Some(ErrorKind::Overloaded)
        );
        assert_eq!(ErrorKind::of_stream("context_length_exceeded"), None);
        assert_eq!(ErrorKind::of_stream("unknown"), None);
    }

    /// **Model ids are remembered per provider, up to a fixed bound.**
    #[test]
    fn model_ids_are_kept_per_provider_and_bounded() {
        let mut seen = Observations::default();
        seen.record_model(Provider::Codex, "gpt-a");
        seen.record_model(Provider::Codex, "gpt-a");
        assert_eq!(seen.model_ids(Provider::Codex).len(), 1);
        assert!(seen.model_ids(Provider::Anthropic).is_empty());
        for n in 0..100 {
            seen.record_model(Provider::Anthropic, &format!("claude-opus-{n}"));
        }
        assert_eq!(seen.model_ids(Provider::Anthropic).len(), MODEL_IDS_CAP);
    }

    /// **The catalogue groups `anthropic` ids by family, in a fixed order, and invents no
    /// family; a `codex` id is its own model.**
    #[test]
    fn the_catalogue_groups_families_and_invents_none() {
        let seen: BTreeSet<String> = [
            "claude-opus-5-5",
            "claude-fable-5-1",
            "claude-opus-4-6",
            "claude-haiku-5-5",
            "some-other-model",
        ]
        .map(String::from)
        .into();
        let ids = |ids: &[&str]| ids.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            catalogue(Provider::Anthropic, &seen),
            vec![
                ("fable".to_owned(), ids(&["claude-fable-5-1"])),
                (
                    "opus".to_owned(),
                    ids(&["claude-opus-4-6", "claude-opus-5-5"])
                ),
                ("haiku".to_owned(), ids(&["claude-haiku-5-5"])),
            ]
        );
        let codex: BTreeSet<String> = ["gpt-b", "gpt-a"].map(String::from).into();
        assert_eq!(
            catalogue(Provider::Codex, &codex),
            vec![
                ("gpt-a".to_owned(), ids(&["gpt-a"])),
                ("gpt-b".to_owned(), ids(&["gpt-b"])),
            ]
        );
    }

    /// **Out only when nothing can serve; degraded for a blocked model, an empty catalogue
    /// or an error in ten minutes; ok otherwise.**
    #[test]
    fn the_state_rule() {
        let model = |routable| ModelRouting {
            model: "opus".to_owned(),
            ids: Vec::new(),
            routable,
            reason: String::new(),
            frees_at: None,
            accounts_routable: 0,
        };
        let error = |count10m| LastError {
            kind: ErrorKind::Quota,
            at: String::new(),
            count10m,
        };
        assert_eq!(state(false, &[model(true)], None), ProviderState::Out);
        assert_eq!(state(false, &[], None), ProviderState::Out);
        assert_eq!(
            state(true, &[], None),
            ProviderState::Degraded,
            "an empty catalogue alone is degraded, never out"
        );
        assert_eq!(
            state(true, &[model(true), model(false)], None),
            ProviderState::Degraded
        );
        assert_eq!(
            state(true, &[model(true)], Some(&error(1))),
            ProviderState::Degraded
        );
        assert_eq!(
            state(true, &[model(true)], Some(&error(0))),
            ProviderState::Ok,
            "an error older than ten minutes no longer degrades"
        );
        assert_eq!(state(true, &[model(true)], None), ProviderState::Ok);
    }

    /// **The reader takes schema 1, a forecast object included, and refuses every other
    /// answer with a reason.**
    #[test]
    fn parse_takes_schema_one_and_refuses_the_rest() {
        let document = r#"{"schema":1,"generatedAt":"2026-10-26T00:00:00Z",
            "build":{"sha":"abc","dirty":false,"builtAt":"2026-10-26T00:00:00Z"},
            "providers":[],"forecast":null}"#;
        assert_eq!(parse(document).map(|d| d.schema), Ok(1));
        let later = document.replace(r#""forecast":null"#, r#""forecast":{"providers":[]}"#);
        assert!(
            parse(&later).is_ok(),
            "a later schema-1 forecast object is read"
        );
        let refused = |body: &str| parse(body).err().unwrap_or_default();
        assert!(refused("not json").contains("not JSON"));
        assert!(refused(r#"{"schema":2}"#).contains("schema 2"));
        assert!(refused("{}").contains("names no routing schema"));
        assert!(refused(r#"{"schema":1}"#).contains("unreadable"));
    }
}
