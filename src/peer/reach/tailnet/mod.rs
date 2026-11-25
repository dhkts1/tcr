//! Reaching this operator's own Macs over a tailnet: the Tailscale app already
//! running here, the beacon that finds the other Macs on it, and tcr's own
//! tailnet node.

/// Reading the Tailscale app already running on this Mac: its CLI, under a
/// deadline, parsed once. Called by `tcr peer tailnet` and the serving
/// process's browse task.
pub mod app;

/// Finding the operator's own Macs over the tailnet: the beacon socket, the
/// intake that bounds what it hears, and the record each tick writes.
pub mod find;

/// tcr's own tailnet node: one supervised `tailscale::Device`, its key file,
/// and the join and leave orderings. The library itself is compiled only with
/// the `tailnet-node` feature on Apple silicon; the rest is in every build.
pub mod node;
