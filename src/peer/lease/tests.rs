use tcr_peer_wire::{Control, PeerId};

use super::hand::{
    forget_handed_meter, note_handed_baseline, read_handed_bearers, register_handed_meter,
    report_handed_spend,
};
use super::*;

/// A lease on the seven-day window, live well past `now`.
fn live_lease(lease_id: u128, now_ms: i64) -> Lease {
    Lease {
        lease_id,
        window: Window::SevenDay,
        unit: LeaseUnit::Fraction(0.20),
        granted_at_ms: now_ms,
        expires_at_ms: now_ms + 300_000,
        spent: 0.0,
        max_inflight: 2,
        until: None,
    }
}

/// **A rise smaller than the utilization header's own step still costs
/// `MIN_DEBIT`.**
///
/// The header moves in steps, so a request cheaper than one step reports a
/// rise of zero on the answer that paid for it and the whole of it later,
/// on some other answer or on none at all. The serve path charges
/// `MIN_DEBIT` for exactly this (`Ledger::debit`) and the hand path charged
/// nothing, which is a lease a small-request borrower could hold open for
/// as long as it liked.
///
/// Watch it fail by returning `spent: rise` from `usage_hint`: the tiny
/// rise is reported as itself, well under the floor.
#[test]
fn a_positive_rise_under_the_floor_is_charged_the_floor() {
    let tiny = MIN_DEBIT / 10.0;
    let Some(Control::UsageHint { spent, .. }) = usage_hint(7, Some(0.40), Some(0.40 + tiny))
    else {
        panic!("a risen window reports a hint");
    };
    assert!(
        (spent - MIN_DEBIT).abs() < f64::EPSILON,
        "a rise of {tiny} is charged the floor, and this charged {spent}"
    );

    let Some(Control::UsageHint { spent, .. }) = usage_hint(7, Some(0.40), Some(0.47)) else {
        panic!("a risen window reports a hint");
    };
    assert!(
        (spent - 0.07).abs() < 1e-9,
        "a rise above the floor is still itself, and this charged {spent}"
    );

    assert!(
        usage_hint(7, Some(0.40), Some(0.40)).is_none(),
        "an answer that moved nothing is still free: the floor lifts a positive rise and \
         never invents one"
    );
    assert!(
        usage_hint(7, Some(0.40), Some(0.10)).is_none(),
        "and a window that reset under the request is the owner's windfall, never a credit"
    );
}

/// **The meter's baseline only ever moves forward, and it survives the
/// session being re-registered.**
///
/// Two failures in one: with `max_inflight` above one the answers arrive in
/// whatever order the network hands them over, and a baseline pushed
/// BACKWARDS by the older of them bills the same quota twice on the next
/// answer; and a lease re-asked for inside its own life used to reset the
/// meter to `None`, which made the next answer free because one reading is
/// not a rise.
///
/// Watch it fail by putting `meter.last.replace(utilization)` back in
/// `report_handed_spend`: the out-of-order pair leaves the baseline at 0.40
/// and the third answer is charged 0.10 instead of 0.03. And by seeding
/// `HandedMeter { last: None }` in `register_handed_meter`: the re-ask
/// leaves nothing to measure against and the answer after it is free.
#[test]
fn the_baseline_moves_forward_only_and_survives_a_re_ask() {
    let lease_id = 0xBA5E_u128;
    let (sink, mut hints) = tokio::sync::mpsc::unbounded_channel();
    register_handed_meter(lease_id, sink);
    note_handed_baseline(lease_id, 0.30);

    // Out of order: the later answer lands first.
    report_handed_spend(lease_id, Some(0.50));
    report_handed_spend(lease_id, Some(0.40));
    report_handed_spend(lease_id, Some(0.53));

    let charged: Vec<f64> = std::iter::from_fn(|| hints.try_recv().ok())
        .map(|hint| match hint {
            Control::UsageHint { spent, .. } => spent,
            other => panic!("a meter reports usage and nothing else, got {other:?}"),
        })
        .collect();
    assert_eq!(
        charged.len(),
        2,
        "the answer that read BELOW the baseline reports nothing, so two of the three \
         charge: {charged:?}"
    );
    assert!(
        (charged[0] - 0.20).abs() < 1e-9,
        "the first is measured against the handoff's own baseline: {charged:?}"
    );
    assert!(
        (charged[1] - 0.03).abs() < 1e-9,
        "and the third against 0.50, which the out-of-order 0.40 must not have pulled \
         back down: {charged:?}"
    );

    // The re-ask: a second session for the same lease replaces the sink and
    // keeps the figure.
    let (sink, mut hints) = tokio::sync::mpsc::unbounded_channel();
    register_handed_meter(lease_id, sink);
    report_handed_spend(lease_id, Some(0.56));
    let Some(Control::UsageHint { spent, .. }) = hints.try_recv().ok() else {
        panic!("the first answer after a re-ask is charged, not free");
    };
    assert!(
        (spent - 0.03).abs() < 1e-9,
        "and it is charged against the baseline the meter already held: {spent}"
    );
    forget_handed_meter(lease_id);
}

