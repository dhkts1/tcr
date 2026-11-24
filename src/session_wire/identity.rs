//! Bash command identity reduction: the one clause that says what a command is.

/// One position from [`shell_scan`]: the character, its bracket depth, and whether it
/// sits inside an unescaped quote. Mirrors the Python prototype's `scan()` generator,
/// including its quirk that an ESCAPED character's `quoted` flag reflects whatever quote
/// state was already active, not the escape itself — a backslash only suppresses the next
/// character from opening/closing a quote or bracket, it does not itself put that
/// character inside a quote.
struct ScanPos {
    ch: char,
    depth: usize,
    quoted: bool,
}

/// Walk `s` once, character by character, tracking bash quoting (`'...'` disables
/// backslash escapes; `"..."` does not) and bracket depth (`(`, `[`, `{`). Every caller
/// that needs to know "is this character inside quotes, or inside brackets" reads it from
/// here rather than re-deriving it — see the module doc for why a regex cannot replace
/// this walk.
fn shell_scan(s: &str) -> Vec<ScanPos> {
    let mut out = Vec::with_capacity(s.len());
    let mut q: Option<char> = None;
    let mut depth: usize = 0;
    let mut esc = false;
    for ch in s.chars() {
        if esc {
            esc = false;
            out.push(ScanPos {
                ch,
                depth,
                quoted: q.is_some(),
            });
            continue;
        }
        if ch == '\\' && q != Some('\'') {
            esc = true;
            out.push(ScanPos {
                ch,
                depth,
                quoted: q.is_some(),
            });
            continue;
        }
        if let Some(qc) = q {
            if ch == qc {
                q = None;
            }
            out.push(ScanPos {
                ch,
                depth,
                quoted: true,
            });
            continue;
        }
        if ch == '"' || ch == '\'' {
            q = Some(ch);
            out.push(ScanPos {
                ch,
                depth,
                quoted: true,
            });
            continue;
        }
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
        out.push(ScanPos {
            ch,
            depth,
            quoted: false,
        });
    }
    out
}

/// Split `cmd` on `;`, `&&`, `||`, `|` and newline, only where unquoted and at bracket
/// depth zero. Blank segments (after trimming) are dropped, matching the prototype's own
/// `[s for s in out if s.strip()]`.
fn split_segments(cmd: &str) -> Vec<String> {
    let chars: Vec<char> = cmd.chars().collect();
    let state = shell_scan(cmd);
    let n = chars.len();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < n {
        let pos = &state[i];
        if !pos.quoted && pos.depth == 0 {
            if i + 1 < n {
                let two = (chars[i], chars[i + 1]);
                if two == ('&', '&') || two == ('|', '|') {
                    out.push(chars[start..i].iter().collect::<String>());
                    start = i + 2;
                    i += 2;
                    continue;
                }
            }
            if matches!(chars[i], ';' | '\n' | '|') {
                out.push(chars[start..i].iter().collect::<String>());
                start = i + 1;
                i += 1;
                continue;
            }
        }
        i += 1;
    }
    out.push(chars[start..].iter().collect::<String>());
    out.into_iter().filter(|s| !s.trim().is_empty()).collect()
}

/// Trailing characters `safe_cut` and `balance` strip after a cut — the prototype's
/// `rstrip(' \\|&<>=-\t')`.
fn rstrip_cut_junk(s: &str) -> String {
    s.trim_end_matches(|c| " \\|&<>=-\t".contains(c))
        .to_string()
}

/// Cut `s` to at most `n` characters at a quote/bracket-aware boundary — the last
/// unquoted, depth-zero space at or before `n` — but only when that boundary keeps at
/// least 60% of the budget. A boundary that throws away more than that is lossy, not
/// safe: `rg -n "a|b|c" path` has its last unquoted space at index 5, so cutting there
/// yields a bare `rg -n`. Below that threshold, cut at the budget itself and let
/// [`balance_identity`] repair whatever it lands inside.
fn safe_cut(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        return rstrip_cut_junk(s);
    }
    let state = shell_scan(s);
    let mut last = 0usize;
    for (i, pos) in state.iter().enumerate() {
        if i > n {
            break;
        }
        if pos.ch == ' ' && !pos.quoted && pos.depth == 0 {
            last = i;
        }
    }
    let cut: String = if last > 0 && last >= (n * 6) / 10 {
        chars[..last].iter().collect()
    } else {
        chars[..n].iter().collect()
    };
    rstrip_cut_junk(&cut)
}

