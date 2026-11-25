//! Phase 4's gates: what a borrowed request may never carry, and the guard a
//! lease can never spend.
//!
//! Every test here RUNS, and the SERVE stream runs on the REAL listener: a
//! real lender and a real borrower in this one binary, on kernel ports, with a
//! fake upstream behind the lender's own proxy. Nothing here reads the
//! operator's config directory, touches the live proxy on `127.0.0.1:3456`,
//! or reaches the network.
//!
//! **This file no longer stands in for a missing line.** It used to say so:
//! `listener::serve_stream` matched `StreamKind::Serve` to a `bail!`, so
//! `mod mesh` did what that arm had to do and the two halves of the feature
//! were tested against each other with nothing joining them. This change landed
//! the arm, and `mesh::spawn_lender` hands a socket to
//! `listener::serve_on_with`: the shipped accept loop. The one harness left
//! (`mesh::spawn_capturing_lender`) exists to see the DECRYPTED request frame,
//! which the real listener deliberately exposes to nobody.
//!
//! # The credential assertions are the point of this file
//!
//! A SERVE is built at the picker's dry-fleet arm (`src/proxy/mod.rs`), and
//! `build_upstream_headers`: the ONLY code in this tree that removes a
//! client's own `authorization` and substitutes a pooled token, is 215 lines
//! downstream of it (definition `:3455`, sole call site `:2526`). So a relay
//! built at that seam forwards the client's credential to another host BY
//! DEFAULT, and nothing in between would stop it. These tests are what stands
//! where the existing code does not reach.

/// What the real API refuses a request without, shared by every fake upstream
/// in this tree. See that module's docs for why a fake upstream is strict.
#[path = "../../tools/api_contract.rs"]
mod api_contract;

/// The environment every launch of the built `tcr` starts from.
#[path = "../../support/tcr_env.rs"]
mod tcr_env;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tcr::base::log_err::LogErr as _;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use bytes::Bytes;
use tcr::base::provider::Provider;
use tcr::peer::config::{
    Allow, ControlGrants, Endpoint, EndpointSource, LendGrant, PeerFile, PeerRow, PeerStore,
};
use tcr::peer::lease::{self, Ledger, RelayRefusal};
use tcr::peer::serve;
use tcr::peer::serve::PeerLeaseProvider;
use tcr::peer::serve::fallback::{self, Ask, FallbackProvider};
use tcr::peer::tunnel::dial;
use tcr_peer_wire::{Lease, LeaseRefusal, LeaseRequest, LeaseUnit, PeerId, Window};

#[path = "peer_lease/borrow_deadline.rs"]
mod borrow_deadline;
#[path = "peer_lease/cli.rs"]
mod cli;
#[path = "peer_lease/dial_and_paths.rs"]
mod dial_and_paths;
#[path = "peer_lease/fixtures.rs"]
mod fixtures;
#[path = "peer_lease/fleet.rs"]
mod fleet;
#[path = "peer_lease/hostile.rs"]
mod hostile;
#[path = "peer_lease/ledger_and_frame.rs"]
mod ledger_and_frame;
#[path = "peer_lease/ledger_room.rs"]
mod ledger_room;
#[path = "peer_lease/mesh.rs"]
mod mesh;
#[path = "peer_lease/read_surface.rs"]
mod read_surface;
#[path = "peer_lease/scope_and_end.rs"]
mod scope_and_end;
#[path = "peer_lease/served_relay.rs"]
mod served_relay;
