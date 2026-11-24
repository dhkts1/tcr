use super::*;

// ---------------------------------------------------------------------------
// The mapping this node holds open while `peer.internet` is on
// ---------------------------------------------------------------------------

/// How long a mapping is asked for, in seconds.
///
/// Two hours, renewed every [`MAPPING_RENEW_INTERVAL`], so a renewal that is
/// lost still leaves an hour and a half of mapping behind it. The pair of
/// numbers is the shape ruling here: the listener binds ONE stable
/// port and the router mapping is stable. The clock-derived port
/// ([`derived_port`]) is a rendezvous hint for the DIALLER and never something
/// this side rebinds or remaps.
pub const MAPPING_LIFETIME_SECS: u32 = 7_200;

/// How often the mapping is re-asked for: well inside
/// [`MAPPING_LIFETIME_SECS`], so three renewals in a row have to fail before a
/// peer loses the address it was told.
pub const MAPPING_RENEW_INTERVAL: Duration = Duration::from_mins(30);

/// How often the keeper wakes to see whether it has been asked to stop.
///
/// The renewal wait is slept in ticks of this rather than in one long sleep:
/// a shutdown must delete the mapping now, not up to half an hour from now.
pub const MAPPING_STOP_POLL: Duration = Duration::from_millis(250);

/// How long to wait before asking a router that would not map again, by how
/// many refusals in a row it has given.
///
/// The first wait is [`INTERNET_POLL_INTERVAL`], which is what every wait used
/// to be: a router with no mapping service on it was asked again every five
/// seconds, for as long as the process ran, and a day of that is thousands of
/// round trips and two log lines each. The last rung is the cap, so a router
/// that will never map is still re-asked often enough to notice the day it is
/// replaced or its settings change.
///
/// A ladder of fixed rungs rather than a doubling, so the wait an operator
/// sees is one of four numbers they can read here rather than an arithmetic
/// result, and so a test can assert the sequence.
pub const MAPPING_RETRY_LADDER: [Duration; 4] = [
    Duration::from_secs(5),
    Duration::from_secs(30),
    Duration::from_secs(120),
    Duration::from_secs(600),
];

/// How much of a wait passes before the default gateway is re-read, while a
/// refusal is being waited out.
///
/// Read on a clock of its own rather than at every wake, because reading it
/// runs a route-table tool: at the top of the ladder that is one subprocess a
/// half-minute instead of one every five seconds, and the whole point of the
/// ladder is to stop paying per wake for a router that has already answered.
pub const GATEWAY_RECHECK_INTERVAL: Duration = Duration::from_secs(30);

/// What the router last answered a mapping request with.
///
/// Two values rather than a `bool`, because both of them are logged and the
/// name is what the reader of the log sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingAnswer {
    /// A port was mapped.
    Mapped,
    /// No protocol would map one.
    Refused,
}

/// When a node whose router would not map may ask it again.
///
/// Pure, and driven by elapsed time handed in rather than by a clock it reads,
/// so the suite asserts the SEQUENCE of intervals rather than sleeping through
/// them.
///
/// # What it is for
///
/// `keep_internet_mapping` wakes every [`INTERNET_POLL_INTERVAL`] and starts a
/// keeper whenever none is running. A keeper whose router refuses dies within a
/// couple of seconds, so "none is running" was true at every single wake and
/// the router was asked forever, at five-second spacing, by a node that had
/// already been told no.
#[derive(Debug, Clone, Default)]
pub struct MappingRetry {
    /// How many times in a row the router would not map.
    refusals: u32,
    /// How long since the last request went out.
    waited: Duration,
    /// How long since the default gateway was last read.
    since_gateway_read: Duration,
    /// The gateway the last request went to, so a node that has moved to
    /// another network asks the new router straight away.
    gateway: Option<Ipv4Addr>,
}

impl MappingRetry {
    /// A ladder at its first rung, having asked nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// How long this node waits before asking again, given what it has been
    /// answered so far. Zero when nothing has been refused yet, which is the
    /// boot case: the first request goes out at once.
    pub fn interval(&self) -> Duration {
        if self.refusals == 0 {
            return Duration::ZERO;
        }
        let rung = usize::try_from(self.refusals)
            .unwrap_or(MAPPING_RETRY_LADDER.len())
            .min(MAPPING_RETRY_LADDER.len())
            .saturating_sub(1);
        MAPPING_RETRY_LADDER[rung]
    }

    /// Time has passed: `elapsed` more of it.
    pub const fn tick(&mut self, elapsed: Duration) {
        self.waited = self.waited.saturating_add(elapsed);
        self.since_gateway_read = self.since_gateway_read.saturating_add(elapsed);
    }

    /// May the router be asked now?
    pub fn due(&self) -> bool {
        self.waited >= self.interval()
    }

    /// Is enough of a wait behind this node to be worth re-reading the default
    /// gateway? Always false while nothing has been refused, since a node that
    /// is being answered has no ladder to reset.
    pub fn gateway_read_due(&self) -> bool {
        self.refusals > 0 && self.since_gateway_read >= GATEWAY_RECHECK_INTERVAL
    }

    /// The default gateway reads as `gateway` now. Answers whether that is a
    /// different router than the one that refused, which puts the ladder back
    /// on its first rung: a Mac carried to another network is one request away
    /// from a mapping, and making it wait out the old router's ten minutes is
    /// making it wait for nothing.
    pub fn gateway_is_now(&mut self, gateway: Option<Ipv4Addr>) -> bool {
        self.since_gateway_read = Duration::ZERO;
        let moved = self.gateway.is_some() && self.gateway != gateway;
        self.gateway = gateway;
        if moved {
            self.reset();
        }
        moved
    }

    /// A request is going out now.
    pub const fn asked(&mut self) {
        self.waited = Duration::ZERO;
    }

    /// The router answered `answer`, which is what moves the ladder: a refusal
    /// steps it up a rung and a mapping puts it back on the first one.
    ///
    /// Which LEVEL that answer is logged at is decided elsewhere, by
    /// [`mapping_answer_voice`], because the log is per gateway and per
    /// process while this ladder belongs to one loop.
    pub const fn answered(&mut self, answer: MappingAnswer) {
        match answer {
            MappingAnswer::Mapped => self.refusals = 0,
            MappingAnswer::Refused => self.refusals = self.refusals.saturating_add(1),
        }
    }

    /// Back to the first rung: the switch was toggled, or this Mac is on
    /// another router.
    pub const fn reset(&mut self) {
        self.refusals = 0;
        self.waited = Duration::ZERO;
    }

    /// How many refusals in a row are behind the current wait.
    pub const fn refusals(&self) -> u32 {
        self.refusals
    }
}

/// The last answer each gateway gave, so the same answer twice is not the same
/// line twice.
///
/// Process-wide and keyed by gateway, the shape `peer state restored` uses
/// (`crate::peer::state`, `restore_counts_changed`): the two writers are the
/// keeper thread and the loop that starts it, which have no channel between
/// them, and a Mac that moves to another router gets its own first line there.
static LAST_MAPPING_ANSWER: OnceLock<Mutex<HashMap<SocketAddr, MappingAnswer>>> = OnceLock::new();

