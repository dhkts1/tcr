//! The join key: the one line an operator pastes, its three spellings (v1, v2
//! and v3), and the addresses it carries.
//!
//! It sits under `config` because it renders a `config` row: the secret of a
//! [`crate::peer::config::PendingInvite`], plus the registrar's key and the
//! addresses to dial it at. What finds those addresses on this Mac's own
//! interfaces stays in [`crate::peer::pair`].

use std::net::SocketAddr;

use anyhow::{Context, Result, anyhow, bail};
use tcr_peer_wire::{Crockford, PeerId};

use crate::peer::config::dialaddrs;

/// The one-line key an operator pastes.
///
/// Shape: `tcr-join:v2:<addr,addr,…>:<b32 static-pub>:<b32 secret32>`, and a v1
/// key with one address is still read. A version field because this string is
/// the one thing an operator moves between two builds by hand.
///
/// # Why it is a list
///
/// It used to be the one address the peers file's `listen` field held, and a
/// listener bound to `0.0.0.0` made every key say `0.0.0.0`: the friend's `tcr
/// peer join` then dialled its own machine. A bind address is not a dial
/// address, and there is no single dial address either, the friend on the
/// tailnet and the friend in the next room reach this Mac at different ones. So
/// the key carries all of them, best first, and the joiner works down the list.
#[derive(Debug, Clone)]
pub struct JoinToken {
    /// Where to dial, best first and never empty. Addresses, never names to
    /// resolve: discovery is a separate, optional mechanism and a key must work
    /// without it.
    pub addrs: Vec<DialAddress>,
    /// The registrar's static public key, the `IK` half.
    pub registrar: PeerId,
    /// The 32-byte join secret, the `psk1` half.
    pub secret: [u8; 32],
}

/// Two keys are equal when they carry the same bytes.
///
/// [`DialAddressKind`] is deliberately not compared: it is what the minting Mac
/// knew about its own address, it never goes on the wire, and a key that
/// round-trips through a paste buffer would otherwise stop equalling itself.
impl PartialEq for JoinToken {
    fn eq(&self, other: &Self) -> bool {
        self.registrar == other.registrar
            && self.secret == other.secret
            && self.addrs.len() == other.addrs.len()
            && self
                .addrs
                .iter()
                .zip(other.addrs.iter())
                .all(|(ours, theirs)| ours.addr == theirs.addr)
    }
}

impl Eq for JoinToken {}

/// What every join key starts with, whatever version follows it.
pub const KEY_PREFIX: &str = "tcr-join:";

/// The v1 key: exactly one address. Still read, never minted.
pub const TOKEN_PREFIX: &str = "tcr-join:v1:";

/// The v2 key: one or more addresses, comma-separated, best first, in plain
/// text.
///
/// The addresses stay readable here for the reason this doc used to give on
/// its own: an operator reading a key out loud could at least see which
/// machine it points at. That reasoning is overruled: the owner does not want
/// an address visible at a glance, and [`TOKEN_PREFIX_V3`] is now what
/// `tcr peer invite` mints by default. This spelling stays, and is still
/// minted by `--plain`, for a script that greps a key out of stdout and for a
/// friend on a build old enough to only read this one.
pub const TOKEN_PREFIX_V2: &str = "tcr-join:v2:";

/// The v3 key: the same three fields as v2, framed and base32'd as one opaque
/// run rather than three plain-text ones. No crypto change: it is a second
/// spelling of the same bearer secret, not encryption, and every surface that
/// prints one says so.
pub const TOKEN_PREFIX_V3: &str = "tcr-join:v3:";

/// The four magic bytes in front of a v3 key's version byte, a sibling of
/// [`crate::peer::moved::MOVED_LINK_PREFIX`]'s own magic.
const TOKEN_V3_MAGIC: [u8; 4] = *b"TCRK";

/// The only v3 format version there is yet.
const TOKEN_V3_VERSION: u8 = 1;

