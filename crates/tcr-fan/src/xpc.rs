//! libxpc and libdispatch, declared by hand: the root helper's listener, the
//! proxy's client connection, the XPC dictionary behind [`crate::wire`]'s
//! traits, and the dispatch queue, timer and signal source the helper runs on.
//!
//! # Why hand-declared
//!
//! Every call here is a plain C function in libSystem, which every Rust binary
//! on macOS already links. The only two that take an Objective-C block (a
//! connection's event handler, a reply handler) get one from `block2`; the
//! timer and the signal source use the `_f` variants that take a function
//! pointer. Nothing else is linked in, so `otool -L` on the helper lists only
//! system libraries.
//!
//! # Ownership
//!
//! libxpc objects are reference counted by hand here. `create` calls return a
//! reference the caller owns ([`Owned`] releases it on drop). An object handed
//! to an event or reply handler is borrowed for the call: [`Borrowed`] never
//! releases, and a peer connection that is kept past its handler is retained
//! first.
//!
//! # Peer requirements
//!
//! Both ends call `xpc_connection_set_peer_code_signing_requirement` before
//! `xpc_connection_activate`, and a non-zero return is an error the caller
//! sees ([`XpcError::Requirement`]): a listener with no requirement must never
//! start. On a listener a request from a peer that fails the requirement is
//! dropped (`xpc/connection.h`); on a client, a helper that fails it answers
//! `XPC_ERROR_PEER_CODE_SIGNING_REQUIREMENT` instead of the reply.
//!
//! # No blocking send
//!
//! The client never calls `xpc_connection_send_message_with_reply_sync`, which
//! "can block indefinitely": a request the listener dropped may never be
//! answered. [`Client::send`] sends with a reply handler and waits on its own
//! channel with a deadline.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fmt;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use block2::{Block, RcBlock};

use crate::root::PeerId;
use crate::wire::{self, Dict, DictMut, MemDict, Reply, ReplyError, Request, Value};

/// An `xpc_object_t` or a `dispatch_*_t`: an opaque pointer.
type Object = *mut c_void;

/// The type of the statics libxpc and libdispatch export: only their
/// addresses matter.
#[repr(C)]
struct Opaque {
    _private: [u8; 0],
}

/// `xpc/connection.h`: the caller is the listener for a service its launchd
/// plist advertises.
const MACH_SERVICE_LISTENER: u64 = 1 << 0;
/// `xpc/connection.h`: the service lives in the system (root) domain, where a
/// launchd daemon's does.
const MACH_SERVICE_PRIVILEGED: u64 = 1 << 1;
/// `dispatch/time.h`: `DISPATCH_TIME_NOW`.
const DISPATCH_TIME_NOW: u64 = 0;

