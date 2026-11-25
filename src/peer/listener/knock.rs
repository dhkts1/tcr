use anyhow::{Result, bail};
use tcr_peer_wire::PeerId;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::peer::config::id::KEY_BYTES;
use crate::peer::config::{self as config, PeerFile};
use crate::peer::noise::{self, Handshake, PeerSession};

use super::admission::{
    KNOCK_BURST, KNOCK_INTERVAL_MS, MAX_KNOCK_FRAME_BYTES, MESSAGE_1_TIMEOUT, lock_admission,
};
use super::{SessionContext, on_blocking_thread};

/// The two clocks one knock is served against.
///
/// One value rather than two parameters: they are read together at every step
/// and the wall clock alone was already the seventh argument on
/// [`serve_knock`].
#[derive(Debug, Clone, Copy)]
pub(super) struct KnockClock {
    /// This node's own clock, which is what every row this knock writes is
    /// stamped with.
    pub(super) now_ms: i64,
    /// When the socket slot this knock is holding has to be given back,
    /// whatever state the knock has reached.
    ///
    /// An instant rather than a duration because the slot was acquired before
    /// message 1 was read: a knocker that spends four seconds delivering
    /// message 1 gets what is left of the same budget, not a fresh one. See
    /// the call site.
    pub(super) slot_deadline: tokio::time::Instant,
}