/// Remove `2>&1`/`>&1`, process substitution (`<(...)`/`>(...)`, replaced with a `‹sub›`
/// marker so it is never left an eaten orphan `)`), and any other `N< `/`N> ` redirection —
/// all only where unquoted and at bracket depth zero.
fn strip_redirects(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let state = shell_scan(s);
    let n = chars.len();
    let mut keep = String::new();
    let mut i = 0usize;
    while i < n {
        let pos = &state[i];
        if !pos.quoted && pos.depth == 0 {
            if chars_eq(&chars, i, "2>&1") {
                i += 4;
                continue;
            }
            if chars_eq(&chars, i, ">&1") {
                i += 3;
                continue;
            }
            if matches!(chars[i], '<' | '>') && i + 1 < n && chars[i + 1] == '(' {
                let mut d = 0i32;
                let mut j = i + 1;
                while j < n {
                    if chars[j] == '(' {
                        d += 1;
                    } else if chars[j] == ')' {
                        d -= 1;
                        if d == 0 {
                            j += 1;
                            break;
                        }
                    }
                    j += 1;
                }
                keep.push_str("\u{2039}sub\u{203a}");
                i = j;
                continue;
            }
            if chars[i].is_ascii_digit() && i + 1 < n && matches!(chars[i + 1], '<' | '>') {
                i += 1;
                continue;
            }
            if matches!(chars[i], '<' | '>') {
                let mut j = i;
                while j < n && matches!(chars[j], '<' | '>' | '&') {
                    j += 1;
                }
                while j < n && chars[j] == ' ' {
                    j += 1;
                }
                while j < n && !matches!(chars[j], ' ' | '\t') {
                    j += 1;
                }
                i = j;
                continue;
            }
        }
        keep.push(chars[i]);
        i += 1;
    }
    keep.trim().to_string()
}

fn chars_eq(chars: &[char], i: usize, pat: &str) -> bool {
    let pat: Vec<char> = pat.chars().collect();
    i + pat.len() <= chars.len() && chars[i..i + pat.len()] == pat[..]
}

/// Make a cut identity well-formed WITHOUT throwing its content away: drop a dangling
/// escape, close an open quote, close any open brackets, and mark the repair with `…`.
/// Trimming BACKWARD to the last safe point instead is what produced a bare `rg -n` for
/// 532 commands in the prototype's own history — a command whose whole payload is one
/// long quoted token (`printf '...'`, `jq '{...}'`) has no safe cut point before the width
/// budget, so trimming back gives the bare verb, which says nothing.
fn balance_identity(s: &str) -> String {
    let mut q: Option<char> = None;
    let mut esc = false;
    let mut stack: Vec<char> = Vec::new();
    for ch in s.chars() {
        if esc {
            esc = false;
            continue;
        }
        if ch == '\\' && q != Some('\'') {
            esc = true;
            continue;
        }
        if let Some(qc) = q {
            if ch == qc {
                q = None;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            q = Some(ch);
            continue;
        }
        match ch {
            '(' | '[' | '{' => stack.push(ch),
            ')' | ']' | '}' if !stack.is_empty() => {
                stack.pop();
            }
            _ => {}
        }
    }
    if q.is_none() && stack.is_empty() {
        return s.to_string();
    }
    let mut out = s.trim_end_matches(['\\', ' ']).to_string();
    if !out.is_empty() && (q.is_some() || !stack.is_empty()) {
        out.push('\u{2026}');
    }
    if let Some(qc) = q {
        out.push(qc);
    }
    for c in stack.iter().rev() {
        out.push(match c {
            '(' => ')',
            '[' => ']',
            '{' => '}',
            _ => unreachable!("stack only ever holds an open bracket"),
        });
    }
    out
}

fn is_path_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.' | '@' | '+' | '-')
}

