//! The root fan helper's XPC contract: four operations, their replies and the
//! typed error codes, encoded into and decoded out of a dictionary.
//!
//! Every request and reply is one XPC dictionary. Integers are `uint64`, an rpm
//! is a `double`, a flag is a `bool`, the build is a `string`. Decoding is
//! strict in both directions: a dictionary whose key count, key names or value
//! types are not exactly its operation's row is refused, and root touches no SMC
//! key for a request it refused ([`decode_request`] answers before any write).
//!
//! | op (`"op"`) | request keys | success reply keys |
//! |---|---|---|
//! | 1 version | `op` | `status` 0, `protocol` (1), `build` (at most 64 bytes) |
//! | 2 set-manual | `op`, `fan` | `status` 0, `unlocked` |
//! | 3 set-target | `op`, `fan`, `rpm` | `status` 0, `rpm` (what was written) |
//! | 4 hand-back | `op` | `status` 0 |
//!
//! No request carries a mode. Set-manual writes 1 to the fan's mode key and
//! hand-back writes 0, and neither takes a value that could become anything
//! else: a caller that passes the signature check still cannot write a mode
//! number of its choosing.
//!
//! The codec runs over two small traits, [`Dict`] and [`DictMut`], so the same
//! code is tested on Linux over [`MemDict`] and used on macOS over a real XPC
//! dictionary (`xpc::XpcDict`).

use std::collections::BTreeMap;
use std::fmt;

use tcr_smc::{FanError, FanIndex, SmcError};

/// The one protocol this crate speaks. A helper answering another number is a
/// helper from another release: the caller stops rather than guess.
pub const PROTOCOL_VERSION: u64 = 1;

/// The longest `build` string a version reply may carry. Root cuts its own
/// build stamp to this; a client refuses a longer one as malformed.
pub const MAX_BUILD_BYTES: usize = 64;

/// The dictionary keys, each written once.
pub mod key {
    pub const OP: &str = "op";
    pub const FAN: &str = "fan";
    pub const RPM: &str = "rpm";
    pub const STATUS: &str = "status";
    pub const PROTOCOL: &str = "protocol";
    pub const BUILD: &str = "build";
    pub const UNLOCKED: &str = "unlocked";
    pub const COUNT: &str = "count";
    pub const KR: &str = "kr";
    pub const FANS: &str = "fans";
    pub const FTST: &str = "ftst";
}

/// The four operations. There is no fifth: a "disown" or a "set mode N" is
/// covered by the disconnect rule and by [`HelperError::NotHolder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Version,
    SetManual,
    SetTarget,
    HandBack,
}

impl Op {
    pub const ALL: [Self; 4] = [
        Self::Version,
        Self::SetManual,
        Self::SetTarget,
        Self::HandBack,
    ];

    pub const fn code(self) -> u64 {
        match self {
            Self::Version => 1,
            Self::SetManual => 2,
            Self::SetTarget => 3,
            Self::HandBack => 4,
        }
    }

    pub const fn from_code(code: u64) -> Option<Self> {
        match code {
            1 => Some(Self::Version),
            2 => Some(Self::SetManual),
            3 => Some(Self::SetTarget),
            4 => Some(Self::HandBack),
            _ => None,
        }
    }

    /// The keys a request for this op carries, with their types: the whole
    /// row, so a decode can insist on exactly this and nothing more.
    const fn request_keys(self) -> &'static [(&'static str, Kind)] {
        match self {
            Self::Version | Self::HandBack => &[(key::OP, Kind::U64)],
            Self::SetManual => &[(key::OP, Kind::U64), (key::FAN, Kind::U64)],
            Self::SetTarget => &[
                (key::OP, Kind::U64),
                (key::FAN, Kind::U64),
                (key::RPM, Kind::F64),
            ],
        }
    }

    const fn success_keys(self) -> &'static [(&'static str, Kind)] {
        match self {
            Self::Version => &[
                (key::STATUS, Kind::U64),
                (key::PROTOCOL, Kind::U64),
                (key::BUILD, Kind::Str),
            ],
            Self::SetManual => &[(key::STATUS, Kind::U64), (key::UNLOCKED, Kind::Bool)],
            Self::SetTarget => &[(key::STATUS, Kind::U64), (key::RPM, Kind::F64)],
            Self::HandBack => &[(key::STATUS, Kind::U64)],
        }
    }
}