// SAFETY: each declaration below matches its prototype in the macOS SDK's
// `xpc/xpc.h`, `xpc/connection.h`, `dispatch/queue.h`, `dispatch/time.h`,
// `dispatch/source.h` and `dispatch/object.h`; all are in libSystem.
unsafe extern "C" {
    static _xpc_type_connection: Opaque;
    static _xpc_type_dictionary: Opaque;
    static _xpc_type_error: Opaque;
    static _xpc_type_uint64: Opaque;
    static _xpc_type_double: Opaque;
    static _xpc_type_bool: Opaque;
    static _xpc_type_string: Opaque;
    static _xpc_error_connection_interrupted: Opaque;
    static _xpc_error_connection_invalid: Opaque;
    static _xpc_error_termination_imminent: Opaque;
    static _xpc_error_peer_code_signing_requirement: Opaque;
    static _dispatch_source_type_signal: Opaque;

    fn xpc_get_type(object: Object) -> *const Opaque;
    fn xpc_retain(object: Object) -> Object;
    fn xpc_release(object: Object);

    fn xpc_connection_create_mach_service(
        name: *const c_char,
        targetq: Object,
        flags: u64,
    ) -> Object;
    fn xpc_connection_set_event_handler(connection: Object, handler: &Block<dyn Fn(Object)>);
    fn xpc_connection_set_target_queue(connection: Object, targetq: Object);
    fn xpc_connection_set_peer_code_signing_requirement(
        connection: Object,
        requirement: *const c_char,
    ) -> c_int;
    fn xpc_connection_activate(connection: Object);
    fn xpc_connection_cancel(connection: Object);
    fn xpc_connection_send_message(connection: Object, message: Object);
    fn xpc_connection_send_message_with_reply(
        connection: Object,
        message: Object,
        replyq: Object,
        handler: &Block<dyn Fn(Object)>,
    );

    fn xpc_dictionary_create(
        keys: *const *const c_char,
        values: *const Object,
        count: usize,
    ) -> Object;
    fn xpc_dictionary_create_reply(original: Object) -> Object;
    fn xpc_dictionary_get_count(dictionary: Object) -> usize;
    fn xpc_dictionary_get_value(dictionary: Object, key: *const c_char) -> Object;
    fn xpc_dictionary_set_uint64(dictionary: Object, key: *const c_char, value: u64);
    fn xpc_dictionary_set_double(dictionary: Object, key: *const c_char, value: f64);
    fn xpc_dictionary_set_bool(dictionary: Object, key: *const c_char, value: bool);
    fn xpc_dictionary_set_string(dictionary: Object, key: *const c_char, value: *const c_char);
    fn xpc_uint64_get_value(object: Object) -> u64;
    fn xpc_double_get_value(object: Object) -> f64;
    fn xpc_bool_get_value(object: Object) -> bool;
    fn xpc_string_get_string_ptr(object: Object) -> *const c_char;

    fn dispatch_queue_create(label: *const c_char, attr: Object) -> Object;
    fn dispatch_release(object: Object);
    fn dispatch_time(when: u64, delta: i64) -> u64;
    fn dispatch_after_f(
        when: u64,
        queue: Object,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
    fn dispatch_source_create(
        kind: *const Opaque,
        handle: usize,
        mask: usize,
        queue: Object,
    ) -> Object;
    fn dispatch_source_set_event_handler_f(source: Object, handler: extern "C" fn(*mut c_void));
    fn dispatch_activate(object: Object);
    fn dispatch_main() -> !;
}

/// What went wrong talking XPC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XpcError {
    /// A label or requirement with an interior NUL: no C string can carry it.
    BadName,
    /// libxpc or libdispatch returned no object.
    NoObject(&'static str),
    /// `xpc_connection_set_peer_code_signing_requirement` returned non-zero:
    /// the requirement did not parse, or this macOS has no such call.
    Requirement(c_int),
    /// `XPC_ERROR_CONNECTION_INVALID`: no such service, not registered,
    /// awaiting approval, or the connection was cancelled.
    Invalid,
    /// `XPC_ERROR_CONNECTION_INTERRUPTED`: the helper exited or crashed with
    /// the message in flight. launchd starts it again on the next message.
    Interrupted,
    /// `XPC_ERROR_PEER_CODE_SIGNING_REQUIREMENT`: whatever answered on the
    /// service is not the helper this build trusts.
    PeerCodeSigningRequirement,
    /// `XPC_ERROR_TERMINATION_IMMINENT`.
    TerminationImminent,
    /// An error object this code does not name, or an object of another type
    /// where a dictionary or an error was due.
    Unexpected,
    /// No reply within the deadline.
    NoReply(Duration),
    /// A reply that is not the contract's shape.
    Reply(ReplyError),
}

impl fmt::Display for XpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadName => write!(f, "a label or requirement contains a NUL byte"),
            Self::NoObject(call) => write!(f, "{call} returned no object"),
            Self::Requirement(rc) => write!(
                f,
                "xpc_connection_set_peer_code_signing_requirement returned {rc}"
            ),
            Self::Invalid => write!(
                f,
                "connection invalid (XPC_ERROR_CONNECTION_INVALID): no such service, not registered, or not yet approved"
            ),
            Self::Interrupted => write!(
                f,
                "connection interrupted (XPC_ERROR_CONNECTION_INTERRUPTED): the helper went away mid-call"
            ),
            Self::PeerCodeSigningRequirement => write!(
                f,
                "the service's signature does not satisfy this build's helper requirement (XPC_ERROR_PEER_CODE_SIGNING_REQUIREMENT)"
            ),
            Self::TerminationImminent => write!(f, "XPC_ERROR_TERMINATION_IMMINENT"),
            Self::Unexpected => write!(f, "an XPC event this code does not name"),
            Self::NoReply(after) => write!(f, "no reply within {} ms", after.as_millis()),
            Self::Reply(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for XpcError {}

fn c_string(text: &str) -> Result<CString, XpcError> {
    CString::new(text).map_err(|_nul| XpcError::BadName)
}

/// Is `object` of this XPC type?
fn is(object: Object, kind: *const Opaque) -> bool {
    // SAFETY: `object` is a live XPC object for the duration of the handler
    // or the owner that passed it in; `xpc_get_type` only reads its isa.
    let actual = unsafe { xpc_get_type(object) };
    std::ptr::eq(actual, kind)
}

/// Which connection error `event` is, by identity with libxpc's own error
/// objects. Errors are global objects, compared by address.
fn error_kind(event: Object) -> XpcError {
    let at = event.cast_const().cast::<Opaque>();
    if std::ptr::eq(at, &raw const _xpc_error_connection_invalid) {
        XpcError::Invalid
    } else if std::ptr::eq(at, &raw const _xpc_error_connection_interrupted) {
        XpcError::Interrupted
    } else if std::ptr::eq(at, &raw const _xpc_error_peer_code_signing_requirement) {
        XpcError::PeerCodeSigningRequirement
    } else if std::ptr::eq(at, &raw const _xpc_error_termination_imminent) {
        XpcError::TerminationImminent
    } else {
        XpcError::Unexpected
    }
}

/// One owned reference to an XPC object, released on drop.
pub struct Owned(Object);

impl Owned {
    /// Take ownership of a +1 reference; `None` for null.
    fn from_create(object: Object) -> Option<Self> {
        (!object.is_null()).then_some(Self(object))
    }

    /// Retain a borrowed object so it outlives its handler.
    fn retain(object: Object) -> Self {
        // SAFETY: `object` is a live XPC object borrowed for this call;
        // retaining it adds the reference this value releases on drop.
        Self(unsafe { xpc_retain(object) })
    }

    const fn raw(&self) -> Object {
        self.0
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: this value owns exactly one reference, taken at creation
        // or by `xpc_retain`, and drops it exactly once.
        unsafe { xpc_release(self.0) }
    }
}

// SAFETY: XPC objects are reference counted with atomic operations and their
// connection calls are documented as safe from any thread.
unsafe impl Send for Owned {}
// SAFETY: as above; a shared `&Owned` only ever reads the pointer.
unsafe impl Sync for Owned {}

/// An XPC dictionary that implements the wire traits.
pub struct XpcDict<'a> {
    object: Object,
    _life: PhantomData<&'a ()>,
}

impl XpcDict<'_> {
    /// A dictionary borrowed for the length of a handler call.
    const fn borrowed(object: Object) -> Self {
        Self {
            object,
            _life: PhantomData,
        }
    }
}

