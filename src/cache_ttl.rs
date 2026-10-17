//! Whether a request asked Anthropic's prompt cache for the EXTENDED (1-hour)
//! TTL, read straight off the buffered body bytes already parsed once for
//! [`crate::model::parse_request_model`].
//!
//! Anthropic's `cache_control` breakpoints can appear at several depths — the
//! last `system` block, a tool definition, a message content block — and every
//! shape shares one property that matters here: the client only ever sets
//! `"ttl"` at all to ask for the NON-default window (`"1h"`; the field is
//! omitted entirely for the ordinary 5-minute default).
//!
//! This used to be a bounded byte scan for `"cache_control"` followed by a
//! nearby `"ttl"`. That is blind to JSON structure: it fires on an unrelated
//! sibling key that merely sits close to a real `cache_control` object, and —
//! worse — it fires on a JSON *string value* that happens to quote the shape,
//! which is exactly what an agent session embeds in a request body the moment
//! it reads or diffs this very file's own doc comments and test fixtures.
//!
//! So instead this is a typed [`serde_json`] peek, modelled on
//! [`crate::model::parse_request_model`]: a minimal `#[derive(Deserialize)]`
//! shape that only reads `cache_control` off the three structurally legal
//! positions (a `system` block, a tool definition, a message content block)
//! and checks *that object's own* `ttl` field. A `"ttl":"1h"` anywhere else —
//! including inside a string, a mismatched key, or a sibling object — is not
//! visible to this shape at all, so it can never be mistaken for the field.
//! This still does not allocate a generic `serde_json::Value` tree: every
//! field it does not care about is simply skipped by `serde` during the same
//! single pass.

use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;

/// The `cache_control` object itself: only its own `ttl` matters here.
#[derive(Deserialize)]
struct CacheControl {
    #[serde(default)]
    ttl: Option<String>,
}

/// `true` iff `cache_control` is present and its own `ttl` is the extended
/// `"1h"` window. Absent, malformed-away, or any other value (e.g. `"5m"`) is
/// `false` — the ordinary default-window behaviour.
fn is_extended(cache_control: &Option<CacheControl>) -> bool {
    matches!(
        cache_control.as_ref().and_then(|c| c.ttl.as_deref()),
        Some("1h")
    )
}

/// A single content block — the shape shared by `system` array entries and a
/// message's `content` array entries. Everything but `cache_control` is
/// skipped.
#[derive(Deserialize)]
struct ContentBlock {
    #[serde(default)]
    cache_control: Option<CacheControl>,
}

/// `system` and a message's `content` are both legally either a plain string
/// (no `cache_control` possible) or an array of content blocks (where it is).
/// A string value — including one that happens to quote a `cache_control`
/// object's JSON shape — never matches the `Blocks` arm.
#[derive(Deserialize)]
#[serde(untagged)]
enum TextOrBlocks {
    // The string itself is never read — its only job is to consume the
    // `Text` shape so it does not fall through to `Blocks` and expose
    // whatever `cache_control`-shaped substring it might quote.
    Text(#[allow(dead_code)] String),
    Blocks(Vec<ContentBlock>),
}

impl TextOrBlocks {
    fn any_extended(&self) -> bool {
        match self {
            Self::Text(_) => false,
            Self::Blocks(blocks) => blocks.iter().any(|b| is_extended(&b.cache_control)),
        }
    }
}

/// One entry of `messages`. Everything but `content` is skipped.
#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    content: Option<TextOrBlocks>,
}

/// One entry of `tools`. Everything but `cache_control` is skipped.
#[derive(Deserialize)]
struct ToolDef {
    #[serde(default)]
    cache_control: Option<CacheControl>,
}

/// The three top-level positions a `cache_control` object can legally appear
/// under. Everything else in the request body is skipped.
#[derive(Deserialize)]
struct TtlPeek {
    #[serde(default)]
    system: Option<TextOrBlocks>,
    #[serde(default)]
    messages: Option<Vec<Message>>,
    #[serde(default)]
    tools: Option<Vec<ToolDef>>,
}

/// `true` iff `body` contains at least one structurally legal `cache_control`
/// object — on a `system` block, a tool definition, or a message content
/// block — whose own `ttl` is the extended `"1h"` window. Anything else —
/// absent, `"5m"`, malformed, truncated, not JSON at all, or a `cache_control`
/// shape that only appears inside a string — is `false`, which is today's
/// 15-minute pin behaviour. This must never be a way to get a LONGER pin from
/// a malformed or merely lookalike body: the only path to `true` is a real
/// `cache_control` object, in one of the three legal positions, whose own
/// `ttl` is exactly `"1h"`.
/// The request path reads this answer through [`crate::request_body::RequestBody`], which
/// finds it in its one walk; this is the definition that walk is tested against.
pub fn requests_extended_ttl(body: &[u8]) -> bool {
    let Ok(peek) = serde_json::from_slice::<TtlPeek>(body) else {
        return false;
    };

    if peek.system.is_some_and(|s| s.any_extended()) {
        return true;
    }
    if let Some(messages) = peek.messages
        && messages
            .iter()
            .any(|m| m.content.as_ref().is_some_and(TextOrBlocks::any_extended))
    {
        return true;
    }
    if let Some(tools) = peek.tools
        && tools.iter().any(|t| is_extended(&t.cache_control))
    {
        return true;
    }
    false
}