/// How loudly a router's answer is worth saying.
///
/// The decision, named and returned, rather than a `bool` each log site reads
/// its own way: the two sites are three hundred lines apart and one of them
/// used to write a warning every five seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerVoice {
    /// Something an operator has not been told: one line at WARN.
    News,
    /// The same answer as last time: kept at DEBUG, or at INFO for a mapping,
    /// where the line is the ordinary record of a keeper starting.
    Repeat,
}

/// Record what `gateway` answered and say how loudly to say it.
///
/// The two answers are not symmetric, and that is the whole of this function:
/// a refusal is news unless the last answer from this router was also a
/// refusal, while a mapping is news ONLY when the last answer was a refusal,
/// because the first mapping after a boot is the ordinary case and not a thing
/// to warn anybody about.
pub fn mapping_answer_voice(gateway: SocketAddr, answer: MappingAnswer) -> AnswerVoice {
    let previous = LAST_MAPPING_ANSWER
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(gateway, answer);
    let news = match answer {
        MappingAnswer::Refused => previous != Some(MappingAnswer::Refused),
        MappingAnswer::Mapped => previous == Some(MappingAnswer::Refused),
    };
    if news {
        AnswerVoice::News
    } else {
        AnswerVoice::Repeat
    }
}

/// What a keeper did to the router, in the order it did it.
///
/// Recorded as a list rather than inferred from the router's state because the
/// ORDER is the contract: a renewal before the map, or a delete that never
/// ran, are both invisible in a snapshot of the mapping table and both are
/// exactly what this is here to catch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingStep {
    /// The first request, which created the mapping.
    Mapped,
    /// A refresh of the mapping this keeper already holds.
    Renewed,
    /// The lifetime-0 request that took it away again.
    Deleted,
    /// The first request, granted over UPnP IGD after NAT-PMP said nothing.
    ///
    /// Its own step and not a second `Mapped`, because which protocol answered
    /// is the fact an operator needs when a mapping appears on one router and
    /// not on another, and a step list that hid it would make the two routers
    /// look identical.
    MappedOverUpnp,
}

/// The UPnP search every production caller uses: the IGD multicast group.
///
/// Named here rather than written at each call site so `tcr peer reach` and the
/// mapping keeper cannot end up searching two different ways and reporting two
/// different answers about one router.
pub const fn upnp_discoverer() -> crate::peer::reach_upnp::Discoverer {
    crate::peer::reach_upnp::Discoverer::multicast()
}

/// What a UPnP mapping this node asked for is called in a router's own table.
///
/// An operator reading the admin page has to be able to tell which row is
/// theirs, and "tcr" alone would not say which of the two protocols wrote it.
const UPNP_MAPPING_DESCRIPTION: &str = "tcr peer internet (upnp)";

/// The external socket this node is currently mapped at, for the one producer
/// of `Hello.addrs` to read.
///
/// A process-local register rather than a parameter, and the reason is the
/// shape of the two sides: the keeper runs on its own thread for the life of
/// the listener, and `Hello.addrs` is assembled per connection in
/// `crate::peer::listener::NodeFacts::listening`, three call sites away with
/// no channel between them. Threading it through would put an
/// `Option<SocketAddr>` in every signature between the accept loop and the
/// handshake, which is a wider change than the fact deserves.
///
/// `None` is the default and the common case: no mapping asked for, or the
/// router refused one.
fn mapped_external() -> &'static Mutex<Option<SocketAddr>> {
    static MAPPED: OnceLock<Mutex<Option<SocketAddr>>> = OnceLock::new();
    MAPPED.get_or_init(|| Mutex::new(None))
}

/// The external socket a peer off this LAN could reach this node at, or
/// [`None`] when nothing is mapped.
pub fn external_socket() -> Option<SocketAddr> {
    match mapped_external().lock() {
        Ok(held) => *held,
        // A poisoned lock here means a keeper panicked mid-write. The value
        // behind it is still the last address the router confirmed, and
        // dropping it would silently stop advertising a live mapping.
        Err(poisoned) => *poisoned.into_inner(),
    }
}

/// Where a keeper writes the mapping it holds, or [`None`] in a process that
/// never asked for one.
///
/// A register rather than a parameter for the same reason [`mapped_external`]
/// is one: the keeper runs on its own thread for the life of the listener, and
/// the state file it should write to is known at boot, three call sites away.
/// Set by [`record_mappings_at`], and a process that never calls it records
/// nothing at all, which is what keeps every test that drives a keeper against
/// a fake gateway from writing to a real state file.
fn mapping_record_path() -> &'static Mutex<Option<std::path::PathBuf>> {
    static RECORD_AT: OnceLock<Mutex<Option<std::path::PathBuf>>> = OnceLock::new();
    RECORD_AT.get_or_init(|| Mutex::new(None))
}

/// Record every mapping this process holds in the state file at `path`, so
/// `tcr peer reach` can report it without asking the router.
///
/// Called once, at listener boot, beside the keeper it belongs to.
pub fn record_mappings_at(path: std::path::PathBuf) {
    let mut held = match mapping_record_path().lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    };
    *held = Some(path);
}

/// The external socket a live, unexpired mapping in the state file at
/// `state_path` records, read cold rather than off [`external_socket`].
///
/// `tcr peer invite` and `tcr peer moved mint` both run as their own,
/// short-lived process: neither spawns the keeper that fills
/// [`mapped_external`], so [`external_socket`] would read `None` in either
/// one every time, even with a mapping alive on the router right now. The
/// state file is the one place that mapping is written down for a process
/// that is not the one holding it, `Manager`'s own keeper saves it there on
/// every renewal, so this is the single place both verbs read it back from.
///
/// The second element is the sentence to say when a record exists but yields
/// no socket to dial, in the caller's own voice: this function only ever
/// returns the fact, never prints it, so `mint`'s wording and `invite`'s stay
/// two calls of the one function, not two copies of the one sentence.
pub fn recorded_external_socket(
    state_path: &Path,
    now_ms: i64,
) -> (Option<SocketAddr>, Option<String>) {
    let Some(record) = crate::peer::state::load(state_path, now_ms)
        .ok()
        .and_then(|state| state.mapping)
        .filter(|record| record.expires_at_ms > now_ms)
    else {
        return (None, None);
    };

    match record.external_address.as_deref() {
        Some(text) => match text.parse::<SocketAddr>() {
            Ok(addr) => (Some(addr), None),
            Err(why) => (
                None,
                Some(format!(
                    "the held mapping's external address did not parse ({why})"
                )),
            ),
        },
        None => (
            None,
            Some(
                "the router mapped a port and would not name its own external address".to_string(),
            ),
        ),
    }
}

/// The gateway's view of this Mac's address on the internet, NAT-PMP first
/// and UPnP IGD when NAT-PMP refuses: the same fallback order
/// [`MappingKeeper::map`] runs for the mapping itself, so `tcr peer reach`
/// never claims a router answers nothing when it answers over the one
/// protocol NAT-PMP is not.
///
/// The label says which protocol answered, because a router that answers
/// only one of the two is exactly the case an operator runs this verb to
/// find out about.
pub fn external_address_report(
    client: &NatPmp,
    discoverer: &crate::peer::reach_upnp::Discoverer,
) -> String {
    let nat_pmp_err = match client.external_address() {
        Ok(found) => return format!("{} (nat-pmp)", found.addr),
        Err(err) => err,
    };
    match crate::peer::reach_upnp::UpnpClient::discover(discoverer)
        .and_then(|upnp| upnp.get_external_address())
    {
        Ok(addr) => format!("{addr} (upnp)"),
        Err(upnp_err) => format!("unavailable: {nat_pmp_err} (nat-pmp), {upnp_err} (upnp)"),
    }
}

