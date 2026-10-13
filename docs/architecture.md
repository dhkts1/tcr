# How `tcr` works

The request path, the two entry modes, how an account gets picked, how quota is kept
fresh, and the files it keeps, how to check a download and how to remove it. Every tunable named here has its type, default and source citation in
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
  v  local routes: GET /_tcr/status | POST /_tcr/accounts/disabled
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
Anthropic's own API hosts using a locally generated leaf, which is why the client needs
your CA in `NODE_EXTRA_CA_CERTS`. Every other host is copied through as raw bytes and is
never decrypted. That is what keeps Claude Code's other endpoints working through a single
`HTTPS_PROXY`, and it is also a real property to understand before you run it; see
[Security](../README.md#security).

`tcr run` prefers forward-proxy mode: Claude Code decides whether you are a first-party
client by string-comparing `ANTHROPIC_BASE_URL`, so leaving that variable alone and
intercepting one layer down at `HTTPS_PROXY` keeps the client in its normal configuration.

## Account selection

Account selection is not one sort. On the normal path, `tcr` picks the eligible account
minimising `(priority ascending, least-recently-selected, soonest weekly reset)`, so
`priority` is a hard tier and rotation breaks ties within it. Eligibility means: not
disabled, not in an error state, not under a live rate-limit hold, under the switch
threshold, and not blocked for this request's model class. Four things change that picture:
`lockAccount` short-circuits everything to one account, an honoured session-affinity pin is
served even over the utilization threshold, the pacing fallback ranks in-flight count above
priority, and `revalidationServe` (on by default) serves the least-utilized survivor rather
than returning an error when the whole fleet reads over the soft threshold.

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
Anthropic's OAuth usage endpoint. It issues no `/v1/messages` call, so it creates no
messages.

**Each account is probed on its own randomly drawn schedule, not a fleet sweep.** Boot does
one whole-fleet pass so the bars populate immediately, and from then on every account
sleeps for an independently drawn interval, centred on the configured cadence, before its
own next probe, with a random initial offset so a restart re-scatters the fleet instead of
re-aligning it. This replaced a 75-second fleet-wide sweep that touched every account
inside the same window, on the dot.

Keep-warm (`warmupSeconds`) got the same treatment; it is the opposite of the probe and is
off by default: it really does post messages, and spends quota to do it.

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

## Files on disk

`tcr` keeps two directories. The config directory is always `$HOME/.config`: it does not
follow `XDG_CONFIG_HOME`, because the Node teamclaude proxy uses that path and `tcr` drops in
for it. The cache directory is `$XDG_CACHE_HOME/teamclaude` when that variable is set, and
`~/.cache/teamclaude` otherwise.

| Path | What it holds | Mode |
|---|---|---|
| `~/.config/teamclaude.json` | Every account, with its **working** OAuth access and refresh tokens or API key, and the proxy's own API key if you set one | `0600` |
| `~/.config/.teamclaude.json.tcr-*.tmp` | Left behind when a config write is killed halfway. Same contents as the config, and nothing removes them for you | `0600` |
| `~/.config/tcr-ca.pem`, `tcr-ca.key` | The local CA that signs the leaf certificate, and its private key. `NODE_EXTRA_CA_CERTS` points at the `.pem` | `0644`, `0600` |
| `~/.config/tcr-leaf.pem`, `tcr-leaf.key`, `tcr-leaf.sans` | The leaf certificate for Anthropic's hosts, its key, and the host list it was made for | `0644`, `0600`, `0644` |
| `~/.config/tcr-peers.json` | Peers only: your peer settings, the Macs you trust and any join secrets | `0600` |
| `~/.config/tcr-node.key`, `tcr-node.pub` | Peers only: this Mac's peer identity key pair | `0600`, `0644` |
| `~/.config/teamclaude-rs/teamclaude-rs-receipt.json` | Written by the cargo-dist installer (it does follow `XDG_CONFIG_HOME`), for its updater | |
| `<cache>/session-affinity.json` | Which session is pinned to which account | `0600` |
| `<cache>/session-wire.json` | The Sessions and Tools lists, including up to 120 characters of each slow or running tool call's command | `0600` |
| `<cache>/peer-state.json` | Peers only: what the proxy has learned about other Macs | `0600` |
| `<cache>/usage/<UTC-date>.jsonl` | The usage ledger behind `tcr wrap` and the spend figures, one line per request, 90 days by default | directory `0700`, files `0600` |
| `<cache>/logs/teamclaude-rs.log.<date>` | The server log, which names accounts by email. Five daily files are kept | directory `0700` |
| `$TMPDIR/teamclaude-rs.log` | The single log file older builds wrote. Newer builds leave it where it was | |

If you also run the Node teamclaude proxy, its `teamclaude-leaf.pem`, `teamclaude-leaf.key` and
`teamclaude-ca.pem` in `~/.config` belong to it. `tcr` reuses them when they are there and
never writes them.

TcrBar adds three things of its own: `/Applications/TcrBar.app`, which carries its own copy
of `tcr` and prefers it over the one on your `PATH`; its preferences, in the
`io.github.dhkts1.tcrbar` defaults domain; and a login item, if you turned on launch at
login. The proxy it runs uses the same config and cache files as the CLI.

## Verifying a download

Get `tcr` from this repository only: its
[release page](https://github.com/dhkts1/teamclaude-rs/releases) or `install.sh` from `main`,
which downloads from that page. A copy repackaged anywhere else has not been checked by
anyone here, and it is a program you are about to hand your account tokens to.

**The CLI archives.** Every `.tar.xz` on a release has a `.sha256` file beside it, and
`sha256.sum` lists them all. With both files in one directory:

```sh
shasum -a 256 -c teamclaude-rs-aarch64-apple-darwin.tar.xz.sha256
# expect: teamclaude-rs-aarch64-apple-darwin.tar.xz: OK
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
4. **Delete the config.** `~/.config/teamclaude.json` holds working credentials for every
   account in the pool, and they stay valid after `tcr` is gone. Delete it, any
   `.teamclaude.json.tcr-*.tmp` beside it, and the other `~/.config` files in the table above.
5. **Delete the cache:** `~/.cache/teamclaude/` (or `$XDG_CACHE_HOME/teamclaude/`). Its logs
   name your accounts, and `session-wire.json` holds fragments of commands your sessions ran.
6. **Undo what points at the proxy:** any `ANTHROPIC_BASE_URL`, `HTTPS_PROXY` or
   `NODE_EXTRA_CA_CERTS` you exported, in your shell or in the `env` block of a Claude Code
   settings file. `tcr run` sets these only on the session it launches, so it leaves nothing
   behind.

`tcr` never writes Claude Code's own stored login; it only reads it, to import it. If `tcr`
refreshed an imported login, that login's old refresh token no longer works, and `claude`
asks you to log in through the browser once.
