//! The Mac's System Management Controller, read through AppleSMC without root:
//! fan speeds, fan modes and temperature sensors.
//!
//! Reading needs no privilege. Writing does: a non-root write came back
//! `kIOReturnNotPrivileged` (0xe00002c1) on 2026-10-18, so
//! [`SmcConnection::write`] exists for a root helper built on this crate, and
//! nothing in tcr itself writes a fan.
//!
//! # Where this came from
//!
//! A port of TcrBar's Swift `TcrSMC` library and its C shim `CSMC` (tcr#554),
//! with every fix that library's review made. The protocol follows Stats
//! (exelban/stats, MIT, `SMC/smc.swift`): selector 2 of the AppleSMC user
//! client, a one-byte command in `data8`, and an 80-byte parameter struct. The
//! places that follow Stats say so.
//!
//! # Why a crate of its own
//!
//! The main crate is `#![forbid(unsafe_code)]`, and talking to IOKit is
//! `unsafe` by construction. As with `tcr-fdpass`, the unsafe lives here, in
//! one file, and the proxy keeps its guarantee.
//!
//! # Linux
//!
//! CI builds the workspace on ubuntu. Everything that touches AppleSMC is
//! `#[cfg(target_os = "macos")]`; elsewhere [`HeatSource::read`] answers
//! `None`, the same answer a Mac VM with no SMC gets.
//! The codecs and the reader's rules are plain Rust and are tested everywhere.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

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

/// The check [`SmcConnection::write`] makes before it sends anything: the type
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

/// What [`HeatReader`] needs from an SMC: one key's value, and the key table.
/// [`SmcConnection`] is the real one; tests hand in a fake.
pub trait KeySource {
    fn read(&mut self, key: FourCc) -> Result<SmcValue, SmcError>;
    fn keys(&mut self) -> Result<Vec<FourCc>, SmcError>;
}

#[cfg(target_os = "macos")]
pub use macos::SmcConnection;

#[cfg(target_os = "macos")]
mod macos {
    use std::cell::Cell;
    use std::ffi::{c_char, c_void};
    use std::marker::PhantomData;

    use super::{FourCc, KeySource, MAX_VALUE_SIZE, SmcError, SmcKeyInfo, SmcValue, check_write};

    type MachPort = u32;
    type KernReturn = i32;