/// Publish (or withdraw) the external socket [`external_socket`] reports, and
/// record it where a CLI can read it.
///
/// The two go together on purpose: an advertised address and a recorded one
/// that disagree would have `tcr peer reach` describe a mapping no `Hello`
/// carries, which is worse than either alone.
///
/// A failed record is logged and never propagated. The mapping itself is
/// granted and in use by then, and refusing to serve because a report could
/// not be written would trade a working listener for a status line.
fn publish_external(addr: Option<SocketAddr>, held_mapping: Option<&Mapping>) {
    let mut held = match mapped_external().lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    };
    *held = addr;
    drop(held);

    let path = match mapping_record_path().lock() {
        Ok(path) => path.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    let Some(path) = path else {
        return;
    };
    let record = held_mapping.map(|mapping| crate::peer::state::MappingRecord {
        external_address: addr.map(|addr| addr.to_string()),
        external_port: mapping.external_port,
        internal_port: mapping.internal_port,
        expires_at_ms: crate::now_ms()
            .saturating_add(i64::from(mapping.lifetime_secs).saturating_mul(1_000)),
    });
    if let Err(err) = crate::peer::state::save_mapping(&path, record) {
        tracing::warn!(
            error = %err,
            "peer internet: the mapping is held, but it could not be recorded, so \
             `tcr peer reach` will report none until the next renewal"
        );
    }
}

/// A NAT-PMP client holding one mapping open, with the steps it took.
///
/// Synchronous, like [`NatPmp`] itself, so the suite can drive a whole
/// map/renew/delete cycle against a fake gateway on loopback with no runtime
/// and no network.
#[derive(Debug)]
pub struct MappingKeeper {
    client: NatPmp,
    internal_port: u16,
    lifetime_secs: u32,
    held: Option<Mapping>,
    steps: Vec<MappingStep>,
    /// Where to look for a UPnP IGD gateway when NAT-PMP says nothing, or
    /// [`None`] for a keeper that only speaks NAT-PMP.
    ///
    /// Injected rather than built here so the suite can point it at a fake on
    /// loopback: the alternative is a multicast search in a test, which is the
    /// one thing this whole module is arranged to avoid.
    upnp: Option<crate::peer::reach_upnp::Discoverer>,
    /// The UPnP client that granted the held mapping, kept for the renewal and
    /// the delete.
    ///
    /// Held rather than rediscovered, because a second search could land on a
    /// different device and then the renewal would extend a mapping on one
    /// router while the delete took one away on another.
    upnp_client: Option<crate::peer::reach_upnp::UpnpClient>,
}

/// Which protocol granted the mapping a [`MappingKeeper`] holds.
///
/// # Why the keeper has to remember, and what it cost not to
///
/// A mapping is not a fact about a port, it is a fact about a CONVERSATION
/// with one gateway: the renewal and the delete have to go back to the
/// protocol that granted it. Before this existed, `renew` and `delete` spoke
/// NAT-PMP unconditionally, so a UPnP mapping was renewed by asking a silent
/// NAT-PMP gateway (which fails, is logged, and the loop carries on) and
/// deleted the same way. The visible consequences were a mapping that lapsed
/// after [`MAPPING_LIFETIME_SECS`] with peers still holding the address, and a
/// forward left standing on the router at shutdown, pointing at a listener
/// that had stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappedOver {
    /// NAT-PMP answered the first request.
    NatPmp,
    /// The router had no NAT-PMP service, by silence or by refusing the probe,
    /// and UPnP IGD answered.
    Upnp,
}

impl MappingKeeper {
    /// A keeper for `internal_port` against `client`, holding nothing yet.
    pub const fn new(client: NatPmp, internal_port: u16, lifetime_secs: u32) -> Self {
        Self {
            client,
            internal_port,
            lifetime_secs,
            held: None,
            steps: Vec::new(),
            upnp: None,
            upnp_client: None,
        }
    }

    /// Fall back to UPnP IGD, through `discoverer`, when the router has no
    /// NAT-PMP service: [`ReachError::no_natpmp_service`] decides which
    /// answers those are.
    ///
    /// Silence and a refused probe, and nothing else. A gateway that READ the
    /// request and answered no has made a decision this node does not get to
    /// route around by asking a second protocol; a gateway that says nothing,
    /// or refuses the packet outright, is one that may not speak NAT-PMP at
    /// all, which is the case UPnP exists for here.
    #[must_use]
    pub const fn with_upnp(mut self, discoverer: crate::peer::reach_upnp::Discoverer) -> Self {
        self.upnp = Some(discoverer);
        self
    }

    /// Ask the router for the mapping.
    ///
    /// The RFC's recommended first attempt: suggest the internal port, and
    /// advertise whatever the gateway hands back instead.
    pub fn map(&mut self) -> Result<Mapping, ReachError> {
        let asked = self.client.map(
            MapProtocol::Tcp,
            self.internal_port,
            self.internal_port,
            self.lifetime_secs,
        );
        let mapping = match asked {
            Ok(mapping) => {
                self.steps.push(MappingStep::Mapped);
                mapping
            }
            // A router with no NAT-PMP service on it, by silence or by
            // refusal, is the only thing UPnP is asked about.
            Err(err) if err.no_natpmp_service() && self.upnp.is_some() => {
                let mapping = self.map_over_upnp()?;
                self.steps.push(MappingStep::MappedOverUpnp);
                mapping
            }
            Err(err) => return Err(err),
        };
        self.held = Some(mapping);
        Ok(mapping)
    }

    /// Ask a UPnP IGD gateway for the same mapping NAT-PMP would not answer
    /// about, and record the external address it names.
    ///
    /// Every refusal keeps its own name on the way out
    /// ([`crate::peer::reach_upnp::UpnpError`] is `Display`), because the three
    /// this client invents rather than reads off the wire,
    /// `UntrustedLocation`, `ControlUrlElsewhere` and `Redirected`, are the
    /// ones an operator cannot diagnose from "no mapping": each says a device
    /// answered and was refused BY THIS NODE, and for a reason worth reading.
    fn map_over_upnp(&mut self) -> Result<Mapping, ReachError> {
        use crate::peer::reach_upnp::UpnpClient;

        let discoverer = self
            .upnp
            .as_ref()
            .ok_or_else(|| ReachError::Malformed("peer internet: no UPnP discoverer".into()))?;
        // A UPnP silence stays a SILENCE on the way out, not a "the gateway's
        // answer was not readable": both protocols saying nothing is the one
        // outcome an operator fixes by looking at the router, and `tcr peer
        // reach` reads this variant to print exactly that. Every other refusal
        // keeps its own words, `UntrustedLocation`, `ControlUrlElsewhere` and
        // `Redirected` included, because each of those says a device answered
        // and THIS NODE refused it.
        let gateway = discoverer.destination();
        let named = move |err: crate::peer::reach_upnp::UpnpError| match err {
            crate::peer::reach_upnp::UpnpError::Silent { .. } => {
                ReachError::Silent { gateway, tries: 1 }
            }
            other => ReachError::Malformed(other.to_string()),
        };

        let client = UpnpClient::discover(discoverer).map_err(named)?;
        let internal = self.client.internal_address().map_err(|err| {
            ReachError::Malformed(format!(
                "peer internet: UPnP needs this Mac's address on the LAN and it could not be read: {err}"
            ))
        })?;
        client
            .add_port_mapping(
                MapProtocol::Tcp,
                self.internal_port,
                self.internal_port,
                internal,
                UPNP_MAPPING_DESCRIPTION,
                self.lifetime_secs,
            )
            .map_err(named)?;
        let external = client.get_external_address().map_err(named)?;

        tracing::info!(
            external = %external,
            port = self.internal_port,
            "peer internet: this router has no NAT-PMP and UPnP IGD mapped this node's listener \
             port"
        );
        // UPnP IGD confirms the port asked for or refuses, so there is no
        // granted-port field to read back the way NAT-PMP has one.
        let mapping = Mapping {
            protocol: MapProtocol::Tcp,
            internal_port: self.internal_port,
            external_port: self.internal_port,
            lifetime_secs: self.lifetime_secs,
            epoch_secs: 0,
        };
        // Published with the mapping beside it rather than the address alone:
        // the record and the advertised address are written together, so a
        // reader can never see one without the other.
        publish_external(
            Some(SocketAddr::new(IpAddr::V4(external), self.internal_port)),
            Some(&mapping),
        );
        // Kept, so the renewal and the delete go back to the gateway that
        // granted this, over the protocol that granted it.
        self.upnp_client = Some(client);
        Ok(mapping)
    }