/// An XPC dictionary this code created and owns.
pub struct OwnedDict(Owned);

impl OwnedDict {
    pub fn new() -> Result<Self, XpcError> {
        // SAFETY: an empty dictionary: null key and value arrays with count 0
        // is the documented way to make one.
        let object = unsafe { xpc_dictionary_create(std::ptr::null(), std::ptr::null(), 0) };
        Owned::from_create(object)
            .map(Self)
            .ok_or(XpcError::NoObject("xpc_dictionary_create"))
    }

    /// The reply dictionary for `request`, or `None` when the sender asked
    /// for no reply.
    fn reply_to(request: Object) -> Option<Self> {
        // SAFETY: `request` is a live dictionary received in a handler.
        Owned::from_create(unsafe { xpc_dictionary_create_reply(request) }).map(Self)
    }

    const fn as_dict(&self) -> XpcDict<'_> {
        XpcDict::borrowed(self.0.raw())
    }
}

fn dict_value(dictionary: Object, name: &str) -> Option<Value> {
    let name = CString::new(name).ok()?;
    // SAFETY: `dictionary` is a live XPC dictionary and `name` a C string that
    // outlives the call. The value returned is borrowed from the dictionary.
    let value = unsafe { xpc_dictionary_get_value(dictionary, name.as_ptr()) };
    if value.is_null() {
        return None;
    }
    // Each getter below reads a `value` just checked to be of its type,
    // borrowed from a dictionary that outlives this call.
    Some(if is(value, &raw const _xpc_type_uint64) {
        // SAFETY: an XPC uint64, per the check above.
        Value::U64(unsafe { xpc_uint64_get_value(value) })
    } else if is(value, &raw const _xpc_type_double) {
        // SAFETY: an XPC double, per the check above.
        Value::F64(unsafe { xpc_double_get_value(value) })
    } else if is(value, &raw const _xpc_type_bool) {
        // SAFETY: an XPC bool, per the check above.
        Value::Bool(unsafe { xpc_bool_get_value(value) })
    } else if is(value, &raw const _xpc_type_string) {
        // SAFETY: an XPC string, per the check above.
        let text = unsafe { xpc_string_get_string_ptr(value) };
        if text.is_null() {
            Value::Other
        } else {
            // SAFETY: libxpc hands out a NUL-terminated string that lives as
            // long as the string object.
            match unsafe { CStr::from_ptr(text) }.to_str() {
                Ok(text) => Value::Str(text.to_owned()),
                Err(_not_utf8) => Value::Other,
            }
        }
    } else {
        Value::Other
    })
}

