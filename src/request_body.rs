//! One `/v1/messages` request body, parsed once into everything the proxy reads from it.
//!
//! Every consumer on the request path used to parse the body on its own: the model for
//! selection ([`crate::model`]), the session peek (the wire session id, the tool events and the
//! account-bound tokens, [`crate::session_wire`]), the affinity key (`metadata.user_id`, or a
//! hash of `system` + `tools`), the extended-cache-window answer ([`crate::cache_ttl`]), the
//! `ttl: 1h` rewrite itself, and the account-uuid splice on every attempt
//! ([`crate::account_uuid`]). Each was a pass over the whole body, and the bound-token scan
//! alone was three; at a multi-megabyte conversation that was most of what tcr spent on a
//! request. [`RequestBody::parse`] walks the body once and keeps only what those consumers
//! read: a few owned strings, the tool events of the newest turn, the bound tokens, two byte
//! ranges and an offset. The body itself stays one [`Bytes`], forwarded as the client sent it
//! unless the rewrite changed a `ttl`.
//!
//! # One walk
//!
//! The walk visits the top-level object and reads each member once:
//!
//! - `model`, `metadata`, `previous_message_id` and `thread` are borrowed as raw spans and
//!   parsed after the walk; they are a few bytes each.
//! - `system` and `tools` are borrowed as raw spans (the affinity hash reads their exact
//!   bytes) and then walked for their cache breakpoints. They are a small share of a long
//!   conversation, so this is the one place a part of the body is read twice.
//! - `messages` is walked in place, once: each content block's `cache_control` is read for
//!   the rewrite and the extended-window answer, the bound shapes (`thinking.signature`,
//!   `redacted_thinking.data`, `server_tool_use.id`) for the bound tokens, and the fields the
//!   tool events need are kept as borrowed spans for the newest
//!   [`crate::session_wire::TAIL_SCAN_CAP`] messages only. A tool result's content, a thinking
//!   block's text and a tool call's input are skipped, never copied.
//!
//! # The same answers
//!
//! Each consumer gets the answer its own parse gave, including when that parse failed: a
//! failure is tracked per consumer, not for the body. The old peeks were `#[derive]` structs,
//! which reject a member they model with the wrong type and any member they model that
//! appears twice, and a rejection lost the whole peek; so, for example, a `thread` that is a
//! string still empties the session peek (no session id, no tool events, no bound tokens),
//! and a duplicated `metadata` still loses the affinity key's tier 2 and 3 but not the model.
//! `tests` drives the old functions, kept test-only, beside this one on realistic and hostile
//! bodies and requires the same answers. The answers are taken on the body as forwarded,
//! that is AFTER the rewrite, as they were when each consumer parsed the rewritten bytes: the
//! affinity hash covers the rewritten `system` and `tools`, and the extended-window answer
//! sees every rewritten breakpoint at `1h`.
//!
//! Where they deliberately differ, both in bodies Anthropic rejects:
//!
//! - a body that is not UTF-8 answers nothing at all. The old peeks parsed bytes and skipped
//!   the strings they did not model without checking them, so a model could be read out of
//!   such a body; JSON text is UTF-8 (RFC 8259 §8.1) and the rewrite already refused it.
//! - a top-level JSON array answers nothing. A `#[derive]` struct also accepts its fields as
//!   an array, in declaration order, so `["claude-x"]` used to name a model.
//!
//! Inside `messages`, a block or message in that array form is likewise read as no block.

use std::collections::VecDeque;
use std::ops::Range;

use bytes::Bytes;
use serde::Deserializer as _;
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use serde_json::value::RawValue;

use crate::cache_ttl::{Splice, extend_cache_control, offset_in};
use crate::session_wire::{
    BOUND_TOKEN_SCAN_CAP, BoundHistory, TAIL_SCAN_CAP, ToolResultEvent, ToolUseEvent,
    content_mentions_timeout, extract_session_id, tool_use_event_from_block,
};

/// A request body and every answer the proxy reads from it, built once by
/// [`RequestBody::parse`].
#[derive(Debug, Clone)]
pub struct RequestBody {
    /// The bytes to forward: the client's own, or the client's with every breakpoint's
    /// `ttl` set to `1h`.
    bytes: Bytes,
    /// The client's length, when the rewrite changed the body.
    rewritten_from: Option<usize>,
    model: Option<String>,
    /// `metadata.user_id`, as the affinity key's tier 2 reads it.
    affinity_user_id: Option<String>,
    /// Where `system` and `tools` are in [`Self::bytes`], for the affinity key's tier 3.
    /// `None` when that peek failed; each inner `None` is an absent member.
    affinity_prefix: Option<PrefixSpans>,
    wire: WireFacts,
    extended_ttl: bool,
    /// Where `metadata.user_id`'s `account_uuid` value starts in [`Self::bytes`].
    account_uuid_at: Option<usize>,
}

/// Where `system` and `tools` sit in the forwarded body; `None` for an absent member.
type PrefixSpans = (Option<Range<usize>>, Option<Range<usize>>);

/// What the session peek reads: the wire session, the newest turn's tool events, and the
/// account-bound state the history carries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WireFacts {
    /// The `session_id` embedded in `metadata.user_id`.
    pub session_id: Option<String>,
    pub tool_uses: Vec<ToolUseEvent>,
    pub tool_results: Vec<ToolResultEvent>,
    /// The bound tokens in `messages`, before any `previous_message_id` is folded in.
    pub bound: BoundHistory,
    /// Each server-side thread id the body names, with the path it was found at:
    /// `previous_message_id`, then `thread.previous_message_id`.
    pub previous_message_ids: Vec<(&'static str, String)>,
}

impl RequestBody {
    /// Walk `bytes` once. With `rewrite_ttl`, every cache breakpoint in a legal position that
    /// does not already ask for the 1-hour window is set to it, exactly as
    /// [`crate::cache_ttl::extend_all_ttls`] does, and every answer is the one the rewritten
    /// body gives. A body that is not JSON answers nothing and is forwarded untouched.
    pub fn parse(bytes: Bytes, rewrite_ttl: bool) -> Self {
        let scanned = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|text| Scan::run(text).map(|scan| scan.finish(text, rewrite_ttl)));
        let Some(found) = scanned else {
            return Self::unparsed(bytes);
        };
        let rewritten_from = found.rewritten.as_ref().map(|_| bytes.len());
        Self {
            bytes: found.rewritten.map_or(bytes, Bytes::from),
            rewritten_from,
            model: found.model,
            affinity_user_id: found.affinity_user_id,
            affinity_prefix: found.affinity_prefix,
            wire: found.wire,
            extended_ttl: found.extended_ttl,
            account_uuid_at: found.account_uuid_at,
        }
    }

    fn unparsed(bytes: Bytes) -> Self {
        Self {
            bytes,
            rewritten_from: None,
            model: None,
            affinity_user_id: None,
            affinity_prefix: None,
            wire: WireFacts::default(),
            extended_ttl: false,
            account_uuid_at: None,
        }
    }

    /// The body to forward, before any per-account patch. A refcount clone, never a copy.
    pub const fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// The client's body length when the rewrite changed it, else `None`.
    pub const fn rewritten_from(&self) -> Option<usize> {
        self.rewritten_from
    }

    /// The top-level `model`.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// `metadata.user_id`, the affinity key's tier 2.
    pub fn affinity_user_id(&self) -> Option<&str> {
        self.affinity_user_id.as_deref()
    }

    /// The exact text of `system` and `tools` in the forwarded body, the affinity key's
    /// tier 3 input, or `None` when the body gave no answer for that tier.
    pub fn affinity_prefix(&self) -> Option<(Option<&str>, Option<&str>)> {
        let (system, tools) = self.affinity_prefix.as_ref()?;
        let text = |range: &Option<Range<usize>>| {
            range
                .as_ref()
                .and_then(|r| self.bytes.get(r.clone()))
                .and_then(|b| std::str::from_utf8(b).ok())
        };
        Some((text(system), text(tools)))
    }

    /// What the session peek reads.
    pub const fn wire(&self) -> &WireFacts {
        &self.wire
    }

    /// Whether a breakpoint in a legal position asks for the 1-hour window.
    pub const fn extended_ttl(&self) -> bool {
        self.extended_ttl
    }

    /// The body to send on behalf of the account whose uuid is `account_uuid`: the
    /// forwarded body with `metadata.user_id`'s `account_uuid` replaced, same length, or the
    /// same `Bytes` when there is nothing to replace. See [`crate::account_uuid`].
    pub fn for_account(&self, account_uuid: &str) -> Bytes {
        match crate::account_uuid::patch_at(&self.bytes, account_uuid, self.account_uuid_at) {
            std::borrow::Cow::Owned(patched) => Bytes::from(patched),
            std::borrow::Cow::Borrowed(_) => self.bytes.clone(),
        }
    }
}

