//! The dead drop: where two Macs that both moved leave each other an address.
//!
//! # What this is not
//!
//! It is not a relay and not a rendezvous server. Nothing of ours holds state
//! and nothing of ours learns who talks to whom: the operator points this at a
//! surface they already own, and to that surface a record is a random name
//! holding random bytes. `docs/peers.md` § "Reaching a Mac off your network"
//! says "There is no server anywhere in this", and this module is written to
//! keep that sentence true.
//!
//! # Where the key comes from
//!
//! One HKDF expand round over [`crate::peer::config::PeerRow::rendezvous_secret`],
//! under this module's own versioned domain string, exactly the shape
//! [`crate::peer::reach::port_secret`] uses over the handshake hash. Not from
//! the handshake hash itself: that value is memory-only by decision (see
//! `PeerRow::rendezvous_secret`'s doc, and `reach::port_secret`'s) and a drop
//! whose key dies on restart is useless in the one case it exists for.
//!
//! Three keys and not one. `name_key` answers "where does this pair write in
//! this hour" and `seal_key` answers "what do those bytes say", and keeping
//! them apart costs one keyed hash and makes the two questions two different
//! capabilities.
//!
//! # What a fetched address is worth
//!
//! One dial attempt and nothing else. A store can withhold, and a peer that
//! was forgotten on this Mac still holds the seal key it was given while it
//! was a peer, so nothing here is authorization: identity is re-proven by the
//! Noise handshake against the pinned static key, every time, the rule
//! `crate::peer::mod`'s invariant 2 states.
//!
//! # Nothing is spawned yet
//!
//! This file is the crypto, the naming, the store client, and now the
//! publisher and fetch logic: [`own_endpoints`], [`publish_for`],
//! [`fetch_for`] and [`keep_drops_published`]. What is still not here is the
//! boot wiring: nothing calls [`keep_drops_published`] from
//! `server::boot_peer_listener`, nothing calls [`fetch_for`] from the dial
//! order, and there is no CLI verb to turn a store on. Those are a separate
//! change and none of this runs at boot yet.
//!
//! [`own_endpoints`]: crate::peer::reach::drop_exchange::own_endpoints
//! [`publish_for`]: crate::peer::reach::drop_exchange::publish_for
//! [`fetch_for`]: crate::peer::reach::drop_exchange::fetch_for
//! [`keep_drops_published`]: crate::peer::reach::drop_exchange::keep_drops_published

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tcr_peer_wire::PeerId;

/// What one Mac sends one friend after it changed networks: a sealed record
/// saying where it is now. The seal and the refusals only, called by nothing
/// yet.
pub mod moved;

/// HMAC-SHA256, by hand over `sha2`, in the twenty lines RFC 2104 is.
///
/// # Why this is not the `hmac` crate
///
/// Measured rather than assumed: `sha2` 0.10.9 is already in `Cargo.lock`
/// (`snow`'s `use-sha2` resolves it), so calling it is one dependency EDGE and
/// zero new packages, the same precedent `getrandom` is in the root
/// `Cargo.toml` under. `hmac` is **not** in the lock, so reaching for it would
/// be a genuinely new package for the construction below. Re-derive with
/// `cargo tree -i sha2` and `cargo tree -i hmac` (the second prints nothing).
///
/// Verified against RFC 4231's test case 2 rather than against itself:
/// `hmac_sha256_matches_rfc_4231_case_2` in `tests/peer_pairing.rs`. A
/// hand-rolled MAC that is only ever compared to its own output is
/// self-consistent and agrees with nobody, including the next build of this
/// program, if the construction is ever replaced by the `hmac` crate.
///
/// `pub` and over `&[u8]` rather than private and over `&[u8; 32]`, for that
/// test's sake and for one reason beyond it: the RFC's vectors use keys of
/// several widths, and a signature that could only take this program's own key
/// width could only be checked against this program's own output. Every branch
/// the RFC names is therefore written, including the long-key one no caller
/// here reaches.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};

    /// SHA-256's block size.
    const BLOCK: usize = 64;

    // RFC 2104: a key longer than the block is replaced by its own hash. No
    // caller in this tree reaches it, the network key is 32 bytes, but a
    // function that silently truncated a long key would be a function whose
    // agreement with the RFC held only for the inputs it was tested on.
    let mut shortened = [0_u8; 32];
    let key: &[u8] = if key.len() > BLOCK {
        shortened.copy_from_slice(&Sha256::digest(key));
        &shortened
    } else {
        key
    };

    let mut ipad = [0x36_u8; BLOCK];
    let mut opad = [0x5c_u8; BLOCK];
    for (slot, byte) in ipad.iter_mut().zip(key.iter()) {
        *slot ^= byte;
    }
    for (slot, byte) in opad.iter_mut().zip(key.iter()) {
        *slot ^= byte;
    }

    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(message);
    let inner = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    outer.finalize().into()
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// How long one drop name is valid: one hour.
///
/// Not [`crate::peer::reach::SLOT_SECONDS`] (30), and the difference is the
/// point: a port slot is computed locally and costs nothing, a drop slot costs
/// one write to somebody else's surface per friend. Thirty-second slots would
/// be 2,880 writes per friend per day.
pub const DROP_SLOT_SECONDS: u64 = 3_600;