    /// `MACH_PORT_NULL`, which IOKit documents as "the default main port". Not
    /// the `kIOMainPortDefault` symbol: that exists only from macOS 12, the
    /// binary's floor is 11, and a strong reference to it stops tcr launching
    /// there at all.
    const DEFAULT_MAIN_PORT: MachPort = 0;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOServiceMatching(name: *const c_char) -> *mut c_void;
        /// Consumes one reference to `matching`, so the caller never releases it.
        fn IOServiceGetMatchingService(main_port: MachPort, matching: *mut c_void) -> MachPort;
        fn IOServiceOpen(
            service: MachPort,
            owning_task: MachPort,
            kind: u32,
            connect: *mut MachPort,
        ) -> KernReturn;
        fn IOServiceClose(connect: MachPort) -> KernReturn;
        fn IOObjectRelease(object: MachPort) -> KernReturn;
        fn IOConnectCallStructMethod(
            connection: MachPort,
            selector: u32,
            input: *const c_void,
            input_size: usize,
            output: *mut c_void,
            output_size: *mut usize,
        ) -> KernReturn;
    }

    unsafe extern "C" {
        /// What the `mach_task_self()` macro reads: this task's own port.
        static mach_task_self_: MachPort;
    }

    #[repr(C)]
    #[derive(Default)]
    struct SmcVersion {
        major: u8,
        minor: u8,
        build: u8,
        reserved: u8,
        release: u16,
    }

    #[repr(C)]
    #[derive(Default)]
    struct SmcPowerLimit {
        version: u16,
        length: u16,
        cpu: u32,
        gpu: u32,
        mem: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct SmcKeyInfoData {
        data_size: u32,
        data_type: u32,
        data_attributes: u8,
        /// The C struct's tail padding, declared so it is zeroed rather than
        /// left as whatever the stack held.
        _pad: [u8; 3],
    }

    /// The AppleSMC parameter struct, in C layout. The kernel reads the bytes,
    /// not the declaration, so the layout is the contract. Every padding byte
    /// the C compiler would insert is a declared, zeroed field here, so no
    /// byte the kernel is sent is uninitialised.
    #[repr(C)]
    #[derive(Default)]
    struct SmcParam {
        key: u32,
        version: SmcVersion,
        _pad_after_version: [u8; 2],
        power_limit: SmcPowerLimit,
        key_info: SmcKeyInfoData,
        result: u8,
        status: u8,
        data8: u8,
        _pad_after_data8: u8,
        data32: u32,
        bytes: [u8; MAX_VALUE_SIZE],
    }

    // The size and offsets the kernel expects, measured on a Mac16,7 with the
    // probe the C shim grew out of. A layout that comes out differently fails
    // the build rather than sending the kernel a struct it misreads.
    const _: () = {
        assert!(size_of::<SmcVersion>() == 6);
        assert!(size_of::<SmcPowerLimit>() == 16);
        assert!(size_of::<SmcKeyInfoData>() == 12);
        assert!(size_of::<SmcParam>() == 80);
        assert!(std::mem::offset_of!(SmcParam, key) == 0);
        assert!(std::mem::offset_of!(SmcParam, version) == 4);
        assert!(std::mem::offset_of!(SmcParam, power_limit) == 12);
        assert!(std::mem::offset_of!(SmcParam, key_info) == 28);
        assert!(std::mem::offset_of!(SmcParam, result) == 40);
        assert!(std::mem::offset_of!(SmcParam, status) == 41);
        assert!(std::mem::offset_of!(SmcParam, data8) == 42);
        assert!(std::mem::offset_of!(SmcParam, data32) == 44);
        assert!(std::mem::offset_of!(SmcParam, bytes) == 48);
    };

    /// The most `keys` reserves before it starts: 3,462 on a Mac16,7, so this
    /// covers every real SMC, and a garbage `#KEY` cannot ask for gigabytes.
    const KEY_RESERVATION_CAP: usize = 8192;

    const USER_CLIENT_METHOD: u32 = 2;
    const READ_BYTES: u8 = 5;
    const WRITE_BYTES: u8 = 6;
    const READ_INDEX: u8 = 8;
    const READ_KEY_INFO: u8 = 9;

    /// One open AppleSMC connection. Open once and keep it: the connection is
    /// closed when this value is dropped.
    ///
    /// Not `Sync`: share one through a lock. `Send` because a connection is a
    /// Mach port name, which any thread of this task may use. The marker field
    /// is what makes the compiler hold this: `Cell<()>` is `Send` and not
    /// `Sync`, and a bare `u32` would make the struct both.
    ///
    /// ```compile_fail
    /// fn shared<T: Sync>() {}
    /// shared::<tcr_smc::SmcConnection>();
    /// ```
    ///
    /// ```
    /// fn sent<T: Send>() {}
    /// sent::<tcr_smc::SmcConnection>();
    /// ```
    pub struct SmcConnection {
        handle: MachPort,
        _not_sync: PhantomData<Cell<()>>,
    }

    impl SmcConnection {
        /// [`SmcError::Unavailable`] when the Mac has no AppleSMC (a VM).
        pub fn open() -> Result<Self, SmcError> {
            // SAFETY: the argument is a NUL-terminated literal that outlives the
            // call; the function returns a new dictionary or null.
            let matching = unsafe { IOServiceMatching(c"AppleSMC".as_ptr()) };
            if matching.is_null() {
                return Err(SmcError::Unavailable);
            }
            // SAFETY: `DEFAULT_MAIN_PORT` is the documented null main port. `matching`
            // is the live dictionary from above, whose one reference this call
            // consumes; it is not used again.
            let service = unsafe { IOServiceGetMatchingService(DEFAULT_MAIN_PORT, matching) };
            if service == 0 {
                return Err(SmcError::Unavailable);
            }
            let mut handle: MachPort = 0;
            // SAFETY: `service` is the live service object from above; reading
            // the task-port static is what `mach_task_self()` does; `handle`
            // is a valid place for the out-parameter.
            let kr = unsafe { IOServiceOpen(service, mach_task_self_, 0, &raw mut handle) };
            // SAFETY: `service` is ours to release, released once, and not used
            // after. A failed release leaks one port reference and changes
            // nothing about the connection, so its return is not acted on.
            unsafe { IOObjectRelease(service) };
            match kr {
                0 => Ok(Self {
                    handle,
                    _not_sync: PhantomData,
                }),
                SmcError::NOT_FOUND_CODE => Err(SmcError::Unavailable),
                SmcError::NOT_PRIVILEGED_CODE => Err(SmcError::NotPrivileged),
                other => Err(SmcError::Kernel(other)),
            }
        }

        /// One round trip through selector 2: `input` in, the kernel's answer
        /// back, and the IOKit return.
        fn call(&self, input: &SmcParam) -> (KernReturn, SmcParam) {
            let mut output = SmcParam::default();
            let mut output_size = size_of::<SmcParam>();
            // SAFETY: both pointers are to live, properly aligned `SmcParam`s of
            // exactly the size passed beside them; the kernel writes at most
            // `output_size` bytes into `output`, and `handle` is a connection
            // this value owns and has not closed.
            let kr = unsafe {
                IOConnectCallStructMethod(
                    self.handle,
                    USER_CLIENT_METHOD,
                    std::ptr::from_ref(input).cast(),
                    size_of::<SmcParam>(),
                    std::ptr::from_mut(&mut output).cast(),
                    &raw mut output_size,
                )
            };
            (kr, output)
        }

        /// The key's own type and size, as the SMC declares them, without its value.
        pub fn key_info(&self, key: FourCc) -> Result<SmcKeyInfo, SmcError> {
            let input = SmcParam {
                key: key.0,
                data8: READ_KEY_INFO,
                ..SmcParam::default()
            };
            let (kr, out) = self.call(&input);
            if let Some(error) = SmcError::check(kr, out.result, key) {
                return Err(error);
            }
            Ok(SmcKeyInfo {
                ty: FourCc(out.key_info.data_type),
                size: out.key_info.data_size as usize,
            })
        }

        /// Refuses with [`SmcError::WrongSize`] when the SMC says the key is
        /// wider than [`MAX_VALUE_SIZE`], rather than returning the first 32
        /// bytes of a longer value as if they were all of it.
        pub fn read(&self, key: FourCc) -> Result<SmcValue, SmcError> {
            let info = self.key_info(key)?;
            if info.size > MAX_VALUE_SIZE {
                return Err(SmcError::WrongSize {
                    ty: info.ty,
                    expected: MAX_VALUE_SIZE,
                    actual: info.size,
                });
            }
            let input = SmcParam {
                key: key.0,
                key_info: SmcKeyInfoData {
                    data_size: info.size as u32,
                    ..SmcKeyInfoData::default()
                },
                data8: READ_BYTES,
                ..SmcParam::default()
            };
            let (kr, out) = self.call(&input);
            if let Some(error) = SmcError::check(kr, out.result, key) {
                return Err(error);
            }
            Ok(SmcValue::new(info.ty, out.bytes[..info.size].to_vec()))
        }

        /// Needs root: a non-root caller gets [`SmcError::NotPrivileged`].
        ///
        /// Reads the key's own type and size first and sends nothing unless
        /// `value` matches both ([`check_write`]).
        pub fn write(&self, key: FourCc, value: &SmcValue) -> Result<(), SmcError> {
            check_write(value, self.key_info(key)?)?;
            let mut bytes = [0_u8; MAX_VALUE_SIZE];
            // `check_write` bounds the length by MAX_VALUE_SIZE.
            bytes[..value.bytes.len()].copy_from_slice(&value.bytes);
            let input = SmcParam {
                key: key.0,
                key_info: SmcKeyInfoData {
                    data_size: value.bytes.len() as u32,
                    data_type: value.ty.0,
                    ..SmcKeyInfoData::default()
                },
                data8: WRITE_BYTES,
                bytes,
                ..SmcParam::default()
            };
            let (kr, out) = self.call(&input);
            SmcError::check(kr, out.result, key).map_or(Ok(()), Err)
        }

        /// Every key name the SMC reports, in its own order. 3,462 on a
        /// Mac16,7: thousands of kernel calls, so callers enumerate once and
        /// keep the list.
        pub fn keys(&self) -> Result<Vec<FourCc>, SmcError> {
            let count = self.read(FourCc::new(*b"#KEY"))?.uint32_value()?;
            let mut keys = Vec::with_capacity((count as usize).min(KEY_RESERVATION_CAP));
            for index in 0..count {
                let input = SmcParam {
                    data8: READ_INDEX,
                    data32: index,
                    ..SmcParam::default()
                };
                let (kr, out) = self.call(&input);
                if let Some(error) = SmcError::check_index(kr, out.result, index) {
                    return Err(error);
                }
                keys.push(FourCc(out.key));
            }
            Ok(keys)
        }
    }

    impl Drop for SmcConnection {
        fn drop(&mut self) {
            // SAFETY: `handle` is the connection `open` returned, closed once,
            // here, and never used after. A close that fails leaves nothing to
            // undo: the port dies with the process either way, and a drop has
            // nowhere to report to.
            unsafe { IOServiceClose(self.handle) };
        }
    }

    impl KeySource for SmcConnection {
        fn read(&mut self, key: FourCc) -> Result<SmcValue, SmcError> {
            Self::read(self, key)
        }

        fn keys(&mut self) -> Result<Vec<FourCc>, SmcError> {
            Self::keys(self)
        }
    }
}