/// Answer one knock: bucket, queue, one ack byte, close.
///
/// Every refusal returns before a byte is written. That is the whole reason the
/// bucket check sits between the handshake and the ack rather than after the
/// knock is read: a rate-limited address must not be able to tell a full bucket
/// from a machine that is not there.
pub(super) async fn serve_knock<S>(
    stream: &mut S,
    context: &SessionContext,
    addr: &str,
    pattern: Handshake,
    message_1: &[u8],
    file: &PeerFile,
    clock: KnockClock,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let KnockClock {
        now_ms,
        slot_deadline,
    } = clock;
    // The network key decides WHICH knock pattern this node answers, and a
    // mismatch is refused inside message 1, a Mac without the key cannot
    // reach the queue, and a Mac with one cannot knock at a node that has
    // none. Checked here, before `snow` is handed anything, so the refusal
    // names the cause instead of surfacing as a decryption failure.
    match (file.network_psk(), pattern) {
        (Some(_), Handshake::Knock) => bail!(
            "peer knock: this Mac has a network key set, so a knock must run {} with it; \
             the one that arrived carries no key and gets nothing",
            noise::PATTERN_KNOCK_PSK
        ),
        (None, Handshake::KnockPsk) => bail!(
            "peer knock: this Mac has no network key, so there is nothing to verify a {} \
             knock against; it gets nothing",
            noise::PATTERN_KNOCK_PSK
        ),
        _ => {}
    }

    // The bucket, before the handshake completes: over it, the socket closes
    // after message 1 with zero bytes written, which is what
    // `abuse-resistance.md` asks for verbatim.
    {
        let mut guard = lock_admission(&context.admission);
        if !guard.take_knock_token(addr, now_ms) {
            bail!(
                "peer knock: {addr} is over its rate limit ({} per {}ms, burst {}); closed \
                 after message 1 with nothing written",
                1,
                KNOCK_INTERVAL_MS,
                KNOCK_BURST
            );
        }
    }

    // Banned, muted or over the queue cap: refused here, before `snow` has
    // written a single byte of message 2 back. These three depend only on
    // `addr`, never on the knock's own payload, which is what makes checking
    // them possible before the handshake that reveals that payload even
    // finishes. See `PeerState::refusal_for_knock_source`. Checking this
    // only after `finish_responder` (as before) let a muted or over-cap
    // source earn message 2 on the wire before being refused two reads
    // later; that is no longer possible.
    //
    // The check AND the reservation happen under the same lock
    // `record_knock` re-acquires below, via `reserve_knock_slot`. A read-only
    // check here, even a locked one, would still leave a window between
    // "checked" and "the handshake that spends minutes on the wire" in which
    // another concurrent knock could read the same "there is room" state,
    // so this reserves the row for `addr` right here, not just checks it: a
    // caller that finds no room is refused before message 1 is ever
    // answered, and one that finds room has already claimed its slot, so a
    // sibling connection racing it a moment later sees the reservation and is
    // refused in turn.
    // **MESSAGE 1 IS VALIDATED BEFORE ANYTHING IS RESERVED**, and the ordering
    // defect that forced it: the reservation was taken first,
    // so a stranger sending the right NUMBER of wrong bytes claimed one of the
    // eight slots and made this node write its state file twice (the
    // reservation, then the release) before the cryptography had said a word
    // about the sender.
    //
    // `read_message_1_matching` is pure, it decrypts and matches against the
    // network key and writes NOTHING to the socket, so moving it above the
    // reservation costs nothing and keeps both properties the old order was
    // protecting: the reservation is still taken before message 2 is written
    // (`finish_responder`, below), so a caller that finds no room is refused
    // before this node answers a byte, and a sibling racing it a moment later
    // sees the reservation rather than the same "there is room" snapshot.
    let psk = file.network_psk();
    let psks: &[[u8; KEY_BYTES]] = match psk {
        Some(key) => std::slice::from_ref(key),
        None => &[],
    };
    let mut scratch = vec![0_u8; noise::HANDSHAKE_SCRATCH_BYTES];
    // A knock has no static key on either side, so the secret handed to the
    // builder is an ephemeral throwaway and never this node's identity, the
    // same reasoning [`crate::peer::noise::send_knock`] gives on the dialling
    // side.
    let throwaway = config::id::random_secret()?;
    let read = noise::read_message_1_matching(&throwaway, pattern, psks, message_1, &mut scratch)?;

    // **On a blocking thread, not this runtime's.** `FileLock::acquire` waits
    // for a contended lock with `std::thread::sleep`, up to
    // [`crate::peer::config::LOCK_WAIT_MS`], and the load and the save are
    // synchronous file IO under it. Run inline, on an unauthenticated path any
    // LAN host can reach, a handful of simultaneous knocks park that many of
    // the proxy's worker threads for seconds each, and what stops answering is
    // the LOCAL proxy: the thing the mesh is not allowed to cost anything.
    //
    // Under `slot_deadline` as well, because waiting for that lock is the
    // longest thing a knock does and it is the one step whose length another
    // process decides.
    let reservation = tokio::time::timeout_at(
        slot_deadline,
        on_blocking_thread({
            let state_path = context.state_path.clone();
            let addr = addr.to_string();
            move || {
                let _lock = config::FileLock::acquire(&state_path)?;
                let mut peer_state = crate::peer::state::load(&state_path, now_ms)?;
                let created = match peer_state.reserve_knock_slot(&addr, now_ms) {
                    Ok(created) => created,
                    Err(refusal) => {
                        bail!("peer knock: {refusal}; closed after message 1 with nothing written")
                    }
                };
                crate::peer::state::save(&state_path, &peer_state)?;
                Ok(created)
            }
        }),
    )
    .await;
    let reservation_created = match reservation {
        Ok(created) => created?,
        Err(elapsed) => {
            return Err(anyhow::Error::new(elapsed).context(
                "peer knock: the pairing queue could not be reserved before this connection's \
                 unauthenticated deadline; closed with nothing written",
            ));
        }
    };

    // A wrong or missing network key, a malformed message, a dropped
    // connection, any of these must still reach NO row, exactly as before
    // the reservation existed. The reservation above only proves this
    // address is not banned, muted or over the cap; it says nothing about
    // whether the handshake itself will succeed, so a failure here has to
    // release it rather than leave a placeholder for a knock that never
    // actually completed.
    //
    // **And it is BOUNDED**. Message 1 has had a
    // five-second deadline for a long time (`MESSAGE_1_TIMEOUT`) and the two
    // writes after it had none, so a sender that delivered a valid message 1
    // and then went quiet held its reserved slot for as long as the TCP
    // connection lived: one eighth of this node's pairing queue, per stalled
    // connection, with no operator-visible cause. The same deadline, because
    // the remaining work is one message 2 and one small frame on a LAN, which
    // is the identical cost profile message 1 already has.
    //
    // Whichever of the two deadlines comes first: its own five seconds for the
    // remaining frames, and never past the moment the socket slot is due back.
    let outcome = match tokio::time::timeout_at(
        slot_deadline.min(tokio::time::Instant::now() + MESSAGE_1_TIMEOUT),
        finish_knock_handshake(stream, read, pattern),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(elapsed) => Err(anyhow::Error::new(elapsed).context(
            "peer knock: the knock handshake did not complete within the pre-authentication \
             timeout; the reservation is released",
        )),
    };
    let (mut session, knock) = match outcome {
        Ok(pair) => pair,
        Err(err) => {
            if reservation_created {
                release_reserved_knock_slot(context, addr, now_ms).await;
            }
            return Err(err);
        }
    };

    // The claimed name is attacker-chosen text that lands in a panel row, a
    // `tcr peer pending` line and a log line. A name that fails the whitelist
    // drops the NAME and keeps the row, which is the same rule
    // `crate::peer::discovery::discovered_row` follows: the row is still a
    // machine the operator may want to accept, and it shows its address.
    let proposed_name = knock
        .proposed_name
        .as_deref()
        .and_then(|name| tcr_peer_wire::sanitize_label(name).ok());

    // One read-mutate-write of the state file, under the same lock the peers
    // file uses, because two knocks landing together would otherwise each write
    // a queue built on a stale read, and the eight-row cap would not be a cap.
    //
    // This always coalesces rather than adding a row: the reservation above
    // already holds this address's slot (or, if it coalesced onto an
    // existing pending row instead of creating one, that row already
    // existed). `record_knock`'s own `Ok(bool)` therefore no longer means
    // "is this address new" the way it used to, `reservation_created`
    // above is that signal now.
    //
    // On a blocking thread, for the reason the reservation above gives: the
    // lock waits with `std::thread::sleep` and everything under it is
    // synchronous file IO, on a path no caller has authenticated.
    // Under `slot_deadline` for the same reason the reservation is, and with
    // the reservation released on the way out: a knock that is cut off here
    // has one, and leaving it behind would spend an eighth of the pairing
    // queue on a knock that never reached a row.
    let recorded = tokio::time::timeout_at(
        slot_deadline,
        on_blocking_thread({
            let state_path = context.state_path.clone();
            let addr = addr.to_string();
            let instance_id = knock.instance_id;
            let wire_version = knock.wire_version;
            let listen_port = knock.listen_port;
            move || {
                let _lock = config::FileLock::acquire(&state_path)?;
                let mut peer_state = crate::peer::state::load(&state_path, now_ms)?;
                peer_state
                    .record_knock(
                        &addr,
                        instance_id,
                        proposed_name,
                        wire_version,
                        listen_port,
                        now_ms,
                    )
                    .map_err(anyhow::Error::new)?;
                crate::peer::state::save(&state_path, &peer_state)?;
                Ok(peer_state.knocks_queued())
            }
        }),
    )
    .await;
    let pending = match recorded {
        Ok(Ok(pending)) => pending,
        Ok(Err(err)) => {
            if reservation_created {
                release_reserved_knock_slot(context, addr, now_ms).await;
            }
            return Err(err);
        }
        Err(elapsed) => {
            if reservation_created {
                release_reserved_knock_slot(context, addr, now_ms).await;
            }
            return Err(anyhow::Error::new(elapsed).context(
                "peer knock: the pairing row could not be written before this connection's \
                 unauthenticated deadline; the reservation is released",
            ));
        }
    };

    // The ack is written only now: after the ban, the mute, the bucket and the
    // queue cap have all said yes.
    noise::write_knock_ack(stream, &mut session).await?;

    // The port it says its own listener is on, which is what an Accept dials
    // back to, beside the address it arrived from. `not-said` rather than an
    // empty field: an older build sends no port at all, and a reader of this
    // line who saw nothing there could not tell that apart from a port this
    // line forgot to print.
    let listen_port = knock
        .listen_port
        .map_or_else(|| "not-said".to_string(), |port| port.to_string());
    tracing::info!(
        peer_addr = %addr,
        listen_port = %listen_port,
        instance = %knock.instance_id,
        wire_version = knock.wire_version,
        fresh = reservation_created,
        pending,
        "peer knock: a Mac wants to pair (`tcr peer pending` lists it; accept, ignore or \
         block it, nothing else has happened)"
    );
    Ok(())
}

