# Troubleshooting

Symptoms, in the order people hit them. Flag-level detail lives in
[`cli.md`](cli.md); config keys and their defaults live in
[`configuration.md`](configuration.md).

## "The fix is in `main`" and "the fix is in the process serving traffic" are different facts

The running proxy can be several commits behind your checkout. `tcr status --json` reports
the SHA of the build that is actually serving; check it before concluding a change is live.

A restart does not always close the gap. When TcrBar runs the proxy, it runs the `tcr` bundled
inside the app ahead of the one on your `PATH`, so restarting it on the old app brings the old
build back: update TcrBar first.

## Where the logs are

`tcr server --headless` writes to stdout *and* to a daily-rotating file under the cache
directory. The file is the half that survives supervision, because anything running the
proxy as a background child, TcrBar included, discards its stdout. That file is where to
look when a supervised proxy misbehaves. The exact path, the permissions it enforces, and the
environment variable that relocates it are in [Where the logs actually
go](cli.md#where-the-logs-actually-go).

### More detail from one module: `RUST_LOG`

The log level comes from `RUST_LOG` and is `info` when it is unset or does not parse. Raise
one module and leave the rest alone:

```
RUST_LOG=info,teamclaude_rs::proxy=debug tcr server --headless
```

The module names are the library's paths: `teamclaude_rs::proxy` for the request path,
`teamclaude_rs::manager` for account selection and quota (it covers every module under it),
`teamclaude_rs::peer` for peers. A bare `RUST_LOG=debug` also turns on debug output from every
dependency that logs, which is far more than you want.

`RUST_LOG` is read once, when the process starts. Setting it in your shell does nothing to a
proxy that is already running, and a proxy TcrBar starts inherits TcrBar's environment, not
your shell's. Seeing the new level means starting the proxy again, which costs every live
session its prompt cache (see [The port is already in use](#the-port-is-already-in-use)).

**Redact before you paste.** Log lines name the account they are about, and an account's name
is usually its email address. An error line can also carry upstream's own error body. Replace
names, emails and organization names before putting a log into a public issue.

## What the `gate` value means

`tcr status --json` gives every account a `gate`: why it is out of rotation right now, or `ok`
when it is not. The dashboard shows the same value as a chip in each row. When more than one
gate is active, the one that clears last is shown, so clearing it can reveal the next one.
Every value the status output can print has a row here, and a test fails when a new one is
added without its row.

```
tcr status --json | jq -r '.[] | "\(.name)\t\(.gate)"'
```

| gate | chip | what it means | what to do |
|---|---|---|---|
| `ok` | `OK` | In rotation: no hard gate is active. Pacing can still make it wait its turn, but pacing is never shown here. | Nothing. |
| `hold` | `HOLD 12s` | Upstream answered 429 and the hold tcr set from it has not run out. The hold follows upstream's `retry-after` where there is one, capped at one hour. | Nothing. It clears by itself at the time shown. |
| `refused` | `REFUSED 120s` | Upstream answered 403 to this account's credential. tcr sent the request to another account and holds this one out for 120 seconds. The account's status stays `active`. | Usually nothing, it clears by itself. If it comes back after every hold, upstream keeps refusing this account: the log's `upstream error response` line for it carries upstream's own error type and message. |
| `five-hour` | `5H 47m` | The 5-hour quota is at or over the account's `switchThreshold` (0.95 unless you set it). | Wait for the reset shown. Other accounts take the traffic meanwhile. |
| `seven-day` | `7D 2d` | The weekly quota is at or over `switchThreshold`. | Wait for the reset shown. |
| `fable-weekly` | `FABLE-7D` | The Fable weekly quota is at or over its threshold. It holds the account out for Fable requests only. `tcr status` reports the fleet as a request with no model in mind sees it, so it does not print this value today; read `sevenDayOiState` instead. | Nothing. The account keeps serving other models. |
| `standard` | `STD` | An API-key account's token or request limit is at or over `switchThreshold`. It never applies to a subscription login, and tcr serves subscription accounts only, so you should not see it. | If you do, the config holds an API-key account. Take it out of the pool with `tcr remove <name>`. |
| `login` | `LOGIN` | The account's refresh token was rejected, so it cannot get a working access token. tcr retries the refresh on a growing backoff, but only a new login replaces the token. | Log in again: `tcr login --account <name>`. On a Codex account, `tcr login --codex --account <name>`, or `tcr login --from-codex --account <name>` to import it again from the folder it came from; its `loginError` in `tcr status --json` names why the login died (`refresh_token_reused`: another holder of the login refreshed it first). |
| `rejected` | `REJECTED` | Anthropic marked the account `rejected` in its rate-limit headers. There is no reset time to count down to. It clears when the background quota probe sees a quota window start over. | Nothing. It clears on a probe after the window rolls. |
| `disabled` | `OFF` | Someone ran `tcr disable` on it. | `tcr enable <name>`. |
| `reserved` | `RESERVED` | The account is in a group marked reserved, so it only takes requests that ask for that group (`tcr run --group <group>`). `tcr status` reports the fleet as a request with no group sees it, so a reserved account reads `reserved` even while it serves its group's sessions. | Nothing, if you meant it. `tcr group unreserve <group>` opens it to all traffic. |
| `parked` | `PARKED` | The account is in a group marked parked, which holds it out of every request, even one that asks for that group. An account that is also disabled on its own reads `disabled`, not `parked`. | `tcr group unpark <group>`. An account disabled on its own stays disabled. |

On a Codex row, ChatGPT's usage-limit refusal reads `rejected` only when nothing else explains
it. It reads as the window that refused it, `five-hour` or `seven-day`, with that window's reset.
Past the reset the row keeps that window, with no time, until the next quota probe confirms the
window rolled. A refusal no full window explains, for credits or a spend cap, reads `hold` while
the hold tcr set for it lasts, or `refused` if an upstream 403 holds the account. A quota probe
ends the refusal when it sees a window roll or reads the account allowed again. The details are
in [`cli.md`](cli.md#codex-rows-on---json). When an upstream 403 refuses a Codex account and no
other Codex account can take the request, `codex` gets tcr's answer for a refused login, which
names the account and `tcr login --codex --account <name>`
([`cli.md`](cli.md#--codex-launching-codex-through-the-pool)), and its requests during the
120-second hold get a 429 it retries.

In the dashboard a Codex row's chip uses ChatGPT's terms: `rejected` reads `REFUSED`, `hold`
reads `REFUSED` and the time left while ChatGPT's refusal stands, `reserved` reads `UNREACHABLE`,
and a usage window that is not 5 hours or 7 days long is named by its length (`30D 12d`).
`refused` reads `REFUSED` and the time left too while ChatGPT's refusal stands, which outlasts the
hold; otherwise it reads as on a Claude row, the seconds left of the upstream 403's hold
(`REFUSED 107s`).

## A response that breaks partway through is not moved to another account

tcr picks an account for each request inside a retry loop. A 401, a 403, a 429 or a
connection failure that arrives *before* upstream's response headers can be retried, on the
same account or on another one. Once upstream answers with a status tcr passes on, the loop is over: the
headers go to the client, and a streamed reply is copied to the client piece by piece as it
arrives. From then on there is no second account to try, because the client already holds
part of the answer.

What the client sees when it breaks:

- **The upstream connection dies mid-stream.** tcr passes the broken connection on, so the
  client's stream ends early. The log has an `upstream transport failure mid-stream` warning
  naming the account.
- **Anthropic sends an `error` event inside the stream.** It reaches the client unchanged,
  like any other piece of the stream.
- **A reply that is not streamed fails while tcr reads it.** The client gets a 502
  `proxy_error`, `Failed to read upstream response body.`

The first two count toward that account's `streamErrorCount` in `tcr status --json`. That count
is only for you to read; it never changes which account is picked. If the client retries, the
retry is a new request, and tcr chooses an account for it from the start.

## The port is already in use

By default a second `tcr` stands down rather than killing the incumbent, because two
proxies sharing one config both hold the same single-use refresh tokens, and the first
refresh by either revokes the other's. `tcr server --replace` takes the port deliberately.
Prefer standing down: a restart costs every live session its prompt cache, which is
per-account at Anthropic's end and the most expensive event in this system.

The stand-down's exit code tells you which case you are in: a healthy peer, an incumbent
running a different commit, or a wedged process holding the socket without answering. See
[Taking over the port](cli.md#taking-over-the-port).

## A restart is not automatically total loss

With `sessionAffinity` on, pins are persisted continuously and restored at boot within a
TTL; a restart inside that window keeps most sessions warm, and one outside it restores
nothing at all. The server logs how many pins it restored at boot; read that line rather
than assuming either outcome. With `sessionAffinity` off, which is now an explicit opt-out
rather than the default, there are no pins to restore. The TTL and the reasoning are in
[Session-affinity pins survive a
restart](architecture.md#session-affinity-pins-survive-a-restart).

## An account is disabled in the config but still getting traffic

`tcr enable` and `tcr disable` ask the running server first and fall back to the file only
when nothing is listening. Against a proxy too old for that route, the CLI warns that the
change will not take effect until the server restarts. Believe the warning.

## 503 with `Retry-After: 5`

Name resolution is failing, which means this machine is offline. No account was rotated and
no account was blamed; retry. That is deliberately not a 502, which would point you at the
upstream for a local fault.

## `tcr login` refuses to run

A running proxy on its own no longer causes this: `tcr login` adds the account to a live
proxy without stopping it. The refusal means the proxy holding the port is an **older build
without the `/_tcr/accounts` route**, and against that build the original hazard is real: it
reads the config at boot and its next token refresh writes its boot-time tokens back over the
file, silently clobbering the fresh login.

Update the running proxy and log in again, or stop it, log in, and start it again. `--force`
also gets past it, but it skips the live path and writes the file, which is the unsafe
behaviour this refusal exists to prevent. Reach for it last, not first.

If the pid named belongs to TcrBar, quit the application rather than killing the pid: killing
it skips shutdown and loses the session pin map.

## `tcr login --from-codex` says the proxy predates Codex accounts

```
the proxy on :3456 (pid 4242) predates Codex accounts and would store this login as a Claude account and send its token to Anthropic — nothing was written. Restart the proxy on this build (update TcrBar first if it supervises the proxy), then re-run `tcr login --from-codex`. --force does not get past this: it never trades a confirmed live route for a file write, and that proxy would refuse the file at its next start, or read it the same way if it is older still.
```

Nothing was written. The running proxy answered the import's probe without naming `codex`
among the providers it can store, which is how every proxy built before Codex accounts
answers. Why tcr refuses rather than hand it the login, and why `--force` does not get past the
refusal, is in [`cli.md`](cli.md#--from-codex-importing-the-login-codex-already-has). Restart
the proxy on this build at a quiet moment, then run `tcr login --from-codex` again. If TcrBar
runs the proxy, [update TcrBar first](#the-fix-is-in-main-and-the-fix-is-in-the-process-serving-traffic-are-different-facts).
`tcr login --codex` meets the same refusal before `codex login` runs, and its message names
`tcr login --codex` as the command to run again.

If the import instead fails saying the proxy stored the login as a Claude account, or did not
say how it stored it, the row is already live, and on disk, as a Claude account, and its token
may already have reached Anthropic: run the `tcr remove` the message names at once, then sign
the account in afresh with `tcr login --codex`, which leaves the login the `codex` CLI holds
alone (or `codex login`, then `tcr login --from-codex` again).

## After a Codex import an older proxy refuses to load the config

Every Codex row is written at `credentialVersion: 2`. A `tcr` that has the `credentialVersion`
check but predates Codex accounts (0.2.46 onward, up to the release that adds them)
understands version 1 only. Rather than read the Codex row as a Claude account, it refuses the
whole file, and its server refuses to start:

```
config at /Users/example/.config/teamclaude.json is unreadable/corrupt: config parse error: account "alice@example.com" carries credentialVersion 2, which this build only understands up to 1; refusing to load rather than silently reinterpret its credential shape — refusing to start rather than serve an empty fleet; fix the file and restart
```

The file is intact, whatever the message says, so do not edit it: lowering the version by
hand is exactly what would let that build serve the Codex login as a Claude account. Upgrade
that `tcr`, the one TcrBar bundles included, and start it again. If you have to go back to the
older build, remove the Codex account with the current one first (`tcr remove '<name>'`): the
rows left are Anthropic rows at version 1, which it reads.

`tcr` 0.2.45 and older predate the `credentialVersion` check and refuse nothing: they would
load the Codex row as a Claude account and send its OpenAI tokens to Anthropic. That is why
every `tcr` that may boot on this config has to be upgraded before a Codex login is imported,
not after.

**A Codex login warns about the older `tcr`s it can find.** Once the account is saved,
`tcr login --codex` and `tcr login --from-codex` run `--version`, and `login --help` when they
need to, on every `tcr` on `PATH`, in `~/.local/bin`, `~/.cargo/bin`, `/opt/homebrew/bin` and
`/usr/local/bin`, and on TcrBar's bundled one (`/Applications/TcrBar.app/Contents/MacOS/tcr`).
Each one that predates Codex accounts gets one warning, naming the binary, its version and what
it would do:

```
[tcr] warning: the tcr at /usr/local/bin/tcr (0.2.44) predates Codex accounts: update it before it next starts — it would read this login as a Claude account and send its token to Anthropic
[tcr] warning: the tcr at /opt/homebrew/bin/tcr (1.1.18) predates Codex accounts: update it — until then it refuses this config, so every command it runs fails
```

The first is a `tcr` at 0.2.45 or older; the second, a later one whose `tcr login --help` has no
`--from-codex`. Update the binary each warning names, or remove it if nothing uses it; for
TcrBar's, update TcrBar. The check never warns about the `tcr` running it, and skips a binary
that does not answer within five seconds or whose version it cannot read, so a missing warning
is not proof that every `tcr` is current: a `tcr` anywhere else is not looked for.

## `tcr login --codex` cannot find codex

```
could not find the codex CLI (searched TCR_CODEX_BIN, CODEX_BIN, PATH, /Users/example/.local/bin, /Users/example/.cargo/bin, /opt/homebrew/bin, /usr/local/bin, /Users/example/.npm-global/bin). Install it, or set TCR_CODEX_BIN to its path. Nothing was written.
```

Nothing ran and nothing was written. The list is every place tcr looked, in order: the two
variables, `PATH`, the four directories TcrBar searches, and the `bin` of npm's global prefix
(`NPM_CONFIG_PREFIX`, else `npm_config_prefix`, else `~/.npm-global`). `tcr run --codex` uses the
same search, and fails with the same sentence. The search goes past `PATH` because a TcrBar
started from Finder runs with a `PATH` that holds none of the places `codex` is usually
installed.

A `codex` installed somewhere else, such as under nvm or asdf in a directory of its own version,
is found by naming it: set `TCR_CODEX_BIN` to its path, the one `command -v codex` prints in a
shell where `codex` works. A variable that is set always decides, and never falls through to the
rest of the search, so a stale one is an error of its own:

```
TCR_CODEX_BIN is /Users/example/old/codex, which is not an executable file. Point it at the codex executable, or unset it. Nothing was written.
```

Fix the path, or unset the variable; `CODEX_BIN` is read the same way when `TCR_CODEX_BIN` is
unset. The rules are in [configuration.md](configuration.md#environment-overrides).

## `tcr run --codex` says it is launching codex directly (not pooled)

```
[tcr] launching codex directly (not pooled): <reason>
```

`codex` is running on its own login, around the pool: `tcr run --codex` pools a session only
when the running proxy reports a leaf it minted that intercepts `chatgpt.com`, lists a Codex
account, and that leaf's CA is on disk. Each `<reason>`, verbatim, and its fix are in
[`cli.md`](cli.md#--codex-launching-codex-through-the-pool).

One reason sends you here, because a restart does not fix it: `the proxy on :<port> is
serving the JS teamclaude leaf …`. That proxy found the JS teamclaude proxy's
`teamclaude-leaf.pem` and `teamclaude-leaf.key` in `~/.config` and loads them at every boot,
whenever both are there and load. It cannot read which hosts that leaf names, so it claims
`api.anthropic.com` alone and tunnels `chatgpt.com` untouched, and it never re-mints a leaf it
did not mint. To pool `codex`, move both files out of `~/.config` and restart the proxy at a
quiet moment: its next boot reuses or mints `~/.config/tcr-ca.pem` and a leaf under it, and
every client that trusted only `teamclaude-ca.pem` then has to trust `~/.config/tcr-ca.pem`
instead (`tcr run` points the sessions it launches at the right one by itself).

## Codex works but `tcr status` shows no Codex traffic

Then either `codex` runs on its own login, around the pool, or tcr is refusing it. Start from
the line `tcr status` prints above its header: `codexVerdict: codex is not pooled: <reason>` (`verdict:` in a fleet
of Codex accounts alone) says why the proxy cannot pool `codex`, in the words
`tcr run --codex` uses, and [`cli.md`](cli.md#--codex-launching-codex-through-the-pool) has
each reason and its fix. A session started with `tcr run --codex` said the same when it
launched, on its `[tcr] launching codex directly (not pooled): …` line.

With no such line, the proxy can pool `codex`, so look at the two notes `tcr status` prints on
stderr. `note codex-refusals` means `codex` did reach the pool and tcr refused it with a 429,
because no Codex account could serve: the Codex rows say why each is out. `note codex-unpooled`
means requests reached tcr from a login that names a FedRAMP or data-residency routing of its
own, which tcr relays on that login's credential, never pooled. Both count the last ten minutes,
and a restart of the proxy empties them.

With neither note, `codex` is not reaching tcr at all. A
`codex` you start yourself is pooled only when the proxy variable it reads for `https://` aims
at this proxy (`HTTPS_PROXY`, else `ALL_PROXY`, in either case), it trusts tcr's CA (through
`CODEX_CA_CERTIFICATE`, else `SSL_CERT_FILE`, or the system store), no `NO_PROXY` entry exempts
`chatgpt.com`, and the proxy intercepts `chatgpt.com`, which it does only while its pool holds
a Codex account; `tcr run --codex` checks and arranges all of that. A login whose ChatGPT
workspace is FedRAMP, or routed to a data residency or to a backend of its own, never shows
here: `codex` sends that workspace's requests to its own backend, or marks them with its
routing, and tcr tunnels the one untouched and relays the other on the login's own credential,
never pooled. `tcr login --from-codex` refuses to import such a login, saying why.

## A corporate `SSL_CERT_FILE` is ignored under `tcr run` and `tcr run --codex`

`codex` takes one extra CA bundle: `CODEX_CA_CERTIFICATE` when it is set, and `SSL_CERT_FILE`
only when it is not (`openai/codex@f82e365`, `codex-rs/http-client/src/custom_ca.rs:393-411`).
A pooled `tcr run --codex` sets `CODEX_CA_CERTIFICATE` to tcr's CA, and so does a plain
`tcr run` in see-through mode, so a `codex` that a Claude Code tool or plugin runs inside that
session inherits it. Either way it replaces any value you exported, so a corporate CA that
reached `codex` through `SSL_CERT_FILE` no longer does. For `chatgpt.com`, while tcr decrypts
it, `codex` only ever sees tcr's leaf. The hosts tcr tunnels untouched still present whatever
certificate your network presents, `auth.openai.com` among them, where `codex` refreshes its
own login, and `chatgpt.com` itself while the pool holds no Codex account, so behind a
TLS-inspecting proxy whose CA reaches `codex` only through `SSL_CERT_FILE` those connections
fail. This is a known limitation of `tcr run --codex`, and of a `codex` started inside a
`tcr run` session, and no tcr setting changes it. `codex` itself accepts a PEM file holding
several certificates, so a `codex` you route through tcr by hand can be pointed at one bundle
holding both CAs.

## `tcr ui` or a TcrBar update will not replace the app

You cannot replace `/Applications/TcrBar.app` while the proxy is running: TcrBar supervises
the bundled `tcr` as a child, so macOS sees an executing image inside the bundle being
swapped and Finder and Sparkle both refuse. Quitting TcrBar clears it, which means every
app update also restarts the proxy, with the prompt-cache cost above. Plan it for a quiet
moment.

## The TcrBar dev build fights the release build for the menu bar

macOS keys the menu-bar status item on the bundle id, and two processes registering the
same one get that id blacklisted by ControlCenter permanently, across reboots. The dev
build (`TCRBAR_DEV_BUILD=1`) has its own bundle id as well as its own path for exactly this
reason; do not give it the release id.

## The TcrBar build stops asking for `git fetch --unshallow`

The app's version is derived from the repository's commit count, so a `--depth 1` clone
cannot build it. Unshallow the clone and build again.