/// One fan, as read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FanReading {
    /// 0-based, the `n` in `F{n}Ac`.
    pub index: FanIndex,
    pub actual_rpm: f64,
    /// 0 when it did not read this time.
    pub min_rpm: f64,
    /// 0 when it did not read this time: "no maximum", not a ceiling of 0.
    pub max_rpm: f64,
    /// The fan's mode key reads 1 (forced): a program fixed its speed, so it
    /// no longer follows heat. 0 and 3 are both automatic (Stats' `FanMode`).
    pub is_manual: bool,
}

/// How hot the CPU is and how hard the fans work.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HeatReading {
    /// The hottest CPU sensor, °C, always within [`PLAUSIBLE_CELSIUS`].
    /// `None` when this Mac has neither `TCMz` nor any readable `Tp*` sensor,
    /// or when none of them read this time.
    pub cpu_celsius: Option<f64>,
    /// Empty on a Mac with no fans (`FNum` 0, a MacBook Air), and missing any
    /// fan whose speed did not read.
    pub fans: Vec<FanReading>,
}

/// A sensor that reads 0 or below, or past 150 °C, is one not fitted or not
/// powered, not a hot CPU; the reading drops it, whichever key it came from.
/// NaN is outside every range.
pub const PLAUSIBLE_CELSIUS: std::ops::RangeInclusive<f64> = 0.5..=150.0;

/// How long a failed `Tp*` enumeration waits before the next try. A starting
/// value, not a measured one: long enough that a key table that keeps failing
/// does not cost 3,462 kernel calls every poll.
pub const ENUMERATION_RETRY_DELAY: Duration = Duration::from_secs(60);

const CPU_MAX_KEY: FourCc = FourCc::new(*b"TCMz");
const FAN_COUNT_KEY: FourCc = FourCc::new(*b"FNum");
const LOWER_MODE_PROBE_KEY: FourCc = FourCc::new(*b"F0md");

/// `Some(celsius)` when it is inside [`PLAUSIBLE_CELSIUS`].
pub fn plausible(celsius: f64) -> Option<f64> {
    PLAUSIBLE_CELSIUS.contains(&celsius).then_some(celsius)
}

/// A fan's index, 0 to 9: the `n` in `F{n}Ac`, which is one ASCII digit, so
/// ten fans is the most a key can name. Parsed once, by [`Self::new`], so a key
/// built from a fan index can never name `F:Ac` or worse, whatever number came
/// in (a root helper builds keys from an index that crossed a wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FanIndex(u8);

impl FanIndex {
    /// `None` past 9.
    pub const fn new(index: u8) -> Option<Self> {
        if index < 10 { Some(Self(index)) } else { None }
    }

    pub const fn get(self) -> u8 {
        self.0
    }
}

/// `F{n}{suffix}`, for a fan index already known to be one digit.
pub const fn fan_key(fan: FanIndex, suffix: [u8; 2]) -> FourCc {
    FourCc::new([b'F', b'0' + fan.0, suffix[0], suffix[1]])
}

/// Where a reader's "now" comes from: time since the reader was made, so a
/// test can hand in its own clock.
pub type Clock = Box<dyn Fn() -> Duration + Send>;
/// Where a reader's one-per-distinct-error lines go.
pub type Report = Box<dyn FnMut(&str) + Send>;

/// The reader: one key source, and the per-Mac facts (where the CPU
/// temperature lives, which spelling the fan mode key uses, each fan's
/// limits) found once and kept.
///
/// Not locked: [`HeatReader::read`] takes `&mut self`, so a caller sharing one
/// puts it behind its own lock.
pub struct HeatReader<S> {
    smc: S,
    now: Clock,
    report: Report,
    /// `TCMz` is a 4-byte `flt ` on this Mac (`Some(true)`), is not or is
    /// absent (`Some(false)`), or has not answered yet (`None`).
    use_max_key: Option<bool>,
    /// The `Tp*` sensors, once an enumeration has succeeded.
    processor_keys: Option<Vec<FourCc>>,
    enumeration_failed_at: Option<Duration>,
    mode_key_is_lower: Option<bool>,
    /// Each fan's `Mn` and `Mx`, kept once both read: they do not change.
    fan_limits: HashMap<FanIndex, (f64, f64)>,
    /// Every line already reported, so a key that fails on every poll is one
    /// log line, not one a poll.
    reported: HashSet<String>,
}

impl<S: KeySource> HeatReader<S> {
    /// A reader on `smc` with the real clock, reporting through `report`.
    pub fn new(smc: S, report: Report) -> Self {
        let start = Instant::now();
        Self::with_clock(smc, Box::new(move || start.elapsed()), report)
    }

    pub fn with_clock(smc: S, now: Clock, report: Report) -> Self {
        Self {
            smc,
            now,
            report,
            use_max_key: None,
            processor_keys: None,
            enumeration_failed_at: None,
            mode_key_is_lower: None,
            fan_limits: HashMap::new(),
            reported: HashSet::new(),
        }
    }

    /// The key source, for a test that counts what was asked of it.
    pub const fn source(&self) -> &S {
        &self.smc
    }

    /// Mutable access to the key source, for a test that changes what it serves.
    pub const fn source_mut(&mut self) -> &mut S {
        &mut self.smc
    }

    /// Read now. Every error is reported once per distinct error, and the
    /// reading carries whatever did read: a bad fan key never removes the CPU,
    /// and the reverse.
    pub fn read(&mut self) -> HeatReading {
        HeatReading {
            cpu_celsius: self.read_cpu(),
            fans: self.read_fans(),
        }
    }

    fn note(&mut self, what: &str, error: &SmcError) {
        let line = format!("SMC: {what}: {error}");
        if !self.reported.contains(&line) {
            (self.report)(&line);
            self.reported.insert(line);
        }
    }

    // CPU