/// The root domain separator. Versioned so a scheme change changes every name
/// rather than half of them, the rule `reach::PORT_INFO`'s doc states: two
/// derivations from one secret with no domain separator are one derivation
/// whose outputs are related.
const DROP_ROOT_INFO: &[u8] = b"tcr peer dead-drop root v1";

/// The name key's domain separator. See [`DROP_ROOT_INFO`].
const DROP_NAME_INFO: &[u8] = b"tcr peer dead-drop name v1";

/// The seal key's domain separator. See [`DROP_ROOT_INFO`].
const DROP_SEAL_INFO: &[u8] = b"tcr peer dead-drop seal v1";

/// The record's first four bytes, outside the sealed part on purpose, so "this
/// is not a tcr record at all" is a different refusal from "this did not
/// open". A surface serving an HTML error page must not read as a forgery.
const DROP_MAGIC: [u8; 4] = *b"TCRD";

/// The only format version.
pub(crate) const DROP_VERSION: u8 = 1;

/// ChaCha20-Poly1305's nonce width.
///
/// Visible to the rest of the peer module because [`seal_payload`] and
/// [`open_payload`] are, and a second module framing a sealed payload has to
/// size its own header off the same number rather than writing 12 again.
pub(crate) const NONCE_BYTES: usize = 12;

/// Poly1305's tag width, appended to the ciphertext by the AEAD. See
/// [`NONCE_BYTES`] for why it is not private.
pub(crate) const TAG_BYTES: usize = 16;

/// Magic and version: everything this module puts in front of the sealed
/// payload, which carries its own nonce ([`seal_payload`]).
const FRAME_BYTES: usize = DROP_MAGIC.len() + 1;

/// How old a record may be and still be applied.
pub const MAX_RECORD_AGE: Duration = Duration::from_secs(2 * DROP_SLOT_SECONDS);

/// How far into the future a publisher's clock may run before its record is
/// refused rather than believed.
pub const MAX_CLOCK_SKEW: Duration = Duration::from_secs(300);

/// How many addresses one record carries.
///
/// Capped so the sealed record stays small enough that a text-only surface
/// with a per-value size limit remains a possible backend later.
pub const MAX_RECORD_ENDPOINTS: usize = 4;

// ---------------------------------------------------------------------------
// Keys and names
// ---------------------------------------------------------------------------

/// The keys one pair's dead drop runs on.
///
/// Held by value and never logged: `Debug` prints the shape and no bytes, the
/// rule `crate::peer::config::NetworkKey`'s own `Debug` follows, because a
/// `{:?}` in a log line is how a shared secret ends up in a file. There is no
/// accessor for either key and neither is serialized anywhere.
pub struct DropKeys {
    name_key: [u8; 32],
    seal_key: [u8; 32],
}

impl std::fmt::Debug for DropKeys {
    /// The bytes are deliberately not printed. See the type docs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DropKeys(set)")
    }
}

impl DropKeys {
    /// Derive this pair's drop keys from the secret already on its row.
    ///
    /// `rendezvous_secret` is [`crate::peer::reach::port_secret`]'s output and
    /// therefore already a pseudorandom key, so one expand round is the whole
    /// ladder: RFC 5869 § 2.3 with `info` this module's own versioned string
    /// and the counter byte the RFC's first round carries. No second extract,
    /// for the reason `port_secret`'s doc gives.
    pub fn derive(rendezvous_secret: &[u8; 32]) -> Self {
        let root = expand(rendezvous_secret, DROP_ROOT_INFO);
        Self {
            name_key: expand(&root, DROP_NAME_INFO),
            seal_key: expand(&root, DROP_SEAL_INFO),
        }
    }
}