/// One request, parsed. A fan index is a [`FanIndex`] from here on, so a key
/// built from it can only name `F0` to `F9`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Request {
    Version,
    /// Mode key to 1, through the `Ftst` unlock where the SMC refuses.
    SetManual {
        fan: FanIndex,
    },
    /// `rpm` may be anything a `double` holds on the wire; root refuses NaN and
    /// the infinities with [`HelperError::NotANumber`] and clamps the rest.
    SetTarget {
        fan: FanIndex,
        rpm: f64,
    },
    /// Every mode key to 0, then `Ftst` to 0.
    HandBack,
}

impl Request {
    pub const fn op(&self) -> Op {
        match self {
            Self::Version => Op::Version,
            Self::SetManual { .. } => Op::SetManual,
            Self::SetTarget { .. } => Op::SetTarget,
            Self::HandBack => Op::HandBack,
        }
    }
}

/// Why [`decode_request`] refused a dictionary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Unknown op, a missing or extra key, or a value of the wrong XPC type.
    BadRequest,
    /// A well-formed request naming a fan past 9, which no SMC key can name.
    /// Root answers it as [`HelperError::NoSuchFan`], with this Mac's count.
    FanPastMax { fan: u64 },
}

/// A refusal from root, by its wire code. Each carries exactly the fields its
/// row names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperError {
    /// 1: unknown op, extra or missing key, wrong XPC type.
    BadRequest,
    /// 2: `fan` is not below `count`, this Mac's `FNum` capped at 10.
    NoSuchFan { fan: u64, count: u64 },
    /// 3: rpm NaN or infinite, refused and never clamped.
    NotANumber,
    /// 4: the fan's `Mn`/`Mx` are not finite, or the minimum is above the maximum.
    LimitsUnreadable { fan: u64 },
    /// 5: the mode key stayed automatic through the whole unlock sequence.
    ModeLocked { fan: u64 },
    /// 6: a read or write failed. `kr` is the IOKit `kern_return_t` (values
    /// from `0xe0000000` up), or, when IOKit succeeded and the SMC itself
    /// refused, the SMC's own result byte (below `0x100`).
    SmcRefused { kr: u64 },
    /// 7: no AppleSMC.
    SmcUnavailable,
    /// 8: a key's type or size is not what the writer expects: a bug, or a Mac
    /// it does not know, never a lock.
    KeyTypeMismatch,
    /// 9: after the hand-back's writes, a re-read still shows a mode key (bit
    /// `n` of `fans`) or `Ftst` not 0.
    HandBackIncomplete { fans: u64, ftst: bool },
    /// 10: set-target or hand-back from a connection that does not hold the
    /// fans while another one does, or set-target while nobody holds them.
    NotHolder,
}

impl HelperError {
    pub const fn code(self) -> u64 {
        match self {
            Self::BadRequest => 1,
            Self::NoSuchFan { .. } => 2,
            Self::NotANumber => 3,
            Self::LimitsUnreadable { .. } => 4,
            Self::ModeLocked { .. } => 5,
            Self::SmcRefused { .. } => 6,
            Self::SmcUnavailable => 7,
            Self::KeyTypeMismatch => 8,
            Self::HandBackIncomplete { .. } => 9,
            Self::NotHolder => 10,
        }
    }

    /// The writer's error, as the wire carries it.
    pub fn from_fan(error: FanError) -> Self {
        match error {
            FanError::NoSuchFan { fan, count } => Self::NoSuchFan {
                fan: u64::from(fan.get()),
                count: u64::from(count),
            },
            FanError::NotANumber => Self::NotANumber,
            FanError::LimitsUnreadable { fan } => Self::LimitsUnreadable {
                fan: u64::from(fan.get()),
            },
            FanError::ModeLocked { fan } => Self::ModeLocked {
                fan: u64::from(fan.get()),
            },
            FanError::Smc(error) => Self::from_smc(error),
            FanError::HandBackIncomplete(incomplete) => Self::HandBackIncomplete {
                fans: u64::from(incomplete.fans),
                ftst: incomplete.ftst,
            },
        }
    }