/// The extended window every rewritten breakpoint is set to.
pub(crate) const EXTENDED_TTL: &str = "1h";

/// Set `"ttl":"1h"` on every `cache_control` object in the three legal
/// positions (a `system` block, a tool definition, a message content block) that
/// does not already carry it. `None` when nothing would change: no
/// `cache_control` at all, every breakpoint already `1h`, or a body that is not
/// a JSON object (the proxy then forwards the client's bytes untouched).
///
/// Why rewrite at all: measured 2026-10-06 on a Max 20x account with no other
/// traffic, one token kind per run, a 1h cache write and a 5m cache write cost
/// the 5-hour window the same (about 1.08M tokens per percent each, within one
/// request of each other), and a cache read did not move it in 23.6M tokens.
/// The API's 2x price for a 1h write does not apply to the subscription
/// window, so a prefix that stays warm for an hour instead of five minutes costs
/// nothing extra and turns every 5-60 minute idle gap from a full re-write into
/// a near-free read.
///
/// How: the body is never built into a [`serde_json::Value`] tree. Every
/// Claude Code request carries a `cache_control`, and a long conversation is
/// megabytes, so the tree (one allocation per string, then a full
/// re-serialisation) was paid on almost every request. Instead ONE serde pass
/// walks `system`, `tools` and `messages[].content` down to each block and
/// skips every other member unparsed. Only a block's `cache_control` object,
/// a few bytes, is kept, as a borrowed [`RawValue`] that points into `body`, so
/// its offset is a pointer difference. Each `ttl` to change becomes a splice
/// there: replace the existing `ttl` value's span with `"1h"`, or insert
/// `"ttl":"1h"` before the object's closing brace. The body is then copied once
/// with the splices applied. (Reading each level as a `RawValue` and parsing it
/// again one level down is not the same thing: it scans the conversation once
/// per level, and measured slower than the tree.)
///
/// The structural rules are the ones the old tree walk had, and the reference
/// copy of it in this module's tests holds this to them:
/// - only the three legal positions are walked, so a `cache_control` nested
///   deeper (inside a `tool_result`'s content) or quoted inside a string is
///   never touched;
/// - a `cache_control` that is not an object (null, a string, a number) is left
///   alone;
/// - a `ttl` is `"1h"` when it unescapes to `1h`, so a `1h` spelled with a
///   `\u` escape counts; any other value, a number included, is replaced;
/// - a key that appears twice counts as its LAST occurrence, as a parsed map
///   does, so the splice lands on the occurrence the upstream parser reads.
///
/// What differs from the tree walk:
/// - the rewritten body keeps the client's key order and whitespace; the tree
///   walk re-serialised every object with its keys sorted (`serde_json`'s map is
///   a `BTreeMap`). The content Anthropic hashes for the cache is the blocks'
///   text, not the key order around it, and now the bytes outside the edited
///   `ttl` fields are the client's own.
/// - members this function does not read are skipped, not parsed, and
///   `serde_json`'s skip is laxer than its full parse: a lone UTF-16 surrogate
///   escape (`"\ud800"`), a number outside `f64` range, or nesting deeper than
///   128 levels inside a skipped member made the tree walk give up and return
///   `None`, where this rewrites the breakpoints. The body is still checked to
///   be one JSON object with valid UTF-8 and valid syntax throughout.
pub fn extend_all_ttls(body: &[u8]) -> Option<Vec<u8>> {
    if !body
        .windows(b"\"cache_control\"".len())
        .any(|w| w == b"\"cache_control\"")
    {
        return None;
    }
    // One UTF-8 check up front, so every span below is a `&str` into `body` and
    // a member that is skipped rather than parsed is still checked for it.
    let text = std::str::from_utf8(body).ok()?;
    let mut splices: Vec<Splice> = Vec::new();
    let mut de = serde_json::Deserializer::from_str(text);
    // A top level that is not an object is an error here, so `None`, as before.
    de.deserialize_map(Root(Walk {
        body: text,
        splices: &mut splices,
    }))
    .ok()?;
    // Trailing bytes after the object make the body not JSON, as before.
    de.end().ok()?;

    if splices.is_empty() {
        return None;
    }
    // `system`, `tools` and `messages` can come in any order in the body.
    splices.sort_unstable_by_key(|s| s.start);
    let added: usize = splices.iter().map(|s| s.text.len()).sum();
    let mut out = Vec::with_capacity(body.len() + added);
    let mut copied = 0usize;
    for splice in &splices {
        out.extend_from_slice(body.get(copied..splice.start)?);
        out.extend_from_slice(splice.text.as_bytes());
        copied = splice.end;
    }
    out.extend_from_slice(body.get(copied..)?);
    Some(out)
}

/// One edit to the body: replace `body[start..end]` with `text` (`start == end`
/// is an insert).
pub(crate) struct Splice {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) text: &'static str,
}

/// The `ttl` value every rewritten breakpoint gets, as JSON.
const EXTENDED_TTL_JSON: &str = "\"1h\"";

/// The splices one part of the walk queues, and the body their offsets are
/// measured from.
struct Walk<'s, 'b> {
    body: &'b str,
    splices: &'s mut Vec<Splice>,
}