/// The top-level members read, by index into [`TOP`].
const TOP: [&str; 7] = [
    "model",
    "metadata",
    "previous_message_id",
    "thread",
    "system",
    "tools",
    "messages",
];
const MODEL: usize = 0;
const METADATA: usize = 1;
const PREVIOUS: usize = 2;
const THREAD: usize = 3;
const SYSTEM: usize = 4;
const TOOLS: usize = 5;
const MESSAGES: usize = 6;

/// A content block's members read, by index into [`BLOCK`].
const BLOCK: [&str; 10] = [
    "cache_control",
    "type",
    "id",
    "signature",
    "data",
    "name",
    "input",
    "tool_use_id",
    "is_error",
    "content",
];
const B_CACHE: usize = 0;
const B_TYPE: usize = 1;
const B_ID: usize = 2;
const B_SIGNATURE: usize = 3;
const B_DATA: usize = 4;
const B_NAME: usize = 5;
const B_INPUT: usize = 6;
const B_TOOL_USE_ID: usize = 7;
const B_IS_ERROR: usize = 8;
const B_CONTENT: usize = 9;

/// A message's members read.
const MESSAGE: [&str; 2] = ["role", "content"];

/// What the extended-window answer has seen, kept so it equals
/// [`crate::cache_ttl::requests_extended_ttl`] on the body as forwarded.
#[derive(Debug, Clone, Copy)]
struct Ttl {
    /// Every legal position had the shape that peek accepts: `system` and a message's
    /// `content` null, a string or a list of objects, `messages` and `tools` null or a list
    /// of objects, each `cache_control` null or an object, and no member it reads twice.
    shape_ok: bool,
    /// A `ttl` in an object `cache_control` that is neither a string nor null. The rewrite
    /// replaces it, so it costs the answer only on a body that is not rewritten.
    ttl_value_bad: bool,
    /// A `cache_control` whose own `ttl` unescapes to `1h`.
    any_1h: bool,
    /// Any object `cache_control`: after the rewrite, each one says `1h`.
    any_object: bool,
}

impl Default for Ttl {
    fn default() -> Self {
        Self {
            shape_ok: true,
            ttl_value_bad: false,
            any_1h: false,
            any_object: false,
        }
    }
}

/// Everything the walk keeps, borrowed from the body.
#[derive(Default)]
struct Scan<'de> {
    /// The whole body, which every span below borrows from.
    body: &'de str,
    counts: [u8; 7],
    model: Option<&'de RawValue>,
    metadata: Option<&'de RawValue>,
    previous: Option<&'de RawValue>,
    thread: Option<&'de RawValue>,
    system: Option<&'de RawValue>,
    tools: Option<&'de RawValue>,
    messages: Messages<'de>,
    ttl: Ttl,
    /// The rewrite's splices for `system`, `tools` and `messages`, each from the member's
    /// last occurrence, as a parsed map would hold it.
    splices: [Vec<Splice>; 3],
    /// A `cache_control` key spelled without escapes was seen in a walked position, so the
    /// literal the rewrite's pre-check looks for is in the body.
    literal_key: bool,
    /// A breakpoint's span did not point into the body; the rewrite is then abandoned, as
    /// [`crate::cache_ttl::extend_all_ttls`] abandons it.
    rewrite_failed: bool,
}

/// The walk of one occurrence of `messages`.
#[derive(Default)]
struct Messages<'de> {
    /// `messages` is an array (the bound tokens and tool events read nothing otherwise).
    is_array: bool,
    bound: BoundHistory,
    /// The newest [`TAIL_SCAN_CAP`] messages, oldest first.
    tail: VecDeque<MessageSpans<'de>>,
}

/// One message as the tool-event tail reads it.
struct MessageSpans<'de> {
    is_object: bool,
    /// `role` appeared twice, which the tool-event peek rejected.
    role_twice: bool,
    /// `content` appeared twice, which every message peek rejected.
    content_twice: bool,
    role: Option<&'de RawValue>,
    content: Content<'de>,
}

/// A message's `content`, or a list's value in general.
enum Content<'de> {
    Absent,
    Text,
    Blocks(Vec<BlockSpans<'de>>),
    Other,
}

/// One content block's members, as borrowed spans; the last occurrence of each.
struct BlockSpans<'de> {
    is_object: bool,
    fields: [Option<&'de RawValue>; 10],
    /// The bound-token peek accepts this block: an object whose `type`, `id`, `signature`
    /// and `data` each appear once at most, `type` a string, the other three a string or null.
    bound_ok: bool,
}

/// The answers, owned, once the walk is done.
struct Found {
    rewritten: Option<Vec<u8>>,
    model: Option<String>,
    affinity_user_id: Option<String>,
    affinity_prefix: Option<PrefixSpans>,
    wire: WireFacts,
    extended_ttl: bool,
    account_uuid_at: Option<usize>,
}

impl<'de> Scan<'de> {
    /// The walk itself. `None` when the body is not one JSON object.
    fn run(text: &'de str) -> Option<Self> {
        let mut scan = Scan {
            body: text,
            ..Scan::default()
        };
        let mut de = serde_json::Deserializer::from_str(text);
        de.deserialize_map(Root(&mut scan)).ok()?;
        de.end().ok()?;
        Some(scan)
    }

    fn duplicated(&self, members: &[usize]) -> bool {
        members.iter().any(|&i| self.counts[i] > 1)
    }