    /// Which protocol granted the mapping this keeper holds, or [`None`] when
    /// it holds none.
    pub fn granted_by(&self) -> Option<MappedOver> {
        self.held?;
        match self.upnp_client {
            Some(_) => Some(MappedOver::Upnp),
            None => Some(MappedOver::NatPmp),
        }
    }

    /// The external socket to advertise for the mapping this keeper holds,
    /// asked of the gateway that granted it.
    ///
    /// [`None`] when nothing is held, or when the gateway mapped the port and
    /// would not name its external address: an external port with no address
    /// is not something a peer can dial, so nothing is published rather than
    /// half of one.
    pub fn external_socket(&self) -> Option<SocketAddr> {
        let held = self.held?;
        match (&self.upnp_client, self.granted_by()?) {
            (Some(client), MappedOver::Upnp) => match client.get_external_address() {
                Ok(external) => Some(SocketAddr::new(IpAddr::V4(external), held.external_port)),
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "peer internet: the UPnP gateway mapped a port but would not name its \
                         external address, so no mapped endpoint is advertised"
                    );
                    None
                }
            },
            _ => external_socket_for(&self.client, &held),
        }
    }

    /// Re-ask for the mapping this keeper holds, extending its lifetime.
    ///
    /// Refuses rather than silently mapping when there is nothing held: a
    /// renewal that quietly became a first map would hide a keeper whose
    /// initial request never ran.
    ///
    /// Routed by [`MappedOver`]: a UPnP mapping is renewed by re-issuing
    /// `AddPortMapping` with the same arguments, which is how IGD extends a
    /// lease, and asking NAT-PMP to renew it instead would be a request to a
    /// gateway that never answered in the first place.
    pub fn renew(&mut self) -> Result<Mapping, ReachError> {
        let Some(held) = self.held else {
            return Err(ReachError::Malformed(
                "peer internet: a renewal was asked for before anything was mapped".into(),
            ));
        };
        if let Some(client) = self.upnp_client.clone() {
            return self.renew_over_upnp(&client, held);
        }
        let mapping = self.client.renew(&held, self.lifetime_secs)?;
        self.held = Some(mapping);
        self.steps.push(MappingStep::Renewed);
        Ok(mapping)
    }

    /// Take the mapping away, and record that it went.
    ///
    /// A keeper holding nothing deletes nothing and says so with `Ok(None)`:
    /// at shutdown that is the normal shape for a node whose router refused
    /// the first request.
    ///
    /// Routed by [`MappedOver`], for the reason that enum exists: a UPnP
    /// mapping deleted over NAT-PMP is a forward left on the router, pointing
    /// at a listener this process has just stopped running.
    pub fn delete(&mut self) -> Result<Option<Mapping>, ReachError> {
        let Some(held) = self.held else {
            return Ok(None);
        };
        if let Some(client) = &self.upnp_client {
            client
                .delete_port_mapping(MapProtocol::Tcp, held.external_port)
                .map_err(|err| ReachError::Malformed(err.to_string()))?;
            self.held = None;
            self.upnp_client = None;
            self.steps.push(MappingStep::Deleted);
            return Ok(Some(held));
        }
        let confirmed = self.client.delete(MapProtocol::Tcp, self.internal_port)?;
        self.held = None;
        self.steps.push(MappingStep::Deleted);
        Ok(Some(confirmed))
    }

    /// Re-issue the UPnP `AddPortMapping` that granted the held mapping.
    ///
    /// Split out so [`Self::renew`] reads as the routing decision it is. The
    /// arguments are the ones the first grant used, which is what makes this a
    /// lease extension rather than a second mapping: IGD keys a mapping on
    /// (remote host, external port, protocol), and all three are unchanged.
    fn renew_over_upnp(
        &mut self,
        client: &crate::peer::reach_upnp::UpnpClient,
        held: Mapping,
    ) -> Result<Mapping, ReachError> {
        let internal = self.client.internal_address().map_err(|err| {
            ReachError::Malformed(format!(
                "peer internet: a UPnP renewal needs this Mac's address on the LAN and it \
                 could not be read: {err}"
            ))
        })?;
        client
            .add_port_mapping(
                MapProtocol::Tcp,
                held.external_port,
                held.internal_port,
                internal,
                UPNP_MAPPING_DESCRIPTION,
                self.lifetime_secs,
            )
            .map_err(|err| ReachError::Malformed(err.to_string()))?;
        let renewed = Mapping {
            lifetime_secs: self.lifetime_secs,
            ..held
        };
        self.held = Some(renewed);
        self.steps.push(MappingStep::Renewed);
        Ok(renewed)
    }

    /// The mapping this keeper holds, as the gateway granted it.
    pub const fn held(&self) -> Option<Mapping> {
        self.held
    }

    /// What this keeper did, in order.
    pub fn steps(&self) -> &[MappingStep] {
        &self.steps
    }
}

/// Hold a mapping open until `stop` is set, then take it away.
///
/// Blocking on purpose: the caller runs this on its own thread
/// ([`spawn_mapping_keeper`]), and every socket under it is a blocking
/// [`UdpSocket`] with a read timeout.
///
/// Returns the steps it took. A gateway that refuses the FIRST request is an
/// error the caller reports, because nothing was ever mapped and there is
/// nothing to keep; a refusal on a later renewal is logged and the loop stays
/// up, since the mapping already granted outlives several missed renewals and
/// giving up would throw away an address peers are already using.
pub fn run_mapping(
    client: NatPmp,
    internal_port: u16,
    lifetime_secs: u32,
    renew_every: Duration,
    stop: &AtomicBool,
) -> Result<Vec<MappingStep>, ReachError> {
    run_mapping_with_upnp(
        client,
        None,
        internal_port,
        lifetime_secs,
        renew_every,
        stop,
    )
}