impl Dict for XpcDict<'_> {
    fn count(&self) -> usize {
        // SAFETY: `object` is a live XPC dictionary for this value's lifetime.
        unsafe { xpc_dictionary_get_count(self.object) }
    }

    fn value(&self, key: &str) -> Option<Value> {
        dict_value(self.object, key)
    }
}

impl Dict for OwnedDict {
    fn count(&self) -> usize {
        self.as_dict().count()
    }

    fn value(&self, key: &str) -> Option<Value> {
        self.as_dict().value(key)
    }
}

impl DictMut for OwnedDict {
    fn set(&mut self, key: &'static str, value: &Value) {
        let Ok(key) = c_string(key) else {
            return;
        };
        let (dict, key) = (self.0.raw(), key.as_ptr());
        match value {
            // SAFETY: a live dictionary this value owns; `key` outlives the call.
            Value::U64(v) => unsafe { xpc_dictionary_set_uint64(dict, key, *v) },
            // SAFETY: as for `U64`.
            Value::F64(v) => unsafe { xpc_dictionary_set_double(dict, key, *v) },
            // SAFETY: as for `U64`.
            Value::Bool(v) => unsafe { xpc_dictionary_set_bool(dict, key, *v) },
            Value::Str(v) => {
                if let Ok(text) = c_string(v) {
                    // SAFETY: as for `U64`; libxpc copies the string.
                    unsafe { xpc_dictionary_set_string(dict, key, text.as_ptr()) }
                }
            }
            // Nothing encodes one, and it has no XPC type to write.
            Value::Other => {}
        }
    }
}

/// A serial dispatch queue: everything the helper does runs on one.
pub struct Queue(Object);

// SAFETY: dispatch queues are thread-safe objects; submitting work to one from
// any thread is their purpose.
unsafe impl Send for Queue {}
// SAFETY: as above.
unsafe impl Sync for Queue {}

impl Queue {
    pub fn serial(label: &str) -> Result<Self, XpcError> {
        let label = c_string(label)?;
        // SAFETY: a null attribute makes a serial queue; libdispatch copies
        // the label.
        let queue = unsafe { dispatch_queue_create(label.as_ptr(), std::ptr::null_mut()) };
        if queue.is_null() {
            return Err(XpcError::NoObject("dispatch_queue_create"));
        }
        Ok(Self(queue))
    }

    /// Run `work(context)` on this queue after `delay`. The context is a
    /// number, not a pointer to anything: nothing can dangle.
    pub fn after(&self, delay: Duration, context: u64, work: extern "C" fn(*mut c_void)) {
        let nanos = i64::try_from(delay.as_nanos()).unwrap_or(i64::MAX);
        // SAFETY: a live queue; `work` is a plain function and `context` is
        // an integer it reads back, never dereferenced.
        unsafe {
            dispatch_after_f(
                dispatch_time(DISPATCH_TIME_NOW, nanos),
                self.0,
                context as usize as *mut c_void,
                work,
            );
        }
    }