/// The part of a knock that runs AFTER a slot is reserved, and the only part
/// that writes to the socket: message 2, then the knock frame.
///
/// Pulled out of [`serve_knock`] so its caller has one place to catch a failure
/// and release the reservation [`PeerState::reserve_knock_slot`](crate::peer::state::PeerState::reserve_knock_slot) made, rather
/// than duplicating that release at every `?`, and one place to put a deadline
/// on. Message 1's own validation is NOT in here
/// any more: it writes nothing, so it belongs above the reservation.
async fn finish_knock_handshake<S>(
    stream: &mut S,
    // The whole [`noise::Message1`] rather than its `state` field: snow's
    // `HandshakeState` is not nameable from this file (it is a private import
    // in `crate::peer::noise`), and taking the struct keeps the "what reading
    // message 1 produced" unit intact rather than splitting it at a signature.
    read: noise::Message1,
    pattern: Handshake,
) -> Result<(PeerSession, tcr_peer_wire::Knock)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // **The first byte this node writes in answer to a knock is written
    // here**, which is why message 1's own validation sits above the
    // reservation in the caller and this does not: everything before this
    // function has written nothing.
    let mut session = noise::finish_responder(stream, read.state, pattern, |_| {
        Ok(PeerId([0_u8; KEY_BYTES]))
    })
    .await?;

    let knock = noise::read_knock(stream, &mut session, MAX_KNOCK_FRAME_BYTES).await?;
    Ok((session, knock))
}