/// [`run_mapping`], with a UPnP IGD gateway to fall back on when NAT-PMP says
/// nothing.
///
/// Separate entry point rather than a sixth parameter on `run_mapping`, so the
/// NAT-PMP-only tests that drive the keeper against a fake gateway keep their
/// call unchanged and cannot accidentally acquire a multicast search.
pub fn run_mapping_with_upnp(
    client: NatPmp,
    upnp: Option<crate::peer::reach_upnp::Discoverer>,
    internal_port: u16,
    lifetime_secs: u32,
    renew_every: Duration,
    stop: &AtomicBool,
) -> Result<Vec<MappingStep>, ReachError> {
    run_mapping_while(
        client,
        upnp,
        internal_port,
        lifetime_secs,
        renew_every,
        stop,
        &|| true,
    )
}

/// [`run_mapping_with_upnp`], with a second way to end the loop: a predicate
/// the keeper asks between renewals.
///
/// # Why a keeper has to be able to end itself
///
/// `tcr peer internet off` runs in the CLI process and writes the flag; the
/// keeper lives in the SERVING process and used to read `peer.internet`
/// exactly once, at boot. So the CLI deleted the mapping over NAT-PMP and the
/// keeper's next renewal created it again, up to half an hour later: the
/// operator turned the switch off, saw `tcr peer reach` confirm it, and this
/// Mac went on being reachable from the internet until it was restarted. A
/// UPnP-granted mapping was worse, since the CLI's NAT-PMP delete never took
/// that one away at all.
///
/// Ending the loop here is also what deletes over the RIGHT protocol:
/// [`MappingKeeper::delete`] routes by [`MappedOver`], and it runs on every
/// exit path below.
///
/// The predicate is asked on the stop-poll tick, not once per renewal, so the
/// switch takes effect in about a second rather than in up to
/// [`MAPPING_RENEW_INTERVAL`].
pub fn run_mapping_while(
    client: NatPmp,
    upnp: Option<crate::peer::reach_upnp::Discoverer>,
    internal_port: u16,
    lifetime_secs: u32,
    renew_every: Duration,
    stop: &AtomicBool,
    keep_going: &dyn Fn() -> bool,
) -> Result<Vec<MappingStep>, ReachError> {
    let gateway = client.gateway();
    let mut keeper = MappingKeeper::new(client, internal_port, lifetime_secs);
    if let Some(discoverer) = upnp {
        keeper = keeper.with_upnp(discoverer);
    }
    let mapping = keeper.map()?;
    // Published through the keeper, which asks the gateway that granted the
    // mapping. The UPnP path has already published the address it was given
    // inside `map_over_upnp`, so re-publishing here would ask the router for
    // its external address a second time in the same second; worse, the line
    // this replaces asked NAT-PMP, which had just been silent, and wrote the
    // `None` that answer produces straight over the real address.
    if keeper.granted_by() != Some(MappedOver::Upnp) {
        publish_external(keeper.external_socket(), Some(&mapping));
    }
    // A router that was refusing and now maps is the one mapping event worth a
    // warning: the operator changed something, or this Mac is somewhere else,
    // and either way what they were told last is no longer true.
    if mapping_answer_voice(gateway, MappingAnswer::Mapped) == AnswerVoice::News {
        tracing::warn!(
            external_port = mapping.external_port,
            internal_port = mapping.internal_port,
            lifetime_secs = mapping.lifetime_secs,
            "peer internet: the router mapped this node's listener port"
        );
    } else {
        tracing::info!(
            external_port = mapping.external_port,
            internal_port = mapping.internal_port,
            lifetime_secs = mapping.lifetime_secs,
            "peer internet: the router mapped this node's listener port"
        );
    }

    while !stop.load(Ordering::SeqCst) {
        if !keep_going() {
            tracing::info!(
                "peer internet: the switch is off, so the mapping this node holds is deleted \
                 and the keeper stops"
            );
            break;
        }
        if !sleep_until_while(renew_every, stop, keep_going) {
            break;
        }
        match keeper.renew() {
            Ok(renewed) => {
                publish_external(keeper.external_socket(), Some(&renewed));
            }
            Err(err) => tracing::warn!(
                error = %err,
                "peer internet: a mapping renewal failed; the mapping already granted stands \
                 until its lifetime runs out and the next renewal is tried anyway"
            ),
        }
    }

    // The delete runs on every exit path, including the one where a renewal
    // has been failing: leaving a mapping behind is a port on the router that
    // forwards to a listener this node has just stopped running.
    publish_external(None, None);
    if let Err(err) = keeper.delete() {
        tracing::warn!(
            error = %err,
            "peer internet: the router did not confirm the mapping was deleted; it expires \
             on its own within {lifetime_secs}s"
        );
    }
    Ok(keeper.steps().to_vec())
}

/// Sleep `wait` in [`MAPPING_STOP_POLL`] ticks, answering whether the full
/// wait elapsed (`true`) or it was cut short (`false`), by `stop` being set or
/// by `keep_going` answering no.
fn sleep_until_while(wait: Duration, stop: &AtomicBool, keep_going: &dyn Fn() -> bool) -> bool {
    let deadline = std::time::Instant::now() + wait;
    let mut ticks: u32 = 0;
    loop {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        // Every [`FLAG_POLL_TICKS`] ticks rather than every tick: the
        // predicate reads a file, and a quarter-second file read for half an
        // hour is a cost with no reader.
        if ticks.is_multiple_of(FLAG_POLL_TICKS) && !keep_going() {
            return false;
        }
        ticks = ticks.saturating_add(1);
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return true;
        }
        std::thread::sleep(left.min(MAPPING_STOP_POLL));
    }
}

/// How many [`MAPPING_STOP_POLL`] ticks pass between two reads of
/// `peer.internet`. Eight ticks is two seconds, which is how long
/// `tcr peer internet off` takes to be believed by a running server.
const FLAG_POLL_TICKS: u32 = 8;

/// The external socket to advertise for one mapping: the gateway's own
/// external address, with the port the gateway granted.
///
/// [`None`] when the gateway will not say what its external address is, which
/// is a normal answer from a router that mapped the port anyway: an external
/// port with no address is not something a peer can dial, so nothing is
/// published rather than half of one.
fn external_socket_for(client: &NatPmp, mapping: &Mapping) -> Option<SocketAddr> {
    match client.external_address() {
        Ok(external) => Some(SocketAddr::new(
            IpAddr::V4(external.addr),
            mapping.external_port,
        )),
        Err(err) => {
            tracing::warn!(
                error = %err,
                "peer internet: the router mapped a port but would not name its external \
                 address, so no mapped endpoint is advertised"
            );
            None
        }
    }
}

/// A running keeper. Dropping it stops the thread, which deletes the mapping.
///
/// The guard is what makes "deletes the mapping at shutdown" true on the path
/// that actually happens: the listener's future is DROPPED at shutdown rather
/// than run to completion (`src/peer/listener.rs` says so of its ledger
/// flusher), so a delete written after the accept loop would never run.
#[derive(Debug)]
pub struct MappingGuard {
    stop: Arc<AtomicBool>,
    /// Cleared by the keeper thread on its way out, so a holder can tell a
    /// keeper it stopped itself from one it is still holding.
    ///
    /// The keeper has a second way to end: `run_mapping_while`'s own re-read
    /// of `peer.internet`, which is what `tcr peer internet off` reaches it
    /// through. A holder that assumed a guard meant a running keeper would
    /// answer [`KeeperStep::Renew`] for a thread that had already gone, and
    /// the switch turned back on would start nothing.
    alive: Arc<AtomicBool>,
}