    /// Run `work` on this queue whenever `signal` arrives. The caller must
    /// also set the signal's default action to ignore (`libc::signal(_,
    /// SIG_IGN)`), or the default action ends the process first. The source
    /// lives as long as the returned value.
    pub fn on_signal(
        &self,
        signal: c_int,
        work: extern "C" fn(*mut c_void),
    ) -> Result<SignalSource, XpcError> {
        let handle = usize::try_from(signal).map_err(|_negative| XpcError::BadName)?;
        // SAFETY: the signal source type is libdispatch's own static; a live
        // queue; mask 0 as the header requires for signal sources.
        let source = unsafe {
            dispatch_source_create(&raw const _dispatch_source_type_signal, handle, 0, self.0)
        };
        if source.is_null() {
            return Err(XpcError::NoObject("dispatch_source_create"));
        }
        // SAFETY: `source` was just created and is not yet active, which is
        // when a handler may be set; then it is activated once.
        unsafe {
            dispatch_source_set_event_handler_f(source, work);
            dispatch_activate(source);
        }
        Ok(SignalSource(source))
    }

    const fn raw(&self) -> Object {
        self.0
    }
}

impl Drop for Queue {
    fn drop(&mut self) {
        // SAFETY: the one reference `dispatch_queue_create` returned.
        unsafe { dispatch_release(self.0) }
    }
}

/// A dispatch signal source, kept alive by holding this.
pub struct SignalSource(Object);

impl Drop for SignalSource {
    fn drop(&mut self) {
        // SAFETY: the one reference `dispatch_source_create` returned.
        unsafe { dispatch_release(self.0) }
    }
}

/// Park the main thread and serve every queue. Never returns.
pub fn run() -> ! {
    // SAFETY: called once, from the main thread, after every source and
    // listener is set up.
    unsafe { dispatch_main() }
}

/// What a listener does with its connections. Every call arrives on the
/// listener's queue, one at a time.
pub trait Service: Send + Sync + 'static {
    fn connected(&self, peer: PeerId);
    /// The reply's entries. Sent only when the peer asked for a reply.
    fn message(&self, peer: PeerId, request: &XpcDict<'_>) -> MemDict;
    fn disconnected(&self, peer: PeerId);
    /// An error on the listener itself, for the log.
    fn listener_error(&self, error: XpcError);
}

/// Drop a connection that was never activated, without releasing it.
///
/// libxpc treats the last release of a connection that was never activated as
/// API misuse and traps the process (`_xpc_api_misuse` from `_xref_dispose`,
/// measured 2026-10-18 when a requirement that does not parse was refused). And
/// activating it just to cancel it would, for a listener, accept connections
/// for an instant with no requirement on them. So the one reference is
/// leaked, once per refused call. The helper exits on it, so for the listener
/// that is once per process. For a [`Client`] it is once per
/// [`Client::connect`] that fails this way, so a caller must not retry a
/// [`XpcError::Requirement`] in a loop: the proxy's controller turns it into
/// `Unavailable(HelperSignature)` and retries that on its own only once,
/// `SIGNATURE_RETRY_AFTER` later (`crate::controller`), which bounds the leak
/// to one per refused connect and at most two per command an operator sends
/// (or per start, which applies the saved mode).
const fn abandon(connection: Owned) {
    std::mem::forget(connection);
}

/// A Mach-service connection for `label` on `queue`, with `requirement` set on
/// it, not yet activated. A requirement libxpc refuses abandons the connection.
fn service_connection(
    label: &str,
    requirement: &str,
    queue: &Queue,
    flags: u64,
) -> Result<Owned, XpcError> {
    let name = c_string(label)?;
    let requirement = c_string(requirement)?;
    // SAFETY: `name` outlives the call; a live queue.
    let connection = Owned::from_create(unsafe {
        xpc_connection_create_mach_service(name.as_ptr(), queue.raw(), flags)
    })
    .ok_or(XpcError::NoObject("xpc_connection_create_mach_service"))?;
    // SAFETY: an inactive connection, a C string that outlives the call
    // (libxpc copies it), called once per connection as required.
    let rc = unsafe {
        xpc_connection_set_peer_code_signing_requirement(connection.raw(), requirement.as_ptr())
    };
    if rc != 0 {
        abandon(connection);
        return Err(XpcError::Requirement(rc));
    }
    Ok(connection)
}

