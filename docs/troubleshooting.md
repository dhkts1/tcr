# Troubleshooting

Symptoms, in the order people hit them. Flag-level detail lives in
[`cli.md`](cli.md); config keys and their defaults live in
[`configuration.md`](configuration.md).

## "The fix is in `main`" and "the fix is in the process serving traffic" are different facts

The running proxy can be several commits behind your checkout. `tcr status --json` reports
the SHA of the build that is actually serving; check it before concluding a change is live.

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
RUST_LOG=info,tcr::proxy=debug tcr server --headless
```

The module names are the library's paths: `tcr::proxy` for the request path,
`tcr::manager` for account selection and quota (it covers every module under it),
`tcr::peer` for peers. A bare `RUST_LOG=debug` also turns on debug output from every
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
| `login` | `LOGIN` | The account's refresh token was rejected, so it cannot get a working access token. tcr retries the refresh on a growing backoff, but only a new login replaces the token. | Log in again: `tcr login --account <name>`. |
| `rejected` | `REJECTED` | Anthropic marked the account `rejected` in its rate-limit headers. There is no reset time to count down to. It clears when the background quota probe sees a quota window start over. | Nothing. It clears on a probe after the window rolls. |
| `disabled` | `OFF` | Someone ran `tcr disable` on it. | `tcr enable <name>`. |
| `reserved` | `RESERVED` | The account is in a group marked reserved, so it only takes requests that ask for that group (`tcr run --group <group>`). `tcr status` reports the fleet as a request with no group sees it, so a reserved account reads `reserved` even while it serves its group's sessions. | Nothing, if you meant it. `tcr group unreserve <group>` opens it to all traffic. |
| `parked` | `PARKED` | The account is in a group marked parked, which holds it out of every request, even one that asks for that group. An account that is also disabled on its own reads `disabled`, not `parked`. | `tcr group unpark <group>`. An account disabled on its own stays disabled. |

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
