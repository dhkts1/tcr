//! The SMC's vocabulary: four-character codes, its errors, a key's value with
//! the codecs for each numeric type, and the two traits a key source offers.
//! Plain Rust, tested everywhere.

use std::fmt;

/// A four-character SMC code: a key name (`F0Ac`) or a data type (`flt `).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FourCc(pub u32);

impl FourCc {
    pub const FLT: Self = Self::new(*b"flt ");
    pub const UI8: Self = Self::new(*b"ui8 ");
    pub const UI16: Self = Self::new(*b"ui16");
    pub const UI32: Self = Self::new(*b"ui32");
    pub const FPE2: Self = Self::new(*b"fpe2");

    /// For codes written in source, where the length is checked by the type.
    pub const fn new(bytes: [u8; 4]) -> Self {
        Self(u32::from_be_bytes(bytes))
    }

    /// `None` unless `text` is exactly four bytes of UTF-8.
    pub fn parse(text: &str) -> Option<Self> {
        let bytes: [u8; 4] = text.as_bytes().try_into().ok()?;
        Some(Self::new(bytes))
    }
}

impl fmt::Display for FourCc {
    /// The four characters, with `?` for any byte that is not printable ASCII.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0.to_be_bytes() {
            let shown = if (32..=126).contains(&byte) {
                byte
            } else {
                b'?'
            };
            write!(f, "{}", char::from(shown))?;
        }
        Ok(())
    }
}

impl fmt::Debug for FourCc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\"{self}\"")
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SmcError {
    /// No AppleSMC service: normal in a virtual machine, including CI's macOS
    /// runner, and the only answer on any other operating system.
    Unavailable,
    /// `kIOReturnNotPrivileged` (0xe00002c1): the kernel wants root. Its own
    /// case because a root helper's whole reason to exist is this one error.
    NotPrivileged,
    /// The SMC answered 0x84: this Mac has no such key.
    KeyNotFound(FourCc),
    /// IOKit succeeded but the SMC refused, with its own result byte.
    SmcRefused { key: FourCc, result: u8 },
    /// The SMC refused to name the key at this index of its key table.
    KeyIndexRefused { index: u32, result: u8 },
    /// Any other IOKit error, by its `kern_return_t`.
    Kernel(i32),
    /// A typed decode asked of a value of another type, or a write of a value
    /// whose type is not the key's own.
    WrongType { expected: FourCc, actual: FourCc },
    /// A value whose byte count does not fit its type, a write whose byte
    /// count is not the key's own, or a key the SMC says is wider than the 32
    /// bytes its protocol carries.
    WrongSize {
        ty: FourCc,
        expected: usize,
        actual: usize,
    },
    /// An encode asked for a number its type cannot hold, or one no fan or
    /// sensor can mean: NaN, an infinity, a negative `flt `.
    OutOfRange { ty: FourCc, value: f64 },
    /// A numeric decode of a type this crate does not read.
    UnsupportedType(FourCc),
}

impl fmt::Display for SmcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => write!(f, "no AppleSMC service"),
            Self::NotPrivileged => write!(f, "not privileged (0xe00002c1): needs root"),
            Self::KeyNotFound(key) => write!(f, "no such key {key}"),
            Self::SmcRefused { key, result } => {
                write!(f, "the SMC refused {key} with result {result:#04x}")
            }
            Self::KeyIndexRefused { index, result } => write!(
                f,
                "the SMC refused key-table index {index} with result {result:#04x}"
            ),
            Self::Kernel(code) => write!(f, "IOKit error {:#010x}", *code as u32),
            Self::WrongType { expected, actual } => {
                write!(f, "wrong type: expected {expected}, got {actual}")
            }
            Self::WrongSize {
                ty,
                expected,
                actual,
            } => write!(
                f,
                "wrong size for {ty}: expected {expected} bytes, got {actual}"
            ),
            Self::OutOfRange { ty, value } => write!(f, "{value} does not fit {ty}"),
            Self::UnsupportedType(ty) => write!(f, "unsupported type {ty}"),
        }
    }
}