    fn finish(mut self, text: &'de str, rewrite_ttl: bool) -> Found {
        // The rewrite, exactly as `extend_all_ttls` makes it.
        let mut splices: Vec<Splice> = Vec::new();
        if rewrite_ttl && !self.rewrite_failed {
            for member in &self.splices {
                splices.extend(member.iter().map(|s| Splice {
                    start: s.start,
                    end: s.end,
                    text: s.text,
                }));
            }
            let literal = self.literal_key
                || text
                    .as_bytes()
                    .windows(b"\"cache_control\"".len())
                    .any(|w| w == b"\"cache_control\"");
            if !literal {
                splices.clear();
            }
        }
        splices.sort_unstable_by_key(|s| s.start);
        let rewritten = (!splices.is_empty()).then(|| apply(text.as_bytes(), &splices));
        let rewritten = rewritten.flatten();
        if rewritten.is_none() {
            splices.clear();
        }
        let moved = |offset: usize| moved_by(&splices, offset);
        let span = |raw: &RawValue| {
            offset_in(text, raw.get()).map(|start| moved(start)..moved(start + raw.get().len()))
        };

        let model = (!self.duplicated(&[MODEL]))
            .then(|| parse_opt::<String>(self.model))
            .flatten()
            .flatten();

        // `metadata` as the session peek and the affinity key read it.
        #[derive(serde::Deserialize)]
        struct UserIdMeta {
            user_id: Option<String>,
        }
        let meta: Option<Option<UserIdMeta>> = parse_opt(self.metadata);
        let user_id = || {
            meta.as_ref()
                .and_then(|m| m.as_ref())
                .and_then(|m| m.user_id.clone())
        };

        // The affinity key's peek: `metadata`, `system`, `tools`.
        let affinity_ok = meta.is_some() && !self.duplicated(&[METADATA, SYSTEM, TOOLS]);
        let affinity_user_id = affinity_ok.then(user_id).flatten();
        let affinity_prefix =
            affinity_ok.then(|| (self.system.and_then(span), self.tools.and_then(span)));

        // The session peek: `metadata`, `messages`, `previous_message_id`, `thread`.
        #[derive(serde::Deserialize)]
        struct PreviousMessagePeek {
            previous_message_id: Option<String>,
        }
        let previous: Option<Option<String>> = parse_opt(self.previous);
        let thread: Option<Option<PreviousMessagePeek>> = parse_opt(self.thread);
        let session_ok = meta.is_some()
            && previous.is_some()
            && thread.is_some()
            && !self.duplicated(&[METADATA, MESSAGES, PREVIOUS, THREAD]);
        let wire = if session_ok {
            let (tool_uses, tool_results) = if self.messages.is_array {
                tool_events(&self.messages.tail)
            } else {
                (Vec::new(), Vec::new())
            };
            let mut previous_message_ids = Vec::new();
            if let Some(id) = previous.flatten() {
                previous_message_ids.push(("previous_message_id", id));
            }
            if let Some(id) = thread.flatten().and_then(|t| t.previous_message_id) {
                previous_message_ids.push(("thread.previous_message_id", id));
            }
            WireFacts {
                session_id: user_id().and_then(|blob| extract_session_id(&blob)),
                tool_uses,
                tool_results,
                bound: std::mem::take(&mut self.messages.bound),
                previous_message_ids,
            }
        } else {
            WireFacts::default()
        };

        // The extended-window answer, on the body as forwarded.
        let ttl_ok = self.ttl.shape_ok && !self.duplicated(&[SYSTEM, TOOLS, MESSAGES]);
        let extended_ttl = if rewritten.is_some() {
            ttl_ok && self.ttl.any_object
        } else {
            ttl_ok && !self.ttl.ttl_value_bad && self.ttl.any_1h
        };

        // The account-uuid splice point: `metadata.user_id`, any JSON value.
        #[derive(serde::Deserialize)]
        struct MetaPeek<'a> {
            #[serde(borrow)]
            user_id: Option<&'a RawValue>,
        }
        let account_uuid_at = (!self.duplicated(&[METADATA]))
            .then(|| match self.metadata {
                None => None,
                Some(raw) => serde_json::from_str::<Option<MetaPeek>>(raw.get())
                    .ok()
                    .flatten()
                    .and_then(|m| m.user_id),
            })
            .flatten()
            .and_then(|user_id| {
                let at = offset_in(text, user_id.get())?;
                let within = crate::account_uuid::value_start_in(user_id.get())?;
                Some(moved(at + within))
            });

        Found {
            rewritten,
            model,
            affinity_user_id,
            affinity_prefix,
            wire,
            extended_ttl,
            account_uuid_at,
        }
    }
}

/// Parse an optional member as `Option<T>`: `Some(None)` when absent or null, `None` when
/// it does not parse as `T`.
fn parse_opt<T: serde::de::DeserializeOwned>(raw: Option<&RawValue>) -> Option<Option<T>> {
    match raw {
        None => Some(None),
        Some(raw) => serde_json::from_str::<Option<T>>(raw.get()).ok(),
    }
}

/// The body with every splice applied, as `extend_all_ttls` writes it.
fn apply(body: &[u8], splices: &[Splice]) -> Option<Vec<u8>> {
    let added: usize = splices.iter().map(|s| s.text.len()).sum();
    let mut out = Vec::with_capacity(body.len() + added);
    let mut copied = 0usize;
    for splice in splices {
        out.extend_from_slice(body.get(copied..splice.start)?);
        out.extend_from_slice(splice.text.as_bytes());
        copied = splice.end;
    }
    out.extend_from_slice(body.get(copied..)?);
    Some(out)
}

/// Where `offset` in the client's body lands in the rewritten one. Every splice sits inside
/// a `cache_control` object, so none starts at a member's first byte or just past its last.
fn moved_by(splices: &[Splice], offset: usize) -> usize {
    let mut at = offset;
    for s in splices.iter().take_while(|s| s.start < offset) {
        at = at + s.text.len() - (s.end - s.start);
    }
    at
}

/// The tool events of the newest turn, as [`crate::session_wire::extract_tool_events`] reads
/// them from the last [`TAIL_SCAN_CAP`] messages.
fn tool_events(tail: &VecDeque<MessageSpans<'_>>) -> (Vec<ToolUseEvent>, Vec<ToolResultEvent>) {
    let mut window: Vec<(String, &MessageSpans<'_>)> = Vec::new();
    for msg in tail.iter().rev() {
        if !msg.is_object || msg.role_twice || msg.content_twice {
            continue;
        }
        let Some(role) = parse_opt::<String>(msg.role).map(Option::unwrap_or_default) else {
            continue;
        };
        // `role: null` is not a string: the old peek rejected the message.
        if msg.role.is_some_and(|r| r.get() == "null") {
            continue;
        }
        let is_assistant = role == "assistant";
        window.push((role, msg));
        if is_assistant {
            break;
        }
    }
    window.reverse();

    let mut tool_uses = Vec::new();
    let mut tool_results = Vec::new();
    for (role, msg) in window {
        let Content::Blocks(blocks) = &msg.content else {
            continue;
        };
        for block in blocks.iter().filter(|b| b.is_object) {
            let Some(b) = BlockPeek::read(block) else {
                continue;
            };
            match (role.as_str(), b.kind.as_str()) {
                ("assistant", "tool_use") => {
                    if let Some(id) = b.id {
                        let input = b.input.and_then(value_of);
                        tool_uses.push(tool_use_event_from_block(id, b.name, input.as_ref()));
                    }
                }
                ("user", "tool_result") => {
                    if let Some(id) = b.tool_use_id {
                        let is_error = b.is_error.unwrap_or(false);
                        let timed_out =
                            is_error && content_mentions_timeout(&b.content.and_then(value_of));
                        tool_results.push(ToolResultEvent {
                            id,
                            is_error,
                            timed_out,
                        });
                    }
                }
                _ => {}
            }
        }
    }
    (tool_uses, tool_results)
}

/// A JSON value, `None` for null, as an `Option<Value>` field reads it.
fn value_of(raw: &RawValue) -> Option<Value> {
    serde_json::from_str::<Option<Value>>(raw.get())
        .ok()
        .flatten()
}

/// One block as the tool-event peek reads it: `None` when that peek rejected the block.
struct BlockPeek<'de> {
    kind: String,
    id: Option<String>,
    name: Option<String>,
    input: Option<&'de RawValue>,
    tool_use_id: Option<String>,
    is_error: Option<bool>,
    content: Option<&'de RawValue>,
}