/// **A usage hint landing mid-frame does not cost the borrower its
/// bearer.**
///
/// The reader holds a `biased` `select!` between the hint channel and the
/// stream, so a hint queued while a `Handoff` is half read DROPS the read.
/// `serve::recv_control` is not cancel safe: the bytes it had already taken
/// off the socket went with the dropped future, the next read met the tail
/// of one frame as a length prefix, the Noise stream desynced, and this
/// borrower forgot its bearer for the rest of the lease.
/// `noise::FrameReader` keeps the part-read bytes in the loop's own state,
/// which is the failure its own doc describes.
///
/// Driven against the real function over a duplex with a real Noise
/// session, and the frame is split by hand with the hint pushed between the
/// halves, because the claim is about a cancellation that only happens when
/// those two land in that order.
///
/// Watch it fail by putting `frame = serve::recv_control(&mut stream, &mut
/// session) => frame` back in `read_handed_bearers`'s `select!`: the second
/// bearer never arrives and this times out on the first one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hint_landing_mid_frame_does_not_lose_the_bearer() {
    use tokio::io::AsyncWriteExt as _;

    let lease_id = 0xF4A3_u128;
    let now_ms = crate::now_ms();
    let (borrower_end, mut lender_end) = tokio::io::duplex(64 * 1024);
    let (borrower_secret, borrower_public) =
        crate::peer::noise::generate_static().expect("a borrower keypair");
    let (lender_secret, lender_public) =
        crate::peer::noise::generate_static().expect("a lender keypair");

    let mut borrower_end = borrower_end;
    let dialling = tokio::spawn(async move {
        let session = crate::peer::noise::dial_handshake(
            &mut borrower_end,
            &borrower_secret,
            crate::peer::noise::Handshake::Return,
            Some(&lender_public),
            None,
        )
        .await
        .expect("the borrower dials");
        (borrower_end, session)
    });
    let mut lender = crate::peer::noise::accept_handshake(
        &mut lender_end,
        &lender_secret,
        crate::peer::noise::Handshake::Return,
        &[],
        |offered| {
            let key: [u8; 32] = offered
                .try_into()
                .map_err(|_| crate::peer::noise::PinRefusal::Malformed { len: offered.len() })?;
            Ok(PeerId(key))
        },
    )
    .await
    .expect("the lender accepts");
    let (borrower_end, borrower_session) = dialling.await.expect("the dial finishes");

    let lender_id = PeerId(borrower_public);
    tokio::spawn(read_handed_bearers(
        Box::new(borrower_end),
        borrower_session,
        live_lease(lease_id, now_ms),
        lender_id,
    ));

    let handoff = |token: &str| Control::Handoff {
        lease_id,
        access_token: tcr_peer_wire::HandoffToken::new(token.to_string()),
        expires_at_ms: now_ms + 300_000,
        utilization: Some(0.30),
    };
    let held = |token: &str| {
        handed_tokens()
            .lock()
            .expect("the handed-token store")
            .bearer(lease_id, crate::now_ms())
            == Some(token)
    };
    let wait_for = |token: String| async move {
        for _ in 0..200 {
            if held(&token) {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        false
    };

    let first = serde_json::to_vec(&handoff("fake-first-bearer")).expect("it serializes");
    crate::peer::noise::send_encrypted(&mut lender_end, &mut lender.transport, &first)
        .await
        .expect("the first handoff goes out whole");
    assert!(
        wait_for("fake-first-bearer".to_string()).await,
        "the whole frame arrives, which is the control: without it the split frame below \
         would prove nothing"
    );

    // The second frame, encrypted into a buffer so it can be handed over in
    // two pieces with a cancellation between them.
    let second = serde_json::to_vec(&handoff("fake-second-bearer")).expect("it serializes");
    let mut framed: Vec<u8> = Vec::new();
    crate::peer::noise::send_encrypted(&mut framed, &mut lender.transport, &second)
        .await
        .expect("the second handoff encrypts");
    let split = 3;
    lender_end
        .write_all(&framed[..split])
        .await
        .expect("the first piece goes out");
    lender_end.flush().await.expect("and is flushed");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // THE CANCELLATION: a hint on the biased arm while the read is parked
    // part way through the frame above.
    report_handed_spend(lease_id, Some(0.45));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    lender_end
        .write_all(&framed[split..])
        .await
        .expect("the rest goes out");
    lender_end.flush().await.expect("and is flushed");

    assert!(
        wait_for("fake-second-bearer".to_string()).await,
        "the renewal that straddled the cancelled read must still reach the store; the \
         bearer held is whatever the desynced stream left behind"
    );
}

/// **A handoff with no baseline is refused, and the line names the Mac to
/// update.**
///
/// The field is what a build older than this one leaves out. Storing the
/// bearer anyway would make every answer on that lease free, so it is
/// refused; the borrow falls back to the lender's own serve path, which is
/// a working borrow and not an outage, and the only thing an operator can
/// act on is which Mac to update.
///
/// Watch it fail by returning `Ok(0.0)` for `None` in `handed_baseline`.
#[test]
fn a_handoff_without_a_baseline_is_refused_by_name() {
    let lender = PeerId([9_u8; 32]);
    let refused = handed_baseline(&lender, None).expect_err("no baseline is a refusal");
    assert!(
        refused.contains(&lender.display()),
        "the refusal names the Mac whose build is behind, and it reads: {refused}"
    );
    assert!(
        handed_baseline(&lender, Some(f64::NAN)).is_err(),
        "a figure that is not a number is no baseline either"
    );
    assert_eq!(
        handed_baseline(&lender, Some(0.30)).ok(),
        Some(0.30),
        "and a figure that is one is taken as it stands"
    );
}
