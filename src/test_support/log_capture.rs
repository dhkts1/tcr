//! A tracing writer a test can read back.

use std::sync::{Arc, Mutex};

/// A `MakeWriter` that keeps what was written, so a test can inspect tracing output without
/// capturing the process's real stdout. Clones share one buffer: hand one to the subscriber and
/// read [`SharedBuf::contents`] on the other.
#[derive(Clone, Default)]
pub struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    /// Everything written so far, as text.
    pub fn contents(&self) -> String {
        let bytes = self.0.lock().expect("shared buffer poisoned").clone();
        String::from_utf8_lossy_owned(bytes)
    }

    /// Forget everything written so far.
    pub fn clear(&self) {
        self.0.lock().expect("shared buffer poisoned").clear();
    }
}

impl std::io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("shared buffer poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