impl JoinToken {
    /// A key for a list of addresses, best first, as a pasted key knows them.
    ///
    /// The kinds are [`DialAddressKind::FromKey`], which is what a caller
    /// holding bare sockets can honestly say. [`mint_invite_as`] builds its own
    /// with the kinds it derived.
    ///
    /// [`mint_invite_as`]: crate::peer::pair::mint_invite_as
    pub fn new(addrs: Vec<SocketAddr>, registrar: PeerId, secret: [u8; 32]) -> Self {
        Self {
            addrs: addrs.into_iter().map(DialAddress::from_key).collect(),
            registrar,
            secret,
        }
    }

    /// Every address in the key, in the order to dial them.
    pub fn sockets(&self) -> impl Iterator<Item = SocketAddr> + '_ {
        self.addrs.iter().map(|entry| entry.addr)
    }

    /// Render the key for pasting.
    ///
    /// The addresses stay plain text, an operator who has to read a key out
    /// loud can at least see which machine it points at, and base32ing
    /// `127.0.0.1:9600` hides nothing that the two 52-character fields after it
    /// do not already reveal is there. The two 32-byte fields go through the one
    /// base32 codec this tree has.
    ///
    /// Always v2, including for a single address: one shape to read back means
    /// the parser's v1 arm only ever sees a key an older build minted.
    pub fn to_token(&self) -> String {
        let addrs = self
            .sockets()
            .map(|addr| addr.to_string())
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{TOKEN_PREFIX_V2}{addrs}:{}:{}",
            self.registrar.to_wire(),
            Crockford::encode(&self.secret)
        )
    }

    /// Render the key as one opaque run: magic, version, the addresses through
    /// [`dialaddrs::encode`], the registrar's 32 bytes, the secret's 32 bytes,
    /// all of it through the one base32 codec this tree has.
    ///
    /// It is exactly as much of a bearer secret as [`Self::to_token`] is. This
    /// spelling hides an address from a glance; it does not encrypt anything,
    /// and no surface that prints it may say otherwise.
    pub fn to_token_v3(&self) -> String {
        let mut body = Vec::new();
        body.extend_from_slice(&TOKEN_V3_MAGIC);
        body.push(TOKEN_V3_VERSION);
        body.extend_from_slice(&self.to_v3_body());
        format!("{TOKEN_PREFIX_V3}{}", Crockford::encode(&body))
    }

    /// The three fields, in v3's compact spelling, with no magic and no
    /// version: the address list through [`dialaddrs::encode`], the
    /// registrar's 32 bytes, the secret's 32 bytes.
    ///
    /// [`Self::to_token_v3`] wraps this in a magic, a version and a base32
    /// prefix for a pasted key. `src/peer/ask.rs` seals it directly instead:
    /// its own frame already carries a magic and a version, and wrapping the
    /// key's again would be the same fact spelled twice.
    pub fn to_v3_body(&self) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&dialaddrs::encode(&self.sockets().collect::<Vec<_>>()));
        body.extend_from_slice(&self.registrar.0);
        body.extend_from_slice(&self.secret);
        body
    }

    /// The inverse of [`Self::to_v3_body`]: the three fields back out of the
    /// same compact spelling, with no magic or version to check (the caller
    /// already checked its own).
    pub fn from_v3_body(bytes: &[u8]) -> Result<Self> {
        let (addrs, rest) =
            dialaddrs::decode_prefix(bytes).map_err(|refusal| anyhow!("peer join: {refusal}"))?;
        if rest.len() != 64 {
            bail!(
                "peer join: this key has the wrong number of bytes left after its address \
                 list, which is what a paste cut short or padded looks like"
            );
        }
        if addrs.is_empty() {
            bail!("peer join: this key carries no address to dial");
        }
        let registrar = PeerId(
            rest[..32]
                .try_into()
                .context("peer join: the registrar bytes are the wrong length")?,
        );
        let secret: [u8; 32] = rest[32..64]
            .try_into()
            .context("peer join: the secret bytes are the wrong length")?;
        Ok(Self::new(addrs, registrar, secret))
    }

    /// Parse a pasted key, v3, v2 or v1. Refuses an unknown version rather
    /// than guessing, because the alternative is a silent wrong-key dial.
    ///
    /// v3 is checked first: it is what this build mints by default. Split from
    /// the RIGHT for v2 and v1: an IPv6 address carries colons of its own, and
    /// the two fields after it do not, so the two rightmost separators are the
    /// only ones whose position is known. The address field is then split on
    /// commas, which no address contains (an IPv6 one is bracketed).
    pub fn parse(token: &str) -> Result<Self> {
        let token = token.trim();
        if let Some(body) = token.strip_prefix(TOKEN_PREFIX_V3) {
            return Self::parse_v3(body);
        }
        let (body, version) = if let Some(body) = token.strip_prefix(TOKEN_PREFIX_V2) {
            (body, KeyVersion::V2)
        } else if let Some(body) = token.strip_prefix(TOKEN_PREFIX) {
            (body, KeyVersion::V1)
        } else {
            bail!(
                "peer join: this is not a join key (it must start with {TOKEN_PREFIX_V3:?}, \
                 {TOKEN_PREFIX_V2:?} from `--plain`, or {TOKEN_PREFIX:?} from an older build); \
                 an unknown version is refused rather than guessed, because guessing it would \
                 be a silent dial to the wrong key"
            );
        };

        let mut fields = body.rsplitn(3, ':');
        let (Some(secret), Some(registrar), Some(addrs)) =
            (fields.next(), fields.next(), fields.next())
        else {
            bail!(
                "peer join: a join key is `{TOKEN_PREFIX_V2}<host:port,…>:<registrar>:<secret>`, \
                 and this one has fewer than three fields after the version"
            );
        };

        let addrs = match version {
            KeyVersion::V1 => vec![parse_dial_addr(addrs)?],
            KeyVersion::V2 => {
                let mut parsed = Vec::new();
                for field in addrs.split(',') {
                    parsed.push(parse_dial_addr(field)?);
                }
                parsed
            }
        };
        if addrs.is_empty() {
            bail!("peer join: this key carries no address to dial");
        }

        let registrar = PeerId::parse(registrar).map_err(|refusal| {
            anyhow!("peer join: the registrar field is unreadable: {refusal}")
        })?;
        // The secret is decoded, never echoed: every refusal below names the
        // FIELD and its shape, and no arm of this function prints the value.
        // A pasted token that failed to parse is still a live bearer secret,
        // and the place operators paste one is a terminal that keeps scrollback.
        let secret = Crockford::decode_key32(secret)
            .map_err(|refusal| anyhow!("{refusal}"))
            .context("peer join: the secret field is not 32 base32-encoded bytes")?;
        Ok(Self::new(addrs, registrar, secret))
    }

    /// [`Self::parse`]'s v3 arm: unwrap the base32, check the magic and
    /// version, then read the three fields by offset rather than by split,
    /// which is what an opaque run buys over the comma-and-colon shape of v2.
    fn parse_v3(body: &str) -> Result<Self> {
        let bytes = Crockford::decode(body)
            .map_err(|refusal| anyhow!("{refusal}"))
            .context(
                "peer join: a v3 key is not a whole run of base32 bytes, which is what a paste cut \
             short looks like",
            )?;
        let min_len = TOKEN_V3_MAGIC.len() + 1 + 1 + 32 + 32;
        if bytes.len() < min_len {
            bail!(
                "peer join: this v3 key is shorter than it should be, which is what a paste cut \
                 short looks like"
            );
        }
        if bytes[..TOKEN_V3_MAGIC.len()] != TOKEN_V3_MAGIC {
            bail!("peer join: this does not carry a v3 key's magic bytes");
        }
        let version = bytes[TOKEN_V3_MAGIC.len()];
        if version != TOKEN_V3_VERSION {
            bail!(
                "peer join: this v3 key names format version {version}, which this build does not know"
            );
        }
        let rest = &bytes[TOKEN_V3_MAGIC.len() + 1..];
        Self::from_v3_body(rest)
    }
}