/// Give an inactive connection its handler, then activate it.
fn activate(connection: Object, handler: &Block<dyn Fn(Object)>) {
    // SAFETY: a live, inactive connection the caller holds; libxpc copies the
    // block.
    unsafe {
        xpc_connection_set_event_handler(connection, handler);
        xpc_connection_activate(connection);
    }
}

/// The helper's Mach-service listener.
pub struct Listener {
    connection: Owned,
}

impl Listener {
    /// Create the listener for `label` on `queue`, set `requirement` on it,
    /// and only then activate it. Any failure before activation is returned
    /// and nothing listens.
    pub fn start(
        label: &str,
        requirement: &str,
        queue: &Arc<Queue>,
        service: Arc<dyn Service>,
    ) -> Result<Self, XpcError> {
        let connection = service_connection(label, requirement, queue, MACH_SERVICE_LISTENER)?;
        let next_peer = Arc::new(AtomicU64::new(1));
        let peer_queue = Arc::clone(queue);
        let handler = RcBlock::new(move |event: Object| {
            if is(event, &raw const _xpc_type_connection) {
                let id = PeerId(next_peer.fetch_add(1, Ordering::Relaxed));
                accept(event, id, &peer_queue, Arc::clone(&service));
            } else {
                service.listener_error(error_kind(event));
            }
        });
        activate(connection.raw(), &handler);
        Ok(Self { connection })
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // SAFETY: a live connection; cancel before the release on drop.
        unsafe { xpc_connection_cancel(self.connection.raw()) }
    }
}

/// Take one new peer connection: retain it, put it on the listener's queue,
/// give it a handler, activate it.
fn accept(peer: Object, id: PeerId, queue: &Queue, service: Arc<dyn Service>) {
    let peer = Owned::retain(peer);
    let raw = peer.raw();
    // Released on the connection's last event (invalid), so a cancelled
    // connection does not keep itself alive through its own handler.
    let held = Mutex::new(Some(peer));
    service.connected(id);
    let handler = RcBlock::new(move |event: Object| {
        if is(event, &raw const _xpc_type_dictionary) {
            let answer = service.message(id, &XpcDict::borrowed(event));
            let Some(mut reply) = OwnedDict::reply_to(event) else {
                return;
            };
            answer.copy_into(&mut reply);
            let guard = held.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(peer) = guard.as_ref() {
                // SAFETY: a live, retained peer connection and a live reply.
                unsafe { xpc_connection_send_message(peer.raw(), reply.0.raw()) }
            }
        } else if error_kind(event) == XpcError::Invalid {
            let gone = held.lock().unwrap_or_else(PoisonError::into_inner).take();
            if gone.is_some() {
                service.disconnected(id);
            }
        }
    });
    // SAFETY: a retained, inactive peer connection and a live queue.
    unsafe { xpc_connection_set_target_queue(raw, queue.raw()) }
    activate(raw, &handler);
}

/// The proxy's end: one connection to the helper, in the system domain, that
/// only a helper of this build's identity can answer.
pub struct Client {
    connection: Owned,
    queue: Queue,
}

impl Client {
    /// Connect to `label`, demanding `requirement` of whatever answers.
    /// `on_error` sees every connection-level error (interrupted, invalid, a
    /// requirement failure) on the client's own queue. Nothing is sent until
    /// [`Self::send`], and launchd starts the helper only then.
    pub fn connect(
        label: &str,
        requirement: &str,
        on_error: impl Fn(XpcError) + Send + 'static,
    ) -> Result<Self, XpcError> {
        let queue = Queue::serial(&format!("{label}.client"))?;
        let connection = service_connection(label, requirement, &queue, MACH_SERVICE_PRIVILEGED)?;
        let handler = RcBlock::new(move |event: Object| {
            if is(event, &raw const _xpc_type_error) {
                on_error(error_kind(event));
            }
        });
        activate(connection.raw(), &handler);
        Ok(Self { connection, queue })
    }