impl<'de> BlockPeek<'de> {
    fn read(block: &BlockSpans<'de>) -> Option<Self> {
        let f = &block.fields;
        let kind = match f[B_TYPE] {
            None => String::new(),
            Some(raw) => serde_json::from_str::<String>(raw.get()).ok()?,
        };
        Some(Self {
            kind,
            id: parse_opt(f[B_ID])?,
            name: parse_opt(f[B_NAME])?,
            input: f[B_INPUT],
            tool_use_id: parse_opt(f[B_TOOL_USE_ID])?,
            is_error: parse_opt(f[B_IS_ERROR])?,
            content: f[B_CONTENT],
        })
    }
}

/// A JSON string's text without allocating when it has no escapes; `None` when `raw` is not
/// a string.
fn string_of(raw: &RawValue) -> Option<std::borrow::Cow<'_, str>> {
    let text = raw.get();
    let inner = text.strip_prefix('"')?.strip_suffix('"')?;
    if inner.contains('\\') {
        serde_json::from_str::<String>(text)
            .ok()
            .map(std::borrow::Cow::Owned)
    } else {
        Some(std::borrow::Cow::Borrowed(inner))
    }
}

fn is_null(raw: &RawValue) -> bool {
    raw.get() == "null"
}

/// The index of an object key among `names`, and whether it was spelled without escapes.
struct Key<'n>(&'n [&'static str]);

struct KeyHit {
    index: Option<usize>,
    literal: bool,
}

impl<'de> DeserializeSeed<'de> for Key<'_> {
    type Value = KeyHit;

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<KeyHit, D::Error> {
        d.deserialize_str(self)
    }
}

impl<'de> Visitor<'de> for Key<'_> {
    type Value = KeyHit;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an object key")
    }

    fn visit_borrowed_str<E: serde::de::Error>(self, key: &'de str) -> Result<KeyHit, E> {
        Ok(KeyHit {
            index: self.0.iter().position(|n| *n == key),
            literal: true,
        })
    }

    fn visit_str<E: serde::de::Error>(self, key: &str) -> Result<KeyHit, E> {
        Ok(KeyHit {
            index: self.0.iter().position(|n| *n == key),
            literal: false,
        })
    }
}

/// The top-level object.
struct Root<'s, 'de>(&'s mut Scan<'de>);

impl<'de> Visitor<'de> for Root<'_, 'de> {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let scan = self.0;
        while let Some(hit) = map.next_key_seed(Key(&TOP))? {
            let Some(i) = hit.index else {
                map.next_value::<IgnoredAny>()?;
                continue;
            };
            scan.counts[i] = scan.counts[i].saturating_add(1);
            match i {
                MESSAGES => {
                    scan.messages = Messages::default();
                    let mut splices = Vec::new();
                    map.next_value_seed(MessagesSeed {
                        scan: &mut *scan,
                        splices: &mut splices,
                    })?;
                    scan.splices[2] = splices;
                }
                SYSTEM | TOOLS => {
                    let raw: &'de RawValue = map.next_value()?;
                    let kind = if i == SYSTEM {
                        ListKind::System
                    } else {
                        ListKind::Tools
                    };
                    let mut splices = Vec::new();
                    let mut sub = serde_json::Deserializer::from_str(raw.get());
                    sub.deserialize_any(BlockList {
                        scan: &mut *scan,
                        splices: &mut splices,
                        kind,
                        keep: None,
                    })
                    .map_err(<A::Error as serde::de::Error>::custom)?;
                    scan.splices[i - SYSTEM] = splices;
                    if i == SYSTEM {
                        scan.system = Some(raw);
                    } else {
                        scan.tools = Some(raw);
                    }
                }
                _ => {
                    let raw: &'de RawValue = map.next_value()?;
                    let slot = match i {
                        MODEL => &mut scan.model,
                        METADATA => &mut scan.metadata,
                        PREVIOUS => &mut scan.previous,
                        _ => &mut scan.thread,
                    };
                    *slot = Some(raw);
                }
            }
        }
        Ok(())
    }
}

/// Every scalar, and an array where an object was wanted, as one call.
macro_rules! other_values {
    ($on:ident) => {
        fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok(self.$on())
        }
        fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
            Ok(self.$on())
        }
        fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
            Ok(self.$on())
        }
        fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok(self.$on())
        }
    };
}

/// `messages`: a list of messages.
struct MessagesSeed<'s, 'de> {
    scan: &'s mut Scan<'de>,
    splices: &'s mut Vec<Splice>,
}

impl<'de> DeserializeSeed<'de> for MessagesSeed<'_, 'de> {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_any(self)
    }
}

impl MessagesSeed<'_, '_> {
    /// A `messages` that is neither null nor a list.
    const fn not_a_list(self) {
        self.scan.ttl.shape_ok = false;
    }
}

impl<'de> Visitor<'de> for MessagesSeed<'_, 'de> {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("any JSON value")
    }

    other_values!(not_a_list);

    fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<(), E> {
        self.not_a_list();
        Ok(())
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        self.not_a_list();
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        let MessagesSeed { scan, splices } = self;
        scan.messages.is_array = true;
        loop {
            let mut own = Vec::new();
            let Some(message) = seq.next_element_seed(MessageSeed {
                scan: &mut *scan,
                splices: &mut own,
            })?
            else {
                break;
            };
            splices.append(&mut own);
            collect_bound(&mut scan.messages.bound, &message);
            let tail = &mut scan.messages.tail;
            if tail.len() == TAIL_SCAN_CAP {
                tail.pop_front();
            }
            tail.push_back(message);
        }
        Ok(())
    }
}

/// Fold one message's bound tokens in, as
/// [`crate::session_wire::extract_bound_tokens`] reads them: a message is read only when it
/// is an object with one `content`, a list of blocks the peek accepts, else skipped whole.
fn collect_bound(found: &mut BoundHistory, message: &MessageSpans<'_>) {
    if !message.is_object || message.content_twice {
        return;
    }
    let Content::Blocks(blocks) = &message.content else {
        return;
    };
    if !blocks.iter().all(|b| b.is_object && b.bound_ok) {
        return;
    }
    for block in blocks {
        let f = &block.fields;
        let token_field = match f[B_TYPE].and_then(string_of).as_deref() {
            Some("server_tool_use") => f[B_ID],
            Some("thinking") => f[B_SIGNATURE],
            Some("redacted_thinking") => f[B_DATA],
            _ => continue,
        };
        found.carries_bound_history = true;
        let Some(token) = token_field.and_then(string_of) else {
            continue;
        };
        if found.tokens.len() >= BOUND_TOKEN_SCAN_CAP {
            continue;
        }
        if !found.tokens.iter().any(|t| *t == token) {
            found.tokens.push(token.into_owned());
        }
    }
}

/// One message.
struct MessageSeed<'s, 'de> {
    scan: &'s mut Scan<'de>,
    splices: &'s mut Vec<Splice>,
}