/// Shorten every run of 3+ absolute-path components (`/a/b/c/d` -> `.../c/d`) — a long
/// path cannot widen the panel row any more than a long command can.
fn shorten_paths(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut out = String::new();
    let mut i = 0usize;
    while i < n {
        if chars[i] == '/' {
            let mut j = i;
            let mut components: Vec<String> = Vec::new();
            while j < n && chars[j] == '/' {
                let seg_start = j + 1;
                let mut k = seg_start;
                while k < n && is_path_char(chars[k]) {
                    k += 1;
                }
                if k == seg_start {
                    break;
                }
                components.push(chars[seg_start..k].iter().collect());
                j = k;
            }
            if components.len() >= 3 {
                let last_two = &components[components.len() - 2..];
                out.push_str(".../");
                out.push_str(&last_two.join("/"));
                i = j;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Collapse a run of 2+ whitespace characters to a single space. A single embedded
/// whitespace character (one real newline, from a quoted multi-line string that
/// [`split_segments`] never split on) is left exactly as it is — [`normalize_bash_command`]
/// turns that one into `⏎` afterward.
fn collapse_ws(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut out = String::new();
    let mut i = 0usize;
    while i < n {
        if chars[i].is_whitespace() {
            let mut j = i;
            while j < n && chars[j].is_whitespace() {
                j += 1;
            }
            out.push(if j - i >= 2 { ' ' } else { chars[i] });
            i = j;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Finish an identity candidate: collapse whitespace, shorten paths, cut to `maxlen`, then
/// repair. The prototype's `_fin`.
fn finish(s: &str, maxlen: usize) -> String {
    let collapsed = collapse_ws(s);
    let trimmed = collapsed.trim();
    let shortened = shorten_paths(trimmed);
    let cut = safe_cut(&shortened, maxlen);
    balance_identity(&cut).trim().to_string()
}

/// Every leading `IDENT=value` env assignment, in order — `value` is a double-quoted
/// string, a single-quoted string, or a run of non-whitespace (the prototype's `ENV`).
fn strip_env(s: &str) -> String {
    let mut cur = s.trim_start();
    while let Some(rest) = strip_one_env(cur) {
        cur = rest;
    }
    cur.to_string()
}

/// End positions (char count from the string start) that `(?:"[^"]*"|'[^']*'|\S*)` could
/// land on, starting at `i`, tried in the regex's own left-to-right alternative order — the
/// quoted alternative first (if `chars[i]` opens one), then the always-available `\S*` run.
/// A caller whose own trailing requirement rejects the first candidate must fall through to
/// the next one, exactly as the Python reference's regex engine backtracks into the next
/// alternative when the first one's local match cannot be extended to satisfy what follows
/// it — e.g. `PATH="$(echo "$PATH" | ...)"` has its first `"..."` alternative close at the
/// FIRST embedded quote (regex has no idea the value contains nested quoting), which is not
/// followed by whitespace, so the real match falls through to the bare `\S*` run instead.
fn env_value_ends(chars: &[char], i: usize) -> Vec<usize> {
    let n = chars.len();
    let mut ends = Vec::new();
    if i < n && (chars[i] == '"' || chars[i] == '\'') {
        let q = chars[i];
        if let Some(close) = (i + 1..n).find(|&j| chars[j] == q) {
            ends.push(close + 1);
        }
    }
    let mut j = i;
    while j < n && !chars[j].is_whitespace() {
        j += 1;
    }
    ends.push(j);
    ends
}

fn strip_one_env(s: &str) -> Option<&str> {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut i = 0usize;
    if i >= n || !(chars[i].is_ascii_alphabetic() || chars[i] == '_') {
        return None;
    }
    i += 1;
    while i < n && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
        i += 1;
    }
    if i >= n || chars[i] != '=' {
        return None;
    }
    i += 1;
    for end in env_value_ends(&chars, i) {
        if end < n && chars[end].is_whitespace() {
            let byte_off = s.char_indices().nth(end).map(|(b, _)| b)?;
            return Some(s[byte_off..].trim_start());
        }
    }
    None
}

/// Is `seg` (once trimmed) JUST one `IDENT=value` assignment and nothing else? The
/// prototype's `ASSIGN_ONLY`.
fn is_assign_only(seg: &str) -> bool {
    let s = seg.trim_start();
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut i = 0usize;
    if i >= n || !(chars[i].is_ascii_alphabetic() || chars[i] == '_') {
        return false;
    }
    i += 1;
    while i < n && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
        i += 1;
    }
    if i >= n || chars[i] != '=' {
        return false;
    }
    i += 1;
    env_value_ends(&chars, i)
        .into_iter()
        .any(|end| chars[end..].iter().all(|c| c.is_whitespace()))
}

/// Every leading wrapper (`timeout N`, `nice`, `env`, `command`, `exec`,
/// `henry-unthrottled`, `stdbuf ARG`, `caffeinate`, `sudo`), in order — the prototype's
/// `WRAP`.
fn strip_wrap(s: &str) -> String {
    let mut cur = s.trim_start();
    while let Some(rest) = strip_one_wrap(cur) {
        cur = rest;
    }
    cur.to_string()
}

/// Take the first whitespace-delimited token of `s`, requiring at least one whitespace
/// character after it, returning `(token, rest trimmed of its leading whitespace)`.
fn take_ws_token(s: &str) -> Option<(&str, &str)> {
    let ws = s.find(char::is_whitespace)?;
    Some((&s[..ws], s[ws..].trim_start()))
}

fn is_timeout_duration(tok: &str) -> bool {
    let core = tok.strip_suffix(['s', 'm', 'h']).unwrap_or(tok);
    !core.is_empty() && core.chars().all(|c| c.is_ascii_digit())
}

fn strip_one_wrap(s: &str) -> Option<&str> {
    let (first, rest) = take_ws_token(s)?;
    match first {
        "env" | "command" | "exec" | "henry-unthrottled" | "sudo" => Some(rest),
        "timeout" => {
            if let Some((tok, after)) = take_ws_token(rest) {
                if let Some(stripped) = tok.strip_prefix('-') {
                    let _ = stripped;
                    if let Some((tok2, after2)) = take_ws_token(after)
                        && is_timeout_duration(tok2)
                    {
                        return Some(after2);
                    }
                    return None;
                }
                if is_timeout_duration(tok) {
                    return Some(after);
                }
            }
            None
        }
        "nice" => {
            if let Some((tok, after)) = take_ws_token(rest)
                && let Some(numpart) = tok.strip_prefix("-n")
            {
                if !numpart.is_empty() {
                    let d = numpart.strip_prefix('-').unwrap_or(numpart);
                    if !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()) {
                        return Some(after);
                    }
                } else if let Some((tok2, after2)) = take_ws_token(after) {
                    let d = tok2.strip_prefix('-').unwrap_or(tok2);
                    if !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()) {
                        return Some(after2);
                    }
                }
            }
            Some(rest)
        }
        "stdbuf" => {
            let (_, after) = take_ws_token(rest)?;
            Some(after)
        }
        "caffeinate" => {
            if let Some((tok, after)) = take_ws_token(rest)
                && tok.starts_with('-')
            {
                return Some(after);
            }
            Some(rest)
        }
        _ => None,
    }
}

/// Alternate env/wrap stripping until stable — a wrapper can reveal a new env prefix and
/// vice versa (`timeout 5 FOO=1 cargo test`). The prototype's `_strip`.
fn strip_env_and_wrap(s: &str) -> String {
    let mut cur = s.to_string();
    loop {
        let before = cur.clone();
        cur = strip_env(&cur);
        cur = strip_wrap(&cur);
        if cur == before {
            break;
        }
    }
    cur.trim().to_string()
}

/// Does `seg` (trimmed) match `IDENT=$(...)` with nothing else? Returns the inside of the
/// `$( )`. The prototype's inline `re.match(r'^\s*[A-Za-z_]\w*=\$\((.*)\)\s*$', ..., re.S)`.
fn match_assign_dollar_paren(seg: &str) -> Option<String> {
    let s = seg.trim_start();
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut i = 0usize;
    if i >= n || !(chars[i].is_ascii_alphabetic() || chars[i] == '_') {
        return None;
    }
    i += 1;
    while i < n && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
        i += 1;
    }
    if i >= n || chars[i] != '=' {
        return None;
    }
    i += 1;
    if !(i + 1 < n && chars[i] == '$' && chars[i + 1] == '(') {
        return None;
    }
    let open = i + 2;
    let last_close = chars.iter().rposition(|&c| c == ')')?;
    if last_close < open || !chars[last_close + 1..].iter().all(|c| c.is_whitespace()) {
        return None;
    }
    Some(chars[open..last_close].iter().collect())
}

/// First occurrence anywhere in `s` of `>` (optionally followed by whitespace) then 1+
/// path-like characters — the prototype's `re.search(r'>\s*([\w./@+\-]+)', s)`.
fn find_redirect_target(s: &str) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    for i in 0..n {
        if chars[i] == '>' {
            let mut j = i + 1;
            while j < n && chars[j].is_whitespace() {
                j += 1;
            }
            let start = j;
            while j < n && is_target_char(chars[j]) {
                j += 1;
            }
            if j > start {
                return Some(chars[start..j].iter().collect());
            }
        }
    }
    None
}

fn is_target_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.' | '/' | '@' | '+' | '-')
}