impl MappingGuard {
    /// Stop the keeper without waiting for it, for a caller that wants the
    /// delete to start before it drops the rest of its state.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// A handle on a running mapping keeper, which stops its keeper when it is
/// dropped.
///
/// A trait rather than [`MappingGuard`] itself so [`keep_internet_mapping`]
/// can be driven with a fake in a test: the production starter asks this
/// machine's real router for a mapping, which is the one thing the suite here
/// may never do.
pub trait KeeperHandle {
    /// Is the keeper behind this handle still running?
    fn is_running(&self) -> bool;
}

impl KeeperHandle for MappingGuard {
    fn is_running(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
}

impl Drop for MappingGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What a serving process decided about asking its router for a mapping.
///
/// A typed answer, built once and read by the boot line, the log and the gate,
/// rather than a `bool` each caller re-derives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingBoot {
    /// `peer.internet` is off: nothing is asked for.
    SwitchOff,
    /// The listener is bound to loopback, so a router mapping would forward a
    /// public port at a socket no packet from outside this Mac can reach.
    LoopbackOnly,
    /// A mapping is wanted for this port.
    Wanted {
        /// The port the listener is really bound on, which is the one mapped:
        /// a listener configured on port 0 maps what the kernel gave it.
        internal_port: u16,
    },
}

/// How many times each [`MappingBoot`] has been decided in this process.
///
/// Counters rather than "the last one", because several listeners can boot in
/// one process (every test in a suite binary does) and a gate has to be able
/// to say "a serving process asked for this one" without racing a sibling.
///
/// The decision is what a boot gate can read without a router in the room: the
/// alternative is a test that lets a keeper talk to whatever gateway the
/// machine running the suite happens to sit behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MappingBootCounts {
    /// `peer.internet` was off.
    pub switch_off: u64,
    /// The listener was bound to loopback.
    pub loopback_only: u64,
    /// A keeper was started.
    pub wanted: u64,
}

static BOOTS_SWITCH_OFF: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static BOOTS_LOOPBACK_ONLY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static BOOTS_WANTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Every mapping decision this process has taken, counted by kind.
pub fn mapping_boot_counts() -> MappingBootCounts {
    use std::sync::atomic::Ordering as AtomicOrdering;
    MappingBootCounts {
        switch_off: BOOTS_SWITCH_OFF.load(AtomicOrdering::SeqCst),
        loopback_only: BOOTS_LOOPBACK_ONLY.load(AtomicOrdering::SeqCst),
        wanted: BOOTS_WANTED.load(AtomicOrdering::SeqCst),
    }
}

/// Count one decision.
fn count_mapping_boot(decision: MappingBoot) {
    use std::sync::atomic::Ordering as AtomicOrdering;
    match decision {
        MappingBoot::SwitchOff => &BOOTS_SWITCH_OFF,
        MappingBoot::LoopbackOnly => &BOOTS_LOOPBACK_ONLY,
        MappingBoot::Wanted { .. } => &BOOTS_WANTED,
    }
    .fetch_add(1, AtomicOrdering::SeqCst);
}

/// Whether a serving process bound at `local` should ask its router for a
/// mapping, given `peer.internet`.
///
/// Pure, so both arms are testable without a router.
pub const fn mapping_boot(internet: bool, local: SocketAddr) -> MappingBoot {
    if !internet {
        return MappingBoot::SwitchOff;
    }
    if local.ip().is_loopback() {
        return MappingBoot::LoopbackOnly;
    }
    MappingBoot::Wanted {
        internal_port: local.port(),
    }
}

/// Ask this machine's router to map the peer listener's port, when
/// `peer.internet` says so, and hold it open until the returned guard drops.
///
/// **The one function a serving process calls for the internet half**, and the
/// reason it exists: `listener::serve` did all of this inline and
/// `server::boot_peer_listener` boots through `bind` + `serve_on_with`
/// instead, so the shipped proxy read `peer.internet` nowhere, mapped no port,
/// and `external_socket()` was `None` on every Mac. What that cost was not
/// only a node nobody outside the LAN could dial: it is also the input to
/// `tunnel::reverse_carry_is_wanted`, so every Mac decided it could not be
/// dialled and parked a carrier at every friend.
pub fn start_peer_mapping(
    peers_path: &Path,
    internet: bool,
    local: SocketAddr,
) -> Option<MappingGuard> {
    let decision = mapping_boot(internet, local);
    count_mapping_boot(decision);
    match decision {
        MappingBoot::SwitchOff => {
            tracing::debug!(
                "peer internet: off, so this node asks its router for nothing (`tcr peer \
                 internet on` turns it on)"
            );
            None
        }
        MappingBoot::LoopbackOnly => {
            tracing::info!(
                peer_listen = %local,
                "peer internet: on, but this listener is bound to loopback, so a router \
                 mapping would point at a socket nothing off this Mac can reach; none is \
                 asked for"
            );
            None
        }
        MappingBoot::Wanted { internal_port } => {
            // Where the keeper records what it holds, set before it starts:
            // the state file beside THIS peers file. A CLI cannot read a
            // thread's memory, so this record is the only way `tcr peer reach`
            // reports the live mapping without asking the router and changing
            // the table it is reporting on.
            record_mappings_at(crate::peer::state::peer_state_path(peers_path));
            spawn_mapping_keeper(peers_path, internal_port)
        }
    }
}

/// Start a keeper for `internal_port` on this machine's default gateway.
///
/// Returns [`None`] when there is no gateway to ask, which is a normal
/// outcome and not a failure of this node: the listener goes on serving the
/// LAN exactly as before, and the operator sees the reason in the log.
pub fn spawn_mapping_keeper(peers_path: &Path, internal_port: u16) -> Option<MappingGuard> {
    let client = match NatPmp::on_default_gateway() {
        Ok(client) => client,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "peer internet: on, but this Mac has no NAT-PMP gateway to ask for a mapping; \
                 a peer off this LAN can still reach it over IPv6 if both ends have one"
            );
            return None;
        }
    };
    let gateway = client.gateway();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let alive = Arc::new(AtomicBool::new(true));
    let thread_alive = Arc::clone(&alive);
    let peers_path = peers_path.to_path_buf();
    std::thread::Builder::new()
        .name("tcr-peer-mapping".to_string())
        .spawn(move || {
            // Cleared however this thread leaves, including the panic path:
            // a guard that still said "running" after its keeper died would
            // make the switch unstartable for the life of the process.
            let _running = RunningUntilDropped(thread_alive);
            if let Err(err) = run_mapping_while(
                client,
                Some(upnp_discoverer()),
                internal_port,
                MAPPING_LIFETIME_SECS,
                MAPPING_RENEW_INTERVAL,
                &thread_stop,
                &|| internet_is_on(&peers_path),
            ) {
                // The same answer from the same router is a line nobody reads
                // twice, and this one used to be written every five seconds
                // for as long as the process ran.
                if mapping_answer_voice(gateway, MappingAnswer::Refused) == AnswerVoice::Repeat {
                    tracing::debug!(
                        error = %err,
                        "peer internet: the router would not map this node's listener port"
                    );
                } else {
                    tracing::warn!(
                        error = %err,
                        "peer internet: the router would not map this node's listener port"
                    );
                }
            }
        })
        .map_err(|err| {
            tracing::warn!(
                error = %err,
                "peer internet: could not start the mapping keeper thread"
            );
            err
        })
        .ok()?;
    Some(MappingGuard { stop, alive })
}