impl<'de> DeserializeSeed<'de> for MessageSeed<'_, 'de> {
    type Value = MessageSpans<'de>;

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> MessageSeed<'_, 'de> {
    /// A message that is not an object.
    const fn not_an_object(self) -> MessageSpans<'de> {
        self.scan.ttl.shape_ok = false;
        MessageSpans {
            is_object: false,
            role_twice: false,
            content_twice: false,
            role: None,
            content: Content::Absent,
        }
    }
}

impl<'de> Visitor<'de> for MessageSeed<'_, 'de> {
    type Value = MessageSpans<'de>;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("any JSON value")
    }

    other_values!(not_an_object);

    fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(self.not_an_object())
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        Ok(self.not_an_object())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(self.not_an_object())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let MessageSeed { scan, splices } = self;
        let mut counts = [0u8; 2];
        let mut role = None;
        let mut content = Content::Absent;
        while let Some(hit) = map.next_key_seed(Key(&MESSAGE))? {
            match hit.index {
                None => {
                    map.next_value::<IgnoredAny>()?;
                }
                Some(0) => {
                    counts[0] = counts[0].saturating_add(1);
                    role = Some(map.next_value::<&'de RawValue>()?);
                }
                Some(_) => {
                    counts[1] = counts[1].saturating_add(1);
                    // The rewrite takes the last `content` only.
                    splices.clear();
                    let mut blocks = Vec::new();
                    let kind = map.next_value_seed(BlockList {
                        scan: &mut *scan,
                        splices: &mut *splices,
                        kind: ListKind::Content,
                        keep: Some(&mut blocks),
                    })?;
                    content = match kind {
                        // `content: null` reads as no content everywhere.
                        ListShape::Null => Content::Absent,
                        ListShape::Text => Content::Text,
                        ListShape::List => Content::Blocks(blocks),
                        ListShape::Other => Content::Other,
                    };
                }
            }
        }
        if counts[1] > 1 {
            scan.ttl.shape_ok = false;
        }
        Ok(MessageSpans {
            is_object: true,
            role_twice: counts[0] > 1,
            content_twice: counts[1] > 1,
            role,
            content,
        })
    }
}

/// Which list a [`BlockList`] walks; they differ only in what else they accept.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ListKind {
    /// `system`: a string or a list of blocks.
    System,
    /// `tools`: a list of tool definitions.
    Tools,
    /// A message's `content`: a string or a list of blocks.
    Content,
}

/// What a [`BlockList`] value turned out to be.
enum ListShape {
    Null,
    Text,
    List,
    Other,
}

/// `system`, `tools` or a message's `content`: when a list, each object in it is a block
/// whose `cache_control` is a breakpoint.
struct BlockList<'s, 'de> {
    scan: &'s mut Scan<'de>,
    splices: &'s mut Vec<Splice>,
    kind: ListKind,
    /// Where a message's blocks are kept for the bound tokens and the tool events.
    keep: Option<&'s mut Vec<BlockSpans<'de>>>,
}

impl<'de> DeserializeSeed<'de> for BlockList<'_, 'de> {
    type Value = ListShape;

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<ListShape, D::Error> {
        d.deserialize_any(self)
    }
}

impl BlockList<'_, '_> {
    const fn other(self) -> ListShape {
        self.scan.ttl.shape_ok = false;
        ListShape::Other
    }
}

impl<'de> Visitor<'de> for BlockList<'_, 'de> {
    type Value = ListShape;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("any JSON value")
    }

    other_values!(other);

    fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<ListShape, E> {
        if self.kind == ListKind::Tools {
            return Ok(self.other());
        }
        Ok(ListShape::Text)
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<ListShape, E> {
        Ok(ListShape::Null)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<ListShape, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(self.other())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<ListShape, A::Error> {
        let BlockList {
            scan,
            splices,
            mut keep,
            ..
        } = self;
        while let Some(block) = seq.next_element_seed(BlockSeed {
            scan: &mut *scan,
            splices: &mut *splices,
        })? {
            if let Some(keep) = keep.as_deref_mut() {
                keep.push(block);
            }
        }
        Ok(ListShape::List)
    }
}

/// One block (or tool definition).
struct BlockSeed<'s, 'de> {
    scan: &'s mut Scan<'de>,
    splices: &'s mut Vec<Splice>,
}

impl<'de> DeserializeSeed<'de> for BlockSeed<'_, 'de> {
    type Value = BlockSpans<'de>;

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> BlockSeed<'_, 'de> {
    const fn not_an_object(self) -> BlockSpans<'de> {
        self.scan.ttl.shape_ok = false;
        BlockSpans {
            is_object: false,
            fields: [None; 10],
            bound_ok: false,
        }
    }
}

impl<'de> Visitor<'de> for BlockSeed<'_, 'de> {
    type Value = BlockSpans<'de>;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("any JSON value")
    }

    other_values!(not_an_object);

    fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(self.not_an_object())
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        Ok(self.not_an_object())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(self.not_an_object())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let BlockSeed { scan, splices } = self;
        let mut fields: [Option<&'de RawValue>; 10] = [None; 10];
        let mut counts = [0u8; 10];
        while let Some(hit) = map.next_key_seed(Key(&BLOCK))? {
            let Some(i) = hit.index else {
                map.next_value::<IgnoredAny>()?;
                continue;
            };
            counts[i] = counts[i].saturating_add(1);
            fields[i] = Some(map.next_value::<&'de RawValue>()?);
            if i == B_CACHE && hit.literal {
                scan.literal_key = true;
            }
        }
        if counts[B_CACHE] > 1 {
            scan.ttl.shape_ok = false;
        }
        if let Some(cache_control) = fields[B_CACHE] {
            breakpoint(scan, splices, cache_control);
        }
        let string_or_null = |raw: Option<&RawValue>| {
            raw.is_none_or(|raw| is_null(raw) || raw.get().starts_with('"'))
        };
        let bound_ok = counts[B_TYPE..=B_DATA].iter().all(|&c| c <= 1)
            && fields[B_TYPE].is_none_or(|raw| raw.get().starts_with('"'))
            && string_or_null(fields[B_ID])
            && string_or_null(fields[B_SIGNATURE])
            && string_or_null(fields[B_DATA]);
        Ok(BlockSpans {
            is_object: true,
            fields,
            bound_ok,
        })
    }
}

/// One `cache_control`: what the extended-window answer learns from it, and the rewrite's
/// splice for it.
fn breakpoint<'de>(scan: &mut Scan<'de>, splices: &mut Vec<Splice>, cache_control: &'de RawValue) {
    let text = cache_control.get();
    if is_null(cache_control) {
        return;
    }
    if !text.starts_with('{') {
        scan.ttl.shape_ok = false;
        return;
    }
    scan.ttl.any_object = true;
    match ttl_member(text) {
        Some((count, ttl)) => {
            if count > 1 {
                scan.ttl.shape_ok = false;
            }
            match ttl {
                Some(ttl) if ttl.get().starts_with('"') => {
                    if string_of(ttl).as_deref() == Some(crate::cache_ttl::EXTENDED_TTL) {
                        scan.ttl.any_1h = true;
                    }
                }
                Some(ttl) if is_null(ttl) => {}
                Some(_) => scan.ttl.ttl_value_bad = true,
                None => {}
            }
        }
        None => scan.ttl.shape_ok = false,
    }
    // Offsets are measured from the whole body: `cache_control` borrows from it.
    if extend_cache_control(scan.body, cache_control, splices).is_none() {
        scan.rewrite_failed = true;
    }
}

