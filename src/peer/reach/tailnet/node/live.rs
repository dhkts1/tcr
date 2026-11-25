//! The connections open through the node, counted by far address.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Live connections through the node
// ---------------------------------------------------------------------------

/// The connections open through the node, by far address, for `connected`.
#[derive(Debug, Default)]
pub struct LiveConnections {
    open: Mutex<HashMap<IpAddr, usize>>,
}

impl LiveConnections {
    /// Count one connection from `ip` until the returned guard drops.
    pub fn enter(self: &Arc<Self>, ip: IpAddr) -> LiveGuard {
        let mut open = match self.open.lock() {
            Ok(open) => open,
            Err(poisoned) => poisoned.into_inner(),
        };
        *open.entry(ip).or_default() += 1;
        LiveGuard {
            live: Arc::clone(self),
            ip,
        }
    }

    /// How many distinct far addresses have a connection open now.
    pub fn distinct(&self) -> u32 {
        let open = match self.open.lock() {
            Ok(open) => open,
            Err(poisoned) => poisoned.into_inner(),
        };
        u32::try_from(open.len()).unwrap_or(u32::MAX)
    }
}

/// One counted connection; uncounted when dropped.
pub struct LiveGuard {
    live: Arc<LiveConnections>,
    ip: IpAddr,
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        let mut open = match self.live.open.lock() {
            Ok(open) => open,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(count) = open.get_mut(&self.ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                open.remove(&self.ip);
            }
        }
    }
}