impl Walk<'_, '_> {
    /// The same walk, writing into `splices` instead.
    const fn writing_to<'t>(&self, splices: &'t mut Vec<Splice>) -> Walk<'t, '_> {
        Walk {
            body: self.body,
            splices,
        }
    }
}

/// What the elements of a [`List`] are.
#[derive(Clone, Copy)]
enum Element {
    /// A `system` block, a tool definition, or a message content block: an
    /// object whose own `cache_control` is a breakpoint.
    Block,
    /// A `messages` entry: an object whose `content` may be a list of blocks.
    Message,
}

/// Every visitor below sees values of any JSON type, and the ones it does not
/// walk into it skips.
macro_rules! skip_scalars {
    () => {
        fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<(), E> {
            Ok(())
        }
        fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<(), E> {
            Ok(())
        }
        fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<(), E> {
            Ok(())
        }
        fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<(), E> {
            Ok(())
        }
        fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<(), E> {
            Ok(())
        }
        fn visit_unit<E: serde::de::Error>(self) -> Result<(), E> {
            Ok(())
        }
    };
}

/// The top-level object: walks `system` and `tools` as lists of blocks and
/// `messages` as a list of messages, and skips every other member.
struct Root<'s, 'b>(Walk<'s, 'b>);

impl<'de> Visitor<'de> for Root<'_, '_> {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        // Splices per member, so a member that repeats counts as its last
        // occurrence only, as a parsed map would hold it.
        let mut found: [Vec<Splice>; 3] = Default::default();
        while let Some(slot) = map.next_key_seed(MemberSlot(&["system", "tools", "messages"]))? {
            let Some(i) = slot else {
                map.next_value::<IgnoredAny>()?;
                continue;
            };
            let element = if i == 2 {
                Element::Message
            } else {
                Element::Block
            };
            let splices = &mut found[i];
            splices.clear();
            map.next_value_seed(List(element, self.0.writing_to(splices)))?;
        }
        for splices in found {
            self.0.splices.extend(splices);
        }
        Ok(())
    }
}

/// A value that, when it is an array, holds [`Element`]s to walk; any other
/// value (a string `system` or `content`) is skipped.
struct List<'s, 'b>(Element, Walk<'s, 'b>);

impl<'de> DeserializeSeed<'de> for List<'_, '_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for List<'_, '_> {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("any JSON value")
    }

    skip_scalars!();

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(())
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        let List(element, walk) = self;
        while seq
            .next_element_seed(Item(
                element,
                Walk {
                    body: walk.body,
                    splices: &mut *walk.splices,
                },
            ))?
            .is_some()
        {}
        Ok(())
    }
}

/// One element of a [`List`]. When it is an object: a block's `cache_control`
/// is extended, a message's `content` is walked as a list of blocks. Any other
/// value is skipped.
struct Item<'s, 'b>(Element, Walk<'s, 'b>);