/// Clears an "is it running" flag however the thread holding it leaves.
struct RunningUntilDropped(Arc<AtomicBool>);

impl Drop for RunningUntilDropped {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// How often a serving process re-reads `peer.internet`.
///
/// The same shape the beacon reads `peer.find` with, and a shorter interval
/// because this one waits on a router round trip afterwards rather than on a
/// multicast announce. `tcr peer internet off` is not waiting on this: the CLI
/// deletes the mapping itself and the keeper's own poll notices in about a
/// second. What waits on this is `on`, which nothing else can act on.
pub const INTERNET_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// What a serving process should do with its mapping keeper this wake.
///
/// A typed answer over the two facts, the switch and whether a keeper is
/// running, so both can be decided without a router, a thread or a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeeperStep {
    /// A mapping is wanted and nothing is holding one: start a keeper.
    Start,
    /// A mapping is wanted and a keeper holds one: it renews on its own.
    Renew,
    /// A mapping is not wanted and a keeper holds one: stop it, which deletes
    /// the mapping the router granted.
    Stop,
    /// Nothing is wanted and nothing is running.
    Idle,
}

/// Decide [`KeeperStep`] from the boot decision as the file states it NOW and
/// whether a keeper is running.
///
/// # Why a live server has to keep asking
///
/// `tcr peer internet off` reached a running server (the keeper re-reads the
/// switch between renewals, and the CLI deletes the mapping itself), and
/// `internet on` reached nothing at all: `set_internet` writes the flag and
/// `start_peer_mapping` had only boot callers, so the mapping came back at the
/// next restart and not before. `docs/cli.md` said a running server honours
/// both without one.
pub const fn keeper_step(decision: MappingBoot, running: bool) -> KeeperStep {
    match (decision, running) {
        (MappingBoot::Wanted { .. }, false) => KeeperStep::Start,
        (MappingBoot::Wanted { .. }, true) => KeeperStep::Renew,
        // A listener bound to loopback is the same answer as the switch being
        // off: a mapping would forward a public port at a socket nothing off
        // this Mac can reach.
        (MappingBoot::SwitchOff | MappingBoot::LoopbackOnly, true) => KeeperStep::Stop,
        (MappingBoot::SwitchOff | MappingBoot::LoopbackOnly, false) => KeeperStep::Idle,
    }
}

/// What one run of [`keep_internet_mapping`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InternetWatch {
    /// Keepers started, not counting one the caller handed in.
    pub started: usize,
    /// Keepers stopped because the switch went off.
    pub stopped: usize,
    /// Wakes where a keeper was wanted, none was running, and the router that
    /// refused the last one was left alone: the ladder's whole effect, counted
    /// so a test can see it without a clock.
    pub deferred: usize,
}

/// Keep this process's mapping keeper in step with `peer.internet`, for as
/// long as the process serves.
///
/// `held` is the keeper the caller already started at boot, or [`None`]. The
/// loop takes ownership of it: dropping the handle is what stops a keeper, so
/// the guard has to live here rather than beside the accept loop.
///
/// `start` is a parameter for the reason [`KeeperHandle`] is a trait: the
/// production one asks this machine's real router, and a test may not.
///
/// `rounds` bounds the loop so a test can drive it; [`None`] is the production
/// shape, which is forever.
///
/// # A router that says no is asked less often
///
/// A keeper whose router refuses the first request ends within a couple of
/// seconds, so "a mapping is wanted and none is running" was true at every
/// wake and this loop asked the same router every five seconds for the life of
/// the process: a day of that is one pair of log lines every five seconds and
/// nothing gained. [`MappingRetry`] spaces the asks out instead, along
/// [`MAPPING_RETRY_LADDER`], and puts them back at five seconds the moment the
/// answer could have changed: the switch toggled, this Mac moved to another
/// router, or a request was granted.
pub async fn keep_internet_mapping<H, S>(
    peers_path: std::path::PathBuf,
    local: SocketAddr,
    every: Duration,
    rounds: Option<usize>,
    mut start: S,
    mut held: Option<H>,
) -> InternetWatch
where
    H: KeeperHandle,
    S: FnMut() -> Option<H>,
{
    let mut tally = InternetWatch::default();
    // The ladder that decides when a router which would not map is asked
    // again. A keeper handed in is one already asked for, so its death is this
    // router's answer and not this loop's first question.
    let mut retry = MappingRetry::new();
    let mut expecting = held.is_some();
    let mut ticker = tokio::time::interval(every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first tick is immediate and the boot decision has already been
    // taken by the caller, so it is spent here rather than re-deciding it.
    ticker.tick().await;
    let mut round = 0_usize;
    loop {
        if let Some(limit) = rounds
            && round >= limit
        {
            return tally;
        }
        round += 1;
        ticker.tick().await;
        retry.tick(every);

        // A peers file that does not read for a moment is not the same fact as
        // the switch being off, the rule `internet_is_on` states: the mapping
        // this node holds is kept and the switch is re-read at the next wake.
        let internet = match crate::peer::config::read_or_default(&peers_path) {
            Ok(file) => file.internet,
            Err(err) => {
                tracing::warn!(
                    path = %peers_path.display(),
                    error = %err,
                    "peer internet: the peers file did not read, so the mapping this node \
                     holds is left as it is until the next wake"
                );
                continue;
            }
        };
        let decision = mapping_boot(internet, local);
        match keeper_step(
            decision,
            held.as_ref().is_some_and(KeeperHandle::is_running),
        ) {
            KeeperStep::Idle => {}
            // A keeper that is still running is a router that mapped, which
            // puts the ladder back on its first rung: the next refusal, weeks
            // from now, is not made to wait ten minutes for one that has been
            // answering ever since.
            KeeperStep::Renew => retry.answered(MappingAnswer::Mapped),
            KeeperStep::Start => {
                if expecting {
                    // A keeper this loop is responsible for is gone. It only
                    // ends on its own when the first request was refused, so
                    // this is the router's answer, and the line naming it is
                    // written by the keeper thread itself.
                    retry.answered(MappingAnswer::Refused);
                    expecting = false;
                }
                // A Mac carried to another network is behind another router,
                // and this one may map on the first ask.
                if retry.gateway_read_due() && retry.gateway_is_now(default_gateway().ok()) {
                    tracing::info!(
                        "peer internet: this Mac is behind a different router than the one that \
                         would not map, so it is asked straight away"
                    );
                }
                if !retry.due() {
                    tally.deferred += 1;
                    tracing::debug!(
                        wait_secs = retry.interval().as_secs(),
                        refusals = retry.refusals(),
                        "peer internet: this router would not map, so the next request waits"
                    );
                    continue;
                }
                held = start();
                retry.asked();
                expecting = held.is_some();
                match &held {
                    Some(_) => {
                        tally.started += 1;
                        tracing::info!(
                            peer_listen = %local,
                            "peer internet: the switch is on, so this node is asking its router \
                             for a mapping (`tcr peer reach` reports what it gets)"
                        );
                    }
                    // No keeper at all: there is no gateway to ask, and the
                    // line saying so is written where that is decided. It
                    // counts as a refusal here, or this loop re-asks a Mac
                    // with no router every five seconds for as long as it
                    // runs.
                    None => retry.answered(MappingAnswer::Refused),
                }
            }
            KeeperStep::Stop => {
                // Dropping the handle stops the keeper, which deletes the
                // mapping over the protocol that granted it.
                held = None;
                tally.stopped += 1;
                // The switch being toggled is an operator act, and the next
                // `on` asks the router at once rather than waiting out a
                // ladder built before they touched anything.
                expecting = false;
                retry.reset();
                tracing::info!(
                    "peer internet: the switch is off, so the mapping this node holds is \
                     deleted and no keeper runs"
                );
            }
        }
    }
}

/// Is `peer.internet` still on in the peers file at `path`?
///
/// The keeper's own re-read of the switch, asked between renewals. A file that
/// does not read is answered `true`, with a line: a mapping is a thing an
/// operator asked for, and tearing it down because a read failed for a moment
/// would take the node off the internet on a transient error. The switch being
/// OFF is a fact the file has to state.
pub fn internet_is_on(path: &Path) -> bool {
    match crate::peer::config::read_or_default(path) {
        Ok(file) => file.internet,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "peer internet: the peers file did not read, so the mapping this node holds \
                 is kept; the switch is re-read at the next tick"
            );
            true
        }
    }
}