    /// `TCMz` when this Mac has it as a 4-byte `flt `; otherwise the hottest
    /// `Tp*` sensor.
    fn read_cpu(&mut self) -> Option<f64> {
        if self.use_max_key != Some(false) {
            match self.smc.read(CPU_MAX_KEY) {
                Ok(value) if value.ty == FourCc::FLT && value.bytes.len() == 4 => {
                    self.use_max_key = Some(true);
                    return match value.float_value() {
                        Ok(celsius) => plausible(f64::from(celsius)),
                        Err(error) => {
                            self.note(&format!("{CPU_MAX_KEY} read"), &error);
                            None
                        }
                    };
                }
                // Some other type or size: not the key this reader knows.
                Ok(_) | Err(SmcError::KeyNotFound(_)) => self.use_max_key = Some(false),
                Err(error) => {
                    self.note(&format!("{CPU_MAX_KEY} read"), &error);
                    // A key that has read before failed this once: no figure
                    // this poll. One never yet read: try the sensors, ask
                    // again next poll.
                    if self.use_max_key == Some(true) {
                        return None;
                    }
                }
            }
        }
        let keys = self.resolved_processor_keys()?;
        let mut hottest: Option<f64> = None;
        for key in keys {
            let celsius = match self.smc.read(key).and_then(|value| value.float_value()) {
                Ok(celsius) => f64::from(celsius),
                Err(error) => {
                    self.note(&format!("{key} read"), &error);
                    continue;
                }
            };
            let Some(celsius) = plausible(celsius) else {
                continue;
            };
            hottest = Some(hottest.map_or(celsius, |known| known.max(celsius)));
        }
        hottest
    }

    /// The `Tp*` sensors, enumerated once: 3,462 keys on every poll is not a
    /// price to pay for one number. A failed enumeration is NOT kept: it is
    /// reported and tried again no sooner than [`ENUMERATION_RETRY_DELAY`]
    /// later.
    fn resolved_processor_keys(&mut self) -> Option<Vec<FourCc>> {
        if let Some(keys) = &self.processor_keys {
            return Some(keys.clone());
        }
        if let Some(failed_at) = self.enumeration_failed_at
            && (self.now)().saturating_sub(failed_at) < ENUMERATION_RETRY_DELAY
        {
            return None;
        }
        match self.processor_sensor_keys() {
            Ok(keys) => {
                self.processor_keys = Some(keys.clone());
                self.enumeration_failed_at = None;
                Some(keys)
            }
            Err(error) => {
                self.note(
                    &format!(
                        "key table enumeration (retrying in {} s)",
                        ENUMERATION_RETRY_DELAY.as_secs()
                    ),
                    &error,
                );
                self.enumeration_failed_at = Some((self.now)());
                None
            }
        }
    }

    /// Every `Tp*` key holding a 4-byte `flt ` (102 on a Mac16,7). A key that
    /// will not read now is kept, reported, and read again on the next poll:
    /// one failure is not a reason to drop a sensor for the life of the
    /// process.
    fn processor_sensor_keys(&mut self) -> Result<Vec<FourCc>, SmcError> {
        let tp = u32::from(u16::from_be_bytes(*b"Tp"));
        let mut kept = Vec::new();
        for key in self.smc.keys()? {
            if key.0 >> 16 != tp {
                continue;
            }
            match self.smc.read(key) {
                Ok(value) => {
                    if value.ty == FourCc::FLT && value.bytes.len() == 4 {
                        kept.push(key);
                    }
                }
                Err(error) => {
                    self.note(&format!("{key} read"), &error);
                    kept.push(key);
                }
            }
        }
        Ok(kept)
    }

    // Fans

    fn read_fans(&mut self) -> Vec<FanReading> {
        let count = match self.smc.read(FAN_COUNT_KEY).and_then(|v| v.uint8_value()) {
            Ok(count) => count,
            Err(SmcError::KeyNotFound(_)) => return Vec::new(),
            Err(error) => {
                self.note(&format!("{FAN_COUNT_KEY} read"), &error);
                return Vec::new();
            }
        };
        // Ten fans is the most a fan key's single digit can name.
        (0..count.min(10))
            .filter_map(|fan| FanIndex::new(fan).and_then(|fan| self.read_fan(fan)))
            .collect()
    }

    /// `None` when the fan's speed does not read, or reads as no speed at all
    /// (not finite, or negative).
    fn read_fan(&mut self, fan: FanIndex) -> Option<FanReading> {
        let actual_rpm = self.rpm(fan, *b"Ac")?;
        let (min_rpm, max_rpm) = self.limits(fan);
        Some(FanReading {
            index: fan,
            actual_rpm,
            min_rpm,
            max_rpm,
            is_manual: self.is_manual(fan),
        })
    }

    /// `Mn` and `Mx`, read once both read and then kept. One that does not
    /// read is 0 this poll ([`FanReading::max_rpm`] 0 is "no maximum") and is
    /// asked again next poll.
    fn limits(&mut self, fan: FanIndex) -> (f64, f64) {
        if let Some(known) = self.fan_limits.get(&fan) {
            return *known;
        }
        let low = self.rpm(fan, *b"Mn");
        let high = self.rpm(fan, *b"Mx");
        let (Some(low), Some(high)) = (low, high) else {
            return (low.unwrap_or(0.0), high.unwrap_or(0.0));
        };
        self.fan_limits.insert(fan, (low, high));
        (low, high)
    }

    fn rpm(&mut self, fan: FanIndex, suffix: [u8; 2]) -> Option<f64> {
        let key = fan_key(fan, suffix);
        let value = match self.smc.read(key).and_then(|v| v.numeric_value()) {
            Ok(value) => value,
            Err(error) => {
                self.note(&format!("{key} read"), &error);
                return None;
            }
        };
        (value.is_finite() && value >= 0.0).then_some(value)
    }

    /// The fan's mode key reads 1. No mode key at all (older Intel Macs keep
    /// the bit in `FS! `) means nothing says another app holds this fan.
    fn is_manual(&mut self, fan: FanIndex) -> bool {
        let Some(suffix) = self.mode_key_suffix() else {
            return false;
        };
        let key = fan_key(fan, suffix);
        match self.smc.read(key).and_then(|v| v.uint8_value()) {
            Ok(mode) => mode == 1,
            Err(SmcError::KeyNotFound(_)) => false,
            Err(error) => {
                self.note(&format!("{key} read"), &error);
                false
            }
        }
    }

    /// `md` where `F0md` exists, else `Md`: some Apple Silicon models spell
    /// the mode key in lower case. Stats probes `F0md` the same way. `None`,
    /// and asked again next poll, when the probe itself fails.
    fn mode_key_suffix(&mut self) -> Option<[u8; 2]> {
        if self.mode_key_is_lower.is_none() {
            match self.smc.read(LOWER_MODE_PROBE_KEY) {
                Ok(_) => self.mode_key_is_lower = Some(true),
                Err(SmcError::KeyNotFound(_)) => self.mode_key_is_lower = Some(false),
                Err(error) => {
                    self.note(&format!("{LOWER_MODE_PROBE_KEY} read"), &error);
                    return None;
                }
            }
        }
        Some(if self.mode_key_is_lower == Some(true) {
            *b"md"
        } else {
            *b"Md"
        })
    }
}