/// How many times a `cache_control` object names `ttl`, and its last value.
fn ttl_member(object: &str) -> Option<(u8, Option<&RawValue>)> {
    struct TtlVisitor;
    impl<'de> Visitor<'de> for TtlVisitor {
        type Value = (u8, Option<&'de RawValue>);
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a JSON object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut count = 0u8;
            let mut ttl = None;
            while let Some(hit) = map.next_key_seed(Key(&["ttl"]))? {
                if hit.index.is_some() {
                    count = count.saturating_add(1);
                    ttl = Some(map.next_value::<&'de RawValue>()?);
                } else {
                    map.next_value::<IgnoredAny>()?;
                }
            }
            Ok((count, ttl))
        }
    }
    serde_json::Deserializer::from_str(object)
        .deserialize_map(TtlVisitor)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A uuid of the right shape that no body below carries.
    const SERVING: &str = "99999999-9999-9999-9999-999999999999";

    /// Every answer the handler reads from a body, in one comparable value.
    #[derive(Debug, PartialEq)]
    struct Answers {
        forwarded: Vec<u8>,
        model: Option<String>,
        affinity_user_id: Option<String>,
        affinity_prefix: Option<(Option<String>, Option<String>)>,
        wire: WireFacts,
        extended_ttl: bool,
        for_account: Vec<u8>,
    }

    /// The answers as the handler took them before the one parse: each consumer parsing
    /// the (possibly rewritten) bytes on its own. The session and affinity peeks are the
    /// handler's own code, copied; the rest are the functions it called.
    fn separate_parses(body: &[u8], rewrite: bool) -> Answers {
        let rewritten = if rewrite {
            crate::cache_ttl::extend_all_ttls(body)
        } else {
            None
        };
        let b: &[u8] = rewritten.as_deref().unwrap_or(body);

        #[derive(serde::Deserialize)]
        struct RequestSessionPeek<'a> {
            metadata: Option<RequestSessionMeta>,
            #[serde(borrow)]
            messages: Option<&'a RawValue>,
            previous_message_id: Option<String>,
            thread: Option<PreviousMessagePeek>,
        }
        #[derive(serde::Deserialize)]
        struct RequestSessionMeta {
            user_id: Option<String>,
        }
        #[derive(serde::Deserialize)]
        struct PreviousMessagePeek {
            previous_message_id: Option<String>,
        }
        let wire = match serde_json::from_slice::<RequestSessionPeek>(b) {
            Ok(peek) => {
                let (tool_uses, tool_results) = peek
                    .messages
                    .map(crate::session_wire::extract_tool_events)
                    .unwrap_or_default();
                let bound = peek
                    .messages
                    .map(crate::session_wire::extract_bound_tokens)
                    .unwrap_or_default();
                let mut previous_message_ids = Vec::new();
                if let Some(id) = peek.previous_message_id {
                    previous_message_ids.push(("previous_message_id", id));
                }
                if let Some(id) = peek.thread.and_then(|t| t.previous_message_id) {
                    previous_message_ids.push(("thread.previous_message_id", id));
                }
                WireFacts {
                    session_id: peek
                        .metadata
                        .and_then(|m| m.user_id)
                        .and_then(|blob| extract_session_id(&blob)),
                    tool_uses,
                    tool_results,
                    bound,
                    previous_message_ids,
                }
            }
            Err(_) => WireFacts::default(),
        };

        #[derive(serde::Deserialize)]
        struct SessionKeyPeek<'a> {
            metadata: Option<RequestSessionMeta>,
            #[serde(borrow)]
            system: Option<&'a RawValue>,
            #[serde(borrow)]
            tools: Option<&'a RawValue>,
        }
        let affinity = serde_json::from_slice::<SessionKeyPeek>(b).ok();

        Answers {
            forwarded: b.to_vec(),
            model: crate::model::parse_request_model(b),
            affinity_user_id: affinity
                .as_ref()
                .and_then(|p| p.metadata.as_ref())
                .and_then(|m| m.user_id.clone()),
            affinity_prefix: affinity.as_ref().map(|p| {
                (
                    p.system.map(|r| r.get().to_string()),
                    p.tools.map(|r| r.get().to_string()),
                )
            }),
            wire,
            extended_ttl: crate::cache_ttl::requests_extended_ttl(b),
            for_account: crate::account_uuid::patch_account_uuid(b, SERVING).into_owned(),
        }
    }

    fn one_parse(body: &[u8], rewrite: bool) -> Answers {
        let parsed = RequestBody::parse(Bytes::copy_from_slice(body), rewrite);
        Answers {
            forwarded: parsed.bytes().to_vec(),
            model: parsed.model().map(str::to_string),
            affinity_user_id: parsed.affinity_user_id().map(str::to_string),
            affinity_prefix: parsed
                .affinity_prefix()
                .map(|(s, t)| (s.map(str::to_string), t.map(str::to_string))),
            wire: parsed.wire().clone(),
            extended_ttl: parsed.extended_ttl(),
            for_account: parsed.for_account(SERVING).to_vec(),
        }
    }

    /// The one parse and the separate ones agree on `body`, with the rewrite off and on.
    fn assert_same(name: &str, body: &[u8]) {
        for rewrite in [false, true] {
            assert_eq!(
                one_parse(body, rewrite),
                separate_parses(body, rewrite),
                "{name}, rewrite {rewrite}: {}",
                String::from_utf8_lossy(&body[..body.len().min(300)])
            );
        }
    }

    /// `metadata.user_id` as Claude Code sends it: a JSON object, stringified.
    fn user_id(account_uuid: &str) -> String {
        serde_json::json!({
            "device_id": "d1",
            "account_uuid": account_uuid,
            "session_id": "33333333-3333-3333-3333-333333333333",
        })
        .to_string()
    }

    /// A Claude Code shaped conversation of `turns` tool round trips, with `metadata`
    /// placed `metadata_last` or first, so the account-uuid offset must survive the
    /// rewrite's splices before it.
    fn conversation(turns: usize, metadata_last: bool) -> Vec<u8> {
        let metadata =
            serde_json::json!({"user_id": user_id("11111111-1111-1111-1111-111111111111")});
        let mut messages = vec![serde_json::json!({"role": "user", "content": "Start."})];
        for n in 0..turns {
            let id = format!("toolu_{n:04}");
            messages.push(serde_json::json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": format!("step {n} \"quoted\"\n"), "signature": format!("sig-{n}")},
                {"type": "tool_use", "id": id, "name": "Bash", "input": {"command": format!("cd /tmp && cargo test {n}"), "description": "run"}},
            ]}));
            messages.push(serde_json::json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": id, "is_error": n % 3 == 0, "content": format!("line\\n{n} timed out")},
            ]}));
        }
        if let Some(last) = messages.last_mut() {
            last["content"][0]["cache_control"] = serde_json::json!({"type": "ephemeral"});
        }
        let system = serde_json::json!([
            {"type": "text", "text": "sys"},
            {"type": "text", "text": "sys2", "cache_control": {"type": "ephemeral"}},
        ]);
        let tools = serde_json::json!([
            {"name": "Bash", "input_schema": {"type": "object"}, "cache_control": {"type": "ephemeral", "ttl": "5m"}},
        ]);
        let body = if metadata_last {
            format!(
                r#"{{"model":"claude-opus-5-5","system":{system},"tools":{tools},"messages":{},"stream":true,"metadata":{metadata}}}"#,
                serde_json::Value::Array(messages)
            )
        } else {
            format!(
                r#"{{"model":"claude-opus-5-5","metadata":{metadata},"system":{system},"tools":{tools},"messages":{}}}"#,
                serde_json::Value::Array(messages)
            )
        };
        body.into_bytes()
    }

    /// This and the edge cases below were each watched red three ways: a thinking block's
    /// token read from `data`, the rewritten body's extended-window answer read as the
    /// client's, and every offset left in the client's coordinates.
    #[test]
    fn a_claude_code_conversation_gets_the_answers_the_separate_parses_gave() {
        for metadata_last in [false, true] {
            let body = conversation(30, metadata_last);
            assert_same("conversation", &body);
            let answers = one_parse(&body, true);
            // The fixture exercises every consumer, so agreement is not agreement on nothing.
            assert_eq!(answers.model.as_deref(), Some("claude-opus-5-5"));
            assert!(answers.extended_ttl);
            assert_ne!(answers.forwarded, body, "the rewrite ran");
            assert_ne!(
                answers.for_account, answers.forwarded,
                "the uuid was patched"
            );
            assert_eq!(answers.wire.bound.tokens.len(), BOUND_TOKEN_SCAN_CAP);
            assert_eq!(answers.wire.tool_uses.len(), 1);
            assert_eq!(answers.wire.tool_results.len(), 1);
            assert!(answers.wire.session_id.is_some());
            let (system, _) = answers.affinity_prefix.clone().expect("a prefix");
            assert!(
                system.expect("system").contains(r#""ttl":"1h""#),
                "the prefix hash reads the rewritten system"
            );
        }
    }

    /// The edge cases each consumer's own doc-comment names, and hostile shapes around
    /// them: each gets the answer its old parse gave.
    #[test]
    fn edge_cases_get_the_answers_the_separate_parses_gave() {
        let uid = user_id("11111111-1111-1111-1111-111111111111");
        let uid = serde_json::to_string(&uid).expect("a JSON string");
        let cases: Vec<(&str, String)> = vec![
            // Not JSON, empty, truncated, trailing bytes, not an object.
            ("non-json", "not json at all".into()),
            ("empty", String::new()),
            ("truncated", r#"{"model":"claude-x","messages":[{"role":"user""#.into()),
            ("trailing", r#"{"model":"claude-x"} x"#.into()),
            ("a string", r#""claude-x""#.into()),
            ("a number", "7".into()),
            // model: nested, absent, null, a number, twice.
            ("nested model", r#"{"messages":[{"role":"user","content":{"model":"claude-fable-5"}}]}"#.into()),
            ("no model", r#"{"messages":[]}"#.into()),
            ("null model", r#"{"model":null}"#.into()),
            ("number model", r#"{"model":5,"metadata":{"user_id":"u"}}"#.into()),
            ("model twice", r#"{"model":"a","model":"b","metadata":{"user_id":"u"}}"#.into()),
            // The 1h window: each legal position, the default, inside a string, nested
            // deeper, null, a string cache_control, a number ttl, a ttl twice, an escaped
            // 1h, an escaped key, a block that is not an object.
            ("1h system", r#"{"system":[{"type":"text","text":"x","cache_control":{"type":"ephemeral","ttl":"1h"}}]}"#.into()),
            ("1h tool", r#"{"tools":[{"name":"t","cache_control":{"type":"ephemeral","ttl":"1h"}}]}"#.into()),
            ("1h message", r#"{"messages":[{"role":"user","content":[{"type":"text","text":"x","cache_control":{"ttl":"1h"}}]}]}"#.into()),
            ("5m", r#"{"system":[{"type":"text","cache_control":{"type":"ephemeral","ttl":"5m"}}]}"#.into()),
            ("in a string", r#"{"messages":[{"role":"user","content":"{\"cache_control\":{\"ttl\":\"1h\"}}"}]}"#.into()),
            ("nested deeper", r#"{"messages":[{"role":"user","content":[{"type":"tool_result","content":[{"type":"text","cache_control":{"ttl":"1h"}}]}]}]}"#.into()),
            ("null cache_control", r#"{"system":[{"type":"text","cache_control":null}],"tools":[{"cache_control":{"ttl":"1h"}}]}"#.into()),
            ("string cache_control", r#"{"system":[{"type":"text","cache_control":"1h"}],"tools":[{"cache_control":{"ttl":"1h"}}]}"#.into()),
            ("number ttl", r#"{"system":[{"type":"text","cache_control":{"ttl":5}}],"tools":[{"cache_control":{"ttl":"1h"}}]}"#.into()),
            ("ttl twice", r#"{"system":[{"type":"text","cache_control":{"ttl":"5m","ttl":"1h"}}]}"#.into()),
            ("escaped 1h", r#"{"system":[{"type":"text","cache_control":{"ttl":"1h"}}]}"#.into()),
            ("escaped key", r#"{"system":[{"type":"text","cache_control":{"type":"ephemeral"}}]}"#.into()),
            ("escaped key beside a literal one", r#"{"system":[{"type":"text","cache_control":{"type":"ephemeral"}}],"x":"\"cache_control\""}"#.into()),
            ("string block", r#"{"system":["text",{"type":"text","cache_control":{"ttl":"1h"}}]}"#.into()),
            ("system an object", r#"{"system":{"cache_control":{"ttl":"1h"}},"tools":[{"cache_control":{"ttl":"1h"}}]}"#.into()),
            ("tools a string", r#"{"tools":"x","system":[{"cache_control":{"ttl":"1h"}}]}"#.into()),
            ("content a number", r#"{"messages":[{"role":"user","content":5}],"system":[{"cache_control":{"ttl":"1h"}}]}"#.into()),
            ("message a string", r#"{"messages":["hi"],"system":[{"cache_control":{"ttl":"1h"}}]}"#.into()),
            ("messages an object", r#"{"messages":{"a":1},"system":[{"cache_control":{"ttl":"1h"}}]}"#.into()),
            ("system twice", r#"{"system":[{"cache_control":{"ttl":"1h"}}],"system":"s","metadata":{"user_id":"u"}}"#.into()),
            ("cache_control twice", r#"{"system":[{"cache_control":{"ttl":"1h"},"cache_control":{}}]}"#.into()),
            ("content twice", r#"{"messages":[{"role":"user","content":"a","content":[{"cache_control":{}}]}]}"#.into()),
            // The session peek: metadata, thread state, and the shapes that empty it.
            ("previous ids", format!(r#"{{"metadata":{{"user_id":{uid}}},"previous_message_id":"msg_a","thread":{{"type":"continue","previous_message_id":"msg_b"}},"messages":[]}}"#)),
            ("thread a string", format!(r#"{{"metadata":{{"user_id":{uid}}},"thread":"x","messages":[{{"role":"assistant","content":[{{"type":"thinking","signature":"s"}}]}}]}}"#)),
            ("metadata a string", r#"{"metadata":"x","model":"m","messages":[{"role":"assistant","content":[{"type":"thinking","signature":"s"}]}]}"#.into()),
            ("metadata twice", format!(r#"{{"metadata":{{"user_id":{uid}}},"metadata":{{}},"model":"m","system":"s"}}"#)),
            ("user_id a number", r#"{"metadata":{"user_id":5},"system":"s"}"#.into()),
            ("user_id twice", format!(r#"{{"metadata":{{"user_id":{uid},"user_id":{uid}}}}}"#)),
            ("messages twice", r#"{"messages":[],"messages":[{"role":"assistant","content":[{"type":"thinking","signature":"s"}]}]}"#.into()),
            // Bound tokens: every shape, a block the peek rejects (which skips its whole
            // message), a duplicate, a missing token, the cap.
            ("bound shapes", r#"{"messages":[{"role":"assistant","content":[{"type":"server_tool_use","id":"srvtoolu_1"},{"type":"thinking","signature":"sig"},{"type":"redacted_thinking","data":"enc"},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}]}]}"#.into()),
            ("a string block skips the message", r#"{"messages":[{"role":"assistant","content":["x",{"type":"thinking","signature":"sig"}]},{"role":"assistant","content":[{"type":"thinking","signature":"kept"}]}]}"#.into()),
            ("a number id skips the message", r#"{"messages":[{"role":"assistant","content":[{"type":"thinking","signature":"sig","id":5}]}]}"#.into()),
            ("a null type skips the message", r#"{"messages":[{"role":"assistant","content":[{"type":null},{"type":"thinking","signature":"sig"}]}]}"#.into()),
            ("signature twice", r#"{"messages":[{"role":"assistant","content":[{"type":"thinking","signature":"a","signature":"b"}]}]}"#.into()),
            ("role twice keeps the bound tokens", r#"{"messages":[{"role":"user","role":"assistant","content":[{"type":"thinking","signature":"sig"}]}]}"#.into()),
            ("no signature", r#"{"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"t"}]}]}"#.into()),
            ("escaped type", r#"{"messages":[{"role":"assistant","content":[{"type":"thinking","signature":"sig"}]}]}"#.into()),
            ("repeated token", r#"{"messages":[{"role":"assistant","content":[{"type":"thinking","signature":"s"}]},{"role":"assistant","content":[{"type":"thinking","signature":"s"}]}]}"#.into()),
            // Tool events: the tail, the shapes it skips.
            ("trailing user messages", r#"{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/a"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":[{"type":"text","text":"Timed out"}]}]},{"role":"user","content":"reminder"}]}"#.into()),
            ("role null", r#"{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"t0","name":"Bash","input":{"command":"ls"}}]},{"role":null,"content":[{"type":"tool_result","tool_use_id":"t0"}]}]}"#.into()),
            ("no role", r#"{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"t0","name":"Bash","input":null}]},{"content":[{"type":"tool_result","tool_use_id":"t0"}]}]}"#.into()),
            ("role twice skips the tail message", r#"{"messages":[{"role":"assistant","role":"assistant","content":[{"type":"tool_use","id":"t0","name":"Bash"}]}]}"#.into()),
            ("a number is_error skips the block", r#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t0","is_error":1},{"type":"tool_result","tool_use_id":"t1"}]}]}"#.into()),
            ("a number name skips the block", r#"{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"t0","name":5},{"type":"tool_use","id":"t1","name":"Grep","input":{"pattern":"x"}}]}]}"#.into()),
            ("id twice in a tool block", r#"{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"t0","id":"t9","name":"Bash"}]}]}"#.into()),
            ("a block that is an array", r#"{"messages":[{"role":"assistant","content":[["x"],{"type":"tool_use","id":"t1","name":"Bash"}]}]}"#.into()),
            // The account uuid: equal, absent, not a string.
            ("uuid already the serving one", format!(r#"{{"metadata":{{"user_id":{}}}}}"#, serde_json::to_string(&user_id(SERVING)).expect("json"))),
            ("user_id an object", r#"{"metadata":{"user_id":{"account_uuid":"11111111-1111-1111-1111-111111111111"}}}"#.into()),
            ("user_id null", r#"{"metadata":{"user_id":null},"system":"s"}"#.into()),
        ];
        for (name, body) in &cases {
            assert_same(name, body.as_bytes());
        }
        // Past the tail cap, with no assistant message in reach.
        let mut long = String::from(
            r#"{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"far","name":"Bash"}]}"#,
        );
        for _ in 0..TAIL_SCAN_CAP + 2 {
            long.push_str(
                r#",{"role":"user","content":[{"type":"tool_result","tool_use_id":"far"}]}"#,
            );
        }
        long.push_str("]}");
        assert_same("past the tail cap", long.as_bytes());
    }

    /// The two shapes where the one parse answers differently, both bodies Anthropic
    /// rejects: see the module doc.
    #[test]
    fn a_top_level_array_and_a_non_utf8_body_answer_nothing() {
        let array = br#"["claude-x"]"#;
        assert_eq!(
            crate::model::parse_request_model(array).as_deref(),
            Some("claude-x"),
            "control: the old peek read an array as the struct's fields"
        );
        assert_eq!(one_parse(array, true).model, None);

        let mut not_utf8 = br#"{"model":"claude-x","messages":[{"role":"user","content":"#.to_vec();
        not_utf8.extend_from_slice(b"\"\xff\"}]}");
        assert_eq!(
            crate::model::parse_request_model(&not_utf8).as_deref(),
            Some("claude-x"),
            "control: the old peek skipped the bad string unchecked"
        );
        let parsed = one_parse(&not_utf8, true);
        assert_eq!(parsed.model, None);
        assert_eq!(parsed.forwarded, not_utf8, "forwarded untouched");
    }

    /// A body nothing rewrites is forwarded as the very bytes the client sent, and so is
    /// one whose account uuid is already the serving account's: the same allocation, not a
    /// copy.
    #[test]
    fn an_unchanged_body_is_the_clients_own_bytes() {
        let body = Bytes::from(conversation(5, true));
        let parsed = RequestBody::parse(body.clone(), false);
        assert_eq!(parsed.bytes().as_ptr(), body.as_ptr(), "no copy");
        assert_eq!(parsed.rewritten_from(), None);
        let same = parsed.for_account("11111111-1111-1111-1111-111111111111");
        assert_eq!(same.as_ptr(), body.as_ptr(), "the uuid matches: no copy");
        let rewritten = RequestBody::parse(body.clone(), true);
        assert_eq!(rewritten.rewritten_from(), Some(body.len()));
        let all_1h = Bytes::copy_from_slice(rewritten.bytes());
        let again = RequestBody::parse(all_1h.clone(), true);
        assert_eq!(
            again.bytes().as_ptr(),
            all_1h.as_ptr(),
            "already 1h: no copy"
        );
    }

    /// Timing, not a gate: the one parse against the separate ones it replaced, on a
    /// Claude Code shaped body of about 4 MB. Run with
    /// `cargo test --release --lib request_body -- --ignored --nocapture`.
    #[test]
    #[ignore = "a timing measurement, run by hand with --release"]
    fn one_parse_on_a_long_conversation_costs() {
        let body = conversation(4_000, true);
        assert_same("long", &body);
        let rounds: u32 = 20;
        let start = std::time::Instant::now();
        for _ in 0..rounds {
            std::hint::black_box(separate_parses(std::hint::black_box(&body), true));
        }
        let separate = start.elapsed() / rounds;
        // Both arms build the same `Answers`, copies included; this one also copies the
        // body in, so it is the slower side of a fair comparison.
        let start = std::time::Instant::now();
        for _ in 0..rounds {
            std::hint::black_box(one_parse(std::hint::black_box(&body), true));
        }
        let one = start.elapsed() / rounds;
        println!(
            "{} byte body: separate parses {separate:?}, one parse {one:?} per request",
            body.len()
        );
    }
}