/// The leading run of `[\w./+-]` characters — the prototype's `LEAD`.
fn lead_match(s: &str) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    while i < chars.len() && is_lead_char(chars[i]) {
        i += 1;
    }
    if i == 0 {
        None
    } else {
        Some(chars[..i].iter().collect())
    }
}

fn is_lead_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.' | '/' | '+' | '-')
}

fn basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// `\s*\|\|\s*(exit|true|false|return)\b[^;&\n]*`, removed globally (not quote/depth
/// aware — the prototype's own sub runs over the whole raw string, quotes and all, so this
/// mirrors that exactly rather than "fixing" it).
fn strip_error_handlers(c: &str) -> String {
    let chars: Vec<char> = c.chars().collect();
    let n = chars.len();
    let mut out = String::new();
    let mut i = 0usize;
    while i < n {
        if let Some(end) = match_error_handler(&chars, i) {
            i = end;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

fn match_error_handler(chars: &[char], i: usize) -> Option<usize> {
    let n = chars.len();
    let mut j = i;
    while j < n && chars[j].is_whitespace() {
        j += 1;
    }
    if j + 1 >= n || chars[j] != '|' || chars[j + 1] != '|' {
        return None;
    }
    let mut k = j + 2;
    while k < n && chars[k].is_whitespace() {
        k += 1;
    }
    for kw in ["exit", "true", "false", "return"] {
        let kwlen = kw.chars().count();
        if k + kwlen <= n {
            let slice: String = chars[k..k + kwlen].iter().collect();
            if slice == kw {
                let after = k + kwlen;
                let boundary_ok =
                    after >= n || !(chars[after].is_ascii_alphanumeric() || chars[after] == '_');
                if boundary_ok {
                    let mut e = after;
                    while e < n && !matches!(chars[e], ';' | '&' | '\n') {
                        e += 1;
                    }
                    return Some(e);
                }
            }
        }
    }
    None
}

/// `re.findall(r'\$\(([^()]{4,})\)', c)` — every non-overlapping `$( )` whose inside has
/// 4+ characters that are not `(` or `)`.
fn find_dollar_paren_candidates(c: &str) -> Vec<String> {
    let chars: Vec<char> = c.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < n {
        if i + 1 < n && chars[i] == '$' && chars[i + 1] == '(' {
            let mut j = i + 2;
            while j < n && chars[j] != '(' && chars[j] != ')' {
                j += 1;
            }
            if j - (i + 2) >= 4 && j < n && chars[j] == ')' {
                out.push(chars[i + 2..j].iter().collect());
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// `-[a-z]*[ce][a-z]*(\s|$)` anchored at the start of `rest` — equivalent to "a `-` then a
/// nonempty run of lowercase letters that contains a `c` or an `e`, then a boundary",
/// since the whole run either side of the required `[ce]` is itself `[a-z]*`.
fn matches_dash_ce(rest: &str) -> bool {
    let chars: Vec<char> = rest.chars().collect();
    let n = chars.len();
    if n == 0 || chars[0] != '-' {
        return false;
    }
    let mut i = 1;
    while i < n && chars[i].is_ascii_lowercase() {
        i += 1;
    }
    if i == 1 {
        return false;
    }
    if !chars[1..i].iter().any(|&c| c == 'c' || c == 'e') {
        return false;
    }
    i == n || chars[i].is_whitespace()
}

const TRIV: &[&str] = &[
    "echo", "cd", "mkdir", "export", "set", "true", "false", ":", "printf", "touch", "source", ".",
    "pushd", "popd", "umask", "unset", "wait", "sleep", "exit", "return", "pwd", "clear",
];
const INTERP: &[&str] = &[
    "bash", "sh", "zsh", "python", "python3", "node", "bun", "uv", "uvx", "ruby", "perl", "deno",
    "npx",
];

const MAXLEN: usize = 62;

fn starts_with_control_keyword(c: &str) -> bool {
    ["for", "while", "until", "if", "case"]
        .iter()
        .any(|kw| starts_with_word_boundary(c, kw))
}

fn starts_with_word_boundary(s: &str, word: &str) -> bool {
    s.strip_prefix(word)
        .is_some_and(|rest| match rest.chars().next() {
            None => true,
            Some(c) => !(c.is_alphanumeric() || c == '_'),
        })
}

/// The prototype's `identity(cmd, maxlen=62)`, recursive calls included.
pub(super) fn identity(cmd: &str) -> String {
    identity_at(cmd, MAXLEN)
}

fn identity_at(cmd: &str, maxlen: usize) -> String {
    if cmd.trim().is_empty() {
        return String::new();
    }
    let c = cmd.trim().to_string();
    if starts_with_control_keyword(&c) {
        let segs = split_segments(&c);
        let first = segs.into_iter().next().unwrap_or_else(|| c.clone());
        return finish(&strip_env_and_wrap(&first), maxlen);
    }
    let c = strip_error_handlers(&c);
    let mut fallback = String::new();
    for seg in split_segments(&c) {
        if is_assign_only(&seg) {
            continue;
        }
        let seg = match_assign_dollar_paren(&seg).unwrap_or(seg);
        let mut s = strip_env_and_wrap(&seg);
        s = s.trim_start_matches('(').trim().to_string();
        loop {
            if s.ends_with(')') && s.matches('(').count() < s.matches(')').count() {
                s.pop();
                while s.ends_with(|c: char| c.is_whitespace()) {
                    s.pop();
                }
            } else {
                break;
            }
        }
        let opens = s.matches('(').count();
        let closes = s.matches(')').count();
        if closes > opens {
            let mut to_remove = closes - opens;
            let mut result = String::with_capacity(s.len());
            for ch in s.chars() {
                if ch == ')' && to_remove > 0 {
                    to_remove -= 1;
                    continue;
                }
                result.push(ch);
            }
            s = result;
        }
        if s.is_empty() {
            continue;
        }
        let heredoc = s.contains("<<");
        let tgt = find_redirect_target(&s);
        s = strip_redirects(&s);
        if s.is_empty() {
            continue;
        }
        let Some(lead) = lead_match(&s) else {
            continue;
        };
        let verb = basename(&lead).to_string();
        if verb.is_empty() {
            continue;
        }
        let lead_char_len = lead.chars().count();
        let rest: String = s.chars().skip(lead_char_len).collect::<String>();
        let rest = rest.trim().to_string();
        if TRIV.contains(&verb.as_str()) {
            if fallback.is_empty() {
                fallback = finish(&s, maxlen);
            }
            continue;
        }
        if heredoc {
            return match &tgt {
                Some(t) => finish(&format!("{verb} > {t}"), maxlen),
                None => format!("{} \u{2039}heredoc\u{203a}", finish(&verb, maxlen)),
            };
        }
        if INTERP.contains(&verb.as_str()) {
            if matches_dash_ce(&rest) || rest.starts_with("- ") {
                return format!(
                    "{} \u{2039}script\u{203a}",
                    finish(&format!("{verb} -c"), maxlen)
                );
            }
            for tk in rest.split_whitespace() {
                let bare = tk.trim_matches(|c| c == '"' || c == '\'');
                if bare.contains("$(") || bare.contains('`') {
                    return finish(&format!("{verb} {rest}"), maxlen);
                }
                if bare.starts_with('-') || (bare.contains('=') && !bare.starts_with('/')) {
                    continue;
                }
                let basename_tk = basename(bare);
                if let Some(idx) = rest.find(tk) {
                    let tail = &rest[idx..];
                    let replaced = format!("{basename_tk}{}", &tail[tk.len()..]);
                    return finish(&format!("{verb} {replaced}"), maxlen);
                }
            }
            return verb;
        }
        return finish(&format!("{verb} {rest}"), maxlen);
    }
    for cand in find_dollar_paren_candidates(&c) {
        let got = identity_at(&cand, maxlen);
        if !got.is_empty() {
            let first_word = got.split_whitespace().next().unwrap_or("");
            if !TRIV.contains(&first_word) {
                return got;
            }
        }
    }
    if fallback.is_empty() {
        finish(&strip_env_and_wrap(&c), maxlen)
    } else {
        fallback
    }
}