impl<'de> DeserializeSeed<'de> for Item<'_, '_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Item<'_, '_> {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("any JSON value")
    }

    skip_scalars!();

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let Item(element, walk) = self;
        match element {
            Element::Block => {
                // The last `cache_control` is the one a parsed map holds.
                let mut cache_control: Option<&'de RawValue> = None;
                while let Some(slot) = map.next_key_seed(MemberSlot(&["cache_control"]))? {
                    if slot.is_some() {
                        cache_control = Some(map.next_value()?);
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                if let Some(cache_control) = cache_control {
                    extend_cache_control(walk.body, cache_control, walk.splices).ok_or_else(
                        || {
                            <A::Error as serde::de::Error>::custom(
                                "a cache_control span outside the body",
                            )
                        },
                    )?;
                }
            }
            Element::Message => {
                // Splices of the last `content` only, as with a repeated key above.
                let mut content: Vec<Splice> = Vec::new();
                while let Some(slot) = map.next_key_seed(MemberSlot(&["content"]))? {
                    if slot.is_some() {
                        content.clear();
                        map.next_value_seed(List(Element::Block, walk.writing_to(&mut content)))?;
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                walk.splices.extend(content);
            }
        }
        Ok(())
    }
}

/// Queue the splice that makes `cache_control` carry `ttl: "1h"`, if it is an
/// object that does not already. One that is not an object (null, a string, a
/// number) queues nothing. `None` only when a span does not point into `body`,
/// which `serde_json`'s borrowed `RawValue` rules out; the walk then fails and the
/// body is forwarded unchanged rather than spliced at a wild offset.
pub(crate) fn extend_cache_control(
    body: &str,
    cache_control: &RawValue,
    splices: &mut Vec<Splice>,
) -> Option<()> {
    let Some([ttl]) = members(cache_control, &["ttl"]) else {
        return Some(());
    };
    match ttl {
        Some(ttl) if is_extended_ttl(ttl) => {}
        Some(ttl) => {
            let start = offset_in(body, ttl.get())?;
            splices.push(Splice {
                start,
                end: start + ttl.get().len(),
                text: EXTENDED_TTL_JSON,
            });
        }
        None => {
            // A borrowed object span is exactly `{ ... }`, no surrounding
            // whitespace. Insert after its last member, or as its only one.
            let object = cache_control.get();
            let inside = object.get(1..object.len().checked_sub(1)?)?.trim_end();
            let at = offset_in(body, object)? + 1 + inside.len();
            splices.push(Splice {
                start: at,
                end: at,
                text: if inside.is_empty() {
                    "\"ttl\":\"1h\""
                } else {
                    ",\"ttl\":\"1h\""
                },
            });
        }
    }
    Some(())
}

/// `true` iff the JSON value `ttl` is the string `1h`, compared after
/// unescaping. The common spelling is checked without allocating.
pub(crate) fn is_extended_ttl(ttl: &RawValue) -> bool {
    ttl.get() == EXTENDED_TTL_JSON
        || (ttl.get().starts_with('"')
            && serde_json::from_str::<String>(ttl.get()).is_ok_and(|s| s == EXTENDED_TTL))
}

/// Byte offset of `part` inside `body`. `part` is a span `serde_json` borrowed out
/// of `body`, so pointer distance is the offset; the range check means a span
/// that somehow did not come from `body` is a miss, never a wild splice (the
/// same guard as `crate::account_uuid::locate`).
pub(crate) fn offset_in(body: &str, part: &str) -> Option<usize> {
    let offset = (part.as_ptr() as usize).checked_sub(body.as_ptr() as usize)?;
    (offset.checked_add(part.len())? <= body.len()).then_some(offset)
}

/// The named members of `raw` when it is a JSON object (`None` when it is not),
/// each a span borrowed out of `raw`, or `None` per name when absent.
fn members<'a, const N: usize>(
    raw: &'a RawValue,
    names: &[&'static str; N],
) -> Option<[Option<&'a RawValue>; N]> {
    serde_json::Deserializer::from_str(raw.get())
        .deserialize_map(PickMembers(names))
        .ok()
}

/// Visits a JSON object and keeps the value of each named member as a borrowed
/// [`RawValue`]; every other member is skipped without being parsed. A name that
/// repeats keeps its last value, which is what a parsed map ends up holding.
/// `#[derive(Deserialize)]` cannot stand in: it rejects a repeated field, and
/// `RawValue` does not survive `untagged` or `flatten`.
struct PickMembers<'n, const N: usize>(&'n [&'static str; N]);

impl<'de, const N: usize> Visitor<'de> for PickMembers<'_, N> {
    type Value = [Option<&'de RawValue>; N];

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut picked = [None; N];
        while let Some(slot) = map.next_key_seed(MemberSlot(self.0))? {
            match slot {
                Some(i) => picked[i] = Some(map.next_value::<&'de RawValue>()?),
                None => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(picked)
    }
}

/// A member key, read as the index of the name it matches (after unescaping,
/// with no allocation for an unescaped key), or `None` for any other key.
#[derive(Clone, Copy)]
struct MemberSlot<'n, const N: usize>(&'n [&'static str; N]);

impl<'de, const N: usize> DeserializeSeed<'de> for MemberSlot<'_, N> {
    type Value = Option<usize>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_str(self)
    }
}

impl<const N: usize> Visitor<'_> for MemberSlot<'_, N> {
    type Value = Option<usize>;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an object key")
    }

    fn visit_str<E: serde::de::Error>(self, key: &str) -> Result<Self::Value, E> {
        Ok(self.0.iter().position(|name| *name == key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tree-walking `extend_all_ttls` this module shipped before the splice
    /// version, kept verbatim as the reference the new one must agree with.
    fn reference_extend_all_ttls(body: &[u8]) -> Option<Vec<u8>> {
        if !body
            .windows(b"\"cache_control\"".len())
            .any(|w| w == b"\"cache_control\"")
        {
            return None;
        }
        let mut root: serde_json::Value = serde_json::from_slice(body).ok()?;
        let obj = root.as_object_mut()?;
        let mut changed = 0usize;

        let mut extend_block = |block: &mut serde_json::Value| {
            let Some(cc) = block
                .get_mut("cache_control")
                .and_then(|v| v.as_object_mut())
            else {
                return;
            };
            if cc.get("ttl").and_then(|t| t.as_str()) == Some(EXTENDED_TTL) {
                return;
            }
            cc.insert(
                "ttl".to_string(),
                serde_json::Value::String(EXTENDED_TTL.to_string()),
            );
            changed += 1;
        };

        if let Some(blocks) = obj.get_mut("system").and_then(|s| s.as_array_mut()) {
            blocks.iter_mut().for_each(&mut extend_block);
        }
        if let Some(tools) = obj.get_mut("tools").and_then(|t| t.as_array_mut()) {
            tools.iter_mut().for_each(&mut extend_block);
        }
        if let Some(messages) = obj.get_mut("messages").and_then(|m| m.as_array_mut()) {
            for message in messages.iter_mut() {
                if let Some(blocks) = message.get_mut("content").and_then(|c| c.as_array_mut()) {
                    blocks.iter_mut().for_each(&mut extend_block);
                }
            }
        }

        if changed == 0 {
            return None;
        }
        serde_json::to_vec(&root).ok()
    }

    /// The new and the reference `extend_all_ttls` agree on `body`: both `None`,
    /// or both `Some` and equal once parsed (the new one keeps the client's key
    /// order, so the bytes differ by design).
    /// Returns whether the body was rewritten.
    fn assert_matches_reference(body: &[u8]) -> bool {
        let shown = String::from_utf8_lossy(body);
        match (extend_all_ttls(body), reference_extend_all_ttls(body)) {
            (None, None) => false,
            (Some(new), Some(reference)) => {
                let new_value: serde_json::Value =
                    serde_json::from_slice(&new).unwrap_or_else(|e| {
                        panic!(
                            "the rewrite is not JSON ({e}): {}\n  input: {shown}",
                            String::from_utf8_lossy(&new)
                        )
                    });
                let reference_value: serde_json::Value =
                    serde_json::from_slice(&reference).expect("the reference emits JSON");
                assert_eq!(
                    new_value, reference_value,
                    "the rewrite differs from the reference\n  input: {shown}"
                );
                true
            }
            (new, reference) => panic!(
                "new returned {}, reference returned {}\n  input: {shown}",
                if new.is_some() { "Some" } else { "None" },
                if reference.is_some() { "Some" } else { "None" },
            ),
        }
    }

    /// Every fixture the bridge named, each in its compact form and again
    /// pretty-printed, gets the same answer from the splice version as from the
    /// tree walk it replaced.
    #[test]
    fn extend_all_ttls_matches_the_tree_walk_it_replaced() {
        let fixtures: &[&str] = &[
            // All three positions, mixed absent / explicit 5m / no breakpoint.
            r#"{"model":"claude-x","system":[{"type":"text","text":"a","cache_control":{"type":"ephemeral"}}],"tools":[{"name":"t","cache_control":{"type":"ephemeral","ttl":"5m"}}],"messages":[{"role":"user","content":[{"type":"text","text":"b","cache_control":{"type":"ephemeral"}},{"type":"text","text":"c"}]}]}"#,
            // The same body, hand pretty-printed, keys out of sorted order.
            "{\n  \"model\": \"claude-x\",\n  \"messages\": [\n    {\n      \"role\": \"user\",\n      \"content\": [\n        {\n          \"type\": \"text\",\n          \"text\": \"b\",\n          \"cache_control\": {\n            \"type\": \"ephemeral\"\n          }\n        }\n      ]\n    }\n  ],\n  \"system\": [\n    {\n      \"type\": \"text\",\n      \"text\": \"a\",\n      \"cache_control\": { \"type\": \"ephemeral\", \"ttl\" : \"5m\" }\n    }\n  ]\n}\n",
            // `system` as a string, `content` as a string.
            r#"{"system":"plain system prompt","messages":[{"role":"user","content":"plain"},{"role":"user","content":[{"type":"text","text":"x","cache_control":{"type":"ephemeral"}}]}]}"#,
            // `system` blocks with no breakpoint on any of them.
            r#"{"system":[{"type":"text","text":"a"}],"messages":[{"role":"user","content":[{"type":"text","text":"x","cache_control":{"type":"ephemeral"}}]}]}"#,
            // `cache_control: {}`, with and without whitespace inside.
            r#"{"system":[{"type":"text","text":"a","cache_control":{}}]}"#,
            "{\"system\":[{\"type\":\"text\",\"text\":\"a\",\"cache_control\":{ \n }}]}",
            // Already 1h, plain and escaped.
            r#"{"system":[{"type":"text","text":"a","cache_control":{"type":"ephemeral","ttl":"1h"}}]}"#,
            r#"{"system":[{"type":"text","text":"a","cache_control":{"type":"ephemeral","ttl":"^u0031h"}}]}"#,
            // A `ttl` that is a number, null, an object, or an escaped 5m.
            r#"{"system":[{"type":"text","text":"a","cache_control":{"type":"ephemeral","ttl":3600}}]}"#,
            r#"{"system":[{"type":"text","text":"a","cache_control":{"ttl":null,"type":"ephemeral"}}]}"#,
            r#"{"tools":[{"name":"t","cache_control":{"ttl":{"h":1}}}]}"#,
            r#"{"tools":[{"name":"t","cache_control":{"ttl":"^u0035m"}}]}"#,
            // `cache_control` that is not an object: left alone.
            r#"{"system":[{"type":"text","text":"a","cache_control":null}]}"#,
            r#"{"system":[{"type":"text","text":"a","cache_control":"ephemeral"}]}"#,
            r#"{"tools":[{"name":"t","cache_control":5}]}"#,
            // A breakpoint nested inside a tool_result's content is not a legal
            // position and stays as it is; the outer one is extended.
            r#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"r","cache_control":{"type":"ephemeral"}}],"cache_control":{"type":"ephemeral"}}]}]}"#,
            r#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"r","cache_control":{"type":"ephemeral"}}]}]}]}"#,
            // The shape only inside a text string.
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":"see \"cache_control\": {\"type\":\"ephemeral\"}"}]}]}"#,
            r#"{"system":"quote: {\"cache_control\":{\"type\":\"ephemeral\"}}"}"#,
            // Not JSON, trailing bytes, top level an array or a string, empty.
            "not json \"cache_control\"",
            r#"{"system":[{"cache_control":{}}]} trailing"#,
            r#"[{"system":[{"cache_control":{}}]}]"#,
            r#""\"cache_control\"""#,
            "",
            // Wrong shapes at each level are skipped, not errors.
            r#"{"messages":{"content":[{"cache_control":{}}]},"system":[1,"x",null,{"cache_control":{}}]}"#,
            r#"{"messages":["x",3,{"content":{"cache_control":{}}},{"content":[[{"cache_control":{}}]]}],"tools":"\"cache_control\""}"#,
            // Repeated keys: the last occurrence is the one that counts.
            r#"{"system":[{"cache_control":{"ttl":"5m","ttl":"1h"}}]}"#,
            r#"{"system":[{"cache_control":{"ttl":"1h","ttl":"5m"}}]}"#,
            r#"{"system":[{"cache_control":{"ttl":"5m"},"cache_control":null}]}"#,
            r#"{"system":[{"cache_control":null,"cache_control":{"type":"ephemeral"}}]}"#,
            r#"{"system":[{"cache_control":{}}],"system":"now a string"}"#,
            r#"{"system":"first a string","system":[{"cache_control":{}}]}"#,
            r#"{"messages":[{"content":[{"cache_control":{}}],"content":"now a string"}]}"#,
            r#"{"messages":[{"content":"first a string","content":[{"cache_control":{}}]}]}"#,
            r#"{"messages":[],"messages":[{"content":[{"cache_control":{}}]}],"tools":[{"cache_control":{}}],"tools":null}"#,
            // An escaped member name is the same member.
            r#"{"system":[{"cache^u005fcontrol":{"type":"ephemeral"}},{"cache_control":{"ttl":"1h"}}]}"#,
            r#"{"^u0073ystem":[{"cache_control":{"^u0074tl":"5m"}}]}"#,
            // Many breakpoints, the order in the body not system/tools/messages.
            r#"{"messages":[{"content":[{"cache_control":{}},{"cache_control":{"ttl":"5m"}}]},{"content":[{"cache_control":{"type":"ephemeral"}}]}],"tools":[{"cache_control":{}},{"cache_control":{"ttl":"1h"}}],"system":[{"cache_control":{"type":"ephemeral"}}]}"#,
            // Non-ASCII text around the edits, so offsets are in bytes.
            r#"{"system":[{"text":"שלום ✓ 😀","cache_control":{"type":"ephemeral"}}],"messages":[{"content":[{"text":"日本","cache_control":{}}]}]}"#,
        ];
        let mut rewritten = 0;
        for fixture in fixtures {
            // `^` stands for a backslash, so a `\u` escape reaches the body as an
            // escape however this file is edited.
            let fixture = fixture.replace('^', "\\");
            if assert_matches_reference(fixture.as_bytes()) {
                rewritten += 1;
            }
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&fixture) {
                let pretty = serde_json::to_string_pretty(&value).expect("re-serialize");
                assert_matches_reference(pretty.as_bytes());
            }
        }
        // Both answers are exercised, so agreement is not two `None`s agreeing.
        assert!(
            rewritten >= 15 && fixtures.len() - rewritten >= 10,
            "{rewritten} of {} fixtures rewritten",
            fixtures.len()
        );
        // The escaped spellings are compared unescaped: an escaped 1h is left
        // alone, an escaped 5m and an escaped key name are rewritten. (The byte
        // pre-check still wants one unescaped `"cache_control"` somewhere, here
        // an unrelated top-level key.)
        let escaped = |s: &str| s.replace('^', "\\").into_bytes();
        assert!(escaped("^u0031h").starts_with(b"\\u"));
        assert!(
            extend_all_ttls(&escaped(
                r#"{"system":[{"cache_control":{"ttl":"^u0031h"}}]}"#
            ))
            .is_none()
        );
        assert!(
            extend_all_ttls(&escaped(
                r#"{"system":[{"cache_control":{"ttl":"^u0035m"}}]}"#
            ))
            .is_some()
        );
        assert!(
            extend_all_ttls(&escaped(
                r#"{"system":[{"cache^u005fcontrol":{}}],"x":{"cache_control":0}}"#
            ))
            .is_some()
        );
    }

    /// A body that is not valid UTF-8 is not JSON, even when the bad byte sits
    /// in a member nothing reads, and stays untouched as before.
    #[test]
    fn extend_all_ttls_leaves_invalid_utf8_untouched() {
        let mut body = br#"{"model":"x"#.to_vec();
        body.push(0xff);
        body.extend_from_slice(br#"","system":[{"cache_control":{}}]}"#);
        assert_matches_reference(&body);
        assert!(extend_all_ttls(&body).is_none());
    }

    /// The rewrite edits only the `ttl` fields: every other byte, key order and
    /// whitespace included, is the client's own.
    #[test]
    fn extend_all_ttls_keeps_every_byte_outside_the_ttl_fields() {
        let body = "{\"z\": 1,\n \"system\": [{\"text\": \"a\", \"cache_control\": {\"type\": \"ephemeral\"} }],\n \"tools\": [{\"cache_control\": { }}, {\"cache_control\": {\"ttl\" : \"5m\", \"type\": \"ephemeral\"}}],\n \"a\": 2}";
        let out = extend_all_ttls(body.as_bytes()).expect("three breakpoints to extend");
        assert_eq!(
            String::from_utf8(out).expect("utf-8"),
            "{\"z\": 1,\n \"system\": [{\"text\": \"a\", \"cache_control\": {\"type\": \"ephemeral\",\"ttl\":\"1h\"} }],\n \"tools\": [{\"cache_control\": {\"ttl\":\"1h\" }}, {\"cache_control\": {\"ttl\" : \"1h\", \"type\": \"ephemeral\"}}],\n \"a\": 2}"
        );
    }

    /// The one class where the two disagree, as the doc says: `serde_json`'s full
    /// parse refuses a lone surrogate escape, an out-of-range number or nesting
    /// past its recursion limit of 128, and its skip does not, so the tree walk
    /// forwarded such a body untouched and the splice version extends its
    /// breakpoints. Pinned so a change to either side is noticed.
    #[test]
    fn extend_all_ttls_extends_where_only_the_full_parse_refused() {
        let deep = format!(
            r#"{{"metadata":{}{},"system":[{{"cache_control":{{}}}}]}}"#,
            "[".repeat(200),
            "]".repeat(200)
        );
        // `^` stands for a backslash, as in the equivalence fixtures.
        let lone_surrogate =
            r#"{"model":"^ud800","system":[{"cache_control":{}}]}"#.replace('^', "\\");
        for body in [
            lone_surrogate.as_str(),
            r#"{"metadata":{"n":1e400},"system":[{"cache_control":{}}]}"#,
            deep.as_str(),
        ] {
            assert!(
                reference_extend_all_ttls(body.as_bytes()).is_none(),
                "the full parse refuses {body}"
            );
            let out = extend_all_ttls(body.as_bytes()).expect("the skip accepts it");
            assert!(
                String::from_utf8(out)
                    .expect("utf-8")
                    .contains(r#""cache_control":{"ttl":"1h"}"#)
            );
        }
    }

    /// A conversation of about 2 MB with a dozen breakpoints, the shape of a
    /// long Claude Code session.
    fn long_conversation_body() -> Vec<u8> {
        let filler = "x".repeat(5_000);
        let mut messages = Vec::new();
        for n in 0..400 {
            let cache_control = if n % 40 == 0 {
                r#","cache_control":{"type":"ephemeral"}"#
            } else {
                ""
            };
            messages.push(format!(
                r#"{{"role":"{}","content":[{{"type":"text","text":"turn {n} {filler}"{cache_control}}}]}}"#,
                if n % 2 == 0 { "user" } else { "assistant" }
            ));
        }
        format!(
            r#"{{"model":"claude-x","max_tokens":32000,"system":[{{"type":"text","text":"sys"}},{{"type":"text","text":"sys2","cache_control":{{"type":"ephemeral"}}}}],"tools":[{{"name":"t","description":"d","input_schema":{{"type":"object"}},"cache_control":{{"type":"ephemeral"}}}}],"messages":[{}]}}"#,
            messages.join(",")
        )
        .into_bytes()
    }

    /// Timing, not a gate: run with
    /// `cargo test --release --lib cache_ttl -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing, not a gate: run with --release --ignored --nocapture"]
    fn extend_all_ttls_on_a_long_conversation_costs() {
        let body = long_conversation_body();
        assert_matches_reference(&body);
        let rounds: u32 = 50;
        let start = std::time::Instant::now();
        for _ in 0..rounds {
            std::hint::black_box(reference_extend_all_ttls(std::hint::black_box(&body)));
        }
        let reference = start.elapsed() / rounds;
        let start = std::time::Instant::now();
        for _ in 0..rounds {
            std::hint::black_box(extend_all_ttls(std::hint::black_box(&body)));
        }
        let new = start.elapsed() / rounds;
        println!(
            "extend_all_ttls on a {} byte body with 12 breakpoints: reference {reference:?}, splice {new:?} per call",
            body.len()
        );
    }

    /// Every legal position gets `ttl: 1h`; the rewritten body reads back as
    /// extended through the peek the proxy already trusts.
    #[test]
    fn extend_all_ttls_sets_1h_on_every_legal_breakpoint() {
        let body = br#"{"model":"claude-x","system":[{"type":"text","text":"a","cache_control":{"type":"ephemeral"}}],"tools":[{"name":"t","cache_control":{"type":"ephemeral","ttl":"5m"}}],"messages":[{"role":"user","content":[{"type":"text","text":"b","cache_control":{"type":"ephemeral"}},{"type":"text","text":"c"}]}]}"#;
        let out = extend_all_ttls(body).expect("three breakpoints to extend");
        assert!(requests_extended_ttl(&out));
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["system"][0]["cache_control"]["ttl"], "1h");
        assert_eq!(
            v["tools"][0]["cache_control"]["ttl"], "1h",
            "an explicit 5m becomes 1h"
        );
        assert_eq!(v["messages"][0]["content"][0]["cache_control"]["ttl"], "1h");
        assert!(
            v["messages"][0]["content"][1]
                .get("cache_control")
                .is_none(),
            "a block with no breakpoint gets none"
        );
        assert_eq!(v["model"], "claude-x", "the rest of the body survives");
    }

    /// `None` is the no-change answer: no breakpoint, all already 1h, a string
    /// body, or a quoted lookalike. The proxy forwards the client's bytes.
    #[test]
    fn extend_all_ttls_returns_none_when_nothing_changes() {
        assert!(extend_all_ttls(br#"{"model":"claude-x","messages":[]}"#).is_none());
        assert!(extend_all_ttls(
            br#"{"system":[{"type":"text","text":"a","cache_control":{"type":"ephemeral","ttl":"1h"}}]}"#
        )
        .is_none());
        assert!(extend_all_ttls(br#"{"system":"a plain string system prompt"}"#).is_none());
        assert!(
            extend_all_ttls(
                br#"{"messages":[{"role":"user","content":"this quotes \"cache_control\": {\"ttl\": \"5m\"} in text"}]}"#
            )
            .is_none(),
            "a string value that quotes the shape is not a breakpoint"
        );
        assert!(extend_all_ttls(b"not json \"cache_control\"").is_none());
    }

    #[test]
    fn detects_extended_ttl_on_a_system_block() {
        let body = br#"{"system":[{"type":"text","text":"x","cache_control":{"type":"ephemeral","ttl":"1h"}}]}"#;
        assert!(requests_extended_ttl(body));
    }

    #[test]
    fn detects_extended_ttl_on_a_tool_definition() {
        let body = br#"{"tools":[{"name":"x","cache_control":{"type":"ephemeral","ttl":"1h"}}]}"#;
        assert!(requests_extended_ttl(body));
    }

    #[test]
    fn detects_extended_ttl_on_a_message_content_block() {
        let body = br#"{"messages":[{"role":"user","content":[{"type":"text","text":"x","cache_control":{"type":"ephemeral","ttl":"1h"}}]}]}"#;
        assert!(requests_extended_ttl(body));
    }

    #[test]
    fn default_cache_control_with_no_ttl_is_not_extended() {
        let body =
            br#"{"system":[{"type":"text","text":"x","cache_control":{"type":"ephemeral"}}]}"#;
        assert!(!requests_extended_ttl(body));
    }

    #[test]
    fn explicit_5m_ttl_is_not_extended() {
        let body = br#"{"system":[{"type":"text","text":"x","cache_control":{"type":"ephemeral","ttl":"5m"}}]}"#;
        assert!(!requests_extended_ttl(body));
    }

    #[test]
    fn absent_cache_control_is_not_extended() {
        let body = br#"{"model":"claude-x","messages":[]}"#;
        assert!(!requests_extended_ttl(body));
    }

    #[test]
    fn malformed_json_is_not_extended() {
        assert!(!requests_extended_ttl(b"not json { at all"));
        assert!(!requests_extended_ttl(b""));
    }

    #[test]
    fn spaced_json_ttl_1h_is_detected() {
        let body = br#"{"system": [{"type": "text", "text": "x", "cache_control": {"type": "ephemeral", "ttl": "1h"}}]}"#;
        assert!(requests_extended_ttl(body));
    }

    #[test]
    fn pretty_printed_ttl_1h_across_newlines_is_detected() {
        let body = b"{\n  \"system\": [{\n    \"type\": \"text\",\n    \"text\": \"x\",\n    \"cache_control\": {\n      \"type\": \"ephemeral\",\n      \"ttl\": \"1h\"\n    }\n  }]\n}";
        assert!(requests_extended_ttl(body));
    }

    #[test]
    fn reordered_and_spaced_keys_are_detected() {
        let body = br#"{"system": [{"cache_control": {"ttl": "1h", "type": "ephemeral"}, "type": "text", "text": "x"}]}"#;
        assert!(requests_extended_ttl(body));
    }

    #[test]
    fn spaced_5m_ttl_is_not_extended() {
        let body = br#"{"system": [{"type": "text", "text": "x", "cache_control": {"type": "ephemeral", "ttl": "5m"}}]}"#;
        assert!(!requests_extended_ttl(body));
    }

    /// Reproduced failure #1 from the bridge: a `cache_control` object with no
    /// `ttl`, sitting next to an unrelated sibling object that happens to have
    /// a `"ttl":"1h"` key of its own. Neither is a legal `cache_control`/`ttl`
    /// pairing, so this must be `false`.
    #[test]
    fn unrelated_sibling_ttl_next_to_cache_control_does_not_count() {
        let body = br#"{"cache_control":{"type":"ephemeral"},"config":{"ttl":"1h"}}"#;
        assert!(!requests_extended_ttl(body));
    }

    /// Reproduced failure #2 from the bridge: a real `cache_control` (with no
    /// `ttl`) inside a `system` block, plus an unrelated top-level `"tool"`
    /// (singular — not the legal `"tools"` array) whose own object has a
    /// `"ttl":"1h"`. Must be `false`.
    #[test]
    fn unrelated_singular_tool_ttl_does_not_count() {
        let body = br#"{"system":[{"type":"text","text":"x","cache_control":{"type":"ephemeral"}}],"tool":{"ttl":"1h"}}"#;
        assert!(!requests_extended_ttl(body));
    }

    /// The self-trigger class: a message whose `content` is a plain STRING
    /// that happens to quote the literal `cache_control`/`ttl` JSON shape —
    /// e.g. an agent session pasting this very module's own test fixtures
    /// into a request. A string can never be a `cache_control` object, so
    /// this must be `false` regardless of what text it contains.
    #[test]
    fn ttl_1h_quoted_inside_a_message_string_does_not_count() {
        let body = br#"{"messages":[{"role":"user","content":"see {\"cache_control\":{\"type\":\"ephemeral\",\"ttl\":\"1h\"}} for reference"}]}"#;
        assert!(!requests_extended_ttl(body));
    }
}
