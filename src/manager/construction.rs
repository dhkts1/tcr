//! Building a `Manager` from a config or from pre-seeded runtime rows.

use super::*;

impl Manager {
    /// Build a manager over `config`, using `refresher` for token refresh,
    /// `prober` for background usage reads, and persisting refreshes to
    /// `config_path` (if given).
    pub fn new(
        config: Config,
        refresher: Arc<dyn TokenRefresher>,
        prober: Arc<dyn UsageProber>,
        warmer: Arc<dyn AccountWarmer>,
        config_path: Option<PathBuf>,
    ) -> Arc<Self> {
        let accounts: Vec<AccountRuntime> = config.build_per_account(AccountRuntime::from_config);
        Self::assemble(config, refresher, prober, warmer, config_path, accounts)
    }

    /// Assemble the `Arc<Manager>` from an already-built runtime vec. Shared by
    /// [`Self::new`] (which derives the runtimes from `config`) and
    /// [`Self::from_runtimes`] (which is handed pre-seeded runtimes for the demo).
    fn assemble(
        config: Config,
        refresher: Arc<dyn TokenRefresher>,
        prober: Arc<dyn UsageProber>,
        warmer: Arc<dyn AccountWarmer>,
        config_path: Option<PathBuf>,
        accounts: Vec<AccountRuntime>,
    ) -> Arc<Self> {
        let upstream = config.upstream.clone();
        let codex_upstream = config.codex_upstream.clone();
        let proxy_api_key = config.proxy.api_key.clone();
        let request_key = config
            .proxy
            .request_key
            .clone()
            .filter(|key| !key.is_empty());
        let global_threshold = config.switch_threshold;
        let fable_weekly_threshold = config.fable_weekly_threshold;
        let groups = Arc::new(GroupPolicy::from_config(&config));
        let pacing = config.pacing.clone();
        let account_throttle = config.account_throttle.clone();
        let fleet_throttle = config.fleet_throttle.clone();

        let locked_idx = config.lock_account.as_ref().and_then(|name| {
            let idx = accounts.iter().position(|a| a.name == *name);
            if idx.is_none() {
                let names: Vec<&str> = accounts.iter().map(|a| a.name.as_str()).collect();
                tracing::error!(
                    lock_account = %name, available = ?names,
                    "lockAccount name did not match any account — running UNLOCKED (normal routing)"
                );
            }
            idx
        });
        // Capture the locked account name BEFORE `accounts` is moved into the struct.
        let locked_name = locked_idx.and_then(|i| accounts.get(i).map(|a| a.name.clone()));

        // Clamp defensively even though `config::default_control_reserve` and
        // the field's own doc already promise `[0.0, 0.5]` — a hand-edited
        // config file is not bound by either.
        let control_reserve = config.effective_control_reserve();
        // Read before `config` is moved into the struct below, same reason as
        // `locked_name` above.
        let control_pooled = config.control_pooled;
        let control_identity = config.control_identity;

        // Hours → ms once, at construction. `i64::from` then a checked multiply:
        // a hand-edited `resetUrgencyTierHours` of, say, 100_000_000 would
        // overflow the product and wrap to a negative width, which would invert
        // the bucket ordering rather than fail loudly. Saturating at i64::MAX
        // instead degrades to "one bucket for everyone" — the same shape as the
        // `0` disable, which is the safe direction to fall.
        let reset_urgency_tier_ms =
            i64::from(config.reset_urgency_tier_hours).saturating_mul(3_600_000);

        let control_idx = config.control_account_name().and_then(|name| {
            match crate::identity::match_one(&accounts[..], name) {
                crate::identity::Match::One(idx) => Some(idx),
                crate::identity::Match::None => {
                    let names: Vec<&str> = accounts.iter().map(|a| a.name.as_str()).collect();
                    tracing::error!(
                        control_account = %name, available = ?names,
                        "controlAccount did not match any account — running with NO control account"
                    );
                    None
                }
                // Two or more accounts share this name — only reachable on a
                // config hand-edited past `config::load`'s rename migration.
                // Picking the first one silently, as the old `position(...)`
                // scan did, risks binding control to a disabled Team-seat row
                // while the account that resolved it live stays unpinned.
                crate::identity::Match::Ambiguous(candidates) => {
                    tracing::error!(
                        control_account = %name, candidates = ?candidates,
                        "controlAccount matches more than one account — refusing to guess, running with NO control account"
                    );
                    None
                }
            }
        });
        // The control account is an Anthropic identity: the one Claude Code's
        // bootstrap calls resolve to, probed and `controlIdentity`-checked as one.
        // Codex has no counterpart in this version, so a Codex row named here is
        // warned about and ignored rather than half-honoured. A `match`,
        // so a third provider decides this here instead of inheriting an answer.
        let control_idx = control_idx.filter(|&idx| {
            accounts.get(idx).is_none_or(|account| match account.provider {
                crate::provider::Provider::Anthropic => true,
                crate::provider::Provider::Codex => {
                    tracing::warn!(
                        account = %account.name,
                        "controlAccount names a Codex account; {CODEX_HAS_NO_CONTROL_ACCOUNT}, ignoring it"
                    );
                    false
                }
            })
        });

        let usage = crate::usage::UsageTracker::new(accounts.len(), config.pricing_table());

        let manager = Arc::new(Self {
            me: std::sync::OnceLock::new(),
            forecast: Mutex::new(
                (0..accounts.len())
                    .map(|_| crate::forecast::Calibration::default())
                    .collect(),
            ),
            accounts: Arc::new(RwLock::new(accounts)),
            usage,
            refresher,
            prober,
            plan_prober: RwLock::new(Arc::new(crate::probe::LivePlanProber)),
            warmer,
            warm_in_flight: AtomicBool::new(false),
            auto_resets: Mutex::new(auto_reset::Ledger::default()),
            warm_wake: Notify::new(),
            config: Mutex::new(config),
            config_write: Arc::new(Mutex::new(())),
            config_path,
            mutation_released: AtomicBool::new(false),
            usage_gates: Mutex::new(HashMap::new()),
            wake: crate::wake::WakeDetector::new(),
            upstream,
            codex_upstream,
            proxy_api_key,
            request_key,
            unkeyed: crate::request_key::UnkeyedCounters::default(),
            global_threshold,
            fable_weekly_threshold,
            groups: RwLock::new(groups),
            mitm: std::sync::OnceLock::new(),
            fan: std::sync::OnceLock::new(),
            codex_refused: Mutex::new(CodexRefusals::default()),
            codex_unpooled: Mutex::new(MinuteRing::default()),
            groups_reload_mtime: Mutex::new(None),
            pacing,
            account_throttle,
            fleet_throttle,
            locked_idx,
            control_idx: RwLock::new(control_idx),
            org_tat_ms: AsyncMutex::new(HashMap::new()),
            fleet_tat_ms: AsyncMutex::new(HashMap::new()),
            log: Mutex::new(VecDeque::with_capacity(REQUEST_LOG_CAPACITY)),
            current: Mutex::new(None),
            current_by_provider: Mutex::new(crate::stats::CurrentByProvider::default()),
            select_seq: AtomicU64::new(1),
            affinity: Mutex::new(CappedMap::new(select::AFFINITY_CAP, |&(_, touch)| touch)),
            affinity_extended: Mutex::new(CappedSet::new(select::AFFINITY_CAP)),
            affinity_file: crate::cache_file::SavedFile::new(AFFINITY_TOUCH_REFRESH),
            bound_tokens: sessions::bound_tokens::ShardedBoundTokens::default(),
            bound_tokens_file: crate::cache_file::SavedFile::new(std::time::Duration::ZERO),
            bound_history_logged: Mutex::new(HashSet::new()),
            bound_switch_warned: Mutex::new(HashMap::new()),
            sessions: Mutex::new(CappedMap::new(snapshot::SESSION_CAP, |s| s.last_seen_ms)),
            wire_sessions: Mutex::new(crate::session_wire::WireSessionTracker::new()),
            wire_sessions_file: crate::cache_file::SavedFile::new(WIRE_SESSIONS_REFRESH),
            session_seq: AtomicU64::new(1),
            next_revalidation_at_ms: std::sync::atomic::AtomicI64::new(0),
            next_codex_revalidation_at_ms: std::sync::atomic::AtomicI64::new(0),
            conn_affinity: Mutex::new(CappedMap::new(select::CONN_AFFINITY_CAP, |&(_, touch)| {
                touch
            })),
            divert_ledger: Mutex::new(CappedMap::new(select::DIVERT_LEDGER_CAP, |e| e.until_ms)),
            control_reserve,
            control_pooled,
            control_identity,
            client_identities: crate::control_identity::ClientIdentityCache::new(),
            reset_urgency_tier_ms,
            routing: Mutex::new(crate::observations::Observations::default()),
        });
        // Hand the manager its own `Arc` (weakly) now that one exists — see the
        // `me` field. Infallible: this is the only write, on a just-built value.
        if manager.me.set(Arc::downgrade(&manager)).is_err() {
            tracing::warn!("manager self-reference was already set");
        }

        if let (Some(i), Some(name)) = (locked_idx, locked_name) {
            tracing::warn!(
                account = %name, idx = i,
                "ACCOUNT LOCK ACTIVE — this provider's traffic pinned to one account; rotation/affinity/migration bypassed, no failover"
            );
        }

        manager
    }

