# Configuration reference

Everything `tcr` reads out of its config file, with the real default for each key. The file
is `~/.config/tcr.json`.
Back to [the README](../README.md); the command-line surface is in [cli.md](cli.md).

Cache paths below use `~/.cache/tcr/`. `XDG_CACHE_HOME` replaces `~/.cache`
when set. The usage ledger, session snapshots, peer state and logs follow the same directory.

Every default here was derived from the Rust source, not from a live config file. If you
want to check one, start from `src/config.rs`.

## The minimum viable config

Two keys are hard-required in the whole document, and both live on an account: `name` and
`accessToken` (neither carries a `#[serde(default)]`, so a missing one is a parse error).
Everything else has a default.

```json
{
  "accounts": [
    { "name": "alice@example.com", "accessToken": "sk-ant-oat01-REDACTED" }
  ]
}
```

That runs the proxy on port 3456 against `https://api.anthropic.com`, rotating the one
account at a 0.95 switch threshold, with both egress throttles on.

In practice you never hand-write that block: `tcr login` performs the OAuth flow and
writes the account for you. Hand-editing is for the *settings* keys below.

## How the file is read

The document deserializes into `Config` with `#[serde(rename_all = "camelCase")]`, so every
key is camelCase. Unmodelled keys are preserved verbatim across a load→save round trip
rather than dropped, but eight of them are also *read*, and are documented in their own
section below. Because a misspelled key is kept and does nothing, `tcr doctor` names every
top-level key this build does not recognise, with its nearest known spelling, and the server logs
them once at boot ([`tcr doctor`](cli.md#config-keys-tcr-does-not-recognise)). The list of known
keys is `src/config_keys.rs`.

## Top level

| json key | type | default | required | what it does |
|---|---|---|---|---|
| `proxy` | object | `{}` (all sub-defaults) | no | listener settings, see below |
| `upstream` | string | `"https://api.anthropic.com"` | no | Anthropic API base URL every rotated Claude request is sent to (Codex requests go to `codexUpstream`) |
| `codexUpstream` | string | `"https://chatgpt.com"` | no | where Codex accounts' requests are sent, where their quota probe goes, where `tcr login --from-codex` checks a login's workspace before importing it, and where every other request under `/backend-api/` is relayed, under the client's own bearer when it sent one; written to the file only when it differs from the default. Their token refresh goes to `https://auth.openai.com` whatever this says |
| `switchThreshold` | float | **`0.95`** | no | fraction of an account's quota at which rotation prefers a different account |
| `pacing` | object | both knobs unset → **OFF** | no | per-account concurrency/spacing, see below |
| `accountThrottle` | object | `{minSpacingMs: 350, burst: 24}` → **ON** | no | per-organization egress rate limiter — the real one, see below |
| `fleetThrottle` | object | `{minSpacingMs: 100, burst: 32}` → **ON** | no | separate fleet ceiling for each provider, insurance only, see below |
| `lockAccount` | string | absent → normal routing | no | pin its provider's pooled traffic to one account by `name`; the other provider rotates normally, see below |
| `controlIdentity` | `"refuse"` \| `"warn"` \| `"off"` | **`"warn"`** | no | what to do with a client whose own login is not `controlAccount`, see below |
| `http1Only` | bool | **`false`** (OFF) | no | force the upstream client onto HTTP/1.1, see below |
| `pricing` | object | `{}` → the built-in table only | no | per-model price overrides for the usage figures, see below |
| `usageRetentionDays` | u32 | **`90`** | no | how many days of usage ledger files to keep |
| `resetUrgencyTierHours` | u32 | **`24`** | no | width of the reset-urgency bucket rotation ranks by inside a priority tier; `0` disables it, see below |
| `accounts` | array | `[]` | no | the rotatable accounts |

`switchThreshold` is **0.95**, not 0.90. The default function is
`default_switch_threshold()` and it returns `0.95`; the older README said 0.90, which was
never the shipped value.

## `pricing` and `usageRetentionDays`

`tcr status --json` reports what each account's traffic **would have cost at the API list price of
its model's own provider**. The accounts are subscriptions, so nothing here is a bill; it is the only
unit that compares across accounts, models and days.

Prices come from a table built into the binary (`src/pricing.rs`), matched by longest prefix so a
dated id like `claude-haiku-4-5-20251001` resolves to its family. A model the table has no published
rate for is reported as `costUsd: null` with `unpricedRequests` counting it — never as `$0.00`, which
would be a spend nobody measured presented as one. No OpenAI model has a built-in price, so a Codex
account's traffic is unpriced until `pricing` names its model, and `tcr sessions` and `tcr wrap`
print `n/a` for it, never `$0.00`.

`pricing` is how you add or change a rate. Keys are a model id or any prefix of one; `input` and
`output` are USD per million tokens and both are required. The three cache dimensions default to the
same multipliers the built-in table uses — cache read 0.1x input, a 5-minute cache write 1.25x, a
1-hour cache write 2x — except that for an OpenAI model id (`gpt-…` or `codex-…`) both cache writes
default to 1.0x input, because codex reports a cache write inside its input tokens and OpenAI adds no
write premium. Each can be set explicitly:

```json
"pricing": {
  "claude-sonnet-4-5": { "input": 3, "output": 15 },
  "claude-opus-5": { "input": 5, "output": 25, "cacheRead": 0.5 },
  "gpt-5.1-codex": { "input": 1.25, "output": 10 }
}
```

An override beats the built-in table outright, even when its key is the shorter prefix: a price you
wrote down is what this proxy should believe.

`usageRetentionDays` bounds the ledger under `~/.cache/tcr/usage/`, one `<UTC-date>.jsonl` file
per day at roughly 2 MB per 17k requests. Files older than this are deleted once, at boot. Boot reads
back today's and yesterday's files (that is what makes a restart keep the day's totals), and, when the
fleet holds a Codex account, the six before them too, which is what keeps a Codex account's weekly
window across a restart; the rest are history for you to grep.

Lines are written by a dedicated thread, off the request path, so a stalled disk costs accounting and
never latency. A clean shutdown drains that thread's queue and waits for it to close the file, so the
requests served in the final moments before a restart are in the day it replays. Two things can still
cost lines — a write that fails, and a queue that fills faster than the disk drains it — and both are
said out loud at the moment they start, and again when they stop: the server logs the transition with a
running dropped-line count, and reports `persisting=false` for as long as it lasts — rather than a short
day presented afterwards as a measured one.

`0` is the floor, and it keeps today AND yesterday: those two files are what the boot replay reads, and
the pruner never deletes them whatever this is set to. It has to be both — on a UTC+3 machine the first
three hours of the local day live in yesterday's UTC-named file, so deleting it would cost the second
restart of the day those hours, silently and while still reporting them as measured. A fleet holding a
Codex account keeps today and the seven days before it whatever this says, for the same reason: those
are the files the boot replay reads to rebuild the weekly window.

**"Today" is the local calendar day of the machine the server runs on**, resolved per record against
that record's own instant, so a day boundary is correct across a daylight-saving change. Ledger FILE
names are UTC dates and need not agree with it — the file name is a shard key, not a claim about your
calendar. If the machine's UTC offset cannot be read at all, day boundaries fall back to UTC and the
server says so once in its boot log.

## `proxy.*`

| json key | type | default | required | what it does |
|---|---|---|---|---|
| `proxy.port` | u16 | `3456` | no | port the proxy binds on loopback |
| `proxy.apiKey` | string | absent | no | shared secret required by the local `/_tcr/` control routes; the request path exempts loopback callers, and it is never handed to `claude` |
| `proxy.requestKey` | string | written at first boot | no | the request key: what a caller presents to say it was launched on purpose. Counted, never required, in this version. `tcr request-key` prints it |

`--port` beats the file. `tcr server --port 9000` overrides `proxy.port` at runtime; the
flag is threaded into `ServeOptions.port` and wins wherever it is `Some`.

### `proxy.apiKey` is a security control, not a convenience

When it is set, it is **required on every request to the `/_tcr/` control routes, with no
loopback exemption**. That is deliberate and stricter than the request path: the doc
comment on `local_endpoint_gate` spells out why. The ordinary proxy path exempts loopback
because `claude` authenticates with its own OAuth and never sends the proxy key, but
nothing on the machine has any business reading or steering the fleet without the
operator's secret, so `/_tcr/status` and `/_tcr/accounts/disabled` cost the same secret
that using the proxy does. The check is a constant-time compare of the `x-api-key` header;
a miss is a 401.

Binding to loopback is not authorization. `127.0.0.1` is reachable by every process and
every container on the host, which is exactly the reasoning in that comment. If you set
`proxy.apiKey`, `tcr enable` and `tcr disable` need it too; they reach the running proxy
through the same gate, and a rejected key makes them refuse rather than silently fall back
to a file write.

**`tcr run` does not give this value to `claude`.** It used to export it as
`ANTHROPIC_API_KEY`, and that made Claude Code take an API key as its auth source ahead of
its claude.ai login, which **disables every claude.ai connector**, announced in one
startup line that scrolls away, after which the tools are simply absent. It bought nothing:
the request path exempts loopback clients from this key, so the child was always exempt
anyway. When a key is configured, `tcr run` says on stderr that it is withholding it; a
value the caller exported is inherited untouched. So the key's whole job is the `/_tcr/`
gate above; it is not a credential anything downstream of the proxy needs.

### `proxy.requestKey`: who launched this caller

Binding to loopback does not keep other callers out. A container on this Mac that connects to
`host.docker.internal` arrives at the proxy from `127.0.0.1`, so the loopback exemption above
serves it, and a `CONNECT` through the proxy opens a tunnel for it. The request key is what will
tell a caller tcr launched from one it did not.

The server writes it. On a boot with no `proxy.requestKey` in the file, it generates 32 random
bytes, writes them as 64 hex digits into the file, changing nothing else, and logs one line saying
it did (never the key). Every later write the server makes keeps it. A server started with no
config file to write (a library caller) holds no key. Delete the key and the next boot writes a
new one; every launcher that held the old one then needs the new one.

It is separate from `proxy.apiKey` on purpose: `apiKey` gates the `/_tcr/` control routes, and a
container forwarder that holds the request key must not get control access with it.

A caller presents it in any one of these places:

| where | for | what |
|---|---|---|
| proxy URL userinfo | `HTTPS_PROXY` clients | `http://<user>:<key>@127.0.0.1:3456`, sent as `Proxy-Authorization: Basic` on every `CONNECT`. The user name still routes: `tcr-codex` selects Codex interception with or without a key, any other name means Claude |
| `x-tcr-key: <key>` | base-URL clients | Claude Code sends it through `ANTHROPIC_CUSTOM_HEADERS` |
| `x-api-key: <key>` | SDK clients with no login | the SDK's `ANTHROPIC_API_KEY` |
| `Proxy-Authorization` | `HTTP_PROXY` clients | the same `Basic` value as a `CONNECT` |

`tcr run` presents it for you, on a server that reports it understands keys, and the peer
lender presents it on its relay into its own proxy; see
[the request key on a launch](cli.md#the-request-key-on-a-launch).

In this version nothing is refused. A caller without the key is counted on `GET /_tcr/status`
(`unkeyedConnects`, `unkeyedRequests`) and served exactly as before; see
[the request key on `GET /_tcr/status`](cli.md#the-request-key-on-get-_tcrstatus). The key is
stripped before anything leaves the proxy: `x-tcr-key` never reaches Anthropic, a passthrough
relay or a peer, and an `x-api-key` equal to the key is treated like one equal to `apiKey`
(never forwarded, never used as a session identity).

## `accounts[]`

| json key | type | default | required | what it does |
|---|---|---|---|---|
| `name` | string | n/a | **yes** | the account's name, and the only way any command addresses it. **Unique across the file** — `tcr` renames duplicates on load; see `docs/cli.md` § "Account names are unique". `tcr login` writes the account email here, or `email/<org-slug>` when another row already carries that email |
| `type` | string | `"oauth"` | no | `"oauth"` accounts are refreshed, probed and, on Anthropic, kept warm; `"apikey"` rows are quarantined at load, because tcr serves subscription logins only (a Claude subscription, or a Codex row's ChatGPT login), never API keys; any other string is a static-key account, kept as written |
| `provider` | `"anthropic"` \| `"codex"` | `"anthropic"` | no | which upstream this credential belongs to. Absent means Anthropic, and the key is never written for an Anthropic row. A Codex row (`tcr login --codex` and `tcr login --from-codex` write one) is refreshed against `auth.openai.com`, probed at `/backend-api/wham/usage`, never kept warm, and serves Codex model calls under `/backend-api/codex/` (`responses`, `images/generations`, `images/edits`, `memories/trace_summarize` and `alpha/search`): a Claude request never reaches it, and a Codex request never reaches a Claude row |
| `accountUuid` | string | absent | no | spliced into the outbound body's `metadata.user_id.account_uuid` so it agrees with the injected token. For a Codex row it is the ChatGPT user id, spliced nowhere: what tcr sends is the workspace in `orgUuid` |
| `orgUuid` | string | absent | no | organization identity, used to match an in-memory account back to its on-disk entry. For a Codex row it is the ChatGPT workspace id, the login's ChatGPT account id, sent as the `chatgpt-account-id` header. A Codex row is that pair, user and workspace, so one person in two workspaces is two rows, and so are two seats of one workspace |
| `orgName` | string | absent | no | organization display name; also what a duplicated `name` is renamed from (`Henry Token` → `henry-token`) |
| `organizationType` | string | absent | no | the plan, verbatim as the provider names it, from which `tcr status` derives its `plan` label: `claude_max`, `claude_team`, `claude_pro`, … from Anthropic's profile endpoint, written at login and backfilled by the quota probe on a row that lacks it. For a Codex row it is the ChatGPT plan, verbatim (`pro`, `plus`, `team`, …): read from the login's `chatgpt_plan_type` claim at import, and kept current by the quota probe, which rewrites this key, and no other, when the plan changes. A word tcr does not know is labelled `ChatGPT {word}`, the word as ChatGPT wrote it (`edu` gives `ChatGPT edu`) |
| `accessToken` | string | n/a | **yes** | the OAuth access token |
| `refreshToken` | string | absent | no | used to mint a new access token before expiry — absent means the account cannot refresh itself, see below |
| `expiresAt` | i64 | absent | no | access-token expiry as **epoch milliseconds** |
| `priority` | i64 | absent → `0` | no | rotation order, **lower value = preferred** |
| `switchThreshold` | float | absent → the global value | no | per-account override of the top-level threshold |
| `disabled` | bool | absent → `false` | no | held out of rotation; this is the key `tcr disable` writes |
| `egress` | `"local"` \| `"via <peer>"` | `"local"` | no | pins where this account's requests leave from. `local` is today's behaviour: they leave from whichever Mac is currently serving them. `via <peer>` always sends them out through one named peer instead, so the account keeps a single IP no matter which Mac in the mesh runs it; that peer carries the bytes blind and never sees what is inside them. Not on a Codex account in this version: its pin is treated as a peer that cannot be reached, see `egressStrict`. Local to this file, never sent over the wire |
| `egressStrict` | bool | `false` | no | what to do when `egress` names a peer and that peer cannot be reached: `true` refuses the request by name instead of falling back; `false` (default) sends it from this Mac instead, with a log line saying so |
| `codexHome` | string | absent | no | Codex rows only, written by `tcr`: the folder the login was imported from by `tcr login --from-codex`, as an absolute path, so TcrBar can offer to import from it again. A login of tcr's own (`tcr login --codex`) records none, and signing a row in that way removes the key. A standalone `TCR_CODEX_AUTH` file not named `auth.json` also records none and has no automatic write-back |
| `credentialVersion` | u64 | absent → backfilled on the next save | no | written by `tcr`, not by hand: `1` on an Anthropic row, `2` on a Codex row. A build refuses the whole file when a row carries a version above what it understands for that row's provider. Upgrade every `tcr` that may boot on the file before importing a Codex login: what an older one does with a Codex row is in [troubleshooting](troubleshooting.md#after-a-codex-import-an-older-proxy-refuses-to-load-the-config) |

**A Codex row whose `accountUuid` holds the workspace id and that has no `orgUuid`** is the shape
before the ChatGPT user was stored. It keeps serving, and tcr sends its `accountUuid` as the
workspace. But no login matches it, neither a re-import nor `tcr login --codex --account`,
because a login is matched on its ChatGPT user and workspace and the row names no user. To bring
it to the current shape, `tcr remove` it, then add it again with `tcr login --codex` or
`tcr login --from-codex`, in that order: a login added first finds its name held by another
identity, and lands beside the old row as `<email>/codex`, while one added after the removal
takes the old row's name back. A running proxy takes the removal live, with no restart.

`type` is not decorative. Only `"oauth"` accounts get token refresh, quota probing and, on
Anthropic, keep-warm. tcr serves subscription logins only, a Claude subscription or a Codex
row's ChatGPT login, never API keys, so an `"apikey"` row is quarantined at load with a
warning, and the rest of the config loads. Any other string is treated as a static key and
written back exactly as it was; a `type` that is not a string quarantines its row.

Per-account keys the proxy does not model (`models`, `upstream`, `sx`, any other key) parse fine and survive a load→save round trip untouched, but
nothing reads them.

**An account with no `refreshToken` is not broken, but it is on a clock.** It serves
requests normally until `accessToken` expires, then goes dead — there is nothing for the
refresh loop (`src/manager/refresh.rs`) to mint a new access token from, so it skips the
account cleanly rather than erroring, and it stays skipped. The only account shape that
produces this today is one added via `tcr login --token` (`docs/cli.md` §"`--token`: adding
a `claude setup-token` credential"): that credential's `user:inference`-only scope means the
token endpoint's refresh token is never surfaced to `tcr` at all, so there is genuinely
nothing to store. `tcr login --token` prints this hazard on every successful add, and stamps
`expiresAt` with its own best estimate of when that clock runs out.

## `resetUrgencyTierHours`: spend the quota that is about to expire

Unused weekly quota is worth nothing once its window resets. Accounts do not reset together —
measured on a 13-account fleet on 2026-08-26, every account sat at priority 0 and their weekly
resets were spread from 2.5 hours to 152.5 hours out, with the soonest-resetting account holding
44% of a bucket that would be gone by morning and the furthest holding 92% it had five days to
spend. Plain least-recently-used rotation spreads requests evenly over both, which wastes the
first account's headroom to preserve headroom that was not under any time pressure.

So rotation ranks inside a priority tier by **which bucket an account's governing weekly reset
falls into**, before the least-recently-used tick. At the default 24-hour width that fleet
splits like this:

| bucket | accounts | resets in |
|---|---|---|
| 0 | four | 2.5h, 11.5h, 13.5h, 18.5h |
| 1 | two | 27.5h, 33.5h |
| 2 | two | 49.5h, 52.5h |
| 3–6 | five | 79.5h … 152.5h |

Bucket 0 absorbs today's traffic, and the four accounts in it still fan out among themselves on
the usual least-recently-used order. As each one crosses its threshold it gates out normally and
bucket 1 takes over.

**The width is the whole design, not a tuning detail.** A raw reset instant is millisecond-precise
and essentially never ties, so ranking on it directly would retire the least-recently-used tiebreak
altogether and park every unpinned request on one account until it gated — the same deterministic
pin that ordering by quota headroom was rejected for, and the pool pick carries no in-flight term
to dilute a burst. Bucketing re-creates the tie. Narrower widths concentrate load (at 6 hours the
soonest account sits alone in its bucket, which is the raw behaviour with extra steps); wider ones
weaken the signal (at 48 hours, six of those thirteen collapse into one bucket).

Two behaviours are deliberate:

- An account whose weekly window has **no known live reset** sorts ahead of every bucket, so it
  gets picked and therefore probed. This is the pre-existing "unknown quota → probe it" rule, not
  a new one.
- Nothing starves. Spending an account does not move its reset, but crossing that reset starts a
  fresh window about seven days out, which drops the account to the back of the bucket order. Each
  account's window comes due in turn.

Set `"resetUrgencyTierHours": 0` to disable the term entirely and restore the previous
`(priority, least-recently-selected, reset)` ordering — a config-only rollback, no rebuild.

Like every key here except `groups` and `groupSettings`, this is read **once at boot**. An edit
needs a restart before the serving process rotates by it, and `tcr status` reports the running
process's view rather than the file's.

## `pacing.*`: opt-in, ships OFF

| json key | type | default | required | what it does |
|---|---|---|---|---|
| `pacing.maxInFlightPerAccount` | u32 | unset → no cap | no | an account at or over this many concurrent requests is skipped in selection |
| `pacing.minSpacingMs` | u64 | unset → no spacing | no | minimum gap between two selects of the *same* account |

An absent `pacing` key and a literal `"pacing": {}` are identical, and both are inert. A
configured `0` for the cap is normalised back to "unset"; `Some(0)` would make
`in_flight >= 0` true for every account and hold out the entire fleet permanently.

It ships off on purpose. The doc comment on the pacing settings gives the reason: a
per-account concurrency cap trades prompt-cache locality for load spread, and on a
single-user proxy the cache is the scarce resource while the accounts are not. Turning it
off leaves per-account concurrency genuinely unbounded; the throttle buckets below are
*rate* limiters and are explicitly not a substitute for a concurrency bound.

## `accountThrottle.*` and `fleetThrottle.*`: both ship ON

There are two throttle buckets, and they are keyed on different things.

| json key | type | default | required | what it does |
|---|---|---|---|---|
| `accountThrottle.minSpacingMs` | u64 | **`350`** with the key absent | no | steady-state emission interval **per organization** |
| `accountThrottle.burst` | u32 | **`24`** with the key absent | no | sends admitted instantly after that org has been idle |
| `fleetThrottle.minSpacingMs` | u64 | **`100`** with the key absent | no | steady-state interval across every org of **one provider** combined |
| `fleetThrottle.burst` | u32 | **`32`** with the key absent | no | sends admitted instantly after that provider's fleet has been idle |

`accountThrottle` is the real limiter. Anthropic sets rate limits at the **organization**
level, so pacing per org is what matches the thing actually being limited, and total capacity
scales *with* the size of your account pool rather than being divided across it. A Codex row's
organization is its ChatGPT workspace, so the seats of one workspace share one bucket.

`fleetThrottle` is insurance. It exists only in case something shared — an IP, a client id —
is also limited, which nobody here has measured. Its burst sits above the per-organization
burst, so it never holds back a wave that bucket lets through, and near the fleet's real peaks,
so it still engages on a runaway. Each provider has an independent fleet bucket: Claude sends
never consume Codex's burst budget, and Codex sends never delay Claude through this ceiling.

> **This replaced a single fleet-wide `throttle` key.** A config still carrying `throttle` is
> **migrated automatically at load** — the file self-heals the first time the server boots
> with it, so a customer install that auto-updates onto the new binary never has to hand-edit
> anything. The migration branches on whether the old key was active:
>
> - `throttle` **active** (has an effective `minSpacingMs`) → becomes `accountThrottle`,
>   verbatim — that is what the single old key actually did, a per-organization limiter.
>   `fleetThrottle` is left absent and takes its own default.
> - `throttle` **inert** (`{}`, a `burst` with no spacing, or `minSpacingMs: 0`) → both
>   `accountThrottle` and `fleetThrottle` are set to `{}`. The old key's escape-hatch form
>   meant "no throttling at all", and no single new key means that any more, so an inert
>   legacy key has to disable both buckets to preserve that intent.
>
> If a new key is already present alongside `throttle` (a half-run migration), the new key
> wins and `throttle` is dropped. If `throttle` holds something that does not parse as a
> throttle config at all, it is dropped. Every case loads and boots — there is no input on
> which this key causes a load error any more.

### The absent-versus-empty inversion

These are the keys in the file where leaving one out and writing an empty object do
*opposite* things, and it surprises everyone:

- `accountThrottle` **absent** → `default_account_throttle()` → **ON** at
  `minSpacingMs: 350, burst: 24`. Same for `fleetThrottle` → `100` / `32`.
- `"accountThrottle": {}` **present and empty** → every knob deserializes to `None` →
  `is_active()` is false → that bucket is **OFF**.

So the way to disable a bucket is to write its key, not to delete it. Deleting it turns that
bucket back on.

**Disabling one bucket does not disable the other.** To go fully unthrottled you need both:

```json
"accountThrottle": {},
"fleetThrottle": {}
```

A fresh proxy is therefore rate-limited out of the box. Inside a present throttle object an
unset `burst` is clamped to `1` (strict spacing), not to the default.

### On the 350ms

It is worth knowing that this number is **not measured for the path it governs**. It
originates as `PROBE_SPACING` (`src/probe.rs`), which was proven against `/api/oauth/usage`
— the usage-probe endpoint, not `/v1/messages` — and was transplanted onto the serve path as
a conservative starting value. Nobody has established where `/v1/messages` actually starts
returning 429s, because finding that limit means crossing it, and a real 429 writes shared
account state whose cost lands on every session rather than just the one probing. Treat 350
as an unknown held deliberately tight, not as a limit anyone has observed.

The per-org split was designed so this gap does not have to be closed: per-identity pacing is
unchanged from the value already running in production, and only the fleet ceiling loosened.

## Keys read from the unmodelled map

These eight are not fields of `Config`. They land in the flattened `extra` map and are read
back out by name across the manager (see `src/manager/state.rs` and `src/manager/throttle.rs`).
They are fully live config with a caller in the boot or request path, not compatibility
leftovers.

| json key | type | default | required | what it does |
|---|---|---|---|---|
| `quotaProbeSeconds` | i64 seconds | **`300`** | no | quota probe cadence, the CENTRE of a per-account random draw, not a fleet period; halved for an account held out by its own quota; at most 20 probes per account in any trailing hour; `<= 0` disables probing |
| `warmupSeconds` | i64 seconds | **`0`** (OFF) | no | keep-warm cadence, drawn the same per-account random way; `<= 0` spawns no warm task |

### Both cadences are random per account, not a fleet sweep

Neither `quotaProbeSeconds` nor `warmupSeconds` is a period the fleet fires on. Each is the
**centre of an independent random draw per account** (`src/schedule.rs`):

| draw | distribution | at the 300s default |
|---|---|---|
| initial offset (first fire after boot, and whenever an account becomes eligible) | uniform over `[0, cadence]` seconds | `0..=300s` |
| every subsequent interval | uniform over `[cadence - 30%, cadence + 30%]` seconds, floored at 1s | `210..=390s`, 181 distinct values |

Consequences worth knowing before you tune either number:

- Two accounts do not share a probe instant, and a restart re-draws every offset rather
  than re-anchoring the fleet's phase on boot time.
- A single account's gap between probes is never the number you configured; it is a fresh
  draw each time, centred on it. Halving `quotaProbeSeconds` halves the centre, not a period.
- An account at or over its switch threshold, or one upstream answered `rejected`, is probed
  around HALF the cadence (`105..=195s` at the default). So is a Codex account ChatGPT refused
  (its row carries `limitReachedType`), whatever gate its row shows, except `disabled` and
  `login`, which outrank the refusal. It is out of rotation, so no served response refreshes
  its quota and only a probe sees its window roll. An account serving traffic is not probed any
  faster: every response's headers already rewrite the same windows a probe reads.
- No account is probed more than 20 times in any trailing 60 minutes, whatever the cadence.
  The usage endpoint's budget was measured by claude-swap at about 28 to 30 requests per
  identity in a trailing hour, not a refilling bucket. A `quotaProbeSeconds` under 180 is
  therefore held to that cap rather than run as configured. The boot sweep, a probe's one
  `401` retry and `tcr accounts --probe` are not counted, which is why the cap sits well
  under the measured figure.
- The probe (not keep-warm) still does ONE whole-fleet sweep at boot, spaced 350 ms, so the
  bars populate immediately instead of after a random offset.
- Randomness is drawn from a SplitMix64 generator seeded once from the boot clock. There is
  no `rand` dependency, and per-account draws are decorrelated by construction; re-reading
  the clock per account in a tight loop would not have been.
| `sessionAffinity` | bool | **`true`** (ON) | no | pin a session to the account it started on |
| `revalidationServe` | bool | **`true`** (ON) | no | serve over-threshold rather than synthesizing a 429 when the whole fleet reads over the soft threshold |
| `loadBalanceMigration` | bool | **`false`** (OFF) | no | move an already-warm session to a cooler account to even out pinned-session counts |
| `cacheTtlRewrite` | bool | **on unless some account's `type` is a string tcr does not recognize** | no | rewrite every `cache_control` breakpoint to `"ttl":"1h"` before forwarding. Measured 2026-10-06: a 1h write costs a subscription's 5-hour window what a 5m write does and a read costs it nothing visible, so the hour is free and spares the full re-write after a 5-60 minute idle gap. A row of an unrecognized `type` is a static key tcr cannot classify and may be billed at the published rates, where a 1h write is 2x input against 1.25x, so a fleet holding one forwards bodies as sent unless this is set `true`. An `apikey` row cannot turn it off, because it never loads. `tcr wrap` still prices 1h writes at the API's 2x, so its dollar figure rises while a subscription's quota does not |
| `throttleExemptNoise` | bool | **`false`** (OFF) | no | let `Noise`-classified traffic (`/api/event_logging*`, `/mcp-registry*`) skip the **per-org** bucket instead of queueing behind inference. It still pays `fleetThrottle`, so exempt traffic is never entirely unpaced. Only a request with **no client bearer** is classified at all — one that carries its own is relayed under it and never enters a bucket (see [`controlIdentity`](#controlidentity-the-client-must-be-the-control-account)) |
| `divertBudget` | u32 | **`0`** (unlimited) | no | max distinct destination accounts one session may be diverted to inside a single hold episode; `0` = unlimited, today's behaviour byte-for-byte |
| `claudeCodeImport` | bool | absent (import allowed) | no | `false` stops an empty fleet from importing this Mac's Claude Code login on its own. `tcr remove` writes it when it deletes the last account, so the next `tcr status` (TcrBar polls it every few seconds) does not put the login straight back. `tcr login --from-claude-code` still imports |

### `sessionAffinity` also pins identity-less loopback requests

"Pin a session to the account it started on" normally means keying on an `x-api-key` or the
body's `metadata.user_id` — a stable per-client identity. For a remote client the key comes
first; for a **loopback** client the `user_id` comes first, because a loopback key is whatever
the launcher exported and can be one placeholder shared by every `claude` on the box (measured
2026-09-22: 34 coders under one `ANTHROPIC_API_KEY`, routed as one session). With `sessionAffinity` on, a
**loopback** `POST /v1/messages` that carries **neither** — a plain SDK call or a bare
`curl`, not `claude`, which always sends `metadata.user_id` — is still pinned, on a hash of
its `system` + `tools` fields (its Anthropic prompt-cache prefix) instead of a client
identity. Byte-identical requests land on the same account and keep the prompt cache warm;
different requests spread across the fleet on their own.

This only applies to a loopback caller on `POST /v1/messages` specifically —
`/v1/messages/count_tokens` and every other endpoint are excluded, since Anthropic does not
apply prompt caching to token counting, so pinning it would only concentrate load for no
cache benefit. A caller whose peer address the OS reports as non-loopback (this proxy binds
`127.0.0.1` only, in practice) never gets this pin, so N workers sharing one remote harness
and one system prompt still spread across the fleet rather than collapsing onto one account.

"Loopback" here means whatever socket address the OS hands the listener — the same check
the `proxy.apiKey` loopback exemption above ("`proxy.apiKey` is a security control, not a
convenience") already relies on. A TCP forwarder terminating locally
(`ssh -L 3456:localhost:3456 host`, for instance) makes
every tunneled remote caller present as `127.0.0.1` to this process, just as it already
bypasses the `proxy.apiKey` exemption — this is an existing property of binding to loopback,
not something this tier introduces, but it does mean the collapse this section describes as
excluded for remote callers is not excluded for one tunneled through a locally-terminating
forwarder.

There is no separate flag for this: it is entirely governed by `sessionAffinity`, and turning
that off turns this off too.

### Which knobs are opt-in and which are opt-out

Seven knobs ship OFF and must be turned on deliberately: `warmupSeconds`,
`loadBalanceMigration`, `pacing`, `lockAccount`, `http1Only`, `throttleExemptNoise` and
`divertBudget` (whose off state is `0`, unlimited, today's behaviour). (An older README said
three; `pacing` and `lockAccount` were missing from that list. `sessionAffinity` was on this
list too until it flipped to default ON — see below.)

Two knobs ship ON and are opt-**out**: `revalidationServe`, default `true`, disabled by
writing `"revalidationServe": false`; and `sessionAffinity`, also default `true`, disabled by
writing `"sessionAffinity": false`. So is `throttle`, per the inversion above, though its
off-switch is an empty object rather than a `false`.

`loadBalanceMigration` is off because Anthropic's prompt cache is per-account: every
balancing move costs a full prompt-cache re-creation of the whole conversation prefix on
the target account. The reasoning is in the doc comment on that knob.

### `warmupSeconds` spends real quota

Keep-warm is not a free health check. It issues actual upstream requests on your accounts
on a timer, and that consumption counts against the same quota your sessions draw on,
which is exactly why it defaults to `0` while the probe defaults to `300`. The doc comment
says it outright: it "spends real quota, so it ships dark and is only ever running when
explicitly enabled". Set it only when you have measured that a cold prefix costs you more
than the warming requests do.

It never warms a Codex account: a warm is an Anthropic request, so only Claude accounts are kept
warm, whatever the fleet holds.

## `lockAccount`: read this before setting it

`lockAccount` pins **all** pooled traffic of its account's provider to the single account whose
`name` it names. LRU rotation, session affinity and load-balancing migration are all bypassed.
The lock applies only to that account's provider: a Claude lock leaves the Codex pool
available, and a Codex lock leaves Claude rotation unchanged. The resolved account slot is
authoritative even if another row shares its display name. Calls relayed under the client's
own credential never ask for an account and are unaffected.

The part that bites: a locked account has **no failover**. If it is throttled, disabled or
down, requests fail: the proxy does not rotate away from it, because rotating away is the
behaviour you turned off. On a fleet of one that is no loss; on a fleet you built for
resilience it removes the resilience entirely. Treat it as a debugging tool.

A name matching no account is not a startup failure: the proxy logs an error naming the
available accounts and runs **unlocked**. So a typo'd `lockAccount` looks exactly like an
ordinary rotating proxy unless you read the log at boot.

## `groups[]` and `controlAccount`: the combination that routes nothing

`accounts[].groups` labels an account. A client asks for a label with `tcr run --group
<label>`, which sets the `x-tcr-group` header on its requests, and selection then **prefers**
accounts carrying that label — and, when none can serve, is **strict** about it: `tcr run
--group gil` is a request to be served by `gil`'s accounts or not at all, never quietly by
whichever account happens to be free.

Prefers, but requires on exhaustion (2026-09-13). If some account in the group can serve the
request, that account wins over the whole rest of the pool, even a better-priority account
outside the group — that half has not changed. But if **no** account in the group can serve
it, the proxy no longer falls back to the whole pool by default: it refuses, and the caller's
own retry/backoff (or an honest 429) is what happens next, exactly the way a request against
an exhausted, ungrouped fleet already behaves. The refusal logs one line naming the group and
the reason, at the moment it happens. Before 2026-09-13 this fell back to the whole pool
instead — see [`spillToPool`](#spilltopool-restoring-the-old-fall-back-to-the-pool-behaviour)
below if you relied on that.

The trap is `controlAccount`. That account is the identity plane's designated account, and by
default inference requests **never** select it — not when it is enabled, not when it is idle.
That is the point: it exists so login and quota bookkeeping keep coming from one stable
account, and letting inference spend its quota is how you lose that. (Which account the
connectors, plugins and settings come from is decided by the CLIENT's own login, not by this
setting — see [`controlIdentity`](#controlidentity-the-client-must-be-the-control-account)
for how the two are held together.) So by default a group
whose only member is the control account can never serve inference. Every request asking for
it refuses, on every request. Nothing about the group looks wrong: it has a member, a
colour, and a healthy line in `tcr status`.

Two opt-ins lift the exclusion, both off by default, and both keep the `controlReserve` floor
below in force: `groupSettings.<label>.allowControlAccount` for one group, and
[`controlPooled`](#controlpooled-spends-the-control-accounts-quota) for the whole pool.

`tcr group ls` names this directly:

```
group research accounts=1 members=gil@example.com reserved=no parked=no routes=no route_block=control-account-only ...
group dev      accounts=2 members=a@example.com,b@example.com reserved=no parked=no routes=yes ...
group lab      accounts=1 members=bob@example.com/codex codex_members=1 reserved=no parked=no routes=no route_block=codex-only ...
```

`routes=no` means the group exists and serves nothing. Four ways out: add a second, ordinary
account to the group; move the control account elsewhere with `tcr control --clear`; let this
one group use it with `tcr group allow-control <label>`; or let the whole pool use it with
`controlPooled`. `tcr group add` prints the same warning at the moment you create the
situation.

`routes=` answers whether a Claude session asking for the group can be served by it, so it
counts Claude accounts only: no Codex request carries a group. A group with a Codex member
gains `codex_members=<n>` right after `members=`, and a group whose members are all Codex
accounts reads `routes=no route_block=codex-only`. `--json` carries the same two facts, as
`codexMembers` (present only when above zero) and `routeBlock: "codex-only"`. A fleet with no
Codex account lists exactly what it always did. What a Codex member of a group does and does
not do is [below](#codex-accounts-in-a-group-labelled-and-parked-never-routed).

Reserving solves a different problem, and is no longer the only way to get strictness — every
group is strict on its own exhaustion by default now, `reserved` or not. `tcr group reserve
<label>` makes an account carrying that label off-limits to traffic that did *not* ask for one
of its groups: the group's own `--group <label>` ask can still reach it, but nothing else can.
That inbound half is what `reserved` means today. It does not additionally loosen the
outbound half — a reserved group ignores `spillToPool` (below) and stays strict on its own
traffic even if you set it, because spilling a reserved group's traffic into the pool would
hand it to an account other traffic cannot reach any other way, which defeats the point of
reserving it.

### `spillToPool`: restoring the old fall-back-to-the-pool behaviour

`groupSettings.<label>.spillToPool: true` is the escape hatch back to the pre-2026-09-13
default: with it set, a `--group <label>` request whose members are all unavailable falls
back to the whole pool instead of refusing, the same way an ordinary (ungrouped) request
does. Absent or `false` (the default) is strict.

```json
{
  "groupSettings": {
    "codereview": { "spillToPool": true }
  }
}
```

It has no effect on a `reserved` group — see above. It is read live, the same
`groupSettings` hot-reload cadence as `reserved`/`parked`/`allowControlAccount`: editing it
in the config file takes effect on the next request, no restart.

### `parked`: a whole group out of rotation

`tcr group park <label>` holds **every** account carrying that label out of rotation, the way
`tcr disable` holds one account out: no request reaches it, not even an explicit `--group
<label>` ask. `tcr group unpark <label>` releases them. It is one `groupSettings` key, so a
running proxy picks it up on its next cadence check — no restart.

```
tcr group park henry-team
tcr status | grep parked_groups        # parked_groups=henry-team on every member
tcr group unpark henry-team
```

Two things it deliberately is not:

- **Not a reservation.** A reserved group still serves its own `--group` asks; a parked one
  serves nothing. The flags are independent and compose — a group can be both.
- **Not a per-account write.** Parking never touches `accounts[].disabled`, so unparking does
  not re-enable a member you benched by hand: that row stays disabled until `tcr enable`
  says otherwise, and `tcr status` keeps reporting it as `disabled` rather than `parked`.

### Codex accounts in a group: labelled and parked, never routed

A Codex account can carry a group label, and the label does two things: `tcr group park`
reaches it, and `tcr status` shows it. It never routes. No Codex request carries a group, so a
Codex account is picked for Codex traffic as if it had no label at all. The group verbs say so:

- **`tcr group add <label> <codex account>`** writes the label and prints a warning on stderr
  saying what the label does and does not do. It still exits 0.
- **`tcr group reserve <label>`** counts Claude accounts for its safety rail: it refuses when
  the reservation would leave no enabled, unreserved Claude account, however many Codex
  accounts remain, because they cannot serve a Claude session. It also refuses a group that
  holds a Codex account, in any state, because while the group is reserved its Codex members
  serve nothing; the refusal names them and the `tcr group rm` that takes one out. `--force`
  reserves it anyway, warns on stderr, and the success line counts what is left per provider
  (`… 1 unreserved enabled Claude account(s) and 1 Codex account(s) remain.`). `--force` never
  lifts the Claude rail. Either refusal leaves the file as it was.
- **`tcr run --group <label>`** refuses a group with no Claude account, since a Claude session
  cannot use any of its members, and launches nothing. `spillToPool` does not lift this: the
  check runs at launch, as the one that refuses a label no account carries does.

A fleet with no Codex account sees none of this: every verb prints what it always printed.

## `controlIdentity`: the client must BE the control account

`controlAccount` cannot choose which account a client's connectors come from. The claude.ai
connector transport (`mcp-proxy.anthropic.com`) never passes through the proxy — it is a
blind tunnel carrying the client's own token — and the connector list (`/v1/mcp_servers`),
plugins, skills, settings and the bootstrap are all relayed with the client's own bearer for
the same reason: they are questions about the CLIENT's identity, and a pooled account
answers them for somebody else. So "quota from many accounts, MCP from one" is arranged by
the client being **logged in as** the control account, and the proxy can only check that it
is. `controlIdentity` is that check.

On the first request carrying a given bearer, the proxy resolves the identity behind it
once, via `/api/oauth/profile` on the configured `upstream`, caches the answer under a hash
of the bearer (the bearer itself is never stored), and compares it to `controlAccount` —
by `accountUuid` when both sides carry one, otherwise by the email half of the account
name. A refreshed token is a new bearer and is checked once more; nothing is re-asked per
request.

| value | on a mismatch |
|---|---|
| `"warn"` (default) | served; one `WARN` line per bearer naming both identities |
| `"refuse"` | `403` naming both identities and the fix: `/logout`, then `/login` as the control account. Applies to bookkeeping calls AND inference, so a wrong-account session fails at its first request rather than running for hours on the wrong connectors |
| `"off"` | never resolved |

`warn` is the default for this release so the fleet can measure how common the drift is
before anything is refused: a headless `tcr run` worker sharing a drifted keychain would be
403 on every request after an upgrade, with the cause visible only in a response body nobody
reads. Set `"controlIdentity": "refuse"` on an install where a wrong-account session must
fail loudly; the intent is to make that the default once a release of warnings has been
read.

Inert without a `controlAccount`, for a request that carries no `Authorization: Bearer`
(an API-key client), and for every Codex request: under `refuse`, a Codex client is served
whatever ChatGPT login it runs under. Fail-open on anything that is not a verified mismatch: a
profile fetch that fails, or answers with neither email nor uuid, is served and is not
cached, so a transient upstream failure neither blocks traffic nor pins a wrong answer. A
bearer the profile endpoint refuses outright (401/403 — an expired or revoked token) is
served too, and remembered as unresolvable for 60 seconds so a stale session does not pay
a profile round trip before every request. A control account whose name is not an email
and whose row has no `accountUuid` cannot be verified against anything and is never
refused.

For a Claude request, "pooled" means exactly two paths: `/v1/messages` and
`/v1/messages/count_tokens`. Any other path a client sends with its own bearer — including
one that merely starts with `/v1/messages`, such as a batches endpoint — is the client's own
call and is relayed under the client's own credential and quota, the same split
`RequestClass` draws for bearer-less traffic.

Measured before this existed (2026-09-22): a Claude Code keychain login that silently
flipped to another account produced two hours of wrong-org 401/403/404 answers on every
bookkeeping call, forced-refresh storms on the control account, and `Server not found` on
every connector — and nothing anywhere named the cause. Boot-time snapshot: an edit needs a
restart, and the `server started` line reports `control_identity=`.

## `controlPooled`: spends the control account's quota

Off by default. On, the control account joins general inference rotation like any other
account, and the only thing holding it back is the `controlReserve` floor: it stops being
picked once its utilization reaches `switchThreshold - controlReserve` (0.95 - 0.05 = **0.90**
on the defaults), leaving the last five points for the identity plane.

```json
{ "controlAccount": "ops@example.com", "controlPooled": true, "controlReserve": 0.05 }
```

Turn it on when a designated control account otherwise sits idle and that idle quota costs
more than an identity-plane outage would. Read the rest of this section before you decide it
does.

**The floor can be lost, and it has been.** `controlReserve` is our own arithmetic over
Anthropic's rate-limit headers, and those lag — upstream keeps answering `200` for accounts it
is about to bench. Measured on a live 13-account fleet on 2026-08-29: an account tracked the
whole way up still crossed into `quota.status == "rejected"`, which is upstream's verdict and
not ours to argue with. A control account in that state fails `control_eligible`, so the
identity plane loses its designated account and degrades to ordinary rotation until the weekly
window resets — which can be most of a day.

So the trade is: idle quota now, against an identity-plane outage you cannot shorten. Three
alternatives cost less if they fit:

- **`tcr group allow-control <label>`** — same access, scoped to sessions that explicitly ask
  with `tcr run --group <label>`. The blast radius is one group instead of all traffic.
- **`tcr control <some-idle-account>`** — move the designation onto a Claude account you were
  not going to spend anyway (a Codex account cannot be the control account: `tcr control`
  refuses one, and boot ignores one written by hand). Nothing is sacrificed and no flag is
  needed.
- **`tcr control --clear`** — no control account at all. Bookkeeping calls rotate with
  everything else, which is how the proxy behaved before the control account existed.

Boot-time snapshot: an edit needs a proxy restart, and the `server started` log line reports
`control_pooled=` so you can confirm which way the running process read it.

## `http1Only`: caps blast radius, costs multiplexing

A connection-level fault on HTTP/2 (GOAWAY, a framing error) kills **every** multiplexed
stream sharing that connection at once — one fault can take down several concurrent
sessions on the same account. `http1Only` forces the upstream-forwarding client onto
HTTP/1.1, which does not multiplex, so the same fault costs exactly one in-flight request
instead. It is a structural cap on blast radius, not a statistical one, and it stacks with
the per-account connection pool (each account already owns its own client, so this narrows
the blast radius further within an account rather than duplicating that isolation).

The trade is real, which is why it ships OFF: h1 gives up multiplexing entirely, opens one
TCP+TLS connection per concurrent request instead of sharing one, and raises the open-socket
count against Anthropic's edge. Turn it on only if you have actually observed the
connection-fault symptom (many sessions failing at once, not a single request failing).

Its state is visible from outside the process on purpose — a knob nobody can see the state
of is how this repo once lost seven hours of prompt-cache to session-affinity being silently
off. Check `tcr status` (`http1Only=` on the `source=` line, `http1Only` per row with
`--json`) or the `"server started"` log line at boot.

## Environment overrides

Four keys are not part of the config file; they are read from the process environment by
`tcr run` (and by `--group`'s version check, which also launches `claude`) to decide which
`claude` binary to run, or, with `--codex`, which `codex` binary. `tcr login --codex` reads the
two `codex` keys too, to pick the `codex` whose `codex login` it runs. None of them exists in the
file above because none is a setting a config edit can change without a rebuild of the
session's shell environment anyway.

| env var | default | what it does |
|---|---|---|
| `TCR_CLAUDE_BIN` | absent | path or name of the `claude` binary to launch, outranking `CLAUDE_BIN` |
| `CLAUDE_BIN` | absent | same, as a fallback when `TCR_CLAUDE_BIN` is unset; kept for whatever already sets it outside `tcr` |
| `TCR_CODEX_BIN` | absent | path or name of the `codex` binary `tcr run --codex` launches and `tcr login --codex` runs, outranking `CODEX_BIN` |
| `CODEX_BIN` | absent | same, as a fallback when `TCR_CODEX_BIN` is unset |

With neither of a pair set, `claude` is resolved from `PATH`. `codex` is found by one search
that serves `tcr run --codex`, `tcr login --codex` and `tcr doctor` alike: `PATH`, then
`~/.local/bin`, `~/.cargo/bin`, `/opt/homebrew/bin` and `/usr/local/bin`, then the `bin` of npm's
global prefix, which is `NPM_CONFIG_PREFIX`, else `npm_config_prefix`, else `~/.npm-global` (tcr
never runs npm). **An empty value in either counts as unset** — `TCR_CLAUDE_BIN=` left behind by
a shell does not silently win over a real `CLAUDE_BIN`, and two empty values fall all the way
through to the default. A set `codex` key always decides and never falls through: a value with a `/` must be
an executable file, a bare name must be one on `PATH`, and either failing is an error that names
the key, rather than whatever other `codex` the search would have found.

Two more `codex` keys exist for tcr's own tests, and nothing else should set them.
`TCR_CODEX_SEARCH_DIRS`, a colon-separated list, replaces every directory the search looks in
after `PATH`, and the directories the older-`tcr` check of `tcr login --codex` and
`--from-codex` scans. `TCR_BUNDLED_TCR` replaces the path of TcrBar's bundled `tcr` in that
check, so setting it outside a test hides that `tcr` from the older-`tcr` warnings.

`tcr login --from-codex` reads `CODEX_HOME` and `TCR_CODEX_AUTH` as well, to find the login
it imports; they are documented with it in
[cli.md](cli.md#--from-codex-importing-the-login-codex-already-has).

## File permissions and secrecy

The config holds live OAuth access and refresh tokens for every account. It is written
`0600` (owner read/write only) and that is enforced twice: the temp file is created with
mode `0o600` before any bytes are written, and after the atomic rename an explicit
`set_permissions` normalises the mode so a restrictive umask cannot leave it somewhere
unexpected. Every write path funnels through that one function: `save`, `save_tokens`,
`save_disabled`, and by extension `tcr login` and the CLI mutators. The session-affinity
pin file and the singleton owner file get the same treatment.

Never commit this file, never paste its contents into an issue, a PR or a chat, and never
copy it into a repository checkout, including this one, which is public. Tests in this
repo write their own temporary configs with obviously fake values; do the same.

## `tcr-peers.json`

A second file, `tcr-peers.json` in the operator's config directory, mode `0600`, holds everything about the LAN peer
mesh: who this Mac trusts, what each trusted Mac may do, and the office network key. It is
never merged into the main account config above; a walkthrough of what the mesh does is in
[peers.md](peers.md), and this section is the file's key reference, read from
`src/peer/config.rs`.

A missing file is the default: nothing pinned, the listener off. A file that exists but
cannot be parsed is a named refusal, same rule as the main config. A key this build does
not know is kept and written back on the next save, so an older `tcr` (the one bundled in
TcrBar, say) cannot erase a setting a newer one wrote.

**Two tiers, one reload rule.** [`listen`](#top-level-keys) and `maxHops` are read once at
boot: turning the listener on is a restart-worthy act. `discovery` is not: a running `tcr`
re-reads it every 20 seconds, so `tcr peer find on|off` needs no restart. Everything under a
peer's `allow` and `lend` is re-read whenever this file's mtime moves, the same
`reload_groups_if_changed` mechanism the main config already uses for group membership, so
`tcr peer allow` and `tcr peer lend` take effect with no restart and no cost to the prompt
cache.

### Top level keys

| json key | type | default | boot-time or hot | what it does |
|---|---|---|---|---|
| `listen` | string (`host:port`) | absent | boot-time | where the peer listener binds. **Absent means the feature is off**: a fresh install discloses nothing, opens no port, answers nobody |
| `discovery` | bool | `false` | hot, re-read every 20 s | whether this Mac announces itself and looks for others (`tcr peer find`) |
| `name` | string | absent | hot | this Mac's display name (`tcr peer name`). Absent means "use the host name", resolved at use and never written as a fallback value, so a name the operator never chose still reads as unset |
| `announceName` | bool | `false` | hot | whether the discovery beacon includes `name`. Off leaves the beacon as presence and a port only; it never carries a key or a peer id either way |
| `networkKey` | string (52-char base32) | absent | boot-time for the beacon, hot for a pairing request | the opt-in office password (`tcr peer network-key`); an admission ticket to the beacon layer, not an identity |
| `maxHops` | int | `1` | boot-time | how many times a frame of this Mac's may be forwarded; `0` disables forwarding entirely, which is also what makes `tcr peer forget` mesh-wide again |
| `internet` | bool | `false` | hot | whether this Mac tries to be reached from off its own network at all (`tcr peer internet on\|off`). A running `tcr` re-reads this switch on its own, so turning it on asks the router for a mapping on the listener's port within a few seconds and turning it off deletes the mapping the same way, neither needing a restart. While on, the mapping is renewed every 30 minutes, ahead of its 2-hour lifetime; it is also deleted at shutdown. Off leaves this Mac reachable on its LAN only |
| `paths` | object | absent | hot | policy for choosing which endpoint to dial: `paths.prefer` (`"direct"` or `"via"`), `paths.maxLossPct` (default `5`), `paths.viaAllow` (which peers may carry when `via` is preferred). Absent means the built-in order, direct first, newest endpoint first |
| `peers` | array of peer rows | `[]` | hot | one row per pinned peer, see below |
| `pendingInvites` | array of invites | `[]` | hot | one-use join keys minted by `tcr peer invite` that have not been spent yet |
| `deadDrop` | object | absent | boot-time for the publisher, hot for the fetch | where this Mac leaves its current address for a friend that has also moved. **Absent means off, which is the default**: nothing is published and nothing is fetched. A Mac that never turned it on writes no such key |
| `deadDrop.enabled` | bool | `false` | boot-time | the switch |
| `deadDrop.store` | object | absent | boot-time | the surface to publish on. Absent means off regardless of `enabled` |
| `deadDrop.store.kind` | `"https"` \| `"gist"` | | boot-time | which surface |
| `deadDrop.store.url` | string | | boot-time | `https` only: a template containing `{name}`, used for both the write and the read |
| `deadDrop.store.gistId` | string | | boot-time | `gist` only |
| `deadDrop.store.token` | string | absent | boot-time | bearer token for the surface. **Held in the clear in this file, mode 0600**, the same as `networkKey` and every `rendezvousSecret`; it goes here and never in the main account config, because this is the file whose mode is checked on read |
| `deadDrop.slotSeconds` | int | `3600` | boot-time | how long one drop name is valid. Both Macs must agree: a mismatch means they never meet |

### One entry in `peers[]`

| json key | type | required | what it does |
|---|---|---|---|
| `node` | string (52-char base32) | **yes** | the pinned static key. **This is the authorization.** A changed key on a pinned peer is a refusal naming both fingerprints, never a silent re-pin |
| `label` | string | **yes** | the operator's name for the peer |
| `endpoints` | array of endpoint objects | no, default `[]` | where this peer was reached, newest first, at most 8. Routing advice only; identity is re-proven by the handshake every time, never trusted from this list. Replaces `addrs`, which a file written before this key still uses and which is still read |
| `addedAt` | int (unix ms) | **yes** | when this peer was pinned |
| `allow` | object | no, default all false | what this peer may do, see below. A bare pin can do nothing but say hello |
| `lend` | array of lend grants | no, default `[]` | what this peer may borrow, per window; an empty list is "nothing" |

One entry in `endpoints[]`. A peer is its key, not its address: this list is how to reach it,
never whether to trust it, so a wrong entry costs a connect timeout and nothing else.

| json key | type | what it does |
|---|---|---|
| `kind` | `"direct"` \| `"via"` | whether this Mac opens the socket itself, or reaches the peer through another pinned Mac that forwards. `via` is tried last: a hop spends a second machine's bytes and its consent |
| `addr` | string (`host:port`) | the socket to open. `direct` only |
| `node` | string (52-char base32) | the forwarding peer's pinned key. `via` only |
| `observedAtMs` | int (unix ms) | when this Mac observed the endpoint, on its own clock, never a time taken off the wire |
| `source` | `"paired"` \| `"hello"` \| `"beacon"` \| `"mapping"` \| `"brief"` \| `"drop"` \| `"moved"` | what taught this Mac the endpoint: the pairing or enrolment itself, a `Hello` inside a session whose key checked out, a discovery beacon matching a pinned instance, this Mac's own port mapping, a trusted peer's word about a mutual friend, a record left at a dead drop, or a sealed link a person pasted (`tcr peer moved open`). The last three are the weakest: a brief is dialled after everything this Mac worked out for itself, and the last two share the bottom band, since both are sealed under a key a Mac this one has since forgotten still holds |

A file written before this key holds `addrs: ["host:port", ...]` instead. It is still read: each
string becomes a `direct` endpoint sourced `paired` and dated `addedAt`, since the legacy key
carried no time of its own and the pin that wrote the address is when it was observed. The next
save writes `endpoints`. An entry there that is not a socket address is dropped with a warning
naming it, rather than refusing the file.

`allow` fields, every one off by default:

| json key | what it grants |
|---|---|
| `gateway` | this peer may ask us to carry bytes out to an allow-listed origin, blind. We hold no key for what we carry |
| `relay` | this peer may ask us to forward to a peer WE have pinned. **Transitive**: a node two hops out reaches our gateway with this peer's authority, so `tcr peer forget` does not revoke egress still reachable through a still-trusted relay |
| `inspect` | this peer may open a lease to us. We read its requests in full and serve them on our own account |
| `allowDisclose` | we may open a lease to this peer. It reads our requests in full, prompts included |
| `acceptMove` | this peer may hand us an account. The only grant under which a credential crosses a host boundary at all |
| `control.briefs` | this peer is told about our other peers, one hop out |
| `control.lendable` | this peer is told our per-window lendable amounts and account counts |
| `control.diag` | this peer is told our build sha and boot id |
| `control.drop` | this peer and this Mac leave each other addresses at a dead drop. Read on both sides: a peer this Mac does not publish for does not get to write endpoints onto its rows either |

One entry in `lend[]` (`tcr peer lend`, `tcr peer share`). A Mac may hold several of these at
once, one per `(window, scope)` pair, because lending a new scope adds a lease rather than
replacing the ones already granted:

| json key | type | what it does |
|---|---|---|
| `id` | string (32 hex chars) | the wire lease id this grant mints leases under, the handle `--revoke`/`--relend` take. `0` is a grant written before ids existed; the next save mints one |
| `scope` | `"all"` \| `{"group": "<name>"}` \| `{"accounts": ["<label>", ...]}` | what this grant lends FROM. Defaults to `"all"`, which is what every grant written before scopes existed already meant. Never crosses the wire. A Codex account is never lent, whatever the scope names: `all` and a group draw on Claude accounts only, and `tcr peer lend` and `tcr peer share on` refuse a scope that names a Codex account |
| `window` | `5h` \| `7d` \| `7d_oi` | which rate-limit window this grant draws from |
| `fraction` | float | ceiling on any one lease, as a fraction of the SCOPE's headroom. Clamped to `0.0..=0.5`, mirroring the clamp the main config applies to its own reserve |
| `ttlS` | int | how long a granted lease lives, in seconds, before it needs renewing |
| `maxInflight` | int | how many borrowed requests may be in flight against one lease at once |
| `until` | int (unix seconds), absent for no end | the absolute end of the LENDING (`--for`/`--until`), distinct from `ttlS`'s renewal deadline. Absent is the default: no end |
| `between` | string (`"HH:MM-HH:MM"`), absent for every hour | the daily window this grant lends in, local time (`--between`). A window whose end is before its start crosses midnight. Unlike `until`, which ends the lending once, this closes and re-opens every day; a request outside it is refused `OutsideSchedule` |
| `days` | string (`"mon,tue"`), absent for every day | the days the `between` window may START on (`--days`). An overnight window granted on `fri` therefore runs into Saturday morning, and not into Sunday |
| `ended` | bool | whether `until` has passed. **Derived on read and never trusted from the file**: a hand-planted or stale `"ended": true` on a live grant reads as `false` |

### An outstanding invite (`pendingInvites[]`)

| json key | type | what it does |
|---|---|---|
| `id` | int | the id `tcr peer invite --revoke <id>` takes |
| `label` | string | the name the joiner will be pinned under |
| `secret` | 32 bytes | the join secret, used as the pairing PSK. Stored in the clear: a stored hash could not be both unusable by an attacker and usable to complete a handshake. What actually bounds it: single use, a short TTL, file mode `0600`, deletion on use, an explicit revoke, and a cap of 8 outstanding at once |
| `expiresAtMs` | int (unix ms) | absolute deadline |
| `usesLeft` | int | how many joins this invite may still admit; deleted at zero |

### File permissions

Same rule as the main account config: mode `0600`, enforced before any bytes are written and
re-checked afterward. `tcr` refuses to trust a peers file it finds in any other mode rather
than silently loosening it. Never commit this file for the same reason as the main config:
`node`, `networkKey` and an outstanding invite's `secret` are all live cryptographic material.