/// RFC 5869 § 2.3, one round: `T(1) = HMAC(prk, info || 0x01)`.
///
/// One round and not a loop, because 32 bytes is one SHA-256 block and the
/// counter never reaches two: a loop here would be dead code that the next
/// reader has to prove is dead.
///
/// Visible to the rest of the peer module rather than private, so a sibling
/// deriving a key off the same [`crate::peer::config::PeerRow::rendezvous_secret`]
/// under its OWN versioned domain string calls this ladder instead of writing a
/// second one. Two spellings of RFC 5869 § 2.3 in one crate is the drift
/// `drop::hmac_sha256`'s own doc warns about.
pub(crate) fn expand(pseudorandom_key: &[u8; 32], info: &[u8]) -> [u8; 32] {
    let mut block = Vec::with_capacity(info.len() + 1);
    block.extend_from_slice(info);
    block.push(0x01);
    hmac_sha256(pseudorandom_key, &block)
}

/// One drop location: 16 bytes, rendered as 32 lower-case hex characters.
///
/// Hex and not Crockford base32, for the reason `config::secret32_hex` gives:
/// the Crockford codec is this tree's identity wire form and a drop name is
/// not an identity. The precedent for a hand-rolled hex wire form on a value
/// of this class is [`tcr_peer_wire::InstanceId::to_wire`].
///
/// To anyone without the pair's `name_key` this is sixteen uniform random
/// bytes, and two names from two slots share no structure: they are two MAC
/// outputs under one key over two different messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DropName([u8; 16]);

impl DropName {
    /// The name `publisher` writes to in `slot`.
    ///
    /// The publisher's id is in the derivation because a drop is
    /// DIRECTIONAL: each Mac publishes at its own name and reads the other's.
    /// Without it both ends would write to one location and overwrite each
    /// other on every slot.
    pub fn for_slot(keys: &DropKeys, publisher: &PeerId, slot: u64) -> Self {
        let wire = publisher.to_wire();
        let mut message = Vec::with_capacity(4 + 8 + wire.len());
        message.extend_from_slice(b"slot");
        message.extend_from_slice(&slot.to_be_bytes());
        message.extend_from_slice(wire.as_bytes());

        let mac = hmac_sha256(&keys.name_key, &message);
        let mut name = [0_u8; 16];
        name.copy_from_slice(&mac[..16]);
        Self(name)
    }

