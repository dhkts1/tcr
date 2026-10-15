# How `tcr` works

The request path, the two entry modes, how a Codex request travels, how an account gets
picked, how quota is kept fresh, and the files it keeps, how to check a download and how to
remove it. Every tunable named here has its type, default and source citation in
[`configuration.md`](configuration.md); this document describes the mechanism rather than
the values.

## One listener, two entry modes

One TCP listener on `127.0.0.1:<port>` serves both entry modes. Which one you get is
decided by a non-destructive peek at the first eight bytes of each connection.

```
client                      tcr: ONE listener on 127.0.0.1:<port>
  |
  |-- peek first 8 bytes: "CONNECT "?
  |     no  -> BASE-URL MODE (cleartext h1 or h2 prior-knowledge) ---+
  |     yes -> parse CONNECT host:port, host on the allowlist?       |
  |             no  -> BLIND TCP BYTE TUNNEL: TLS never terminated,  |
  |                    nothing decrypted, nothing injected           |
  |             yes -> 200 Established, TLS-accept with our leaf ----+
  v
one axum router, identical for both modes
  |
  v  local routes: GET /_tcr/status | GET /_tcr/quota | POST /_tcr/accounts/disabled
  v  everything else -> the forwarding handler
api-key gate -> path-shape gate -> /_tcr guard -> host guard (421)
  -> relay bypass for /v1/code and the OAuth file endpoints
  v
buffer the body once | parse the target model | derive the session key
  v
ROTATION LOOP (bounded)
  select an account -> take an in-flight slot -> refresh the token if it is
  expiring -> splice metadata.user_id.account_uuid -> drop the client's
  authorization and x-api-key, inject ours -> global rate limiter -> send
  401 / 429 / 529 / transport failure -> retry the same account, or rotate
  v
2xx: the response streams back verbatim; SSE chunks are tee'd to a usage parser
```

