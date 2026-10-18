//! Is Macs Fan Control running? Asked by the proxy's controller before every
//! fan write, never by the root helper (root does no process lookup).
//!
//! By bundle identifier, never by process name: every process's executable
//! path comes from `proc_pidpath`, the `.app` that contains it is opened with
//! `CFBundle`, and its `CFBundleIdentifier` is compared. A pid's answer is
//! kept until that pid is gone, so a probe costs one `proc_listallpids` plus a
//! path lookup for each pid it has not seen before.

use std::collections::HashMap;
use std::ffi::c_void;
use std::io;
use std::path::{Path, PathBuf};

use core_foundation::bundle::CFBundle;
use core_foundation::string::CFString;
use core_foundation::url::CFURL;

/// Macs Fan Control's bundle identifier.
pub const MACS_FAN_CONTROL: &str = "com.crystalidea.macsfancontrol";

/// Room for this many more pids than the kernel said were running, for the
/// ones that start between the two calls.
const PID_HEADROOM: usize = 64;

/// The probe and its per-pid cache.
#[derive(Debug, Default)]
pub struct MacsProbe {
    /// pid to the bundle identifier of its app, `None` for a process that is
    /// not inside an `.app` or whose bundle has no identifier.
    seen: HashMap<i32, Option<String>>,
}

impl MacsProbe {
    pub fn new() -> Self {
        Self::default()
    }

    /// Is any running process inside an app whose bundle identifier is
    /// [`MACS_FAN_CONTROL`]?
    pub fn running(&mut self) -> io::Result<bool> {
        let pids = all_pids()?;
        self.seen.retain(|pid, _| pids.contains(pid));
        for &pid in &pids {
            self.seen.entry(pid).or_insert_with(|| {
                pid_path(pid)
                    .as_deref()
                    .and_then(app_of)
                    .and_then(|app| bundle_identifier(&app))
            });
        }
        Ok(self
            .seen
            .values()
            .any(|id| id.as_deref() == Some(MACS_FAN_CONTROL)))
    }
}

/// Every pid on the machine.
fn all_pids() -> io::Result<Vec<i32>> {
    // SAFETY: a null buffer of size 0 asks only for the count.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut pids = vec![0_i32; count as usize + PID_HEADROOM];
    let bytes = i32::try_from(pids.len() * size_of::<i32>())
        .map_err(|_too_many| io::Error::other("pid buffer past i32::MAX bytes"))?;
    // SAFETY: `pids` is a live buffer of exactly `bytes` bytes.
    let filled = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast::<c_void>(), bytes) };
    if filled < 0 {
        return Err(io::Error::last_os_error());
    }
    pids.truncate(filled as usize);
    pids.retain(|&pid| pid > 0);
    Ok(pids)
}

/// The executable path of `pid`, or `None` when it is gone or not ours to
/// read.
fn pid_path(pid: i32) -> Option<PathBuf> {
    let mut buffer = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `buffer` is a live buffer of the size passed.
    let len = unsafe {
        libc::proc_pidpath(
            pid,
            buffer.as_mut_ptr().cast::<c_void>(),
            buffer.len() as u32,
        )
    };
    if len <= 0 {
        return None;
    }
    buffer.truncate(len as usize);
    String::from_utf8(buffer).ok().map(PathBuf::from)
}

/// The outermost `.app` directory containing `executable`.
fn app_of(executable: &Path) -> Option<PathBuf> {
    executable
        .ancestors()
        .filter(|p| p.extension().is_some_and(|e| e == "app"))
        .last()
        .map(Path::to_path_buf)
}

fn bundle_identifier(app: &Path) -> Option<String> {
    let url = CFURL::from_path(app, true)?;
    let bundle = CFBundle::new(url)?;
    let info = bundle.info_dictionary();
    let value = info.find(CFString::from_static_string("CFBundleIdentifier"))?;
    value.downcast::<CFString>().map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_outermost_app_is_the_bundle() {
        assert_eq!(
            app_of(Path::new(
                "/Applications/Macs Fan Control.app/Contents/MacOS/Macs Fan Control"
            )),
            Some(PathBuf::from("/Applications/Macs Fan Control.app"))
        );
        assert_eq!(
            app_of(Path::new(
                "/Applications/A.app/Contents/Library/Helpers/B.app/Contents/MacOS/B"
            )),
            Some(PathBuf::from("/Applications/A.app"))
        );
        assert_eq!(app_of(Path::new("/usr/bin/true")), None);
    }

    /// A positive control for the whole chain: this test's own process is
    /// not in an app, but Finder always is, with a known identifier.
    #[test]
    fn a_known_app_reads_its_identifier() {
        assert_eq!(
            bundle_identifier(Path::new("/System/Library/CoreServices/Finder.app")).as_deref(),
            Some("com.apple.finder")
        );
    }

    #[test]
    fn the_probe_lists_this_process() {
        let pids = all_pids().unwrap();
        assert!(pids.contains(&(std::process::id() as i32)));
        let mut probe = MacsProbe::new();
        // Whatever the answer on this machine, it is an answer, and a second
        // probe reuses the cache rather than growing it.
        probe.running().unwrap();
        let cached = probe.seen.len();
        probe.running().unwrap();
        assert!(probe.seen.len() <= cached + PID_HEADROOM);
    }
}