/// What [`HeatSource::open`] opens: AppleSMC on a Mac.
#[cfg(target_os = "macos")]
pub type PlatformSmc = SmcConnection;
/// What [`HeatSource::open`] opens: nothing, off a Mac.
#[cfg(not(target_os = "macos"))]
pub type PlatformSmc = NoSmc;

/// The key source off a Mac. There is none, so no value of this type exists.
#[cfg(not(target_os = "macos"))]
pub enum NoSmc {}

#[cfg(not(target_os = "macos"))]
impl KeySource for NoSmc {
    fn read(&mut self, _key: FourCc) -> Result<SmcValue, SmcError> {
        match *self {}
    }

    fn keys(&mut self) -> Result<Vec<FourCc>, SmcError> {
        match *self {}
    }
}

/// How a [`HeatSource`] opens its key source.
pub type Opener<S> = Box<dyn FnMut() -> Result<S, SmcError> + Send>;

enum Opened<S> {
    /// Nothing tried yet: the first [`HeatSource::read`] opens.
    NotYet,
    Ready(Box<HeatReader<S>>),
    /// No AppleSMC: a VM, or not a Mac. Nothing about that changes while the
    /// process runs, so it is never asked again.
    Unavailable,
    /// AppleSMC is there and would not open, at this time on the source's
    /// clock. Tried again [`ENUMERATION_RETRY_DELAY`] later: a refusal at
    /// boot is not a reason to go without heat for the life of the process.
    Failed {
        at: Duration,
    },
}

/// A process's one reader, opened on the first read, or why there is none.
pub struct HeatSource<S> {
    opened: Opened<S>,
    open: Opener<S>,
    /// Shared with the reader once one opens, so both the open failures and
    /// the reader's own errors go to the one place.
    report: Arc<Mutex<Report>>,
    now: Clock,
    /// The last open failure reported, so one that repeats every retry is
    /// one line, not one a minute.
    last_failure: Option<SmcError>,
}

impl HeatSource<PlatformSmc> {
    /// The platform's source, with the real clock. Opens nothing yet.
    pub fn open(report: Report) -> Self {
        #[cfg(target_os = "macos")]
        let open: Opener<PlatformSmc> = Box::new(SmcConnection::open);
        #[cfg(not(target_os = "macos"))]
        let open: Opener<PlatformSmc> = Box::new(|| Err(SmcError::Unavailable));
        let start = Instant::now();
        Self::with_opener(open, report, Box::new(move || start.elapsed()))
    }
}

impl<S: KeySource> HeatSource<S> {
    pub fn with_opener(open: Opener<S>, report: Report, now: Clock) -> Self {
        Self {
            opened: Opened::NotYet,
            open,
            report: Arc::new(Mutex::new(report)),
            now,
            last_failure: None,
        }
    }

    fn try_open(&mut self) {
        match (self.open)() {
            Ok(smc) => {
                let shared = Arc::clone(&self.report);
                let report: Report = Box::new(move |line| {
                    let mut report = shared.lock().unwrap_or_else(PoisonError::into_inner);
                    report(line);
                });
                self.opened = Opened::Ready(Box::new(HeatReader::new(smc, report)));
                self.last_failure = None;
            }
            Err(SmcError::Unavailable) => self.opened = Opened::Unavailable,
            Err(error) => {
                if self.last_failure != Some(error) {
                    let mut report = self.report.lock().unwrap_or_else(PoisonError::into_inner);
                    report(&format!(
                        "SMC: AppleSMC would not open (retrying every {} s): {error}",
                        ENUMERATION_RETRY_DELAY.as_secs()
                    ));
                    self.last_failure = Some(error);
                }
                self.opened = Opened::Failed { at: (self.now)() };
            }
        }
    }