/// What `tcr peer internet on|off` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InternetSwitch {
    /// Turned on. The mapping itself is asked for by the listener at boot, so
    /// this reports the port that will be mapped, or [`None`] when no listener
    /// is configured yet.
    On {
        /// The listener's configured port, which is what gets mapped.
        listen_port: Option<u16>,
    },
    /// Turned off, and the mapping this node held was deleted.
    Off {
        /// What the router confirmed, or [`None`] when there was no listener
        /// port to unmap or no gateway to ask.
        deleted: Option<Mapping>,
    },
}

/// Set `peer.internet` and, when turning it OFF, take the router mapping away.
///
/// The delete runs here rather than in the serving process because the two are
/// different processes: `tcr peer internet off` is a CLI run with no way to
/// reach the keeper thread inside a running `tcr` server. A NAT-PMP delete is
/// addressed by internal port, not by who asked, so the CLI can unmap what the
/// server mapped. The server's own keeper notices at its next renewal.
pub fn set_internet(peers_path: &Path, on: bool) -> Result<InternetSwitch> {
    set_internet_via(peers_path, on, None)
}

/// [`set_internet`] with the gateway handed in, so a test can point it at a
/// fake responder on loopback instead of this machine's real router.
pub fn set_internet_via(
    peers_path: &Path,
    on: bool,
    gateway: Option<NatPmp>,
) -> Result<InternetSwitch> {
    use crate::peer::config;

    let _lock = config::FileLock::acquire(peers_path)?;
    let mut file = config::read_or_default(peers_path)?;
    let listen_port = file.switch_internet(on);
    config::save(peers_path, &file)
        .with_context(|| format!("peer internet: could not write {}", peers_path.display()))?;

    if on {
        return Ok(InternetSwitch::On { listen_port });
    }

    publish_external(None, None);
    let Some(port) = listen_port else {
        return Ok(InternetSwitch::Off { deleted: None });
    };
    let client = match gateway {
        Some(client) => client,
        None => match NatPmp::on_default_gateway() {
            Ok(client) => client,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "peer internet: off, and there is no NAT-PMP gateway to unmap at; any \
                     mapping this node holds expires on its own"
                );
                return Ok(InternetSwitch::Off { deleted: None });
            }
        },
    };
    match client.delete(MapProtocol::Tcp, port) {
        Ok(confirmed) => Ok(InternetSwitch::Off {
            deleted: Some(confirmed),
        }),
        Err(err) => {
            tracing::warn!(
                error = %err,
                "peer internet: off, and the router did not confirm the mapping was deleted; \
                 it expires on its own"
            );
            Ok(InternetSwitch::Off { deleted: None })
        }
    }
}

#[cfg(test)]
mod recorded_external_socket_tests {
    use super::*;

    fn state_path() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().expect("a scratch dir");
        let path = dir.path().join("peer-state.json");
        (dir, path)
    }

    fn record(
        external_address: Option<&str>,
        expires_at_ms: i64,
    ) -> crate::peer::state::MappingRecord {
        crate::peer::state::MappingRecord {
            external_address: external_address.map(str::to_string),
            external_port: 7_755,
            internal_port: 41_234,
            expires_at_ms,
        }
    }

    /// A live record yields the socket, and nothing to say about it.
    #[test]
    fn a_live_record_yields_the_socket() {
        let (_dir, path) = state_path();
        let now = 1_000;
        crate::peer::state::save_mapping(
            &path,
            Some(record(Some("198.51.100.9:7755"), now + 120_000)),
        )
        .expect("the record writes");
        let (socket, reason) = recorded_external_socket(&path, now);
        assert_eq!(socket, Some("198.51.100.9:7755".parse().expect("a socket")));
        assert_eq!(
            reason, None,
            "a live, well-formed record has nothing to explain"
        );
    }

    /// A record whose deadline has passed is no mapping, the same rule
    /// `tcr peer moved mint` already enforced before this function existed.
    #[test]
    fn an_expired_record_yields_neither_socket_nor_reason() {
        let (_dir, path) = state_path();
        let now = 1_000;
        crate::peer::state::save_mapping(&path, Some(record(Some("198.51.100.9:7755"), now - 1)))
            .expect("the record writes");
        let (socket, reason) = recorded_external_socket(&path, now);
        assert_eq!(
            socket, None,
            "a lapsed deadline is not a mapping to advertise"
        );
        assert_eq!(
            reason, None,
            "an expired record is silently no mapping, not a mapping this Mac failed to read"
        );
    }

    /// The router mapped a port but would not name its own external address:
    /// nothing to dial, and a sentence saying so.
    #[test]
    fn a_record_with_no_external_address_yields_a_reason() {
        let (_dir, path) = state_path();
        let now = 1_000;
        crate::peer::state::save_mapping(&path, Some(record(None, now + 120_000)))
            .expect("the record writes");
        let (socket, reason) = recorded_external_socket(&path, now);
        assert_eq!(socket, None);
        assert_eq!(
            reason.as_deref(),
            Some("the router mapped a port and would not name its own external address")
        );
    }

    /// A recorded string that does not parse as a socket is said out loud,
    /// never silently dropped: it is the difference between a link a friend
    /// off the LAN can act on and one they cannot.
    #[test]
    fn a_record_that_does_not_parse_yields_a_reason() {
        let (_dir, path) = state_path();
        let now = 1_000;
        crate::peer::state::save_mapping(&path, Some(record(Some("not-a-socket"), now + 120_000)))
            .expect("the record writes");
        let (socket, reason) = recorded_external_socket(&path, now);
        assert_eq!(socket, None);
        let reason = reason.expect("an unparseable record explains itself");
        assert!(
            reason.starts_with("the held mapping's external address did not parse ("),
            "the reason names what went wrong: {reason}"
        );
    }
}