    pub const fn from_smc(error: SmcError) -> Self {
        match error {
            SmcError::Unavailable => Self::SmcUnavailable,
            SmcError::NotPrivileged => Self::SmcRefused {
                kr: SmcError::NOT_PRIVILEGED_CODE as u32 as u64,
            },
            SmcError::Kernel(code) => Self::SmcRefused {
                kr: code as u32 as u64,
            },
            SmcError::KeyNotFound(_) => Self::SmcRefused {
                kr: SmcError::SMC_KEY_NOT_FOUND as u64,
            },
            SmcError::SmcRefused { result, .. } | SmcError::KeyIndexRefused { result, .. } => {
                Self::SmcRefused { kr: result as u64 }
            }
            // An encode that does not fit its type cannot come from a clamped
            // rpm on a fan with a sane range; it means the key is not the type
            // the writer was built for, which is this code's row.
            SmcError::WrongType { .. }
            | SmcError::WrongSize { .. }
            | SmcError::UnsupportedType(_)
            | SmcError::OutOfRange { .. } => Self::KeyTypeMismatch,
        }
    }

    /// The fields this code's reply carries besides `status`.
    const fn fields(code: u64) -> Option<&'static [(&'static str, Kind)]> {
        Some(match code {
            1 | 3 | 7 | 8 | 10 => &[],
            2 => &[(key::FAN, Kind::U64), (key::COUNT, Kind::U64)],
            4 | 5 => &[(key::FAN, Kind::U64)],
            6 => &[(key::KR, Kind::U64)],
            9 => &[(key::FANS, Kind::U64), (key::FTST, Kind::Bool)],
            _ => return None,
        })
    }
}

impl fmt::Display for HelperError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadRequest => write!(f, "bad request"),
            Self::NoSuchFan { fan, count } => write!(f, "no fan {fan} (this Mac has {count})"),
            Self::NotANumber => write!(f, "rpm is not a finite number"),
            Self::LimitsUnreadable { fan } => write!(f, "fan {fan}'s Mn/Mx are not a usable range"),
            Self::ModeLocked { fan } => write!(f, "fan {fan} would not leave automatic"),
            Self::SmcRefused { kr } => write!(f, "the SMC refused ({kr:#x})"),
            Self::SmcUnavailable => write!(f, "no AppleSMC"),
            Self::KeyTypeMismatch => write!(f, "an SMC key is not the type the writer expects"),
            Self::HandBackIncomplete { fans, ftst } => write!(
                f,
                "hand-back incomplete: fans {fans:#06x} not automatic, Ftst raised: {ftst}"
            ),
            Self::NotHolder => write!(f, "another connection holds the fans"),
        }
    }
}

impl std::error::Error for HelperError {}

/// One reply, parsed.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Version { protocol: u64, build: String },
    Manual { unlocked: bool },
    Target { rpm: f64 },
    HandedBack,
    Refused(HelperError),
}

/// Why [`decode_reply`] refused a dictionary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyError {
    /// A `status` this crate has no row for: a helper from another release.
    UnknownStatus(u64),
    /// A missing or extra key, a wrong type, a reply to another op, or a
    /// build string past [`MAX_BUILD_BYTES`].
    Malformed,
}

impl fmt::Display for ReplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownStatus(code) => write!(f, "the helper answered unknown status {code}"),
            Self::Malformed => write!(f, "the helper's reply is not the contract's shape"),
        }
    }
}

impl std::error::Error for ReplyError {}

/// The XPC type of one dictionary value, as far as this contract cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    U64,
    F64,
    Bool,
    Str,
}

/// One dictionary value. `Other` is any XPC type the contract never uses (a
/// signed integer, data, an array, a string that is not UTF-8): present, and
/// wrong, which a strict decode refuses.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    U64(u64),
    F64(f64),
    Bool(bool),
    Str(String),
    Other,
}

impl Value {
    const fn kind(&self) -> Option<Kind> {
        match self {
            Self::U64(_) => Some(Kind::U64),
            Self::F64(_) => Some(Kind::F64),
            Self::Bool(_) => Some(Kind::Bool),
            Self::Str(_) => Some(Kind::Str),
            Self::Other => None,
        }
    }
}

/// A dictionary to decode from.
pub trait Dict {
    /// How many keys it holds, whatever their types.
    fn count(&self) -> usize;
    /// `None` when the key is absent.
    fn value(&self, key: &str) -> Option<Value>;
}