**Base-URL mode** is plain HTTP to the listener. **Forward-proxy mode** terminates TLS for
Anthropic's own API hosts, and for marked `chatgpt.com` connections with a pooled account (see
[Codex requests](#codex-requests)), using a locally generated leaf, which is why the client
needs your CA: Claude Code in `NODE_EXTRA_CA_CERTS`, `codex` in `CODEX_CA_CERTIFICATE`. Every
other host is copied through as raw bytes and is never decrypted. That is what keeps Claude
Code's other endpoints working through a single `HTTPS_PROXY`, and it is also a real property
to understand before you run it; see [Security](../README.md#security).

`tcr run` prefers forward-proxy mode: Claude Code decides whether you are a first-party
client by string-comparing `ANTHROPIC_BASE_URL`, so leaving that variable alone and
intercepting one layer down at `HTTPS_PROXY` keeps the client in its normal configuration.

## Codex requests

tcr routes `codex`, OpenAI's coding CLI on a ChatGPT login, in forward-proxy mode only: it
has no base-URL route for `codex` yet. `codex` keeps its own backend URL, the real
`https://chatgpt.com` for every login tcr pools, sends `CONNECT` through `HTTPS_PROXY`, and
trusts tcr's leaf through `CODEX_CA_CERTIFICATE`; `tcr run --codex` sets the proxy URL to
`http://tcr-codex@127.0.0.1:<port>` and sets the CA unless explicitly supplied
([`cli.md`](cli.md#--codex-launching-codex-through-the-pool)). A request's provider is
decided by its path in base-URL mode: everything under `/backend-api/` is Codex, everything else
is Anthropic, and a request tcr pools is served only by an account of its own provider.
Inside a tunnel, the CONNECT target fixes the upstream: marked `chatgpt.com` connections
relay paths outside `/backend-api/` unchanged with the client's credential. An Anthropic
host carrying a `/backend-api/` path is answered 421, and nothing is forwarded. A request
that declares another host, including loopback, or an unreadable Host, is answered 421 too.

tcr terminates `chatgpt.com` only when CONNECT carries the `tcr-codex` proxy marker and the
pool holds a Codex account, counted on every CONNECT. Codex's HTTP and WebSocket transports
both send proxy URL userinfo as `Proxy-Authorization`. The launch supplies unmarked proxy
values to Codex's shell environment policy for ordinary child tools; explicit child environments
can still override that choice. The marker is routing opt-in, not process authentication.
Unmarked connections, including a
manually set `HTTPS_PROXY=http://127.0.0.1:<port>`, are blind-tunneled and use the client's
own login. A login whose ChatGPT workspace is FedRAMP, routed to a data residency or to a
backend of its own is neither pooled nor importable: another backend is tunneled untouched,
and a request naming routing (`x-openai-account-routing-override` or `x-openai-fedramp`) is
relayed on the client's credential.

Codex stays off the [peer mesh](peers.md) in this version: a Codex account is never lent to a
peer, and a `codex` request is never borrowed from a peer or carried through one, so it always
leaves from this Mac.

The pooled model paths under `/backend-api/codex/` are `responses`, `images/generations`,
`images/edits`, `memories/trace_summarize` and `alpha/search`. Other Codex paths retain the
client's credential.

```
codex   HTTPS_PROXY=http://tcr-codex@127.0.0.1:<port>   CODEX_CA_CERTIFICATE=~/.config/tcr-ca.pem
  |
  |-- CONNECT chatgpt.com:443         marker present, leaf covers it, and a pooled account:
  |                                   TLS terminated (otherwise a blind tunnel)
  |-- CONNECT auth.openai.com:443     blind tunnel: codex refreshes its own login untouched
  |
  |-- GET /backend-api/codex/responses, Upgrade: websocket
  |       -> 426 from tcr itself: no account selected, nothing forwarded
  |       (codex stops asking for a WebSocket for the rest of the session)
  |
  |-- POST /backend-api/codex/responses          body usually zstd-compressed
  |       routing       x-openai-account-routing-override or x-openai-fedramp present:
  |                     relayed under the client's own bearer, never pooled
  |       session key   the session-id header
  |       select        an eligible Codex account, ranked as usual; never a Claude one
  |       refresh       against auth.openai.com, when the token is expired or about to be
  |       headers       drop authorization, x-api-key, accept-encoding, hop-by-hop and
  |                     both routing headers;
  |                     add Bearer <pooled token>, chatgpt-account-id: <its account id>
  |       body          forwarded byte for byte, content-encoding intact
  |       url           codexUpstream + path      (https://chatgpt.com by default)
  |     2xx: the SSE stream flows back; the serving account's x-codex quota headers are
  |          read, then stripped together with its credit and promo headers, which tcr
  |          never reads (x-codex-turn-state passes through);
  |          the turn's closing event records its usage in the ledger
  |     429: that account is held, and the request rotates to another Codex account,
  |          which gets the client's x-codex-turn-state as sent though another account
  |          minted it (how the backend treats that is unmeasured)
  |
  |-- every other /backend-api/ path, wham/usage included
          -> relayed under the client's own bearer, never pooled
```

Three details of the `codex` client shape that path. The citations are to
`openai/codex@f82e365`, under `codex-rs/`:

- **The 426.** `codex` opens its stream over a WebSocket by default, and a 426 answering the
  upgrade is what moves it to HTTP: `core/src/client.rs:2010-2014` matches `UPGRADE_REQUIRED`
  and falls back, and the fallback is session-scoped state (`core/src/client.rs:1098-1104`),
  so it holds for the rest of the session. tcr relays HTTP streaming only, so it answers that
  upgrade itself, before choosing an account, and logs it at debug level. One 426 per session
  is the expected shape, not a fault.
- **The header swap.** `codex` sends its bearer together with the ChatGPT account the bearer
  belongs to, as a `chatgpt-account-id` header
  (`model-provider/src/bearer_auth_provider.rs:31-47`). tcr replaces both: the bearer with the
  serving account's token, and the header with that account's workspace id, its `orgUuid`,
  falling back to its `accountUuid` on a row that has no `orgUuid`. It inserts the header when
  the client sent none, and drops it when the row has neither.
- **The compressed body.** When it talks to the ChatGPT backend, `codex` compresses request
  bodies with zstd (`core/src/client.rs:1704-1713`; the feature is stable and on by default,
  `features/src/lib.rs:1411-1416`). tcr never rewrites a Codex body; an uncompressed one is
  peeked at, read-only, for its model. The body goes upstream byte for byte with its
  `content-encoding`, nothing is spliced into it the way `metadata.user_id.account_uuid` is
  for Claude, and session affinity keys on the `session-id` header instead.

**Quota is mapped by window length.** Codex reports up to two windows, in a primary and a
secondary slot: from its usage endpoint as `primary_window` and `secondary_window`, and on each
response as `x-codex-{primary,secondary}-{used-percent,window-minutes,reset-at}` headers. tcr
files each window into the five-hour or the weekly bar by its length, not by its slot: 600
minutes or shorter is the five-hour bar, anything longer the weekly one, and the slot decides
only when a window carries no length (`src/quota.rs`, `src/codex_probe.rs`). By slot, a Pro
account's numbers would land in the wrong bar: measured on ChatGPT Pro logins, the usage
endpoint reports a single window of 604800 seconds (seven days) in the primary slot, and
`secondary_window: null`. So a Pro account shows its weekly bar only, never a fabricated
five-hour one.

**Probe, refresh, keep-warm.** Between requests, the quota probe sends one
`GET <codexUpstream>/backend-api/wham/usage` per Codex account, with its bearer and
`chatgpt-account-id`; it runs no inference. A Codex token is always refreshed against
`https://auth.openai.com/oauth/token`, whatever `codexUpstream` says. Keep-warm never targets
a Codex account: a warm is an Anthropic request.

**Usage and price.** A Codex turn's usage is read from its closing SSE event
(`response.completed`, or `response.incomplete` when the backend cut it short) into the same
ledger as Claude traffic. The built-in price table holds Claude models only, so those turns
count as unpriced (`costUsd: null`, counted in `unpricedRequests`) unless
[`pricing`](configuration.md#pricing-and-usageretentiondays) names the model.

**Sessions and tool calls.** The `session-id` header names a Codex request's session: it is the
key affinity pins on, and the session `tcr sessions` and TcrBar's Sessions tab list, marked
`provider: "codex"`. The body is never read, so the session's model is the one its stream names,
and its cost is unpriced unless `pricing` names the model. Each finished output item
(`response.output_item.done`) that is a function call, a freeform-tool call or a local shell call
becomes a running call when its stream ends. Its head is the command it runs, through the same
normalizer as a Claude `Bash` call, or its tool's name, and it carries `timeoutSeconds` when its
arguments name `timeout_ms`, which only the one-shot `exec_command` does. The next request on the
same `session-id` closes it, and only a request tcr serves: that request carries the call's
output, in a body tcr cannot read. A request tcr answers itself closes nothing, for example a
refusal, or a request that meets an upstream 403's hold on the only Codex account (one that meets
the hold's last 20 seconds waits it out instead, and closes the calls if it is then served); a
Claude `tool_result` in such a request behaves the same way. So a call's duration is measured, but
whether it failed or timed out is not, and nothing is inferred about what codex is doing in
between. The citations are to `openai/codex@f82e365`, under `codex-rs/`: the finished item under
`item` in `codex-api/src/sse/responses.rs:159-168,344-360`; the `function_call`,
`custom_tool_call` and `local_shell_call` shapes in
`protocol/src/models.rs:1024-1027,1082-1123,1167-1187`; `exec_command`'s `cmd` in
`core/src/tools/handlers/shell_spec.rs:24-115`; and the one-shot schema's `timeout_ms` in
`core/src/tools/spec_plan.rs:1276-1284` and
`core/src/tools/handlers/unified_exec/exec_command.rs:103-108,126-138`.

**When no Codex account can serve,** a pool that cannot recover by waiting gets a 400
`no_codex_account`, with no `retry-after`. This includes an empty pool and one whose remaining
Codex accounts all need a configuration or login change. The message names `tcr login --codex`
or the changes needed to enable an existing account. Codex does not retry this configuration
error. With no Codex account configured, new `chatgpt.com` connections are blind-tunneled;
an existing intercepted tunnel or a request sent directly to tcr's listener can still receive
the error. Configured but unusable accounts keep marked connections intercepted, so their
requests receive the error immediately.

For accounts on temporary gates, the 429 is shaped for `codex` rather than for Claude Code,
on the principle the Claude 429 follows: a quota is the user's limit, and a hold is not. When
the Codex account that frees soonest is out on a usage limit, a five-hour or weekly window or a
refusal from the backend, or none reports when it frees and every one is out on such a limit, it
is `usage_limit_reached`, which `codex` ultimately shows as a usage limit and on which it ends the turn.
Its `resets_in_seconds` is the wait until the soonest Codex account frees, uncapped, beside the
capped `retry-after` that [When every account is out](#when-every-account-is-out) describes.
When some Codex account reports when it frees, `resets_at` is the instant that wait ends, in
Unix seconds, and `codex` names it as the time to try again; when none does, the wait is a flat
60 seconds that nothing measured, so there is no `resets_at`, and `codex` says to try again
later. For other temporary gates, such as a hold on an upstream `retry-after`, it is
`pool_unavailable`: the same
`resets_in_seconds` and capped `retry-after`, never a `resets_at`, and `codex` retries it after
that `retry-after`, up to five times, instead of ending the turn. A
Claude request never spills onto a Codex account, nor a Codex request onto a Claude one.

## Account selection

Account selection is not one sort. On the normal path, `tcr` picks the eligible account
minimising `(priority ascending, least-recently-selected, soonest weekly reset)`, so
`priority` is a hard tier and rotation breaks ties within it. Eligibility means: not
disabled, not in an error state, not under a live rate-limit hold, under the switch
threshold, and not blocked for this request's model class. Four things change that picture:
`lockAccount` short-circuits every pooled request of its account's provider to that account
while the other provider rotates normally, an honoured session-affinity pin is served
even over the utilization threshold, the pacing fallback ranks in-flight count above priority,
and `revalidationServe` (on by default) serves the least-utilized survivor rather than
returning an error when the whole fleet reads over the soft threshold.

Each of those keys (`lockAccount`, `sessionAffinity`, `pacing`, `revalidationServe`, and
the switch threshold itself) is documented with its default in
[`configuration.md`](configuration.md).

### Session-affinity pins survive a restart

With `sessionAffinity` on, the session-to-account pin map is flushed to disk continuously
and reloaded at boot, so a restart does not automatically cold-start every conversation's
prompt cache. Pins expire against `affinity::PIN_TTL_MS`, **15 minutes**: a restart inside
that window restores most of the fleet's pins, and one outside it restores none. The server
logs how many it restored; read that line rather than assuming either outcome. With
`sessionAffinity` off — the explicit opt-out, no longer the default — there are no pins to
restore.

## Keeping the quota bars fresh

Between requests, a quota probe keeps the bars fresh: one plain `GET` per account against
Anthropic's OAuth usage endpoint, or for a Codex account against `/backend-api/wham/usage`
(see [Codex requests](#codex-requests)). It issues no `/v1/messages` call, so it creates no
messages.

**Each account is probed on its own randomly drawn schedule, not a fleet sweep.** Boot does
one whole-fleet pass so the bars populate immediately, and from then on every account
sleeps for an independently drawn interval, centred on the configured cadence, before its
own next probe, with a random initial offset so a restart re-scatters the fleet instead of
re-aligning it. This replaced a 75-second fleet-wide sweep that touched every account
inside the same window, on the dot.

Keep-warm (`warmupSeconds`) got the same treatment; it is the opposite of the probe and is
off by default: it really does post messages, and spends quota to do it. It never targets a
Codex account.

The distributions, the bounds each draw actually produces, and the generator behind them
are in [Both cadences are random per account, not a fleet
sweep](configuration.md#both-cadences-are-random-per-account-not-a-fleet-sweep).

## When every account is out

If the rotation loop finds nothing eligible, `tcr` answers the client with a 429 carrying a
`retry-after`. The number on that header is deliberately **not** the real wait. A quota
window can be sixteen hours from resetting, and a real incident produced advertised waits of
57,950 to 59,975 seconds, but Claude Code ignores any `retry-after` above 60 seconds and
exits instead of sleeping. Advertising the true figure therefore killed the run outright. The
header is capped at 59 seconds; the real wait is still stated in the response body, and both
values are logged.

The cap is 59 rather than 60 because `exhaustion_hint` already uses exactly 60 to mean "no
account advertises a reset at all". Keeping the cap one second below that leaves the two
readable apart: anything at or under 59 came from a measured reset instant, and 60 on the
wire can only be the sentinel. This matters to a log reader, to an operator, and to a `tcr`
chained behind another `tcr`.

Measured against a stub endpoint on 2026-09-15 with client 2.1.272, two runs per value:

| advertised `retry-after` | what the client does |
|---|---|
| 30s | sleeps 30s, retries, exits 0 |
| 59s | sleeps 59s, retries, exits 0 |
| 60s | sleeps 60s, retries, exits 0 |
| 61s and above | never retries, exits 1 in about a second |

The cap holds across repeated refusals, which is what makes it a fix rather than a delay.
Against an endpoint that answered every single request with the same 59, the client kept
sleeping and re-asking on an exact 59.0-second cadence for seven attempts across six minutes,
and was still going when the test harness timed out. It never gave up on its own. So a
genuinely hours-long window no longer ends the run at the first refusal: the client waits
until some account frees, which is what the body text always claimed would happen.

Raising the cap above 60 would restore the immediate death. Lowering it only costs extra
round trips.

**Counting what tcr refused codex.** The proxy keeps two ten-minute counts for Codex requests,
each in ten one-minute buckets in memory, and reports them on its status document as
`codex_traffic`. One is bumped by every 400 or 429 pool refusal tcr sends a Codex request itself
(no usable Codex account, a pool out on a usage limit, or one that cannot serve for now),
recording the body's error type.
The other is bumped by every Codex request relayed on the client's own credential because it
named a FedRAMP or data-residency routing of its own; a bookkeeping call is relayed for its path
and is not counted. `tcr status` prints both as notes, and its `--json` rows carry them. A
restart empties both until the next refusal.

**A pool with no account for the request's provider.** A Claude request to a pool that holds
Codex accounts and no Claude account gets the same 429 as before, but its message says
``No Claude account is configured in tcr; run `tcr login` to add one.`` rather than
"All 0 accounts exhausted". A Codex request to a pool with no usable Codex account gets a 400
`no_codex_account`, whose message names `tcr login --codex` or the changes needed to enable an
existing account. An account deleted while the proxy runs counts toward neither provider's
configured-account count.

**A refused credential.** When every account a request tried was refused by an upstream 403,
the answer is no 429, since waiting will not help. A Claude client gets a 502 naming the
accounts. A Codex client gets a 403 in its own error envelope, naming each refused account and
`tcr login --codex --account <name>`: a 401 would make `codex` reload and refresh its own login,
and a 403 leaves that login alone. Either way the refused account is held for 120 seconds, so
the answer reaches a client at most once per hold. A Codex request inside the hold, with no
other account to take it, gets the retryable `pool_unavailable` 429, which is counted as one;
one that arrives in the hold's last 20 seconds waits it out instead.

## Files on disk

`tcr` keeps two directories. The config directory is always `$HOME/.config`: it does not
follow `XDG_CONFIG_HOME`, because the Node teamclaude proxy uses that path and `tcr` drops in
for it. The cache directory is `$XDG_CACHE_HOME/tcr` when that variable is set, and
`~/.cache/tcr` otherwise.

The project used to be called teamclaude, and an install from then keeps its old names: the
config `~/.config/teamclaude.json` and the cache directory `teamclaude/`, whose logs are
`teamclaude-rs.log.<date>`. `tcr` uses the new name when it exists, else the old one when that
exists, else the new one, and it never moves, copies or links an old file. The rule and every
old name are in `src/paths.rs` and `src/legacy_names.rs`.

| Path | What it holds | Mode |
|---|---|---|
| `~/.config/tcr.json` | Every account, with its **working** OAuth access and refresh tokens or API key, and the proxy's own API key if you set one; the older `teamclaude.json` is used when only it exists | `0600` |
| `~/.config/.tcr.json.tcr-*.tmp` | Left behind when a config write is killed halfway (`.teamclaude.json.tcr-*.tmp` with the older config name). Same contents as the config, and nothing removes them for you | `0600` |
| `~/.config/tcr-codex-login.*` | Beside the config file, and only while `tcr login --codex` runs: the `CODEX_HOME` it gives `codex login`, holding that sign-in's `auth.json` until tcr has read it. tcr deletes it on every way out it can see; a SIGKILL of tcr can leave one behind, holding a live ChatGPT login | directory `0700` |
| `<codexHome>/auth.json` | Imported Codex logins only: guarded, best-effort token sharing; tcr preserves other JSON fields and rejects a changed login seen before replacement | replacement `0600` |
| `<codexHome>/.auth.json.tcr-lock` | Advisory lock that serializes tcr's imported-token writers; Codex does not take this lock | `0600` |
| `~/.config/tcr-ca.pem`, `tcr-ca.key` | The local CA that signs the leaf certificate, and its private key. `NODE_EXTRA_CA_CERTS` points at the `.pem`, and so does `CODEX_CA_CERTIFICATE` for `codex` | `0644`, `0600` |
| `~/.config/tcr-leaf.pem`, `tcr-leaf.key`, `tcr-leaf.sans` | The leaf certificate for Anthropic's hosts and `chatgpt.com`, its key, and the host list it was made for. tcr decrypts marked `chatgpt.com` connections only while the pool holds a Codex account | `0644`, `0600`, `0644` |
| `~/.config/tcr-peers.json` | Peers only: your peer settings, the Macs you trust and any join secrets | `0600` |
| `~/.config/tcr-node.key`, `tcr-node.pub` | Peers only: this Mac's peer identity key pair | `0600`, `0644` |
| `~/.config/tcr/tcr-receipt.json` | Written by the cargo-dist installer (it does follow `XDG_CONFIG_HOME`), for its updater. An install made before the rename has `~/.config/teamclaude-rs/teamclaude-rs-receipt.json` instead | |
| `<cache>/session-affinity.json` | Which session is pinned to which account | `0600` |
| `<cache>/session-wire.json` | The Sessions and Tools lists, including up to 120 characters of each slow or running tool call's command | `0600` |
| `<cache>/peer-state.json` | Peers only: what the proxy has learned about other Macs | `0600` |
| `<cache>/usage/<UTC-date>.jsonl` | The usage ledger behind `tcr wrap` and the spend figures, one line per request, 90 days by default | directory `0700`, files `0600` |
| `<cache>/logs/tcr.log.<date>` | The server log, which names accounts by email. Five daily files are kept | directory `0700` |
| `$TMPDIR/teamclaude-rs.log` | The single log file older builds wrote. Newer builds leave it where it was | |

If you also run the Node teamclaude proxy, its `teamclaude-leaf.pem`, `teamclaude-leaf.key` and
`teamclaude-ca.pem` in `~/.config` belong to it. `tcr` reuses them when they are there and
never writes them. With that leaf in place, tcr never pools `codex`: the leaf covers
`api.anthropic.com` alone, and tcr never re-mints a leaf it did not mint, so it tunnels
`chatgpt.com` untouched
([what to do about it](troubleshooting.md#tcr-run---codex-says-it-is-launching-codex-directly-not-pooled)).

TcrBar adds three things of its own: `/Applications/TcrBar.app`, which carries its own copy
of `tcr` and prefers it over the one on your `PATH`; its preferences, in the
`io.github.dhkts1.tcrbar` defaults domain; and a login item, if you turned on launch at
login. The proxy it runs uses the same config and cache files as the CLI.

## Verifying a download

Get `tcr` from this repository only: its
[release page](https://github.com/dhkts1/tcr/releases) or `install.sh` from `main`,
which downloads from that page. A copy repackaged anywhere else has not been checked by
anyone here, and it is a program you are about to hand your account tokens to.

**The CLI archives.** Every `.tar.xz` on a release has a `.sha256` file beside it, and
`sha256.sum` lists them all. With both files in one directory:

```sh
shasum -a 256 -c tcr-aarch64-apple-darwin.tar.xz.sha256
# expect: tcr-aarch64-apple-darwin.tar.xz: OK
```

The file ends in a blank line, so `shasum` also warns that one line is improperly formatted;
that warning is harmless. The installer script carries the same hashes and checks the
archive it downloads, but only when `sha256sum` is on the machine; without it, it prints that
it skipped the check and carries on. The checksums come from the same release as the
archives, so they catch a corrupted or substituted download, not a release that was itself
tampered with. The release publishes no signatures and no build attestations for the CLI.

**TcrBar.** The `.dmg` has no checksum file. Instead the app is signed with a Developer ID
certificate, and the disk image is notarized by Apple and stapled. With the `.dmg` downloaded
and opened:

```sh
xcrun stapler validate ~/Downloads/TcrBar-<version>.dmg
spctl -a -vvv -t install "/Volumes/TcrBar <version>/TcrBar.app"
# expect: accepted / source=Notarized Developer ID
```

[`RELEASING.md`](RELEASING.md#verifying-a-release-actually-installs) has these checks with the
signature details to expect. Updates installed through the app are checked against the
Sparkle EdDSA key built into your copy before they run.

## Uninstalling

Nothing below runs for you; there is no uninstall command for the CLI.

1. **Stop the proxy.** Press `q` in its terminal dashboard, or quit TcrBar if TcrBar is
   running it.
2. **Remove the binary.** Both `install.sh` and the cargo-dist installer put it at
   `$CARGO_HOME/bin/tcr`, which is `~/.cargo/bin/tcr` when `CARGO_HOME` is unset.
   `scripts/install-cli.sh` puts it at `~/.local/bin/tcr` unless you gave it a destination.
   The cargo-dist installer may also have added a line to your shell's startup files that
   loads `~/.cargo/env`; Rust's own installer uses the same file, so leave it if you use Rust.
3. **Remove TcrBar,** on macOS. From a checkout of this repository,
   `apps/macos/scripts/uninstall.sh` quits TcrBar, which also stops any proxy it was running,
   unregisters the login item and deletes `/Applications/TcrBar.app`. It keeps TcrBar's
   preferences; `defaults delete io.github.dhkts1.tcrbar` clears them.
4. **Delete the config.** `~/.config/tcr.json` and, on an older install,
   `~/.config/teamclaude.json` hold working credentials for every account in the pool, and
   they stay valid after `tcr` is gone. Delete both, any `.tcr.json.tcr-*.tmp` or
   `.teamclaude.json.tcr-*.tmp` and `tcr-codex-login.*` folder beside them, and the other
   `~/.config` files in the table above.
5. **Delete the cache:** `~/.cache/tcr/` and `~/.cache/teamclaude/` (or the same two under
   `$XDG_CACHE_HOME`). Their logs
   name your accounts, and `session-wire.json` holds fragments of commands your sessions ran.
6. **Undo what points at the proxy:** any `ANTHROPIC_BASE_URL`, `HTTPS_PROXY`,
   `NODE_EXTRA_CA_CERTS` or `CODEX_CA_CERTIFICATE` you exported, in your shell or in the `env`
   block of a Claude Code settings file, and any `chatgpt.com` entry you added to `NO_PROXY` for
   tcr's sake. `tcr run` and `tcr run --codex` set their variables only on the session they
   launch, so they leave nothing behind.

`tcr` never writes Claude Code's own stored login; it only reads it, to import it. If `tcr`
refreshed an imported login, that login's old refresh token no longer works, and `claude`
asks you to log in through the browser once.

For Codex accounts imported with `--from-codex`, tcr tries to share refreshed tokens through
the recorded `codexHome/auth.json`. Its writers take a private advisory lock before reading the
file. A matching loaded refresh token permits preparing a `0600` temporary file beside it,
preserving the other JSON fields and updating a returned id token. tcr compares all original
bytes immediately before the atomic rename and skips a detected change. File reads, writes,
fsyncs and lock waits run on blocking workers. Symlinked auth files are not replaced. A failed
publication retains the latest replacement and original spent source token in memory for retry
before later requests; a restart loses that pending retry. Failed temporary publications are
removed when possible, or their token bytes erased before the private handle closes.

Codex's file writer does not take that lock, so its write can still fall between the final
comparison and rename. This is best-effort sharing, not an atomic compare-and-swap with Codex.
`tcr login --codex` avoids sharing the file. tcr adopts a changed, unexpired pair for the same
ChatGPT account when freshness is established; after a permanent refresh rejection it can also
recover when issuance is equal or unreadable and the file timestamp is equal or missing.
An expired access token in a newer file pair is refreshed with that file's refresh token before
application. Known older tokens and different accounts remain rejected. Signing in again with
`codex login` in the shared folder revokes that login and can require tcr to sign in again.
Uninstalling tcr leaves that auth
file in place with its current login.
