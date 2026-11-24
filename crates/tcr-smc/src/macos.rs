//! The AppleSMC connection: the IOKit calls and the parameter struct they
//! carry. All of this crate's `unsafe` is here.

use std::cell::Cell;
use std::ffi::{c_char, c_void};
use std::marker::PhantomData;

use crate::codec::{
    FourCc, KeyAccess, KeySource, MAX_VALUE_SIZE, SmcError, SmcKeyInfo, SmcValue, check_write,
};

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

impl KeyAccess for SmcConnection {
    fn write(&mut self, key: FourCc, value: &SmcValue) -> Result<(), SmcError> {
        Self::write(self, key, value)
    }
}