/// A dictionary to encode into.
pub trait DictMut {
    fn set_u64(&mut self, key: &'static str, value: u64);
    fn set_f64(&mut self, key: &'static str, value: f64);
    fn set_bool(&mut self, key: &'static str, value: bool);
    fn set_str(&mut self, key: &'static str, value: &str);
}

/// An in-memory dictionary: what the tests decode and what root's logic
/// answers into before the helper copies it onto the XPC reply.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemDict(pub BTreeMap<String, Value>);

impl MemDict {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert any value, `Other` included: how a test builds a hostile request.
    pub fn insert(&mut self, key: &str, value: Value) {
        self.0.insert(key.to_owned(), value);
    }

    pub fn keys(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }

    /// Copy every entry onto another dictionary. `Other` cannot be written
    /// and is skipped; nothing in this crate encodes one.
    pub fn copy_into(&self, out: &mut impl DictMut) {
        for (name, value) in &self.0 {
            let Some(name) = static_key(name) else {
                continue;
            };
            match value {
                Value::U64(v) => out.set_u64(name, *v),
                Value::F64(v) => out.set_f64(name, *v),
                Value::Bool(v) => out.set_bool(name, *v),
                Value::Str(v) => out.set_str(name, v),
                Value::Other => {}
            }
        }
    }
}

/// The contract's own key with this name, so [`MemDict::copy_into`] can hand
/// out a `'static` name and never one a peer chose.
fn static_key(name: &str) -> Option<&'static str> {
    [
        key::OP,
        key::FAN,
        key::RPM,
        key::STATUS,
        key::PROTOCOL,
        key::BUILD,
        key::UNLOCKED,
        key::COUNT,
        key::KR,
        key::FANS,
        key::FTST,
    ]
    .into_iter()
    .find(|k| *k == name)
}

impl Dict for MemDict {
    fn count(&self) -> usize {
        self.0.len()
    }

    fn value(&self, key: &str) -> Option<Value> {
        self.0.get(key).cloned()
    }
}

impl DictMut for MemDict {
    fn set_u64(&mut self, key: &'static str, value: u64) {
        self.insert(key, Value::U64(value));
    }
    fn set_f64(&mut self, key: &'static str, value: f64) {
        self.insert(key, Value::F64(value));
    }
    fn set_bool(&mut self, key: &'static str, value: bool) {
        self.insert(key, Value::Bool(value));
    }
    fn set_str(&mut self, key: &'static str, value: &str) {
        self.insert(key, Value::Str(value.to_owned()));
    }
}

/// `true` when `dict` holds exactly these keys with exactly these types.
/// Keys in a dictionary are unique, so the count plus every named key present
/// and typed is the whole key set.
fn exact(dict: &impl Dict, row: &[(&str, Kind)]) -> bool {
    dict.count() == row.len()
        && row
            .iter()
            .all(|(name, kind)| dict.value(name).and_then(|v| v.kind()) == Some(*kind))
}

fn u64_at(dict: &impl Dict, name: &str) -> Option<u64> {
    match dict.value(name) {
        Some(Value::U64(v)) => Some(v),
        _ => None,
    }
}

fn f64_at(dict: &impl Dict, name: &str) -> Option<f64> {
    match dict.value(name) {
        Some(Value::F64(v)) => Some(v),
        _ => None,
    }
}

fn bool_at(dict: &impl Dict, name: &str) -> Option<bool> {
    match dict.value(name) {
        Some(Value::Bool(v)) => Some(v),
        _ => None,
    }
}

pub fn encode_request(request: &Request, out: &mut impl DictMut) {
    out.set_u64(key::OP, request.op().code());
    match *request {
        Request::Version | Request::HandBack => {}
        Request::SetManual { fan } => out.set_u64(key::FAN, u64::from(fan.get())),
        Request::SetTarget { fan, rpm } => {
            out.set_u64(key::FAN, u64::from(fan.get()));
            out.set_f64(key::RPM, rpm);
        }
    }
}

/// Root's parse of one request. Nothing is read from the SMC before this
/// answers, so a refused dictionary touches no key.
pub fn decode_request(dict: &impl Dict) -> Result<Request, Refusal> {
    let op = u64_at(dict, key::OP)
        .and_then(Op::from_code)
        .ok_or(Refusal::BadRequest)?;
    if !exact(dict, op.request_keys()) {
        return Err(Refusal::BadRequest);
    }
    let fan = || -> Result<FanIndex, Refusal> {
        let raw = u64_at(dict, key::FAN).ok_or(Refusal::BadRequest)?;
        u8::try_from(raw)
            .ok()
            .and_then(FanIndex::new)
            .ok_or(Refusal::FanPastMax { fan: raw })
    };
    Ok(match op {
        Op::Version => Request::Version,
        Op::HandBack => Request::HandBack,
        Op::SetManual => Request::SetManual { fan: fan()? },
        Op::SetTarget => Request::SetTarget {
            fan: fan()?,
            rpm: f64_at(dict, key::RPM).ok_or(Refusal::BadRequest)?,
        },
    })
}

pub fn encode_reply(reply: &Reply, out: &mut impl DictMut) {
    match reply {
        Reply::Version { protocol, build } => {
            out.set_u64(key::STATUS, 0);
            out.set_u64(key::PROTOCOL, *protocol);
            out.set_str(key::BUILD, build);
        }
        Reply::Manual { unlocked } => {
            out.set_u64(key::STATUS, 0);
            out.set_bool(key::UNLOCKED, *unlocked);
        }
        Reply::Target { rpm } => {
            out.set_u64(key::STATUS, 0);
            out.set_f64(key::RPM, *rpm);
        }
        Reply::HandedBack => out.set_u64(key::STATUS, 0),
        Reply::Refused(error) => {
            out.set_u64(key::STATUS, error.code());
            match *error {
                HelperError::BadRequest
                | HelperError::NotANumber
                | HelperError::SmcUnavailable
                | HelperError::KeyTypeMismatch
                | HelperError::NotHolder => {}
                HelperError::NoSuchFan { fan, count } => {
                    out.set_u64(key::FAN, fan);
                    out.set_u64(key::COUNT, count);
                }
                HelperError::LimitsUnreadable { fan } | HelperError::ModeLocked { fan } => {
                    out.set_u64(key::FAN, fan);
                }
                HelperError::SmcRefused { kr } => out.set_u64(key::KR, kr),
                HelperError::HandBackIncomplete { fans, ftst } => {
                    out.set_u64(key::FANS, fans);
                    out.set_bool(key::FTST, ftst);
                }
            }
        }
    }
}

/// The client's parse of the reply to a request for `op`.
pub fn decode_reply(op: Op, dict: &impl Dict) -> Result<Reply, ReplyError> {
    let status = u64_at(dict, key::STATUS).ok_or(ReplyError::Malformed)?;
    if status == 0 {
        if !exact(dict, op.success_keys()) {
            return Err(ReplyError::Malformed);
        }
        return match op {
            Op::Version => {
                let protocol = u64_at(dict, key::PROTOCOL).ok_or(ReplyError::Malformed)?;
                let Some(Value::Str(build)) = dict.value(key::BUILD) else {
                    return Err(ReplyError::Malformed);
                };
                if build.len() > MAX_BUILD_BYTES {
                    return Err(ReplyError::Malformed);
                }
                Ok(Reply::Version { protocol, build })
            }
            Op::SetManual => Ok(Reply::Manual {
                unlocked: bool_at(dict, key::UNLOCKED).ok_or(ReplyError::Malformed)?,
            }),
            Op::SetTarget => Ok(Reply::Target {
                rpm: f64_at(dict, key::RPM).ok_or(ReplyError::Malformed)?,
            }),
            Op::HandBack => Ok(Reply::HandedBack),
        };
    }
    let fields = HelperError::fields(status).ok_or(ReplyError::UnknownStatus(status))?;
    let mut row = vec![(key::STATUS, Kind::U64)];
    row.extend_from_slice(fields);
    if !exact(dict, &row) {
        return Err(ReplyError::Malformed);
    }
    let u = |name| u64_at(dict, name).ok_or(ReplyError::Malformed);
    let error = match status {
        1 => HelperError::BadRequest,
        2 => HelperError::NoSuchFan {
            fan: u(key::FAN)?,
            count: u(key::COUNT)?,
        },
        3 => HelperError::NotANumber,
        4 => HelperError::LimitsUnreadable { fan: u(key::FAN)? },
        5 => HelperError::ModeLocked { fan: u(key::FAN)? },
        6 => HelperError::SmcRefused { kr: u(key::KR)? },
        7 => HelperError::SmcUnavailable,
        8 => HelperError::KeyTypeMismatch,
        9 => HelperError::HandBackIncomplete {
            fans: u(key::FANS)?,
            ftst: bool_at(dict, key::FTST).ok_or(ReplyError::Malformed)?,
        },
        10 => HelperError::NotHolder,
        other => return Err(ReplyError::UnknownStatus(other)),
    };
    Ok(Reply::Refused(error))
}

/// `build` cut to at most [`MAX_BUILD_BYTES`], on a character boundary.
pub fn bounded_build(build: &str) -> String {
    let mut end = build.len().min(MAX_BUILD_BYTES);
    while !build.is_char_boundary(end) {
        end -= 1;
    }
    build[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fan(index: u8) -> FanIndex {
        FanIndex::new(index).unwrap()
    }

    fn every_request() -> Vec<Request> {
        vec![
            Request::Version,
            Request::SetManual { fan: fan(1) },
            Request::SetTarget {
                fan: fan(0),
                rpm: 3000.5,
            },
            Request::HandBack,
        ]
    }

    fn encoded(request: &Request) -> MemDict {
        let mut dict = MemDict::new();
        encode_request(request, &mut dict);
        dict
    }

    #[test]
    fn request_round_trip() {
        for request in every_request() {
            assert_eq!(decode_request(&encoded(&request)), Ok(request));
        }
    }

    /// No mode value crosses the wire: every request's key set is exactly its
    /// row, none is a mode, and a request that adds one is refused.
    #[test]
    fn wire_has_no_mode_field() {
        let rows: [(Request, &[&str]); 4] = [
            (Request::Version, &["op"]),
            (Request::SetManual { fan: fan(0) }, &["fan", "op"]),
            (
                Request::SetTarget {
                    fan: fan(0),
                    rpm: 2000.0,
                },
                &["fan", "op", "rpm"],
            ),
            (Request::HandBack, &["op"]),
        ];
        for (request, keys) in rows {
            let dict = encoded(&request);
            assert_eq!(dict.keys(), keys, "{request:?}");
            assert!(
                !dict
                    .keys()
                    .iter()
                    .any(|k| k.to_lowercase().contains("mode")),
                "{request:?}"
            );
            for smuggled in ["mode", "md", "value"] {
                let mut with_mode = dict.clone();
                with_mode.insert(smuggled, Value::U64(3));
                assert_eq!(
                    decode_request(&with_mode),
                    Err(Refusal::BadRequest),
                    "{request:?} + {smuggled}"
                );
            }
        }
    }

    #[test]
    fn strict_schema_refuses_wrong_types_missing_keys_and_unknown_ops() {
        let mut wrong_type = encoded(&Request::SetManual { fan: fan(0) });
        wrong_type.insert(key::FAN, Value::F64(0.0));
        let mut other_type = encoded(&Request::SetManual { fan: fan(0) });
        other_type.insert(key::FAN, Value::Other);
        let mut rpm_as_int = encoded(&Request::SetTarget {
            fan: fan(0),
            rpm: 1.0,
        });
        rpm_as_int.insert(key::RPM, Value::U64(3000));
        let mut missing = MemDict::new();
        missing.set_u64(key::OP, Op::SetTarget.code());
        missing.set_u64(key::FAN, 0);
        let mut unknown_op = MemDict::new();
        unknown_op.set_u64(key::OP, 5);
        let mut op_zero = MemDict::new();
        op_zero.set_u64(key::OP, 0);
        let mut op_as_string = MemDict::new();
        op_as_string.set_str(key::OP, "1");
        for (name, dict) in [
            ("fan as double", wrong_type),
            ("fan as another type", other_type),
            ("rpm as uint64", rpm_as_int),
            ("set-target without rpm", missing),
            ("op 5", unknown_op),
            ("op 0", op_zero),
            ("op as string", op_as_string),
            ("empty", MemDict::new()),
        ] {
            assert_eq!(decode_request(&dict), Err(Refusal::BadRequest), "{name}");
        }
    }

    #[test]
    fn a_fan_past_nine_is_named_not_truncated() {
        for raw in [10, 255, 256, u64::MAX] {
            let mut dict = MemDict::new();
            dict.set_u64(key::OP, Op::SetManual.code());
            dict.set_u64(key::FAN, raw);
            assert_eq!(decode_request(&dict), Err(Refusal::FanPastMax { fan: raw }));
        }
    }

    #[test]
    fn nan_crosses_the_wire_for_root_to_refuse() {
        let request = Request::SetTarget {
            fan: fan(0),
            rpm: f64::NAN,
        };
        let Ok(Request::SetTarget { rpm, .. }) = decode_request(&encoded(&request)) else {
            panic!("a NaN rpm is a well-formed request; root refuses it, not the codec");
        };
        assert!(rpm.is_nan());
    }

    fn every_reply() -> Vec<(Op, Reply)> {
        let mut replies = vec![
            (
                Op::Version,
                Reply::Version {
                    protocol: 1,
                    build: "abc1234".into(),
                },
            ),
            (Op::SetManual, Reply::Manual { unlocked: true }),
            (Op::SetTarget, Reply::Target { rpm: 1350.25 }),
            (Op::HandBack, Reply::HandedBack),
        ];
        for error in [
            HelperError::BadRequest,
            HelperError::NoSuchFan { fan: 4, count: 2 },
            HelperError::NotANumber,
            HelperError::LimitsUnreadable { fan: 1 },
            HelperError::ModeLocked { fan: 0 },
            HelperError::SmcRefused { kr: 0xe000_02c1 },
            HelperError::SmcUnavailable,
            HelperError::KeyTypeMismatch,
            HelperError::HandBackIncomplete {
                fans: 0b10,
                ftst: true,
            },
            HelperError::NotHolder,
        ] {
            for op in Op::ALL {
                replies.push((op, Reply::Refused(error)));
            }
        }
        replies
    }

    #[test]
    fn reply_round_trip() {
        for (op, reply) in every_reply() {
            let mut dict = MemDict::new();
            encode_reply(&reply, &mut dict);
            assert_eq!(
                decode_reply(op, &dict),
                Ok(reply.clone()),
                "{op:?} {reply:?}"
            );
        }
    }

    #[test]
    fn codes_are_one_to_ten_and_distinct() {
        let codes: Vec<u64> = every_reply()
            .into_iter()
            .filter_map(|(op, reply)| match reply {
                Reply::Refused(e) if op == Op::Version => Some(e.code()),
                _ => None,
            })
            .collect();
        assert_eq!(codes, (1..=10).collect::<Vec<_>>());
        for op in Op::ALL {
            assert_eq!(Op::from_code(op.code()), Some(op));
        }
    }

    #[test]
    fn strict_reply_refusals() {
        // A success reply to another op.
        let mut manual = MemDict::new();
        encode_reply(&Reply::Manual { unlocked: false }, &mut manual);
        assert_eq!(
            decode_reply(Op::SetTarget, &manual),
            Err(ReplyError::Malformed)
        );
        // An extra key on an error.
        let mut extra = MemDict::new();
        encode_reply(&Reply::Refused(HelperError::NotHolder), &mut extra);
        extra.set_u64(key::FAN, 0);
        assert_eq!(
            decode_reply(Op::HandBack, &extra),
            Err(ReplyError::Malformed)
        );
        // A status this release does not know.
        let mut unknown = MemDict::new();
        unknown.set_u64(key::STATUS, 11);
        assert_eq!(
            decode_reply(Op::Version, &unknown),
            Err(ReplyError::UnknownStatus(11))
        );
        // A build past 64 bytes.
        let mut long = MemDict::new();
        encode_reply(
            &Reply::Version {
                protocol: 1,
                build: "x".repeat(MAX_BUILD_BYTES + 1),
            },
            &mut long,
        );
        assert_eq!(decode_reply(Op::Version, &long), Err(ReplyError::Malformed));
    }

    #[test]
    fn build_is_cut_on_a_character_boundary() {
        assert_eq!(bounded_build("abc"), "abc");
        assert_eq!(bounded_build(&"a".repeat(80)).len(), MAX_BUILD_BYTES);
        let wide = format!("{}é", "a".repeat(MAX_BUILD_BYTES - 1));
        assert_eq!(bounded_build(&wide), "a".repeat(MAX_BUILD_BYTES - 1));
    }

    #[test]
    fn smc_errors_map_to_their_rows() {
        assert_eq!(
            HelperError::from_smc(SmcError::Unavailable),
            HelperError::SmcUnavailable
        );
        assert_eq!(
            HelperError::from_smc(SmcError::Kernel(0xe000_02c2_u32 as i32)),
            HelperError::SmcRefused { kr: 0xe000_02c2 }
        );
        assert_eq!(
            HelperError::from_smc(SmcError::SmcRefused {
                key: tcr_smc::FourCc::new(*b"F0md"),
                result: 0x82
            }),
            HelperError::SmcRefused { kr: 0x82 }
        );
        assert_eq!(
            HelperError::from_smc(SmcError::UnsupportedType(tcr_smc::FourCc::UI8)),
            HelperError::KeyTypeMismatch
        );
    }
}