/// Undo a reservation [`PeerState::reserve_knock_slot`](crate::peer::state::PeerState::reserve_knock_slot) made for `addr` at
/// `reserved_at_ms`, because the handshake that would have earned it real
/// details never completed. Logged rather than propagated: the connection is
/// already being torn down for its own error, and a state-file write failure
/// here must not shadow that original cause, it costs one placeholder row
/// that ages out after [`crate::peer::state::KNOCK_TTL_MS`] at worst.
async fn release_reserved_knock_slot(context: &SessionContext, addr: &str, reserved_at_ms: i64) {
    let released = on_blocking_thread({
        let state_path = context.state_path.clone();
        let addr = addr.to_string();
        move || {
            let _lock = config::FileLock::acquire(&state_path)?;
            let mut peer_state = crate::peer::state::load(&state_path, reserved_at_ms)?;
            peer_state.release_knock_reservation(&addr, reserved_at_ms);
            crate::peer::state::save(&state_path, &peer_state)?;
            Ok(())
        }
    })
    .await;
    if let Err(err) = released {
        tracing::warn!(
            peer_addr = %addr,
            error = %err,
            "peer knock: failed to release a reservation after the handshake did not \
             complete; the row ages out on its own"
        );
    }
}

/// Record the static key an `XX` handshake learned against the accepted window
/// for that address, so `tcr peer block` can ban the key as well.
pub(super) async fn record_pairing_key(
    context: &SessionContext,
    addr: &str,
    key: PeerId,
    now_ms: i64,
) -> Result<()> {
    on_blocking_thread({
        let state_path = context.state_path.clone();
        let addr = addr.to_string();
        move || {
            let _lock = config::FileLock::acquire(&state_path)?;
            let mut peer_state = crate::peer::state::load(&state_path, now_ms)?;
            if !peer_state.learn_pairing_key(&addr, key) {
                return Ok(());
            }
            crate::peer::state::save(&state_path, &peer_state)
        }
    })
    .await
}