impl std::error::Error for SmcError {}

impl SmcError {
    pub const NOT_PRIVILEGED_CODE: i32 = 0xe000_02c1_u32 as i32;
    pub const NOT_FOUND_CODE: i32 = 0xe000_02f0_u32 as i32;
    pub const SMC_KEY_NOT_FOUND: u8 = 0x84;

    /// One IOKit return plus the SMC's own result byte, as a typed error, or
    /// `None` when both say success.
    pub const fn check(kr: i32, smc_result: u8, key: FourCc) -> Option<Self> {
        if kr == Self::NOT_PRIVILEGED_CODE {
            return Some(Self::NotPrivileged);
        }
        if kr != 0 {
            return Some(Self::Kernel(kr));
        }
        if smc_result == Self::SMC_KEY_NOT_FOUND {
            return Some(Self::KeyNotFound(key));
        }
        if smc_result != 0 {
            return Some(Self::SmcRefused {
                key,
                result: smc_result,
            });
        }
        None
    }

    /// The same for a key-table index call, which names no key: its refusal
    /// is [`Self::KeyIndexRefused`], never a "no such key".
    pub const fn check_index(kr: i32, smc_result: u8, index: u32) -> Option<Self> {
        if kr == Self::NOT_PRIVILEGED_CODE {
            return Some(Self::NotPrivileged);
        }
        if kr != 0 {
            return Some(Self::Kernel(kr));
        }
        if smc_result != 0 {
            return Some(Self::KeyIndexRefused {
                index,
                result: smc_result,
            });
        }
        None
    }
}

/// The most bytes one SMC call carries: the protocol struct's `bytes[32]`.
pub const MAX_VALUE_SIZE: usize = 32;

/// One key's value as the SMC holds it: its type and raw bytes. The typed
/// accessors decode; the associated constructors encode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmcValue {
    pub ty: FourCc,
    pub bytes: Vec<u8>,
}

impl SmcValue {
    pub const fn new(ty: FourCc, bytes: Vec<u8>) -> Self {
        Self { ty, bytes }
    }

    /// The bytes as an array of exactly `N`, when the type is `wanted`.
    fn expect<const N: usize>(&self, wanted: FourCc) -> Result<[u8; N], SmcError> {
        if self.ty != wanted {
            return Err(SmcError::WrongType {
                expected: wanted,
                actual: self.ty,
            });
        }
        self.bytes
            .as_slice()
            .try_into()
            .map_err(|_| SmcError::WrongSize {
                ty: self.ty,
                expected: N,
                actual: self.bytes.len(),
            })
    }

    /// `flt `: an IEEE single, LITTLE-endian (`00409c45` is 5000.0, measured).
    /// Every other numeric SMC type is big-endian.
    pub fn float_value(&self) -> Result<f32, SmcError> {
        Ok(f32::from_le_bytes(self.expect(FourCc::FLT)?))
    }

    pub fn uint8_value(&self) -> Result<u8, SmcError> {
        let [byte] = self.expect(FourCc::UI8)?;
        Ok(byte)
    }

    /// `ui16`, big-endian.
    pub fn uint16_value(&self) -> Result<u16, SmcError> {
        Ok(u16::from_be_bytes(self.expect(FourCc::UI16)?))
    }

    /// `ui32`, big-endian. The type of `#KEY`, the key count.
    pub fn uint32_value(&self) -> Result<u32, SmcError> {
        Ok(u32::from_be_bytes(self.expect(FourCc::UI32)?))
    }

    /// `fpe2`: unsigned big-endian fixed point, 14 integer bits and 2
    /// fraction bits, the fan-speed type on Intel Macs.
    pub fn fpe2_value(&self) -> Result<f64, SmcError> {
        Ok(f64::from(u16::from_be_bytes(self.expect(FourCc::FPE2)?)) / 4.0)
    }