    /// The 32-character lower-case hex form, which is the only form.
    pub fn to_wire(&self) -> String {
        let mut out = String::with_capacity(32);
        for byte in &self.0 {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }

    /// The raw bytes, for the sealed record's associated data.
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

/// Which slot a unix timestamp falls in.
///
/// [`crate::peer::reach::current_slot`]'s shape over this module's own slot
/// length.
pub const fn current_slot(unix_seconds: u64) -> u64 {
    unix_seconds / DROP_SLOT_SECONDS
}

/// The slots a reader accepts while in `slot`: the slot itself and the one
/// before it.
///
/// Two and not three, unlike [`crate::peer::reach::accepted_ports`]: a
/// publisher writes only to the slot it is in, so a future slot can hold
/// nothing yet, while the slot just ended is where a record written four
/// minutes ago still sits. Ordered current, previous, which is the order a
/// reader should try them in.
pub const fn accepted_slots(slot: u64) -> [u64; 2] {
    [slot, slot.saturating_sub(1)]
}

// ---------------------------------------------------------------------------
// The record
// ---------------------------------------------------------------------------

/// What one Mac leaves for one friend in one slot.
///
/// `at` is the publisher's own clock and is used for exactly one thing,
/// refusing something too old. It never becomes an `Endpoint::observed_at_ms`:
/// that stays this node's own clock, the rule `Endpoint::observed_at_ms`'s doc
/// states and `config::observe_seen_address` repeats.
///
/// The field names are short because they are on somebody else's surface and
/// every byte is sealed, stored and fetched again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropRecord {
    /// Format version inside the sealed plaintext, [`DROP_VERSION`].
    pub v: u8,
    /// The publisher, checked against the peer the reader expected.
    #[serde(rename = "pub")]
    pub publisher: PeerId,
    /// The slot, checked against the slot the name was derived for.
    pub slot: u64,
    /// The publisher's unix seconds.
    pub at: u64,
    /// Where the publisher believes it is reachable, at most
    /// [`MAX_RECORD_ENDPOINTS`] of them.
    pub eps: Vec<SocketAddr>,
}

/// Why a fetched record was not applied.
///
/// Seven variants and not one, because "the surface served an HTML error
/// page", "the surface served a forged record" and "the surface served last
/// week's record" are three different diagnoses, and an operator reading a log
/// needs to tell them apart. A refusal that answers one word for all three is
/// one nobody can act on.
#[derive(Debug, thiserror::Error)]
pub enum RecordRefusal {
    /// The first four bytes are not [`DROP_MAGIC`]. Reported with what was
    /// found, zero-padded when there were fewer than four bytes to read.
    #[error("dead drop: those bytes are not a tcr record (magic {found:?})")]
    NotARecord { found: [u8; 4] },
    /// The header's version byte names a format this build does not have.
    #[error("dead drop: record version {found} is not {expected}")]
    Version { found: u8, expected: u8 },
    /// The sealed bytes did not authenticate: a wrong key, a wrong name, a
    /// wrong publisher, or bytes somebody edited.
    #[error("dead drop: the record at that name did not open under this pair's key")]
    DidNotOpen,
    /// It opened, and names somebody else.
    #[error("dead drop: the record names peer {found} and this name is {expected}'s")]
    WrongPublisher { found: String, expected: String },
    /// It opened, and claims a slot this name does not belong to.
    #[error("dead drop: the record is from slot {found} and this name is slot {expected}'s")]
    WrongSlot { found: u64, expected: u64 },
    /// It opened, and is older than [`MAX_RECORD_AGE`].
    #[error("dead drop: the record is {age_s}s old, past the {max_s}s ceiling")]
    Stale { age_s: u64, max_s: u64 },
    /// It opened, and is dated further ahead than [`MAX_CLOCK_SKEW`] allows.
    #[error("dead drop: the record is dated {ahead_s}s in the future")]
    FromTheFuture { ahead_s: u64 },
}

/// What the AEAD authenticates but does not encrypt: the name and the
/// publisher.
///
/// # Why the slot is not a third term here
///
/// It is already bound, and binding it twice would cost a diagnosis. The name
/// IS a MAC over the slot, so a record sealed for one slot cannot open at
/// another slot's name: that is [`RecordRefusal::DidNotOpen`], and it is the
/// defence. Feeding the record's own `slot` field in as well would make a
/// record whose inner slot disagrees with its name indistinguishable from a
/// forgery, and [`RecordRefusal::WrongSlot`] would be unreachable. The check
/// that fires instead is the explicit one in [`open`], after the bytes have
/// authenticated, which is the reading an operator can act on.
fn associated_data(name: &DropName, publisher: &PeerId) -> Vec<u8> {
    let wire = publisher.to_wire();
    let mut aad = Vec::with_capacity(name.0.len() + wire.len());
    aad.extend_from_slice(&name.0);
    aad.extend_from_slice(wire.as_bytes());
    aad
}

// ---------------------------------------------------------------------------
// The AEAD body, shared with this module's siblings
// ---------------------------------------------------------------------------

/// Why a payload would not seal.
///
/// Two arms and not a string, because the two are a different kind of event: a
/// CSPRNG that refuses is the platform failing, and an AEAD that refuses at
/// this width cannot happen on inputs this code can build. Each caller writes
/// the sentence in its own vocabulary, which is why this type carries no
/// sentence of its own.
pub(crate) enum SealFailure {
    /// The platform CSPRNG would not fill the nonce.
    Csprng(getrandom::Error),
    /// The AEAD refused the inputs.
    Aead,
}

/// A fresh random nonce and the AEAD's output over `plaintext`, as one run of
/// bytes, authenticating `aad` without encrypting it.
///
/// The nonce is twelve RANDOM bytes from the platform CSPRNG and is never
/// derived: a derived nonce repeats the moment one key seals twice over
/// changed content, and a repeated ChaCha20-Poly1305 nonce under a fixed key is
/// a total break. Twelve random bytes against a handful of seals is nowhere
/// near a birthday problem.
///
/// What is NOT here: the magic, the version, and the meaning of `aad`. Framing
/// is each caller's own, so that "these are not our bytes at all" stays a
/// refusal a caller can make before any key is used.
pub(crate) fn seal_payload(
    seal_key: &[u8; 32],
    aad: &[u8],
    plaintext: &[u8],
) -> std::result::Result<Vec<u8>, SealFailure> {
    let mut nonce = [0_u8; NONCE_BYTES];
    getrandom::fill(&mut nonce).map_err(SealFailure::Csprng)?;
    seal_payload_with_nonce(seal_key, &nonce, aad, plaintext)
}

/// [`seal_payload`] with the nonce handed in. Private, and called only by
/// `seal_payload` and the fixed-vector test below: a caller that picks its own
/// nonce is how a nonce repeats.
fn seal_payload_with_nonce(
    seal_key: &[u8; 32],
    nonce: &[u8; NONCE_BYTES],
    aad: &[u8],
    plaintext: &[u8],
) -> std::result::Result<Vec<u8>, SealFailure> {
    use chacha20poly1305::aead::{Aead as _, KeyInit as _, Payload};
    use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

    let cipher = ChaCha20Poly1305::new(Key::from_slice(seal_key));
    let sealed = cipher
        .encrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        // The AEAD's own error carries nothing beyond "it failed", and at this
        // width it cannot: the key and nonce are fixed-size arrays.
        .map_err(|_| SealFailure::Aead)?;

    let mut payload = Vec::with_capacity(NONCE_BYTES + sealed.len());
    payload.extend_from_slice(nonce);
    payload.extend_from_slice(&sealed);
    Ok(payload)
}

/// Why a payload did not open.
///
/// The two arms exist so a caller that wants to tell a cut-off paste apart from
/// a forgery can, and a caller that does not want to tell them apart says so by
/// mapping both to one refusal. Collapsing them here would take the choice away
/// from both.
pub(crate) enum OpenFailure {
    /// Fewer bytes than a nonce and a tag: there is nothing that could
    /// authenticate, whatever the key.
    TooShort,
    /// The bytes did not authenticate: a wrong key, wrong associated data, or
    /// bytes somebody edited.
    DidNotAuthenticate,
}

/// The inverse of [`seal_payload`]: the plaintext, or which of the two ways it
/// failed.
pub(crate) fn open_payload(
    seal_key: &[u8; 32],
    aad: &[u8],
    payload: &[u8],
) -> std::result::Result<Vec<u8>, OpenFailure> {
    use chacha20poly1305::aead::{Aead as _, KeyInit as _, Payload};
    use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

    let Some(nonce) = payload.get(..NONCE_BYTES) else {
        return Err(OpenFailure::TooShort);
    };
    let Some(body) = payload.get(NONCE_BYTES..) else {
        return Err(OpenFailure::TooShort);
    };
    if body.len() <= TAG_BYTES {
        // Tag and nothing to authenticate. The AEAD would refuse this too;
        // refusing it here keeps the length arithmetic in one place.
        return Err(OpenFailure::TooShort);
    }

    let cipher = ChaCha20Poly1305::new(Key::from_slice(seal_key));
    cipher
        .decrypt(Nonce::from_slice(nonce), Payload { msg: body, aad })
        .map_err(|_| OpenFailure::DidNotAuthenticate)
}

/// Seal `record` for `name`.
///
/// The nonce is twelve RANDOM bytes from the platform CSPRNG and is never
/// derived: a derived nonce repeats the moment a Mac publishes twice in one
/// slot because its address changed, and a repeated ChaCha20-Poly1305 nonce
/// under a fixed key is a total break. Twelve random bytes against a handful
/// of writes per slot is nowhere near a birthday problem.
///
/// The associated data is the name and the publisher, which is what stops the
/// surface moving one pair's record to another name and having it believed.
pub fn seal(keys: &DropKeys, name: &DropName, record: &DropRecord) -> Result<Vec<u8>> {
    if record.v != DROP_VERSION {
        bail!(
            "dead drop: refusing to seal a version {} record, this build writes {DROP_VERSION}",
            record.v
        );
    }
    if record.eps.len() > MAX_RECORD_ENDPOINTS {
        bail!(
            "dead drop: refusing to seal {} addresses, the ceiling is {MAX_RECORD_ENDPOINTS}",
            record.eps.len()
        );
    }

    let plaintext =
        serde_json::to_vec(record).context("dead drop: the record would not serialize")?;

    let aad = associated_data(name, &record.publisher);
    let payload =
        seal_payload(&keys.seal_key, &aad, &plaintext).map_err(|failure| match failure {
            SealFailure::Csprng(err) => {
                anyhow::Error::new(err).context("dead drop: the platform CSPRNG refused")
            }
            SealFailure::Aead => anyhow!("dead drop: the record would not seal"),
        })?;

    let mut out = Vec::with_capacity(FRAME_BYTES + payload.len());
    out.extend_from_slice(&DROP_MAGIC);
    out.push(DROP_VERSION);
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Open and check a record fetched from `name` for `slot`, against `now_s`.
///
/// Every check [`RecordRefusal`] names, in that order, and the refusal says
/// which one fired.
pub fn open(
    keys: &DropKeys,
    name: &DropName,
    slot: u64,
    expected_publisher: &PeerId,
    sealed: &[u8],
    now_s: u64,
) -> std::result::Result<DropRecord, RecordRefusal> {
    // The magic and the version are read before anything else and outside the
    // AEAD, so a surface answering with a login page or an error document is
    // told apart from a surface answering with bytes somebody forged.
    let mut found_magic = [0_u8; 4];
    for (place, byte) in found_magic.iter_mut().zip(sealed.iter()) {
        *place = *byte;
    }
    if found_magic != DROP_MAGIC {
        return Err(RecordRefusal::NotARecord { found: found_magic });
    }

    let Some(&version) = sealed.get(DROP_MAGIC.len()) else {
        // Four bytes of magic and nothing after them: the magic matched, so
        // this is not "some other document", it is a truncated record.
        return Err(RecordRefusal::DidNotOpen);
    };
    if version != DROP_VERSION {
        return Err(RecordRefusal::Version {
            found: version,
            expected: DROP_VERSION,
        });
    }

    let Some(payload) = sealed.get(FRAME_BYTES..) else {
        return Err(RecordRefusal::DidNotOpen);
    };

    let aad = associated_data(name, expected_publisher);
    let plaintext =
        open_payload(&keys.seal_key, &aad, payload).map_err(|failure| match failure {
            // One refusal for both, on purpose: a drop record comes off
            // somebody else's surface, where a short answer is as likely to be
            // a forgery as a truncation, so an operator could do nothing
            // differently with the two told apart. A caller whose bytes came
            // from a person's clipboard should decide otherwise.
            OpenFailure::TooShort | OpenFailure::DidNotAuthenticate => RecordRefusal::DidNotOpen,
        })?;

    // Past this line the bytes authenticated, so what follows can only have
    // been written by a holder of this pair's seal key. A plaintext that is
    // not a record is therefore not an attack, it is this pair's own writer
    // producing something this reader cannot read, and it is refused rather
    // than guessed at.
    let record: DropRecord =
        serde_json::from_slice(&plaintext).map_err(|_| RecordRefusal::DidNotOpen)?;

    if record.v != DROP_VERSION {
        return Err(RecordRefusal::Version {
            found: record.v,
            expected: DROP_VERSION,
        });
    }
    if record.publisher != *expected_publisher {
        return Err(RecordRefusal::WrongPublisher {
            found: record.publisher.display(),
            expected: expected_publisher.display(),
        });
    }
    if record.slot != slot {
        return Err(RecordRefusal::WrongSlot {
            found: record.slot,
            expected: slot,
        });
    }
    if record.at > now_s.saturating_add(MAX_CLOCK_SKEW.as_secs()) {
        return Err(RecordRefusal::FromTheFuture {
            ahead_s: record.at.saturating_sub(now_s),
        });
    }
    let age_s = now_s.saturating_sub(record.at);
    if age_s > MAX_RECORD_AGE.as_secs() {
        return Err(RecordRefusal::Stale {
            age_s,
            max_s: MAX_RECORD_AGE.as_secs(),
        });
    }

    Ok(record)
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// How large a store's answer to `get` may be before it is refused unread.
///
/// A sealed record carrying [`MAX_RECORD_ENDPOINTS`] addresses is a few
/// hundred bytes. Sixty-four KiB is two orders of headroom above that and
/// still refuses a surface that answers with a log file instead of a record.
pub const MAX_STORE_BODY_BYTES: usize = 64 * 1024;

/// Why a store call did not succeed.
///
/// Four variants and not the three the design sketch carries: `TooLarge` is
/// a distinct diagnosis from `Status`, the same reason `RecordRefusal` has
/// seven variants rather than one, an operator reading a log needs to tell
/// them apart. Absence is never a variant here: it is `Ok(None)`, the
/// ordinary outcome `tcr peer reach` gives a router that declines.
#[derive(Debug, thiserror::Error)]
pub enum StoreRefusal {
    /// The call never got an answer: a DNS failure, a refused connection, a
    /// timeout.
    #[error("dead drop: the store could not be reached: {source}")]
    Unreachable {
        #[source]
        source: reqwest::Error,
    },
    /// The store answered, and not with success or an absence.
    #[error("dead drop: the store answered {status} for {name}")]
    Status { status: u16, name: String },
    /// No store is configured at all.
    #[error("dead drop: no store is configured; `tcr peer drop-store set` is the switch")]
    NotConfigured,
    /// The store's answer to `get` passed [`MAX_STORE_BODY_BYTES`] and the
    /// read stopped rather than holding whatever it kept sending.
    #[error("dead drop: the store's answer for {name} passed the {ceiling}-byte ceiling")]
    TooLarge { name: String, ceiling: usize },
}

/// A dumb public key-value surface: an opaque name in, opaque bytes out.
///
/// The seam is the transport and nothing above it. Naming, sealing, freshness
/// and admissibility are the shipped code in every test; a backend swaps only
/// what a remote store would have been. The same rule
/// [`crate::peer::reach::PunchNet`] is written to.
pub trait DeadDropStore {
    /// Place `record` at `name`, replacing whatever was there.
    fn put(
        &self,
        name: &DropName,
        record: &[u8],
    ) -> impl Future<Output = std::result::Result<(), StoreRefusal>> + Send;

    /// Read what is at `name`. `Ok(None)` is "nothing there", an ordinary
    /// outcome and not an error, the shape `reach_upnp` gives a router that
    /// declines.
    fn get(
        &self,
        name: &DropName,
    ) -> impl Future<Output = std::result::Result<Option<Vec<u8>>, StoreRefusal>> + Send;
}

/// The backend that ships first: one URL template, `PUT` and `GET`.
///
/// The client is built once, at construction, and held: no later call
/// re-checks the template and no later call builds a client.
pub struct HttpsTemplateStore {
    template: String,
    token: Option<String>,
    client: reqwest::Client,
}

impl HttpsTemplateStore {
    /// Build one from the configured template. Refuses a template with no
    /// `{name}` placeholder at construction, not at the first `put`.
    ///
    /// The client carries `no_proxy` (an ambient `HTTP_PROXY` very commonly
    /// points at tcr itself) and turns redirects off (a store that redirects
    /// a PUT is doing something this node did not ask for), the shape
    /// `reach_upnp`'s client is built in and for the reasons its comments
    /// give.
    pub fn new(template: &str, token: Option<&str>) -> Result<Self> {
        if !template.contains("{name}") {
            bail!("dead drop: the store template has no {{name}} placeholder: {template}");
        }
        let client = crate::http_client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .context("dead drop: the store's HTTP client would not build")?;
        Ok(Self {
            template: template.to_string(),
            token: token.map(str::to_string),
            client,
        })
    }

    /// The URL for one name.
    fn url_for(&self, name: &DropName) -> String {
        self.template.replace("{name}", &name.to_wire())
    }
}

impl DeadDropStore for HttpsTemplateStore {
    fn put(
        &self,
        name: &DropName,
        record: &[u8],
    ) -> impl Future<Output = std::result::Result<(), StoreRefusal>> + Send {
        let url = self.url_for(name);
        let name_wire = name.to_wire();
        let client = self.client.clone();
        let token = self.token.clone();
        let body = record.to_vec();
        async move {
            let mut request = client
                .put(&url)
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .body(body);
            if let Some(token) = &token {
                request = request.bearer_auth(token);
            }
            let response = request
                .send()
                .await
                .map_err(|source| StoreRefusal::Unreachable { source })?;
            let status = response.status();
            if status.is_success() {
                Ok(())
            } else {
                Err(StoreRefusal::Status {
                    status: status.as_u16(),
                    name: name_wire,
                })
            }
        }
    }

    /// 200 with a body is `Ok(Some(bytes))`. 404 and 410 are `Ok(None)`:
    /// nothing there is an ordinary outcome. Anything else is `Status`, a
    /// redirect included, since the client turns redirects off and never
    /// follows one.
    ///
    /// The body is read under [`MAX_STORE_BODY_BYTES`], twice: a
    /// `Content-Length` above it is refused before a byte of body is read,
    /// and the read itself stops at the ceiling, which is the half that
    /// matters, since a promise is not a limit, the rule `reach_upnp::fetch`
    /// states.
    fn get(
        &self,
        name: &DropName,
    ) -> impl Future<Output = std::result::Result<Option<Vec<u8>>, StoreRefusal>> + Send {
        let url = self.url_for(name);
        let name_wire = name.to_wire();
        let client = self.client.clone();
        let token = self.token.clone();
        async move {
            let mut request = client.get(&url);
            if let Some(token) = &token {
                request = request.bearer_auth(token);
            }
            let response = request
                .send()
                .await
                .map_err(|source| StoreRefusal::Unreachable { source })?;
            let status = response.status();
            if status.as_u16() == 404 || status.as_u16() == 410 {
                return Ok(None);
            }
            if !status.is_success() {
                return Err(StoreRefusal::Status {
                    status: status.as_u16(),
                    name: name_wire,
                });
            }
            if let Some(promised) = response.content_length()
                && promised > MAX_STORE_BODY_BYTES as u64
            {
                return Err(StoreRefusal::TooLarge {
                    name: name_wire,
                    ceiling: MAX_STORE_BODY_BYTES,
                });
            }
            let mut collected: Vec<u8> = Vec::new();
            let mut response = response;
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|source| StoreRefusal::Unreachable { source })?
            {
                if collected.len() + chunk.len() > MAX_STORE_BODY_BYTES {
                    return Err(StoreRefusal::TooLarge {
                        name: name_wire,
                        ceiling: MAX_STORE_BODY_BYTES,
                    });
                }
                collected.extend_from_slice(&chunk);
            }
            Ok(Some(collected))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{NONCE_BYTES, open_payload, seal_payload_with_nonce};

    /// Fixed inputs for the sealed-payload vector below.
    const KEY: [u8; 32] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d,
        0x1e, 0x1f,
    ];
    const NONCE: [u8; NONCE_BYTES] = [
        0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab,
    ];
    const AAD: &[u8] = b"tcr peer seal vector aad";
    const PLAINTEXT: &[u8] = b"{\"v\":1,\"eps\":[\"203.0.113.4:7755\"]}";
    /// What chacha20poly1305 0.10.1 sealed from the inputs above, captured
    /// before the move to 0.11: the nonce, then the ciphertext and tag. Python's
    /// `cryptography` package gives the same bytes:
    /// `ChaCha20Poly1305(bytes(range(32))).encrypt(bytes(range(0xa0, 0xac)), PLAINTEXT, AAD)`.
    const SEALED_HEX: &str = "a0a1a2a3a4a5a6a7a8a9aaab77890e7d77d7ee8fc57f8036c6a1dfc9ad6dfd8f6d\
5b51909aece1864657f62e31fd0761c98811baf4bfd8ffa5c542161b78";

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// A sealed record crosses machines (the dead drop, the moved notice), so
    /// a Mac on an older build and one on a newer build must produce and open
    /// the same bytes. A dependency bump that changed them would strand every
    /// record already published; this pins them to the crate version that
    /// wrote the records in the field.
    #[test]
    fn a_fixed_key_and_nonce_seal_to_the_bytes_the_previous_crate_wrote() {
        let sealed = seal_payload_with_nonce(&KEY, &NONCE, AAD, PLAINTEXT)
            .unwrap_or_else(|_| panic!("the fixed inputs must seal"));
        assert_eq!(hex(&sealed), SEALED_HEX);

        let opened = open_payload(&KEY, AAD, &sealed)
            .unwrap_or_else(|_| panic!("the vector must open under the same key and aad"));
        assert_eq!(opened, PLAINTEXT);
    }
}