/// Which spelling of the key a paste turned out to be. A typed pair rather than
/// a `bool`, because the two arms differ in how the address field is split and
/// a caller reading `true` would have to remember which way round it went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyVersion {
    /// One address, minted by a build before the key carried a list.
    V1,
    /// A comma-separated list, best first.
    V2,
}

/// One address field out of a key: a socket, and one a friend could actually
/// open.
///
/// The unspecified address is the whole reason the key grew a list. An older
/// build copied the peers file's `listen` field in verbatim, so a listener on
/// `0.0.0.0` minted a key reading `0.0.0.0:7755`, and the friend's `tcr peer
/// join` dialled its own machine and reported that nothing answered. It is
/// refused here, with the one sentence that fixes it, rather than dialled.
fn parse_dial_addr(field: &str) -> Result<SocketAddr> {
    let addr: SocketAddr = field
        .parse()
        .with_context(|| format!("peer join: {field:?} is not the host:port to dial"))?;
    if addr.ip().is_unspecified() {
        bail!(
            "peer join: this key says {addr}, which is the address the other Mac LISTENS on \
             and not one anything can dial. Ask for a fresh key: `tcr peer invite` on a build \
             that carries dialable addresses puts every address that Mac answers at into it"
        );
    }
    if addr.port() == 0 {
        bail!("peer join: this key says {addr}, and port 0 is not a port anything listens on");
    }
    Ok(addr)
}