    /// Whichever of the four numeric types this value is, as an `f64`. A fan
    /// speed is `flt ` on Apple Silicon and `fpe2` on Intel; callers that only
    /// want the number read it here.
    pub fn numeric_value(&self) -> Result<f64, SmcError> {
        match self.ty {
            FourCc::FLT => Ok(f64::from(self.float_value()?)),
            FourCc::UI8 => Ok(f64::from(self.uint8_value()?)),
            FourCc::UI16 => Ok(f64::from(self.uint16_value()?)),
            FourCc::FPE2 => self.fpe2_value(),
            other => Err(SmcError::UnsupportedType(other)),
        }
    }

    /// Refuses NaN, either infinity and any negative number: every `flt `
    /// this crate writes is a fan speed, and none of those is one. A root
    /// helper sends what this returns straight to the SMC.
    pub fn flt(value: f32) -> Result<Self, SmcError> {
        if !value.is_finite() || value < 0.0 {
            return Err(SmcError::OutOfRange {
                ty: FourCc::FLT,
                value: f64::from(value),
            });
        }
        Ok(Self::new(FourCc::FLT, value.to_le_bytes().to_vec()))
    }

    pub fn ui8(value: u8) -> Self {
        Self::new(FourCc::UI8, vec![value])
    }

    pub fn ui16(value: u16) -> Self {
        Self::new(FourCc::UI16, value.to_be_bytes().to_vec())
    }

    /// Rounded to the nearest quarter; refuses anything outside 0 to 16,383.75.
    pub fn fpe2(value: f64) -> Result<Self, SmcError> {
        let quarters = (value * 4.0).round();
        if !quarters.is_finite() || quarters < 0.0 || quarters > f64::from(u16::MAX) {
            return Err(SmcError::OutOfRange {
                ty: FourCc::FPE2,
                value,
            });
        }
        // In 0..=65535 by the check above, so the cast is exact.
        let raw = quarters as u16;
        Ok(Self::new(FourCc::FPE2, raw.to_be_bytes().to_vec()))
    }
}

/// A key's declared type and size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmcKeyInfo {
    pub ty: FourCc,
    pub size: usize,
}

/// The check [`SmcConnection::write`](crate::SmcConnection::write) makes
/// before it sends anything: the type
/// is the key's own and the byte count is the key's own. An empty value is a
/// size mismatch whatever the key says.
///
/// The SMC takes whatever bytes it is handed, and a `ui8` written to a `flt `
/// key is a fan target of whatever those bytes happen to mean.
pub fn check_write(value: &SmcValue, fits: SmcKeyInfo) -> Result<(), SmcError> {
    if value.ty != fits.ty {
        return Err(SmcError::WrongType {
            expected: fits.ty,
            actual: value.ty,
        });
    }
    let len = value.bytes.len();
    if len == 0 || len != fits.size || len > MAX_VALUE_SIZE {
        return Err(SmcError::WrongSize {
            ty: fits.ty,
            expected: fits.size,
            actual: len,
        });
    }
    Ok(())
}

/// What [`HeatReader`](crate::HeatReader) needs from an SMC: one key's value, and the key table.
/// [`SmcConnection`](crate::SmcConnection) is the real one; tests hand in a fake.
pub trait KeySource {
    fn read(&mut self, key: FourCc) -> Result<SmcValue, SmcError>;
    fn keys(&mut self) -> Result<Vec<FourCc>, SmcError>;
}

/// What [`FanWriter`](crate::FanWriter) needs on top of reading: one write. Its own trait, so
/// the reader's key source cannot write, and a writer's fake can.
///
/// An implementation sends nothing unless `value` is the key's own type and
/// size ([`check_write`]), as [`SmcConnection::write`](crate::SmcConnection::write) does.
pub trait KeyAccess: KeySource {
    fn write(&mut self, key: FourCc, value: &SmcValue) -> Result<(), SmcError>;
}