    /// Read now. `None` means one thing: this machine has no AppleSMC. An SMC
    /// that would not open is an empty reading, not `None`, and is opened
    /// again once [`ENUMERATION_RETRY_DELAY`] has passed.
    pub fn read(&mut self) -> Option<HeatReading> {
        let due = match &self.opened {
            Opened::NotYet => true,
            Opened::Failed { at } => (self.now)().saturating_sub(*at) >= ENUMERATION_RETRY_DELAY,
            Opened::Ready(_) | Opened::Unavailable => false,
        };
        if due {
            self.try_open();
        }
        match &mut self.opened {
            Opened::Ready(reader) => Some(reader.read()),
            Opened::Unavailable => None,
            Opened::NotYet | Opened::Failed { .. } => Some(HeatReading::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    fn code(text: &str) -> FourCc {
        FourCc::parse(text).unwrap()
    }

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    fn value(ty: FourCc, bytes: &[u8]) -> SmcValue {
        SmcValue::new(ty, bytes.to_vec())
    }

    // The codecs, against bytes read off a real Mac (a Mac16,7, 2026-10-18).

    /// `F0Tg`, `F0Mx` and `F0Ac` as read: `flt ` is little-endian.
    #[test]
    fn float_decodes_the_measured_bytes() {
        assert_eq!(
            value(FourCc::FLT, &hex("00409c45")).float_value(),
            Ok(5000.0)
        );
        assert_eq!(
            value(FourCc::FLT, &hex("0088b445")).float_value(),
            Ok(5777.0)
        );
        let measured = value(FourCc::FLT, &hex("73319d45")).float_value().unwrap();
        assert!((measured - 5030.2).abs() < 0.05, "{measured}");
    }

    #[test]
    fn float_encode_is_the_inverse() {
        assert_eq!(SmcValue::flt(5000.0).unwrap().bytes, hex("00409c45"));
        assert_eq!(SmcValue::flt(5777.0).unwrap().bytes, hex("0088b445"));
        assert_eq!(SmcValue::flt(5030.2).unwrap().float_value(), Ok(5030.2_f32));
        assert_eq!(SmcValue::flt(0.0).unwrap().bytes, [0, 0, 0, 0]);
    }

    /// A root helper sends what `flt` returns straight to the SMC, so NaN, the
    /// infinities and negatives stop here: none of them is a fan speed.
    #[test]
    fn float_encode_refuses_non_finite_and_negative() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1.0, -0.5] {
            match SmcValue::flt(bad) {
                Err(SmcError::OutOfRange { ty, value }) => {
                    assert_eq!(ty, FourCc::FLT);
                    assert!(
                        if bad.is_nan() {
                            value.is_nan()
                        } else {
                            value == f64::from(bad)
                        },
                        "{bad}: {value}"
                    );
                }
                other => panic!("{bad}: {other:?}"),
            }
        }
    }

    /// `write` sends nothing unless the value is the key's own type and size:
    /// a `ui8` to a `flt ` key would be a fan target of whatever byte it is.
    #[test]
    fn write_check_refuses_wrong_type_and_size() {
        let fan_target = SmcKeyInfo {
            ty: FourCc::FLT,
            size: 4,
        };
        assert_eq!(
            check_write(&SmcValue::flt(3000.0).unwrap(), fan_target),
            Ok(())
        );
        assert_eq!(
            check_write(&SmcValue::ui8(1), fan_target),
            Err(SmcError::WrongType {
                expected: FourCc::FLT,
                actual: FourCc::UI8
            })
        );
        assert_eq!(
            check_write(&value(FourCc::FLT, &[0, 0]), fan_target),
            Err(SmcError::WrongSize {
                ty: FourCc::FLT,
                expected: 4,
                actual: 2
            })
        );
        // Empty is a size mismatch even against a key that declares 0 bytes.
        assert_eq!(
            check_write(
                &value(FourCc::UI8, &[]),
                SmcKeyInfo {
                    ty: FourCc::UI8,
                    size: 0
                }
            ),
            Err(SmcError::WrongSize {
                ty: FourCc::UI8,
                expected: 0,
                actual: 0
            })
        );
        // And past the protocol's 32 bytes, whatever the key declares.
        assert_eq!(
            check_write(
                &value(FourCc::UI8, &[0; 33]),
                SmcKeyInfo {
                    ty: FourCc::UI8,
                    size: 33
                }
            ),
            Err(SmcError::WrongSize {
                ty: FourCc::UI8,
                expected: 33,
                actual: 33
            })
        );
    }

    /// A refused key-table index call names an index, not a missing `#KEY`.
    #[test]
    fn key_index_refusal_is_its_own_error() {
        assert_eq!(
            SmcError::check_index(0, 0x84, 7),
            Some(SmcError::KeyIndexRefused {
                index: 7,
                result: 0x84
            })
        );
        assert_eq!(SmcError::check_index(5, 0, 7), Some(SmcError::Kernel(5)));
        assert_eq!(SmcError::check_index(0, 0, 7), None);
    }

    #[test]
    fn integer_types_are_big_endian() {
        assert_eq!(value(FourCc::UI8, &[2]).uint8_value(), Ok(2));
        assert_eq!(value(FourCc::UI16, &[1, 2]).uint16_value(), Ok(0x0102));
        assert_eq!(SmcValue::ui16(0x0102).bytes, [1, 2]);
        assert_eq!(SmcValue::ui8(1).bytes, [1]);
        // `#KEY` on the measured Mac: 3,462 keys.
        assert_eq!(
            value(FourCc::UI32, &[0, 0, 0x0d, 0x86]).uint32_value(),
            Ok(3462)
        );
    }

    /// `fpe2`, Intel's fan-speed type: 14.2 fixed point, big-endian. 5000 rpm
    /// is 20000 quarters, 0x4e20.
    #[test]
    fn fpe2_round_trips() {
        assert_eq!(value(FourCc::FPE2, &[0x4e, 0x20]).fpe2_value(), Ok(5000.0));
        assert_eq!(SmcValue::fpe2(5000.0).unwrap().bytes, [0x4e, 0x20]);
        assert_eq!(SmcValue::fpe2(1350.25).unwrap().fpe2_value(), Ok(1350.25));
        assert_eq!(
            SmcValue::fpe2(16_384.0),
            Err(SmcError::OutOfRange {
                ty: FourCc::FPE2,
                value: 16_384.0
            })
        );
        assert!(SmcValue::fpe2(-1.0).is_err());
        assert!(SmcValue::fpe2(f64::NAN).is_err());
    }

    /// A fan speed is `flt ` on Apple Silicon and `fpe2` on Intel; the number
    /// is the same either way.
    #[test]
    fn numeric_value_reads_either_fan_type() {
        assert_eq!(
            value(FourCc::FLT, &hex("00409c45")).numeric_value(),
            Ok(5000.0)
        );
        assert_eq!(
            value(FourCc::FPE2, &[0x4e, 0x20]).numeric_value(),
            Ok(5000.0)
        );
        assert_eq!(
            value(code("sp78"), &[0x40, 0]).numeric_value(),
            Err(SmcError::UnsupportedType(code("sp78")))
        );
    }

    /// A decode of the wrong type or size is an error, never a number read
    /// from bytes that mean something else.
    #[test]
    fn wrong_type_and_size_are_errors() {
        assert_eq!(
            value(FourCc::UI8, &[1]).float_value(),
            Err(SmcError::WrongType {
                expected: FourCc::FLT,
                actual: FourCc::UI8
            })
        );
        assert_eq!(
            value(FourCc::FLT, &[0, 0]).float_value(),
            Err(SmcError::WrongSize {
                ty: FourCc::FLT,
                expected: 4,
                actual: 2
            })
        );
    }

    #[test]
    fn four_cc_round_trips_and_rejects_wrong_lengths() {
        assert_eq!(code("TCMz").to_string(), "TCMz");
        assert_eq!(FourCc::parse("flt ").map(|c| c.0), Some(0x666c_7420));
        assert_eq!(FourCc::parse("flt"), None);
        assert_eq!(FourCc::parse("F10Ac"), None);
        assert_eq!(fan_key(fan_index(1), *b"md"), code("F1md"));
        assert_eq!(FourCc(0x0000_4142).to_string(), "??AB");
    }

    /// 0xe00002c1, the non-root write on 2026-10-18, is its own case and never
    /// a generic kernel error: a root helper exists because of this one.
    #[test]
    fn not_privileged_is_its_own_case() {
        let key = code("F0Tg");
        assert_eq!(
            SmcError::check(0xe000_02c1_u32 as i32, 0, key),
            Some(SmcError::NotPrivileged)
        );
        assert_eq!(
            SmcError::check(0, 0x84, code("TCMz")),
            Some(SmcError::KeyNotFound(code("TCMz")))
        );
        assert_eq!(
            SmcError::check(0, 0x85, key),
            Some(SmcError::SmcRefused { key, result: 0x85 })
        );
        assert_eq!(SmcError::check(5, 0, key), Some(SmcError::Kernel(5)));
        assert_eq!(SmcError::check(0, 0, key), None);
    }

    // The reader's rules, against a fake SMC.

    /// An SMC made of a table. A key with no entry answers "no such key", as a
    /// real SMC does for a key this Mac lacks.
    #[derive(Default)]
    struct FakeSmc {
        values: HashMap<FourCc, Result<SmcValue, SmcError>>,
        key_table: Option<Result<Vec<FourCc>, SmcError>>,
        reads: Vec<FourCc>,
        keys_calls: usize,
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

    /// A `flt ` built from raw bits, so NaN and negatives can be served: the
    /// checked [`SmcValue::flt`] refuses them, and an SMC need not.
    fn flt(value: f32) -> Result<SmcValue, SmcError> {
        Ok(SmcValue::new(FourCc::FLT, value.to_le_bytes().to_vec()))
    }

    /// A reader on `smc` with a clock the test moves and a log it reads.
    struct Rig {
        reader: HeatReader<FakeSmc>,
        clock: Arc<Mutex<Duration>>,
        reported: Arc<Mutex<Vec<String>>>,
    }

    impl Rig {
        fn new(smc: FakeSmc) -> Self {
            let clock = Arc::new(Mutex::new(Duration::ZERO));
            let reported = Arc::new(Mutex::new(Vec::new()));
            let (c, r) = (Arc::clone(&clock), Arc::clone(&reported));
            let reader = HeatReader::with_clock(
                smc,
                Box::new(move || *c.lock().unwrap()),
                Box::new(move |line| r.lock().unwrap().push(line.to_string())),
            );
            Self {
                reader,
                clock,
                reported,
            }
        }

        fn reported(&self) -> Vec<String> {
            self.reported.lock().unwrap().clone()
        }

        fn set_clock(&self, to: Duration) {
            *self.clock.lock().unwrap() = to;
        }
    }

    /// A CPU key that fails leaves the fans, and it is reported.
    #[test]
    fn a_failing_cpu_key_leaves_the_fans() {
        let mut smc = FakeSmc::default();
        smc.set(
            "TCMz",
            Err(SmcError::SmcRefused {
                key: code("TCMz"),
                result: 0x85,
            }),
        );
        smc.one_fan(2000.0);
        let mut rig = Rig::new(smc);
        let heat = rig.reader.read();
        assert_eq!(heat.cpu_celsius, None);
        assert_eq!(
            heat.fans.iter().map(|f| f.actual_rpm).collect::<Vec<_>>(),
            [2000.0]
        );
        let reported = rig.reported();
        assert_eq!(reported.len(), 1, "{reported:?}");
        assert!(reported[0].contains("TCMz"), "{reported:?}");
    }

    /// A fan key that fails leaves the CPU, and is reported once however many
    /// polls it keeps failing.
    #[test]
    fn a_failing_fan_key_leaves_the_cpu_and_is_reported_once() {
        let mut smc = FakeSmc::default();
        smc.set("TCMz", flt(61.0));
        smc.one_fan(2000.0);
        smc.set("F0Ac", Err(SmcError::Kernel(5)));
        let mut rig = Rig::new(smc);
        for _ in 0..3 {
            let heat = rig.reader.read();
            assert_eq!(heat.cpu_celsius, Some(61.0));
            assert_eq!(heat.fans, []);
        }
        assert_eq!(rig.reported().len(), 1, "{:?}", rig.reported());
    }

    /// A `TCMz` that is not a 4-byte `flt ` is not the CPU key; the hottest
    /// `Tp*` sensor is.
    #[test]
    fn a_tcmz_of_another_type_falls_back_to_the_sensors() {
        let mut smc = FakeSmc::with_table(Ok(vec![code("Tp01"), code("TA0P"), code("Tp02")]));
        smc.set("TCMz", Ok(SmcValue::ui8(3)));
        smc.set("Tp01", flt(50.0));
        smc.set("Tp02", flt(70.0));
        smc.set("TA0P", flt(90.0));
        let mut rig = Rig::new(smc);
        assert_eq!(rig.reader.read().cpu_celsius, Some(70.0));
        assert_eq!(rig.reported(), Vec::<String>::new());
    }

    /// A failed enumeration is reported, not kept, and not retried before the
    /// delay is up.
    #[test]
    fn a_failed_enumeration_is_retried_after_the_delay() {
        let mut rig = Rig::new(FakeSmc::with_table(Err(SmcError::Kernel(5))));
        assert_eq!(rig.reader.read().cpu_celsius, None);
        assert_eq!(rig.reader.source().keys_calls, 1);
        assert_eq!(rig.reported().len(), 1);

        rig.set_clock(Duration::from_secs(ENUMERATION_RETRY_DELAY.as_secs() - 1));
        assert_eq!(rig.reader.read().cpu_celsius, None);
        assert_eq!(
            rig.reader.source().keys_calls,
            1,
            "retried before the delay"
        );

        rig.set_clock(ENUMERATION_RETRY_DELAY);
        rig.reader.source_mut().key_table = Some(Ok(vec![code("Tp01")]));
        rig.reader.source_mut().set("Tp01", flt(55.0));
        assert_eq!(rig.reader.read().cpu_celsius, Some(55.0));
        assert_eq!(rig.reader.source().keys_calls, 2);

        // Once it has worked, it is kept.
        rig.set_clock(ENUMERATION_RETRY_DELAY + Duration::from_secs(1000));
        rig.reader.read();
        assert_eq!(rig.reader.source().keys_calls, 2);
    }

    /// A `Tp*` key that fails once is read again next poll.
    #[test]
    fn a_tp_key_that_fails_once_is_not_dropped() {
        let mut smc = FakeSmc::with_table(Ok(vec![code("Tp01"), code("Tp02")]));
        smc.set("Tp01", flt(50.0));
        smc.set(
            "Tp02",
            Err(SmcError::SmcRefused {
                key: code("Tp02"),
                result: 0x85,
            }),
        );
        let mut rig = Rig::new(smc);
        assert_eq!(rig.reader.read().cpu_celsius, Some(50.0));
        rig.reader.source_mut().set("Tp02", flt(80.0));
        assert_eq!(rig.reader.read().cpu_celsius, Some(80.0));
    }

    /// NaN, infinities, negatives and out-of-band temperatures never leave the
    /// reader.
    #[test]
    fn non_finite_and_out_of_band_readings_are_dropped() {
        for bad in [f32::NAN, f32::INFINITY, 200.0, 0.0, -3.0] {
            let mut smc = FakeSmc::default();
            smc.set("TCMz", flt(bad));
            assert_eq!(Rig::new(smc).reader.read().cpu_celsius, None, "TCMz {bad}");
        }
        let mut smc = FakeSmc::with_table(Ok(vec![code("Tp01"), code("Tp02")]));
        smc.set("Tp01", flt(f32::NAN));
        smc.set("Tp02", flt(48.0));
        smc.set("FNum", Ok(SmcValue::ui8(3)));
        for fan in 0..3 {
            smc.values
                .insert(fan_key(fan_index(fan), *b"Mn"), flt(1000.0));
            smc.values
                .insert(fan_key(fan_index(fan), *b"Mx"), flt(5000.0));
        }
        smc.set("F0Ac", flt(-5.0));
        smc.set("F1Ac", flt(f32::INFINITY));
        smc.set("F2Ac", flt(1200.0));
        let heat = Rig::new(smc).reader.read();
        assert_eq!(heat.cpu_celsius, Some(48.0));
        assert_eq!(
            heat.fans.iter().map(|f| f.index.get()).collect::<Vec<_>>(),
            [2]
        );
    }

    /// `Mn` and `Mx` are read once and kept; `Ac` every poll.
    #[test]
    fn fan_limits_are_read_once() {
        let mut smc = FakeSmc::default();
        smc.one_fan(2000.0);
        let mut rig = Rig::new(smc);
        for _ in 0..3 {
            assert_eq!(
                rig.reader.read().fans.first().map(|f| f.max_rpm),
                Some(5000.0)
            );
        }
        let smc = rig.reader.source();
        assert_eq!(smc.read_count("F0Mn"), 1);
        assert_eq!(smc.read_count("F0Mx"), 1);
        assert_eq!(smc.read_count("F0Ac"), 3);
    }

    /// The mode key: `F0md` where it exists, and 1 is manual.
    #[test]
    fn a_fan_whose_mode_key_reads_one_is_manual() {
        let mut smc = FakeSmc::default();
        smc.one_fan(2000.0);
        smc.set("F0md", Ok(SmcValue::ui8(1)));
        assert_eq!(
            Rig::new(smc)
                .reader
                .read()
                .fans
                .first()
                .map(|f| f.is_manual),
            Some(true)
        );
        let mut smc = FakeSmc::default();
        smc.one_fan(2000.0);
        smc.set("F0Md", Ok(SmcValue::ui8(3)));
        assert_eq!(
            Rig::new(smc)
                .reader
                .read()
                .fans
                .first()
                .map(|f| f.is_manual),
            Some(false)
        );
    }

    /// The one hardware read. Passes, and says so, only where there is no
    /// AppleSMC; any other error, opening or reading, fails it.
    #[cfg(target_os = "macos")]
    #[test]
    fn hardware_read_is_plausible_where_there_is_an_smc() {
        unsafe extern "C" {
            fn getuid() -> u32;
        }
        let smc = match SmcConnection::open() {
            Ok(smc) => smc,
            Err(SmcError::Unavailable) => {
                eprintln!("skipped: no AppleSMC here (a VM)");
                return;
            }
            Err(other) => panic!("AppleSMC would not open: {other}"),
        };
        // The write check runs before anything reaches the kernel: a `ui8` to
        // `#KEY` (a `ui32` on every SMC) is `WrongType`, where a write that
        // reached the kernel would come back `NotPrivileged`. Only as non-root,
        // so nothing could land.
        // SAFETY: `getuid` takes nothing, cannot fail and touches no memory.
        if unsafe { getuid() } != 0 {
            assert_eq!(
                smc.write(code("#KEY"), &SmcValue::ui8(1)),
                Err(SmcError::WrongType {
                    expected: FourCc::UI32,
                    actual: FourCc::UI8
                })
            );
        }
        let errors = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&errors);
        let mut reader = HeatReader::new(
            smc,
            Box::new(move |line| sink.lock().unwrap().push(line.to_string())),
        );
        let heat = reader.read();
        assert_eq!(*errors.lock().unwrap(), Vec::<String>::new());
        let cpu = heat
            .cpu_celsius
            .expect("a Mac with an SMC has a CPU temperature");
        assert!(PLAUSIBLE_CELSIUS.contains(&cpu), "{cpu}");
        for fan in &heat.fans {
            assert!(fan.max_rpm > 0.0, "fan {}", fan.index.get());
            assert!(fan.actual_rpm >= 0.0, "fan {}", fan.index.get());
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn no_smc_off_a_mac() {
        let mut source = HeatSource::open(Box::new(|_| {}));
        assert_eq!(source.read(), None);
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

    /// An opener that fails `failures` times with `error`, then opens a fake
    /// SMC with one fan, counting every attempt.
    fn flaky_opener(
        error: SmcError,
        failures: usize,
        attempts: &Arc<Mutex<usize>>,
    ) -> Opener<FakeSmc> {
        let attempts = Arc::clone(attempts);
        Box::new(move || {
            let mut n = attempts.lock().unwrap();
            *n += 1;
            if *n <= failures {
                return Err(error);
            }
            let mut smc = FakeSmc::default();
            smc.set("TCMz", flt(61.0));
            Ok(smc)
        })
    }

    /// An SMC that would not open is tried again once the delay is up, not
    /// before, and its failure is reported once however often it repeats.
    #[test]
    fn a_failed_open_is_retried_after_the_delay() {
        let attempts = Arc::new(Mutex::new(0));
        let clock = Arc::new(Mutex::new(Duration::ZERO));
        let reported = Arc::new(Mutex::new(Vec::<String>::new()));
        let (c, r) = (Arc::clone(&clock), Arc::clone(&reported));
        let mut source = HeatSource::with_opener(
            flaky_opener(SmcError::Kernel(5), 2, &attempts),
            Box::new(move |line| r.lock().unwrap().push(line.to_string())),
            Box::new(move || *c.lock().unwrap()),
        );
        assert_eq!(source.read(), Some(HeatReading::default()));
        assert_eq!(*attempts.lock().unwrap(), 1);

        *clock.lock().unwrap() = Duration::from_secs(59);
        assert_eq!(source.read(), Some(HeatReading::default()));
        assert_eq!(*attempts.lock().unwrap(), 1, "retried before the delay");

        *clock.lock().unwrap() = Duration::from_secs(60);
        assert_eq!(source.read(), Some(HeatReading::default()));
        assert_eq!(*attempts.lock().unwrap(), 2);
        assert_eq!(
            reported.lock().unwrap().len(),
            1,
            "{:?}",
            reported.lock().unwrap()
        );

        *clock.lock().unwrap() = Duration::from_secs(120);
        assert_eq!(source.read().and_then(|h| h.cpu_celsius), Some(61.0));
        assert_eq!(*attempts.lock().unwrap(), 3);
        // Once open, it stays open.
        *clock.lock().unwrap() = Duration::from_secs(1000);
        source.read();
        assert_eq!(*attempts.lock().unwrap(), 3);
    }

    /// No AppleSMC is asked once: a VM does not grow one.
    #[test]
    fn no_smc_is_never_asked_again() {
        let attempts = Arc::new(Mutex::new(0));
        let clock = Arc::new(Mutex::new(Duration::ZERO));
        let c = Arc::clone(&clock);
        let mut source = HeatSource::with_opener(
            flaky_opener(SmcError::Unavailable, usize::MAX, &attempts),
            Box::new(|_| {}),
            Box::new(move || *c.lock().unwrap()),
        );
        assert_eq!(source.read(), None);
        *clock.lock().unwrap() = Duration::from_secs(3600);
        assert_eq!(source.read(), None);
        assert_eq!(*attempts.lock().unwrap(), 1);
    }
}