/// What kind of path one address in a join key is, as the Mac that minted the
/// key knew it.
///
/// **How this Mac learned the address, not a claim on the wire.** The same rule
/// [`crate::peer::config::EndpointSource`] follows, and the reason it is not
/// rendered into the key: a sender's word about its own address is not
/// evidence, the joiner dials in the order it was given, and a reader that
/// believed a kind field would be believing the one party that cannot be
/// checked. It exists so `tcr peer invite` can tell an operator which of the
/// lines under their key is the tailnet one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialAddressKind {
    /// The listener is bound to one specific address and somebody chose it, so
    /// it is the only address the key carries.
    Chosen,
    /// A tailnet address: a `utun` interface holding one out of
    /// `100.64.0.0/10`. First, because it works from any network the friend is
    /// on without a router forwarding anything.
    Tailscale,
    /// The address of tcr's own tailnet node, as the serving process
    /// recorded it (`crate::peer::tailnet_node`): there is no `utun` for it,
    /// so it is read off the state file rather than an interface. First, for
    /// the reason [`Self::Tailscale`] is.
    TailnetNode,
    /// The external socket a held port mapping published, which is the one
    /// address a friend anywhere on the internet can open.
    Internet,
    /// An address a real interface holds: reachable from this network and no
    /// further.
    Lan,
    /// Read out of a pasted key. This Mac derived nothing about it, which is
    /// what a parsed key honestly knows.
    FromKey,
}

impl DialAddressKind {
    /// The one word a surface prints for this kind. One spelling, so an
    /// operator grepping their scrollback and a reader of this file see the
    /// same token.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Chosen => "chosen",
            Self::Tailscale => "tailscale",
            Self::TailnetNode => "tailnet-node",
            Self::Internet => "internet",
            Self::Lan => "lan",
            Self::FromKey => "from-key",
        }
    }
}

/// One address a friend could dial this Mac at, with what kind of path it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DialAddress {
    /// The full socket, as it goes into the key.
    pub addr: SocketAddr,
    /// See [`DialAddressKind`]: local knowledge, never part of the key.
    pub kind: DialAddressKind,
}

impl DialAddress {
    /// An address read out of a pasted key.
    pub const fn from_key(addr: SocketAddr) -> Self {
        Self {
            addr,
            kind: DialAddressKind::FromKey,
        }
    }
}