    /// Convenience constructor for the real binary (live OAuth refresher + live
    /// usage prober).
    pub fn with_live_refresher(config: Config, config_path: Option<PathBuf>) -> Arc<Self> {
        let prober = LiveUsageProber::new().with_codex_base(config.codex_upstream.clone());
        Self::new(
            config,
            Arc::new(LiveRefresher::new()),
            Arc::new(prober),
            Arc::new(LiveWarmer::new()),
            config_path,
        )
    }

    /// Build a manager from pre-seeded runtime rows, for the `tcr demo` dashboard
    /// (`src/demo.rs`). The live refresher/prober/warmer are held but NEVER invoked
    /// — the demo only ever calls [`Self::snapshot`] and [`Self::set_disabled`], so
    /// no token, probe, or network I/O ever happens. `config_path = None` makes
    /// every persist a no-op, so nothing can be written to disk.
    pub fn from_runtimes(accounts: Vec<AccountRuntime>) -> Arc<Self> {
        #[expect(
            clippy::expect_used,
            reason = "Config has no Default; an empty JSON object parses by construction and the signature has no error channel"
        )]
        let config: Config = serde_json::from_str("{}")
            .expect("an empty JSON object is always a valid default config");
        let prober = LiveUsageProber::new().with_codex_base(config.codex_upstream.clone());
        Self::assemble(
            config,
            Arc::new(LiveRefresher::new()),
            Arc::new(prober),
            Arc::new(LiveWarmer::new()),
            None,
            accounts,
        )
    }

    /// Seed one fake live session into the sessions map, for the `tcr demo`
    /// dashboard alone — never touched by the real serving path (which upserts
    /// sessions in [`Self::record_served`]). Lets the demo paint a populated
    /// sessions pane without routing any real traffic.
    pub fn seed_session(
        &self,
        key: u64,
        account_idx: usize,
        requests: u64,
        last_seen_ms: i64,
        kind: SessionKind,
    ) {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        sessions.insert(
            key,
            SessionStat {
                account_idx,
                requests,
                last_seen_ms,
                kind,
            },
        );
    }
}
