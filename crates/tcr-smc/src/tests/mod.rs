//! The crate's tests, one child per module they test, and the fake SMC the
//! reader's and the writer's tests share.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;

mod codec;
mod reader;
mod writer;

fn code(text: &str) -> FourCc {
    FourCc::parse(text).unwrap()
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

/// An SMC made of a table. A key with no entry answers "no such key", as a
/// real SMC does for a key this Mac lacks.
#[derive(Default)]
struct FakeSmc {
    values: HashMap<FourCc, Result<SmcValue, SmcError>>,
    key_table: Option<Result<Vec<FourCc>, SmcError>>,
    reads: Vec<FourCc>,
    keys_calls: usize,
    /// Every write that landed, in order.
    writes: Vec<(FourCc, SmcValue)>,
    /// Every write asked for, landed or not, in order.
    attempts: Vec<FourCc>,
    /// Refuse this many writes to the key (result 0x82), then take them.
    refusals: HashMap<FourCc, usize>,
    /// Fail every write to the key with this error, before anything else.
    failures: HashMap<FourCc, SmcError>,
    /// Take writes to the key and keep its old value: an SMC that says
    /// yes and does not change.
    sticky: HashSet<FourCc>,
    /// Take writes to the key but keep answering reads with the old
    /// value for this many reads after each write, as a real SMC does
    /// while it applies a mode change.
    lag: HashMap<FourCc, usize>,
    /// A landed write not applied yet: the value, and reads left before it is.
    pending: HashMap<FourCc, (SmcValue, usize)>,
}

impl FakeSmc {
    fn set(&mut self, key: &str, value: Result<SmcValue, SmcError>) {
        self.values.insert(code(key), value);
    }

    fn read_count(&self, key: &str) -> usize {
        self.reads.iter().filter(|k| **k == code(key)).count()
    }

    fn one_fan(&mut self, actual: f32) {
        self.set("FNum", Ok(SmcValue::ui8(1)));
        self.set("F0Ac", flt(actual));
        self.set("F0Mn", flt(1000.0));
        self.set("F0Mx", flt(5000.0));
    }

    fn with_table(table: Result<Vec<FourCc>, SmcError>) -> Self {
        Self {
            key_table: Some(table),
            ..Self::default()
        }
    }
}

impl KeySource for FakeSmc {
    fn read(&mut self, key: FourCc) -> Result<SmcValue, SmcError> {
        self.reads.push(key);
        if let Some((value, left)) = self.pending.get_mut(&key) {
            if *left == 0 {
                let value = value.clone();
                self.pending.remove(&key);
                self.values.insert(key, Ok(value));
            } else {
                *left -= 1;
            }
        }
        self.values
            .get(&key)
            .cloned()
            .unwrap_or(Err(SmcError::KeyNotFound(key)))
    }

    fn keys(&mut self) -> Result<Vec<FourCc>, SmcError> {
        self.keys_calls += 1;
        self.key_table.clone().unwrap_or(Ok(Vec::new()))
    }
}

/// What `SmcConnection::write` does: nothing sent unless the value is the
/// key's own type and size, a missing key is "no such key".
impl KeyAccess for FakeSmc {
    fn write(&mut self, key: FourCc, value: &SmcValue) -> Result<(), SmcError> {
        self.attempts.push(key);
        if let Some(failure) = self.failures.get(&key) {
            return Err(*failure);
        }
        if let Some(left) = self.refusals.get_mut(&key)
            && *left > 0
        {
            *left -= 1;
            return Err(SmcError::SmcRefused { key, result: 0x82 });
        }
        let current = match self.values.get(&key) {
            Some(Ok(current)) => current.clone(),
            Some(Err(error)) => return Err(*error),
            None => return Err(SmcError::KeyNotFound(key)),
        };
        check_write(
            value,
            SmcKeyInfo {
                ty: current.ty,
                size: current.bytes.len(),
            },
        )?;
        self.writes.push((key, value.clone()));
        if let Some(&reads) = self.lag.get(&key) {
            self.pending.insert(key, (value.clone(), reads));
        } else if !self.sticky.contains(&key) {
            self.values.insert(key, Ok(value.clone()));
        }
        Ok(())
    }
}

/// A `flt ` built from raw bits, so NaN and negatives can be served: the
/// checked [`SmcValue::flt`] refuses them, and an SMC need not.
fn flt(value: f32) -> Result<SmcValue, SmcError> {
    Ok(SmcValue::new(FourCc::FLT, value.to_le_bytes().to_vec()))
}

fn fan_index(index: u8) -> FanIndex {
    FanIndex::new(index).unwrap()
}

/// A fan index is one digit: 9 parses, 10 and past do not, so no key can
/// name `F:Ac` or wrap past it.
#[test]
fn fan_index_is_one_digit() {
    assert_eq!(FanIndex::new(0).map(FanIndex::get), Some(0));
    assert_eq!(fan_key(fan_index(9), *b"Ac"), code("F9Ac"));
    for past in [10, 11, 42, 255] {
        assert_eq!(FanIndex::new(past), None, "{past}");
    }
}
