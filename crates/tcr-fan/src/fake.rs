//! A fake SMC for root's tests: a table of keys shared between the writer that
//! root owns and the test that inspects it.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use tcr_smc::{FourCc, KeyAccess, KeySource, SmcError, SmcKeyInfo, SmcValue, check_write};

#[derive(Default)]
pub(crate) struct State {
    values: HashMap<FourCc, SmcValue>,
    /// Every write that landed, in order.
    writes: Vec<(FourCc, SmcValue)>,
    /// Every write asked for, landed or not.
    attempts: Vec<FourCc>,
    /// Keys whose writes the SMC refuses (result 0x82), every time.
    refused: HashSet<FourCc>,
    /// Keys that read this many more times, then fail every read.
    reads_left: HashMap<FourCc, usize>,
}

fn code(key: &str) -> FourCc {
    FourCc::parse(key).unwrap()
}

/// The test's handle on the table.
#[derive(Clone, Default)]
pub(crate) struct Shared(Arc<Mutex<State>>);

impl Shared {
    /// Two fans of the Mac16,7 reading: 1,350 to 5,777 rpm, `flt ` targets,
    /// `F{n}Md` mode keys at 0, `Ftst` at 0.
    pub(crate) fn apple_silicon() -> Self {
        let smc = Self::default();
        smc.set_ui8("FNum", 2);
        for fan in ["F0", "F1"] {
            smc.set_flt(&format!("{fan}Mn"), 1350.0);
            smc.set_flt(&format!("{fan}Mx"), 5777.0);
            smc.set_flt(&format!("{fan}Tg"), 0.0);
            smc.set_ui8(&format!("{fan}Md"), 0);
        }
        smc.set_ui8("Ftst", 0);
        smc
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }

    pub(crate) fn handle(&self) -> FakeSmc {
        FakeSmc(self.clone())
    }

    pub(crate) fn set_ui8(&self, key: &str, value: u8) {
        self.state().values.insert(code(key), SmcValue::ui8(value));
    }

    pub(crate) fn set_flt(&self, key: &str, value: f32) {
        self.state()
            .values
            .insert(code(key), SmcValue::flt(value).unwrap());
    }

    pub(crate) fn ui8(&self, key: &str) -> u8 {
        self.state().values[&code(key)].uint8_value().unwrap()
    }

    pub(crate) fn refuse(&self, key: &str) {
        self.state().refused.insert(code(key));
    }

    /// Reads of `key` work `ok` more times, then fail with an IOKit timeout
    /// until [`Self::accept`].
    pub(crate) fn fail_reads_after(&self, key: &str, ok: usize) {
        self.state().reads_left.insert(code(key), ok);
    }

    /// Writes and reads of `key` work again.
    pub(crate) fn accept(&self, key: &str) {
        let mut state = self.state();
        state.refused.remove(&code(key));
        state.reads_left.remove(&code(key));
    }

    pub(crate) fn writes(&self) -> Vec<(FourCc, SmcValue)> {
        self.state().writes.clone()
    }

    pub(crate) fn written_keys(&self) -> Vec<String> {
        self.state()
            .writes
            .iter()
            .map(|(k, _)| k.to_string())
            .collect()
    }

    pub(crate) fn attempts_at(&self, key: &str) -> usize {
        let key = code(key);
        self.state().attempts.iter().filter(|k| **k == key).count()
    }

    /// Forget every write and attempt so far.
    pub(crate) fn clear_writes(&self) {
        let mut state = self.state();
        state.writes.clear();
        state.attempts.clear();
    }
}

/// What root's writer holds.
pub(crate) struct FakeSmc(Shared);

impl KeySource for FakeSmc {
    fn read(&mut self, key: FourCc) -> Result<SmcValue, SmcError> {
        let mut state = self.0.state();
        if let Some(left) = state.reads_left.get_mut(&key) {
            if *left == 0 {
                // kIOReturnTimeout
                return Err(SmcError::Kernel(0xe000_02d6_u32 as i32));
            }
            *left -= 1;
        }
        state
            .values
            .get(&key)
            .cloned()
            .ok_or(SmcError::KeyNotFound(key))
    }

    fn keys(&mut self) -> Result<Vec<FourCc>, SmcError> {
        Ok(self.0.state().values.keys().copied().collect())
    }
}

impl KeyAccess for FakeSmc {
    fn write(&mut self, key: FourCc, value: &SmcValue) -> Result<(), SmcError> {
        let mut state = self.0.state();
        state.attempts.push(key);
        if state.refused.contains(&key) {
            return Err(SmcError::SmcRefused { key, result: 0x82 });
        }
        let current = state.values.get(&key).ok_or(SmcError::KeyNotFound(key))?;
        check_write(
            value,
            SmcKeyInfo {
                ty: current.ty,
                size: current.bytes.len(),
            },
        )?;
        state.writes.push((key, value.clone()));
        state.values.insert(key, value.clone());
        Ok(())
    }
}
