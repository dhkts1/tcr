use anyhow::{Result, bail};
use tcr_peer_wire::PeerId;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::peer::id::KEY_BYTES;
use crate::peer::noise::{self, Handshake, PeerSession, PinRefusal};
use crate::peer::state::PeerState;

use super::SessionContext;
use super::admission::ACCEPTED_WINDOW_SECS;

/// An authenticated session, plus the one thing about HOW it authenticated
/// that the stream handler cannot re-derive.
///
/// The extra field is the enrolment secret. A join token carries no invite id,
/// so the PSK that decrypted message 1 is the only thing that identifies which
/// invite is being spent, and [`crate::peer::pair::accept_enrolment`] needs it
/// to retire the right row. See the ordering rules in
/// [`crate::peer::noise`]'s module docs. There was no way to carry it out
/// of the handshake, which is exactly why `accept_enrolment` had no production
/// caller and the headless join path pinned nothing on the registrar's side.
pub(super) struct AcceptedSession {
    /// The authenticated session.
    pub(super) session: PeerSession,
    /// The outstanding invite's secret, on [`Handshake::Enrol`] only.
    pub(super) enrol_secret: Option<[u8; KEY_BYTES]>,
}

/// Run the responder's half for a first pairing, a return visit or an
/// enrolment, from the message 1 the caller already holds.
///
/// Knocks do not come here: they are [`serve_knock`](crate::peer::listener::knock::serve_knock), which shares no code with
/// this function because it shares no decision either, a knock has no static
/// key, no pin, no grant and no stream.
///
/// The order for the two 96-byte patterns is the common case first:
/// [`Handshake::Return`] is tried, and only a message that does not
/// authenticate under it is offered to the outstanding invites as an
/// enrolment. Both orders are safe, an `IKpsk1` message 1 never decrypts under
/// plain `IK`, because the PSK is mixed in before it, and this one keeps a
/// returning peer from paying the trial cost.
///
/// A pin refusal is therefore final: it happens only on a message that already
/// authenticated as `IK`, so there is no path where a refused peer is then
/// offered a second pattern.
///
/// # What authorizes an `XX` message 1, which is the operator and nothing else
///
/// Answering an `XX` message 1 discloses this node's static key in message 2.
/// There is no pin to check, that is what "first pairing" means, so the only
/// thing that can stand in for one is the operator having approved THIS
/// machine: [`crate::peer::state::PeerState::accepted_window`] must hold a live
/// window for this source address AND the instance id the message 1 payload
/// carries.
///
/// **A node-wide window was replaced with that pair**, and the
/// difference is the whole point: the older `tcr peer pair` opened two
/// minutes in which ANY host that could reach this port got message 2, so a
/// stranger who happened to be scanning during a legitimate pairing harvested
/// this node's static key. Now a window exists only because the operator
/// pressed Accept on a knock they could see, and it admits one id at one
/// address.
///
/// A banned static key is refused here too, inside the pin callback, which is
/// the earliest moment a key is known. See
/// [`crate::peer::state::PeerState::is_key_banned`] for why the address half of
/// a ban is not enough on its own.
#[expect(
    clippy::too_many_arguments,
    reason = "the stream, the handshake already begun and the pairing state it is checked against"
)]
pub(super) async fn accept_pairing_or_return<S>(
    stream: &mut S,
    context: &SessionContext,
    addr: &str,
    pattern: Handshake,
    message_1: &[u8],
    pinned: &[PeerId],
    psks: &[[u8; KEY_BYTES]],
    peer_state: &PeerState,
    now_ms: i64,
) -> Result<AcceptedSession>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let secret = &context.secret;
    let mut scratch = vec![0_u8; noise::HANDSHAKE_SCRATCH_BYTES];

    if pattern == Handshake::Pair {
        let read = noise::read_message_1(secret, Handshake::Pair, &[], message_1, &mut scratch)?;
        // The payload is read out of the message this node already holds, so
        // the check below costs no round trip and happens before message 2.
        let claimed = noise::Message1 {
            state: read,
            psk: None,
            payload: message_1[32..].to_vec(),
        };
        let instance_id = claimed.instance_id()?;
        if !peer_state.accepted_window(addr, &instance_id, now_ms) {
            bail!(
                "peer listener: a first pairing arrived from {addr} as instance {instance_id} \
                 and no window is open for it, so nothing was answered. A window exists only \
                 after the operator accepts a pairing request on THIS Mac \
                 (`tcr peer pending`, then `tcr peer accept <instance|addr>`), it lasts {ACCEPTED_WINDOW_SECS} \
                 seconds, and it admits that one instance at that one address"
            );
        }
        let session = noise::finish_responder(stream, claimed.state, Handshake::Pair, |_| {
            Ok(PeerId([0_u8; KEY_BYTES]))
        })
        .await?;
        if peer_state.is_key_banned(&session.peer) {
            bail!(
                "peer listener: the static key this pairing revealed is blocked, so it is \
                 refused from this new address too (`tcr peer unblock` lifts it)"
            );
        }
        return Ok(AcceptedSession {
            session,
            enrol_secret: None,
        });
    }

    let returning = noise::read_message_1(secret, Handshake::Return, &[], message_1, &mut scratch);
    match returning {
        Ok(state) => {
            let pinned_ids = pinned.to_vec();
            let banned = peer_state.banned.clone();
            let session =
                noise::finish_responder(stream, state, Handshake::Return, move |remote| {
                    let pinned = noise::pin_check_ids(remote, &pinned_ids)?;
                    // A pinned peer whose KEY is banned is refused, and the refusal
                    // is here rather than after the handshake for the same reason
                    // the pin check is: nothing has been written yet.
                    if banned.iter().any(|ban| ban.key.as_ref() == Some(&pinned)) {
                        return Err(PinRefusal::NotPinned {
                            offered: pinned.display(),
                        });
                    }
                    Ok(pinned)
                })
                .await?;
            Ok(AcceptedSession {
                session,
                enrol_secret: None,
            })
        }
        Err(return_error) => {
            if psks.is_empty() {
                return Err(return_error.context(
                    "peer listener: no outstanding invite to try this as an enrolment \
                     (`tcr peer invite` mints one)",
                ));
            }
            let read = noise::read_message_1_matching(
                secret,
                Handshake::Enrol,
                psks,
                message_1,
                &mut scratch,
            )?;
            // Enrolment's authorization is the PSK, and it was proved inside
            // message 1 before this line ran. The row is written by
            // `crate::peer::pair`, which owns both halves of enrolment.
            //
            // A banned KEY is still refused here. The Pair arm and the Return
            // arm both do it and this one did not, so a blocked key that
            // dialled from an address the ban does not name, holding a live
            // invite, enrolled: a ban that an invite silently lifts is not a
            // ban. The address half fires earlier and catches the rest.
            let banned = peer_state.banned.clone();
            let session =
                noise::finish_responder(stream, read.state, Handshake::Enrol, move |remote| {
                    let joiner = match <[u8; KEY_BYTES]>::try_from(remote) {
                        Ok(key) => PeerId(key),
                        Err(_) => return Err(PinRefusal::Malformed { len: remote.len() }),
                    };
                    if banned.iter().any(|ban| ban.key.as_ref() == Some(&joiner)) {
                        return Err(PinRefusal::NotPinned {
                            offered: joiner.display(),
                        });
                    }
                    Ok(joiner)
                })
                .await?;
            Ok(AcceptedSession {
                session,
                enrol_secret: read.psk,
            })
        }
    }
}