    /// Send one request and wait at most `deadline` for its reply.
    pub fn send(&self, request: &Request, deadline: Duration) -> Result<Reply, XpcError> {
        let mut message = OwnedDict::new()?;
        wire::encode_request(request, &mut message);
        let op = request.op();
        let (tx, rx) = mpsc::channel::<Result<Reply, XpcError>>();
        let handler = RcBlock::new(move |event: Object| {
            let outcome = if is(event, &raw const _xpc_type_dictionary) {
                wire::decode_reply(op, &XpcDict::borrowed(event)).map_err(XpcError::Reply)
            } else if is(event, &raw const _xpc_type_error) {
                Err(error_kind(event))
            } else {
                Err(XpcError::Unexpected)
            };
            if tx.send(outcome).is_err() {
                // The caller's deadline passed and it stopped waiting.
            }
        });
        // SAFETY: a live, active connection, a live dictionary, a live queue;
        // libxpc copies the block and retains the message as it needs.
        unsafe {
            xpc_connection_send_message_with_reply(
                self.connection.raw(),
                message.0.raw(),
                self.queue.raw(),
                &handler,
            );
        }
        rx.recv_timeout(deadline)
            .unwrap_or(Err(XpcError::NoReply(deadline)))
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // SAFETY: a live connection; cancel before the release on drop. This
        // is what tells the helper this end has gone.
        unsafe { xpc_connection_cancel(self.connection.raw()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{HelperError, Op, decode_request, encode_reply};

    /// The codec over a real XPC dictionary: what root decodes is what the
    /// client encoded, with the same strictness as the in-memory one.
    #[test]
    fn xpc_dictionary_round_trip() {
        let fan = tcr_smc::FanIndex::new(1).unwrap();
        for request in [
            Request::Version,
            Request::SetManual { fan },
            Request::SetTarget { fan, rpm: 2500.5 },
            Request::HandBack,
        ] {
            let mut dict = OwnedDict::new().unwrap();
            wire::encode_request(&request, &mut dict);
            assert_eq!(decode_request(&dict), Ok(request));
        }
        let mut reply = OwnedDict::new().unwrap();
        encode_reply(
            &Reply::Version {
                protocol: 1,
                build: "abc".into(),
            },
            &mut reply,
        );
        assert_eq!(
            wire::decode_reply(Op::Version, &reply),
            Ok(Reply::Version {
                protocol: 1,
                build: "abc".into()
            })
        );
        let mut refused = OwnedDict::new().unwrap();
        encode_reply(
            &Reply::Refused(HelperError::HandBackIncomplete {
                fans: 1,
                ftst: true,
            }),
            &mut refused,
        );
        assert_eq!(
            wire::decode_reply(Op::HandBack, &refused),
            Ok(Reply::Refused(HelperError::HandBackIncomplete {
                fans: 1,
                ftst: true
            }))
        );
    }

    #[test]
    fn a_wrong_xpc_type_is_refused() {
        let mut dict = OwnedDict::new().unwrap();
        dict.set_u64(wire::key::OP, Op::SetManual.code());
        dict.set_f64(wire::key::FAN, 0.0);
        assert_eq!(decode_request(&dict), Err(wire::Refusal::BadRequest));
        let mut text = OwnedDict::new().unwrap();
        text.set_str(wire::key::OP, "1");
        assert_eq!(decode_request(&text), Err(wire::Refusal::BadRequest));
    }

    /// A service no launchd job advertises: the client sees an error or no
    /// reply, never a hang past its deadline. Nothing here registers or
    /// starts anything.
    #[test]
    fn a_missing_service_fails_within_the_deadline() {
        let client = Client::connect(
            "io.github.dhkts1.tcr.test-no-such-service",
            crate::identity::FanIdentity::Dev.helper_requirement(),
            |_error| {},
        )
        .unwrap();
        let deadline = Duration::from_secs(2);
        let started = std::time::Instant::now();
        let outcome = client.send(&Request::Version, deadline);
        assert!(outcome.is_err(), "{outcome:?}");
        assert!(started.elapsed() < deadline + Duration::from_secs(1));
    }

    #[test]
    fn a_requirement_that_does_not_parse_is_refused_before_activation() {
        let outcome = Client::connect(
            "io.github.dhkts1.tcr.test-no-such-service",
            "anchor apple generic and identifier",
            |_error| {},
        );
        assert!(
            matches!(outcome, Err(XpcError::Requirement(_))),
            "a broken requirement must stop the connection"
        );
    }
}
