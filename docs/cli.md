# `tcr` command reference

Every subcommand and flag the `tcr` binary accepts, with the behaviour behind it. Back to
[the README](../README.md); the config file those commands read and write is documented in
[configuration.md](configuration.md).

All argument parsing lives in one file, `src/main.rs`; `src/cli.rs` is the *implementation*
of the account subcommands, not the definitions. No flag has a single-dash short form, an
alias, an environment-variable binding or a clap-level default value; every default below is
either a Rust default (`false`, `None`, empty vector) or a fallback applied after parsing,
and each one was derived from the source, so start from `src/main.rs` to check any of them.

## Conventions

`--config <path>` appears on every subcommand except `update`, `demo` and `ui`. Unset,
it resolves to `~/.config/tcr.json`.

Cache paths below use `~/.cache/tcr/`. Usage,
session snapshots, peer state and logs all follow that one directory. `XDG_CACHE_HOME`
replaces `~/.cache` when set.

**A config file that does not exist yet is created, not an error.** The first command you run
on a new machine writes the defaults to that path (`0600`, an empty `accounts` array), prints
`[tcr] created <path> with no accounts` on stderr, and then does its job. That holds for the
server and for every other verb, so `tcr status` on a fresh install answers with an empty
fleet instead of `config i/o error: No such file or directory`. A file that exists but cannot
be parsed is still an error and is never overwritten.

`tcr` and a subcommand cannot be mixed: the parser sets
`args_conflicts_with_subcommands = true`, so `tcr --port 9000 status` is a usage error
rather than a port override on `status`.

---

## `tcr` (bare) and `tcr server`

Runs the proxy. The bare form flattens the same arguments and dispatches to the same
function, so `tcr` and `tcr server` are the same command; the explicit form exists so you
can name it in a script or a launch agent.

| flag | type | default | effect |
|---|---|---|---|
| `--port <u16>` | number | config's `proxy.port` | bind port, overriding the config |
| `--config <path>` | path | `~/.config/tcr.json` | which config to load |
| `--headless` | bool | `false` | run without the TUI |
| `--replace` | bool | `false` | kill a proxy already on the port and take it |
| `--no-replace` | bool | `false` | **deprecated no-op** |

### Taking over the port

By default, starting a second `tcr` while one is already serving does **not** disturb the
incumbent: the new process stands down and exits. That is the safe direction, because replacing a
live proxy wipes its session-to-account pin map and cold-starts every live session's prompt
cache, which is the most expensive event in this system. `--replace` opts into doing it
anyway.

The stand-down carries information in its exit code. `0` means a peer proxy holds the port
and is serving code you have no reason to doubt. `3` means the incumbent is serving a
*different commit* than the binary you just ran, so `cargo build && tcr` stops instead of
proceeding as though your new build were live. `4` means the incumbent holds the listening
socket and never answered the liveness probe, which is the wedged shape and the case where
`--replace` is a recovery rather than an upgrade.

### `--no-replace` is a deprecated no-op

It parses, and it does nothing. Not-disturbing-an-incumbent is the *default's* behaviour;
the flag contributes nothing to it and is kept accepted only so existing scripts and launch
agents that already pass it keep working. Its field is read at no site in the binary.

Do not write `--replace --no-replace` in the same invocation. clap now rejects the pair by
name as a hard `ArgumentConflict`. The previous wiring made `--no-replace` a silent veto
over `--replace`, so an operator adding `--replace` to force a rebuilt binary onto the port
got a stand-down and exit 0 while `--help` told them the flag they had left in place did
nothing. The conflict error is the only outcome that cannot be misread, but it does mean an
invocation that used to "work" now fails loudly.

### Where the logs actually go

`--headless` logs to stdout **and** to a daily-rotating file under
`~/.cache/tcr/logs/` (`Rotation::DAILY`), or the older cache directory under the rule above.
The file sink is not a headless-only
feature: with the TUI running, tracing goes to the file *only*, because writing events to
stdout would corrupt the alternate screen.

The file is the sink that matters in practice. Anything launching the proxy as a background
child (TcrBar included) discards its stdout, so the log file is the only place those
events survive. The directory is created `0700` and re-asserted owner-only at every process
start, because log lines can carry account emails; if it cannot be made owner-only, `tcr`
refuses to log there rather than writing into a world-readable directory. `XDG_CACHE_HOME`
relocates it.

### The dashboard with Codex accounts

With a Codex account in the fleet, the dashboard tells the two providers apart. A fleet with no
Codex account draws exactly the dashboard it always did.

- **A banner segment per provider.** `CLAUDE 1/3 eligible │ CODEX 2/4 eligible`. Each segment
  counts only its own provider's accounts, the ones that provider's clients can get, and when
  none of them is ready it turns red and names its own next free account
  (`CODEX 0/2 eligible · next free carol@example.com in 3d`). A fleet of one provider keeps one
  segment.
- **What tcr knows about codex, on the CODEX segment.** `· not pooled: <why>`, in yellow, while
  this process does not pool codex (`no readable CA at <path>`, …); `· <n> refused in 10m`, in yellow, for the Codex requests tcr
  refused in the last ten minutes; and `· <n> unpooled in 10m`, dim, for those it relayed
  without the pool. When the line is too long for the terminal, the unpooled count drops first,
  then the refused count, then the account names in the next-free tails, then the tails. Last,
  the not-pooled reason is cut with a visible `…`: the counts and `not pooled:` always stay.
- **A section per provider.** The Claude rows sit under the pane's title, then a divider,
  `codex (<n>)`, then the Codex rows, in columns that line up with the ones above. Each section
  marks the account serving its own provider with its own `▶`. When the rows do not fit, each
  section keeps two and the rest go to the section the cursor is in, and each title says how
  many of its rows are on screen (`codex (3/5 ▼)`). `d` and `e` act on the row under the
  cursor; the cursor stays on its account when the config changes, and a row deleted from the
  config is never benched or re-enabled.
- **`none` for a window the plan does not report.** ChatGPT Pro reports its weekly window only,
  so a Pro row's 5h cell reads `none`, dim; `—` still means not measured yet. A Codex row's name
  keeps its end, where `/codex` and `/codex-2` tell two seats of one login apart
  (`bob@e…/codex`), and shows the plan's short form (`Pro`). Its Fable cell is blank.
- **ChatGPT's terms on the chip.** On a Codex row, `REFUSED` says ChatGPT is refusing the
  account, by its quota answer or by an upstream 403, and the chip's countdown says when tcr
  tries it again (`REFUSED 47m`, or `REFUSED 107s` for an upstream 403's hold). `UNREACHABLE`
  marks a Codex member of a reserved group, and a usage window that is not 5 hours or 7 days
  long is named by its length (`30D 12d`).
- **The re-login hint.** With the cursor on a signed-out Codex row, the pane's bottom border
  says who needs a re-login, why, and the command that does it:
  `erin@example.com needs re-login: the codex CLI in ~/.codex refreshed this shared login first — tcr login --codex --account erin@example.com`.
  On a narrow terminal the why is cut first, so the name and the command stay whole.
- **The empty fleet with a codex login on disk.** With no account at all and the codex CLI's
  own login at `~/.codex/auth.json`, the banner reads `FLEET 0/0 eligible`, dim, and the pane
  says `no accounts — tcr login (Claude) · tcr login --codex (Codex)` and where that login is.
  The dashboard checks only that the file exists, and never reads it.

---

## `tcr run [-- <args>]`

Launches Claude Code already pointed at the proxy, or, with `--codex`, OpenAI's `codex` CLI
routed through the pool's Codex accounts.

| flag | type | default | effect |
|---|---|---|---|
| `--config <path>` | path | `~/.config/tcr.json` | which config to read the port from |
| `--codex` | bool | `false` | launch `codex` instead of `claude` — see [below](#--codex-launching-codex-through-the-pool) |
| `-- <args>` | strings | empty | passed verbatim to `claude`, or to `codex` with `--codex` |

Trailing args are captured with `trailing_var_arg` and `allow_hyphen_values`, so
`tcr run -- -p "hi"` reaches `claude` intact, and `tcr run --codex -- exec "hi"` runs
`codex exec "hi"`.

`tcr-run` is `tcr run` under another name, for a tool that takes a Claude Code executable path:
`tcr setup tcr-run` links it beside the `tcr` on `PATH` (refusing a `tcr-run` that is not its own
symlink, and `--remove` takes it out), and `tcr-run <args>` behaves exactly as `tcr run <args>`.

If the proxy is not listening, `tcr run` launches `claude` **untouched** and says so on
stderr, so a stopped proxy never breaks the shell alias. When the proxy is up, the child
gets the routing environment and nothing else, in particular **no `ANTHROPIC_API_KEY`**,
even when `proxy.apiKey` is set. It used to get one, and that broke Claude Code: an
`ANTHROPIC_API_KEY` outranks claude's own claude.ai login as an auth source, which
**disables every claude.ai connector**, announced in one startup line that scrolls away,
after which the tools are simply absent. It bought nothing in exchange: the proxy's
`x-api-key` gate exempts loopback clients and the server binds `127.0.0.1` only, so a
`tcr run` child was always exempt. When a key is configured, `tcr run` prints a line on
stderr saying it is deliberately withholding it.

A value **you** exported is inherited untouched: an explicit choice wins, and it is the
escape hatch for a `claude` with no claude.ai login of its own, which does need some
credential to start. The process exits with `claude`'s own exit code.

**A proxy that holds no Claude account.** With the proxy up, `tcr run` reads its status once
before it launches `claude` (the same read decides [the request key](#the-request-key-on-a-launch)).
When the config holds a Codex account and no Claude account, in any state, and that proxy lists
Codex accounts and no Claude one, it warns, since the proxy will refuse every request the session
sends:

```
[tcr] warning: the proxy on :3456 holds no Claude account, so it will refuse every request from this session — add one with `tcr login`
```

A config that holds a Claude account prints nothing new, and a status that cannot be read prints
no warning. So a proxy started from another config, or whose accounts changed since, can miss it.

### The request key on a launch

`tcr run` presents the [request key](configuration.md#proxyrequestkey-who-launched-this-caller),
so the proxy can tell the sessions it launched from a caller it did not. It sends the key only
to a server whose status reports `requestKeyMode`
([the request key on `GET /_tcr/status`](#the-request-key-on-get-_tcrstatus)). An older server
compares the whole `Proxy-Authorization` header with the bare Codex marker, so a keyed
`tcr-codex` URL would quietly run unpooled there.

| launch | without the key | with it |
|---|---|---|
| Claude, see-through | `HTTPS_PROXY=http://127.0.0.1:<port>` | `HTTPS_PROXY=http://tcr:<key>@127.0.0.1:<port>` |
| Claude, base-URL | `ANTHROPIC_CUSTOM_HEADERS` as you set it | `x-tcr-key: <key>` appended after your headers and `--group`'s |
| `--codex` | `HTTPS_PROXY=http://tcr-codex@127.0.0.1:<port>` | `HTTPS_PROXY=http://tcr-codex:<key>@127.0.0.1:<port>` |
| `--codex` shell tools | `http://127.0.0.1:<port>` | `http://tcr:<key>@127.0.0.1:<port>` |

`https_proxy` always matches `HTTPS_PROXY`. The key comes from `proxy.requestKey` in the config
`tcr run` reads; `tcr run` never writes one. When it launches without the key it says why, in one
line: the config holds none, it holds a character a proxy URL cannot carry as written, the status
could not be read, or the server does not report `requestKeyMode`:

```
[tcr] launching without the request key: the tcr on :3456 does not report requestKeyMode, so it predates request keys or holds none; a restart of it picks them up
```

No line prints the key. The banners show it as `***`
(`tunnelling via http://tcr:***@127.0.0.1:3456`). The child's environment does hold it, as it holds
any proxy URL, so a process of the same user can read it there. The Codex shell-tool URL also
goes on `codex`'s command line, which any local user can read with `ps`.

A Claude launch leaves `CODEX_CA_CERTIFICATE` unchanged. A `codex` started by a Claude tool
inherits an unmarked proxy URL, keeps its CA choice and stays on its own login: tcr tunnels
that `chatgpt.com` connection without terminating TLS. Start it with `tcr run --codex` to opt
into pooling. The Claude launch names only its own CA variable on stderr:

```
[tcr] trusting our MITM leaf via NODE_EXTRA_CA_CERTS=/Users/example/.config/tcr-ca.pem
```

Every child also gets `TCR_RUN_ACTIVE=1`, on every path, the proxy-down passthrough and
`--codex` included. It means one thing: **a `tcr run` is already above you in this chain.**
`tcr run` finds `claude` on `PATH`, so on a machine where something else also wraps
`claude` that lookup can land on a launcher which wraps in `tcr run` again — the routing
environment applied twice is identical and nothing breaks, but every startup line prints
twice and a second `tcr` sits in the process tree for the session. A launcher that checks
this variable hands off to the real `claude` instead. The name avoids the `CMUX_` prefix
on purpose: cmux's own claude wrapper clears every `CMUX_*` variable before exec, so a
marker named after it would be erased in transit.

### `--codex`: launching `codex` through the pool

```
tcr run --codex                    # an interactive codex session
tcr run --codex -- exec "…"        # everything after -- goes to codex
```

The binary is found by the search [`tcr login --codex`](#--codex-a-codex-login-of-tcrs-own) uses, and the
process exits with `codex`'s own exit code: `TCR_CODEX_BIN` or `CODEX_BIN` when set
([configuration.md](configuration.md#environment-overrides)), else `codex` on `PATH`, else in the
usual install directories. Unlike the login, it keeps the shell's `PATH` as it is. The pool needs a
Codex account to serve the session; add one with
[`tcr login --codex`](#--codex-a-codex-login-of-tcrs-own), or import the one `codex` holds with
[`tcr login --from-codex`](#--from-codex-importing-the-login-codex-already-has).

**See-through, or not routed at all.** tcr has no base-URL route for `codex` yet. A pooled
launch leaves `codex` on its own backend URL, which is the real `https://chatgpt.com` for every
login tcr pools, sets `HTTPS_PROXY` and `https_proxy` to
`http://tcr-codex@127.0.0.1:<port>` (`http://tcr-codex:<key>@…` with
[the request key](#the-request-key-on-a-launch)), and sets
`CODEX_CA_CERTIFICATE` to the selected caller bundle, or tcr's CA at `~/.config/tcr-ca.pem`
when neither extra-CA variable is set.
The selected caller bundle (`CODEX_CA_CERTIFICATE`, else `SSL_CERT_FILE`) is preserved
in full and must include tcr's CA. If it does not, tcr launches Codex directly on its own
login and explains how to include both certificates. That variable is where `codex`
takes an extra CA from, and it takes none from `NODE_EXTRA_CA_CERTS`. Of Claude's variables only
`NODE_EXTRA_CA_CERTS` is set, to the same CA, for a `claude` started inside the session, and not
when you exported one, which the session then inherits untouched. A login whose ChatGPT
workspace is FedRAMP, or routed to a data residency or to a backend of its own, is never pooled:
tcr refuses to import it
([`--from-codex`](#--from-codex-importing-the-login-codex-already-has)), tunnels another backend
untouched, and relays a request that names such a routing on the login's own credential. The
launch says so on stderr:

```
[tcr] see-through mode: codex keeps its own backend URL, tunnelling via http://tcr-codex@127.0.0.1:3456; a FedRAMP or residency-routed login runs on its own credential, unpooled
[tcr] trusting our MITM leaf via CODEX_CA_CERTIFICATE=/Users/example/.config/tcr-ca.pem
```

The launch also supplies `shell_environment_policy.set.HTTPS_PROXY` and
`shell_environment_policy.set.https_proxy` overrides with the unmarked loopback URL, carrying the
request key as the `tcr` user's password when the launch sends it. Ordinary
Codex shell tools such as curl, Python and git then tunnel `chatgpt.com` on their own TLS and
credentials. These two launch overrides replace any proxy values in the config file's shell
policy while preserving other keys and filters; later explicit `-c` arguments can override
them. No Codex config file is written.

Codex's default stdio MCP environment omits proxy variables: its MCP launcher clears the
child environment and restores an allowlist. Explicit MCP `env_vars` can opt back into
inheriting them. Codex hooks inherit its session environment, so they can also carry the
marker. Give those helpers unmarked `HTTPS_PROXY` and `https_proxy` values, rather than
forwarding the marked values. For a shell-based hook, prefix the command with
`HTTPS_PROXY=http://127.0.0.1:<port> https_proxy=http://127.0.0.1:<port>`, or, so the proxy
counts it as launched on purpose, with `http://tcr:<key>@127.0.0.1:<port>` in both, the key from
[`tcr request-key`](#tcr-request-key).

The checked Codex source is [`rmcp-client/src/utils.rs`](https://github.com/openai/codex/blob/e89b7c442c0433ad5167e72a5a25bb6d0712766a/codex-rs/rmcp-client/src/utils.rs),
[`utils/pty/src/child_command.rs`](https://github.com/openai/codex/blob/e89b7c442c0433ad5167e72a5a25bb6d0712766a/codex-rs/utils/pty/src/child_command.rs), and
[`hooks/src/registry.rs`](https://github.com/openai/codex/blob/e89b7c442c0433ad5167e72a5a25bb6d0712766a/codex-rs/hooks/src/registry.rs).
The marker is routing opt-in, not process authentication; a helper deliberately given the
marked proxy is opted in as well.

**When no Codex account can serve right now.** A pooled launch whose running proxy reports no
Codex account able to serve says so first, from the same read, and then launches as usual, since
an account may free while `codex` starts:

```
[tcr] no Codex account can serve right now: 1 in the weekly window, 1 needing re-login — the soonest frees at 2026-10-15 14:00:00 UTC
```

It counts the pool's Codex accounts by why each is out, in the words tcr's own pool refusal
lists them with, but in Codex terms: an account ChatGPT refused reads `refused by ChatGPT`, and so does one
held by an upstream hold or 403 while that refusal stands, since the refusal outlasts the hold;
one in a reserved group reads `unreachable in a reserved group`, since a Codex request carries no
group that could reach it; one held by an upstream 403 alone reads `refused upstream (403)`. The
time is when the first of them frees, with the date in front when that is a day or more away.
When none reports one, the line ends `— none reports when it frees`.

**The running proxy decides.** The launcher reads the running proxy's `/_tcr/status` document
once, and pools the session only when that proxy reports a leaf it minted itself that intercepts
`chatgpt.com` for marked connections, lists a Codex account, and that leaf's CA file is on disk. It never decides from
the certificate files alone: the proxy loads its leaf once, at boot, so an older proxy, or one
whose TLS material failed to load, does not intercept what the files on disk would cover. Every
other case launches `codex` directly, on its own login, with one line saying why:

```
[tcr] launching codex directly (not pooled): no proxy answered on 127.0.0.1:3456
```

`tcr status` words the same decision, from the same function, above its header
([`codexVerdict:`](#tcr-status)), and its `--json` rows carry it as `codexRoute`
([Codex rows on `--json`](#codex-rows-on---json)), so the launch and the status never disagree.
Each reason, verbatim, and what it needs:

- `no proxy answered on 127.0.0.1:<port>`. Start the proxy, then run `tcr run --codex` again.
- `could not read the running proxy's status (<why>)`. Something holds the port, but its status
  could not be read, and `<why>` says what failed. `HTTP 401 Unauthorized — the proxy api-key
  in the config was rejected` means `proxy.apiKey` in the config is not the running proxy's,
  and `the server did not answer within 5s` means the proxy is wedged: check `tcr status` and
  [the log](troubleshooting.md#where-the-logs-are). `the response was not a tcr status
  payload (…) — an older tcr forwards this path upstream` means a `tcr` too old to have a
  status route holds the port: restart it on this build, as for the next reason.
- `the proxy on :<port> (build <sha>) predates Codex interception — restart it on this build
  at a quiet moment`. That build predates the `mitm` key of its status document, so it cannot
  say what it intercepts. Restart it on this build when a cold prompt cache costs least (see
  [A restart is not automatically total loss](troubleshooting.md#a-restart-is-not-automatically-total-loss)),
  then run `tcr run --codex` again: the restarted proxy reports what it intercepts, and if that
  is one of the cases below, its entry applies. If TcrBar runs the proxy, update TcrBar first
  ([why](troubleshooting.md#the-fix-is-in-main-and-the-fix-is-in-the-process-serving-traffic-are-different-facts)).
- `the proxy on :<port> is running without MITM: its TLS material failed to load at boot — see
  its log`. The proxy could not load or mint its TLS material when it started, so it decrypts
  nothing and tunnels every `CONNECT`. Its log has a `MITM disabled: could not load/generate
  TLS material` line carrying the error. A restart loads the material again, so it helps only
  once that cause is fixed.
- `` the proxy on :<port> holds no Codex account, so it blind-tunnels chatgpt.com and codex keeps
  its own login — add one with `tcr login --codex` ``. The proxy terminates marked `chatgpt.com` connections
  only while its pool holds a Codex account, in any state, disabled included, and tunnels it
  untouched otherwise, so the session would run on `codex`'s own login whatever the launch set.
  Add one with [`tcr login --codex`](#--codex-a-codex-login-of-tcrs-own):
  a running proxy takes it at once and counts its Codex accounts on every `CONNECT`, so the next
  `tcr run --codex` is pooled with no restart.
- `no readable CA at <path>`. The proxy can intercept marked Codex connections with a leaf it minted, but
  `<path>`, where this launcher expects the CA that leaf chains to, holds no file, so `codex`
  cannot be told to trust it. If the file was moved, put it back. If it is gone, restart the
  proxy at a quiet moment: with the CA file missing, its next boot mints a new CA and leaf, and
  anything that trusted the old CA then has to trust the new one.
- `the proxy on :<port> serves a leaf it minted but does not intercept chatgpt.com (it
  intercepts [<hosts>])`. A proxy of this build that serves a leaf it minted lists
  `chatgpt.com` in `interceptedHosts`, so this one is a different build. Compare the
  `serverSha` that `tcr status --json` reports with your build, and upgrade whichever of the
  two is older.
- `the proxy on :<port> (build <sha>) reports a leaf this build does not know ("<leaf>") — run
  tcr run --codex from the same build as the proxy`. This build knows the `js`, `minted` and
  `none` leaves only, so the proxy is a different build, most likely a newer one. Upgrade
  whichever of the two is older, then run `tcr run --codex` again.

**A direct launch removes the proxy variables that point at this tcr.** Any of `HTTPS_PROXY`,
`ALL_PROXY` and `HTTP_PROXY`, in either case, whose value names `127.0.0.1`, `localhost` or
`[::1]` on this proxy's port is removed from `codex`'s environment, and a second line names
them: `[tcr] removed HTTPS_PROXY from codex's environment: it points at tcr on :3456, which
this launch bypasses`. So a tcr that is down, or not intercepting, cannot capture the launch
that goes around it. A proxy variable aimed anywhere else, a corporate egress proxy or a local
proxy on another port, is left as it was.

**A pooled launch drops the `NO_PROXY` entries that would exempt `chatgpt.com`.** An entry in
`NO_PROXY` or `no_proxy` that matches `chatgpt.com` by the rules `codex`'s HTTP stack applies
(`chatgpt.com`, `.chatgpt.com`, a parent domain such as `.com`, or `*`) would send the session
around tcr while the banner said it was pooled. Those entries are dropped, and the launch says
so: `[tcr] dropped .chatgpt.com from NO_PROXY for codex: it exempts chatgpt.com from the proxy,
and the session would bypass tcr`. Every other entry keeps exempting what it did, and a
variable left with no entries stays set, as an empty value.

**`CODEX_CA_CERTIFICATE` outranks `SSL_CERT_FILE` in `codex`.** tcr keeps the selected
bundle, including corporate CAs, and pools only when it also contains tcr's certificate; see
[troubleshooting.md](troubleshooting.md#a-corporate-ssl_cert_file-is-ignored-under-tcr-run-and-tcr-run---codex).

**`--group` is refused.** `tcr run --codex --group <name>` fails before anything launches, with
`--group is not supported with --codex yet; run without --group`. A group reaches the proxy as
a header Claude Code sends from `ANTHROPIC_CUSTOM_HEADERS`, and `tcr run` has no way yet to make
`codex` send it, so the session would quietly route to the whole Codex pool instead.

**A 426 at the start of every session is expected.** `codex` first asks for a WebSocket, and
tcr, which relays Codex over HTTP streaming only, answers that upgrade with a 426 before
choosing any account. That answer is what makes `codex` use SSE for the rest of the session,
and tcr logs it at debug level only.

**When no account can serve.** A Codex request is served by a Codex account or not at all.
With none eligible, `codex` gets an error shaped for it:

- `usage_limit_reached` when the Codex account that frees soonest is out on a usage limit (a
  five-hour or weekly window, or a refusal from the backend), or when none reports when it frees
  and every one is out on such a limit. This 429 carries `resets_in_seconds` and, if some Codex
  account reports when it frees, `resets_at`, the time `codex` tells you to try again, and
  `codex` ends the turn on it.
- `pool_unavailable`, a 429 for other temporary gates, such as a hold on an upstream `retry-after`.
  It carries `resets_in_seconds` but
  no `resets_at`, and the same `retry-after` a Claude client gets, and `codex` retries it after
  that wait, up to five times, instead of ending the turn.
- `no_codex_account`, a 400 when the pool holds no Codex account or every remaining account
  needs a configuration or login change. Its message names `tcr login --codex` or the changes
  needed to enable an existing account. It has no `retry-after`, and Codex does not retry it:
  waiting cannot make the pool usable. An account deleted while the proxy runs no longer
  counts. With none configured, tcr blind-tunnels new `chatgpt.com` connections; an existing
  intercepted tunnel or a request sent directly to tcr's listener can still receive this
  error. Configured but unusable accounts keep marked connections intercepted, so their
  requests receive the error immediately.

These pool refusals are tcr's own, and are counted: `tcr status` notes them on stderr, and
`codexTraffic` carries the count on `--json` ([Codex rows on `--json`](#codex-rows-on---json)).

**A Codex login refused upstream.** When ChatGPT answers 403 to a pooled account's credential,
tcr tries the next Codex account. When every account it tried was refused, `codex` gets a 403, in
its own error envelope, with no `retry-after`, whose message names each refused account, in the
order they were refused, and the command that signs it in again:

```
{"error":{"type":"proxy_error","code":"credential_refused","message":"ChatGPT refused bob@example.com/codex's login — sign it in again with `tcr login --codex --account bob@example.com/codex` or TcrBar's Re-login…"}}
```

A 403, because it leaves `codex`'s own login alone: a 401 is the one status that makes `codex`
reload its `auth.json` and refresh its token, which would spend that login on a refusal that is
tcr's. `codex` retries the 403 a few times, showing the message with each `Reconnecting…` line,
and tcr's log names the same command for each refused account. The refused account is then held
for 120 seconds and reads `refused` on `tcr status`. While it is held and no other Codex account
can take a request, `codex`'s requests get the retryable `pool_unavailable` 429, which
`codexTraffic` counts, rather than the refusal again. A Claude client's answer to the same
refusal is unchanged: a 502.

The request path in full is in [architecture.md](architecture.md#codex-requests).

**`claude` inside a `tcr run --codex` session.** A pooled session gives `codex`
`TCR_RUN_ACTIVE=1` like every `tcr run` child, `HTTPS_PROXY` aimed at tcr, and
`NODE_EXTRA_CA_CERTS` naming tcr's CA. A `claude` started inside the session, by a codex tool
for instance, inherits all three, so it trusts the leaf tcr presents for `api.anthropic.com`,
even when a wrapper that honours the marker hands the call straight to the real `claude`. A
`NODE_EXTRA_CA_CERTS` you exported is passed through untouched instead; a nested `claude` then
trusts tcr's leaf only if that file holds tcr's CA.

---

## `tcr setup`

Makes typing `claude` go through tcr, one of two ways. TcrBar asks once which one you want, on a
fleet with a Claude account to serve claude (a Mac that pools Codex alone is never asked), and has a
row in Settings > General to change or undo it; these are the commands it runs.

| command | what it writes | covers |
|---|---|---|
| `tcr setup shell` | a `claude` function in your shell's startup file | terminals |
| `tcr setup claude-settings` | `env.ANTHROPIC_BASE_URL` in `~/.claude/settings.json` | every claude launch |
| `tcr setup status [--json]` | nothing | reports both, and anything in the way |

Each install takes `--remove`, which undoes it.

### `tcr setup shell [--shell zsh|bash|fish] [--remove]`

Adds this block to the shell's startup file (default: the shell `$SHELL` names):

```sh
# >>> tcr >>>  (added by tcr; remove with: tcr setup shell --remove)
claude() {
  if [ -n "$TCR_RUN_ACTIVE" ]; then command claude "$@"
  else tcr run -- "$@"; fi
}
# <<< tcr <<<
```

`tcr run` already launches plain `claude` when the proxy is down, and `TCR_RUN_ACTIVE` marks that a
`tcr run` is already above you, so neither a stopped proxy nor a nested shell breaks the command.
When no `tcr` is on `PATH` (TcrBar runs the copy inside its bundle), the block names that binary's
full path instead.

| shell | file |
|---|---|
| zsh | `$ZDOTDIR/.zshrc`, else `~/.zshrc` |
| bash | `~/.bashrc`; on macOS also the login file bash reads (`~/.bash_profile`, `~/.bash_login` or `~/.profile`, the first that exists) unless it already sources `~/.bashrc` |
| fish | `~/.config/fish/functions/claude.fish`, a file of its own |

tcr only ever edits between its own two marker lines. Running it again changes nothing, and
`--remove` takes the block out of every shell's files and leaves each one byte-identical to before
(the fish file, which held nothing else, is deleted). A symlinked startup file stays a symlink.

**A `claude` that is already defined is refused, never overwritten.** An `alias claude=…`, a
`claude()` or `function claude` (and in fish an `abbr` for `claude`, or a `claude.fish` tcr did not
write) in any of that shell's startup files would decide what `claude` means: in zsh an alias wins
over a function outright, so the block would silently do nothing. tcr exits non-zero and prints each
line it found with its `file:line`. Remove or rename that definition, then run it again. It reads
only the startup files themselves, so a definition pulled in from somewhere they `source` is not
seen.

The block covers what a terminal starts. An IDE extension or an app that launches claude on its own
never reads it.

### `tcr setup claude-settings [--force] [--remove]`

Merges one key into `~/.claude/settings.json`:

```json
"env": { "ANTHROPIC_BASE_URL": "http://127.0.0.1:3456" }
```

The port comes from the tcr config (`--config`). Every other key, and the file's own formatting,
stays as it was: the edit is made in the text and then checked against a parse, and anything that
would change more than the one key writes nothing.

**The cost, said plainly:** every claude process now talks to tcr, so while tcr is not running
claude cannot reach Anthropic at all, until tcr starts or the key is removed.

An `ANTHROPIC_BASE_URL` that already points somewhere else is refused and named; `--force` replaces
it. `--remove` takes out only a value tcr writes (`http://127.0.0.1:<port>`), and `env` with it when
that was its only key. It refuses to remove any other value.

### `tcr setup status [--json]`

```
$ tcr setup status
route: shell
shell: zsh
shellRoute: installed (/Users/example/.zshrc)
claudeSettings: not installed
```

`route` is `off`, `shell`, `everyLaunch` or `both`. Each conflict found gets its own `conflict:` line
with its `file:line`. `--json` is one object (`route`, `shell`, `shellRoute`, `claudeSettings`,
`conflicts`, `port`); each conflict carries `route` (`shell` or `claudeSettings`), `kind` (`alias`,
`function`, `abbreviation`, `foreignFile`, `baseUrl`), `path`, `line` and `text`. It exits 0
whatever it finds.

---

## `tcr login`

Runs the browser OAuth flow and adds the resulting account to the pool. With `--codex` it signs
in a Codex account instead, by running `codex login` in a folder of tcr's own; with `--from-codex`
it imports the login the `codex` CLI already holds.

| flag | type | default | effect |
|---|---|---|---|
| `--config <path>` | path | `~/.config/tcr.json` | config to write into |
| `--force` | bool | `false` | override a refusal and write the config file anyway — never overrides a confirmed live route or a rejected api-key |
| `--account <name>` | string | none | re-login a specific existing account, and refuse to write anything unless the identity that comes back resolves to it. A Codex account is refused, before anything runs, without `--codex` or `--from-codex` |
| `--token` | bool | `false` | add an account from a `claude setup-token` credential instead of the browser flow — see below |
| `--from-claude-code` | bool | `false` | add an account from the login the `claude` CLI on this machine already holds, instead of the browser flow — see below. A first run does this by itself |
| `--from-codex` | bool | `false` | add a Codex account from the login the `codex` CLI on this machine already holds — see below. Refused together with `--token` or `--from-claude-code` |
| `--codex` | bool | `false` | sign in a Codex account by running `codex login` in a throwaway folder of tcr's own, so the `codex` CLI keeps its own login — see below. Refused together with `--token`, `--from-claude-code` or `--from-codex` |
| `--codex-home <dir>` | path | none | read `<dir>/auth.json` instead of `$CODEX_HOME/auth.json`; accepted only with `--from-codex` |
| `--name <name>` | string | none | name this account explicitly instead of letting `login` mint a name for it; refused when another account already has that name |
| `--non-interactive` | bool | `false` | drive the login from another program: never reads stdin, never opens the browser itself (under `--codex`, `codex` opens it), reports progress as JSON lines — see below |

### `--codex`: a Codex login of tcr's own

```bash
tcr login --codex
tcr login --codex --account bob@example.com     # sign that Codex account in again
```

tcr runs `codex login` with `CODEX_HOME` set to a throwaway folder, `tcr-codex-login.*` beside
the config file (mode `0700`), reads the login `codex` leaves there, deletes the folder, and
imports the login the way [`--from-codex`](#--from-codex-importing-the-login-codex-already-has) does, workspace check
included. `codex` does the OAuth in the browser, which it opens itself; the login it keeps for
its own use is never touched, and the import's notice about shared token updates does not print
here. A failed `codex login` (a non-zero exit, or no login
left behind) writes nothing, and says how it ended; under `--non-interactive`, which passes
`codex`'s own lines to stderr rather than to the terminal, it quotes the last of them too.

The folder is deleted as soon as the login is read, before the workspace check goes out on the
network, and on every way out tcr can see: a failed sign-in, a refusal, and a cancel. SIGTERM,
SIGINT or SIGHUP (TcrBar's Cancel, Ctrl-C, a closed terminal) stops `codex login` and everything it
started, deletes the folder and exits with 128 plus the signal's number. A signal tcr does not trap
(a SIGKILL above all, or the SIGQUIT that Ctrl-\ sends) can leave a folder behind, holding a live
login; [architecture.md](architecture.md#files-on-disk) lists it.

```
$ tcr login --codex
[tcr] signing in a Codex account: `codex login` runs in a folder of tcr's own, so the codex CLI keeps the login it has
…  codex login's own output, passed through unchanged  …
Saved account 'bob@example.com' to /Users/example/.config/tcr.json
[tcr] 'bob@example.com' is a Codex account (ChatGPT Pro): Codex sessions started with `tcr run --codex` can use it
[tcr] tcr pools marked Codex connections to chatgpt.com: use `tcr run --codex`; keep the marker scoped to that launch, since unmarked connections keep their own login
[tcr] codex needs a login of its own to start, even when pooled, and none is at /Users/example/.codex/auth.json: unless codex keeps its login in the keyring, run `codex login` once
[tcr] warning: the tcr at /usr/local/bin/tcr (0.2.44) predates Codex accounts: update it before it next starts — it would read this login as a Claude account and send its token to Anthropic
Logged in as 'bob@example.com'.
```

The plan in parentheses is the label `tcr status` shows, left out when the login names none.
The account reaches the config the way every `tcr login` does, over a running proxy's live route
or into the file; a running proxy that cannot store a Codex account refuses the sign-in before
`codex` runs, as it refuses [`--from-codex`](#--from-codex-importing-the-login-codex-already-has).

**Two guards, before anything runs.** `tcr login --account <a Codex account>` without `--codex`
or `--from-codex` is refused, because plain `tcr login` signs in Claude accounts only, and
`tcr login --codex --account <a Claude account>` is refused the other way round. Each names the
command that does fit, opens no browser and writes nothing.

**Identity on re-login.** `tcr login --codex --account <name>` signs that account in again only
when the login that comes back is the same ChatGPT user in the same workspace; anything else
writes nothing. A different user, which is what a browser already signed in as someone else
returns, is refused naming both: sign out of chatgpt.com first, or use a private window, then
retry. The same user in another workspace is refused naming both workspaces (the first eight
characters of each id) and their plans: retry, and choose the account's workspace when ChatGPT
asks which one to use. The check keys on the ChatGPT user id, never the email: a Codex account
stores no email, and its name may be a `--name`.

**Where it looks for `codex`.** One search serves `tcr login --codex`, `tcr run --codex` and
[`tcr doctor`](#the-codex-block)'s `codexBin:` line, in this order: `TCR_CODEX_BIN`,
`CODEX_BIN`, `PATH`, then `~/.local/bin`, `~/.cargo/bin`, `/opt/homebrew/bin` and
`/usr/local/bin`, then the `bin` of npm's global prefix, which is `NPM_CONFIG_PREFIX`, else
`npm_config_prefix`, else `~/.npm-global` (tcr never runs npm). A set variable always decides
and never falls through to the rest ([configuration.md](configuration.md#environment-overrides)).
With nothing found, the refusal names every place it searched. `codex login` runs with the directory `codex` was found in first on its
`PATH`, then tcr's own `PATH`, then whichever of `/opt/homebrew/bin` and `/usr/local/bin` that
lacks: an npm `codex` is a `#!/usr/bin/env node` launcher whose `node` sits beside it, and a
TcrBar started from Finder has neither directory on its `PATH`.

**The notices**, printed after the account is saved, each only when it holds:

| notice | when |
|---|---|
| `tcr pools marked Codex connections to chatgpt.com: …` | the config held no Codex account before this one, and the proxy can intercept marked `chatgpt.com` connections: the running proxy serves a leaf it minted that covers it, or none is running, so the next one mints such a leaf |
| `codex is not pooled: <reason>` | the config held no Codex account before this one, and the proxy cannot intercept marked `chatgpt.com` connections. The reason is the one `tcr run --codex` prints ([the reasons](#--codex-launching-codex-through-the-pool)): what the running proxy reported, or that its status could not be read |
| `codex needs a login of its own to start, even when pooled, and none is at <path>: …` | there is no `auth.json` under `$CODEX_HOME`, or under `~/.codex` when `CODEX_HOME` is unset. Checked for existence, never read |
| `warning: the tcr at <path> (<version>) predates Codex accounts: update it before it next starts — …` | a `tcr` at 0.2.45 or older is in reach: it would read this login as a Claude account and send its token to Anthropic |
| `warning: the tcr at <path> (<version>) predates Codex accounts: update it — until then it refuses this config, …` | a `tcr` from 0.2.46 on whose `login --help` lacks `--from-codex` is in reach: it refuses the whole config |

The older-`tcr` check runs `--version`, and `login --help` when it needs to, on every `tcr` in an
absolute `PATH` entry, in the four directories above, and TcrBar's bundled one
(`/Applications/TcrBar.app/Contents/MacOS/tcr`). A relative `PATH` entry such as `.` is skipped:
it names a directory under wherever the login was started, and nothing there is run. It warns once
per binary however many ways it is reached, never about the `tcr` running the check, and says
nothing about a binary that does not answer within five seconds or whose version does not parse.
`--from-codex` prints the same notices.

### `--non-interactive`: driving the login from another program

This is the mode TcrBar's own "Add account…" sheet runs the CLI in, and it is usable by
anything else that can read lines and open a URL. It changes four things and nothing else:

* **stdin is never read.** The pasted-code fallback and the name prompt are not merely
  ignored, they are not constructed, so the loopback callback is the only way the login
  can complete. A profile with no email at all — the inference-only case that would
  otherwise prompt — is named with the same `unnamed` default an empty answer gives; pass
  `--name` to choose.
* **The browser is not opened here.** The URL is printed instead and the caller opens it,
  because only the caller can bring its own window forward afterwards. The exception is
  `--codex`: `codex login` opens the browser itself, and the `browser` event says so with
  `"opened":true`, so the caller does not open it a second time.
* **Progress is JSON, one object per line on stdout**, and nothing else is printed there:

  ```
  {"event":"browser","url":"https://claude.ai/oauth/authorize?…"}
  {"event":"waiting"}
  {"event":"saved","account":"alice@example.com"}
  ```

  A Codex sign-in reads, keys sorted:

  ```
  {"event":"browser","opened":true,"url":"https://auth.openai.com/oauth/authorize?…"}
  {"event":"waiting"}
  {"event":"notice","text":"tcr pools marked Codex connections to chatgpt.com: …"}
  {"account":"bob@example.com","event":"saved","plan":"ChatGPT Pro","provider":"codex"}
  ```

  A failure is `{"event":"error","reason":"…"}` on stdout instead of `saved`.

  | event | keys | meaning |
  |---|---|---|
  | `browser` | `url`, and `opened` (only ever `true`) | the sign-in page; `opened` when it was opened for the caller already |
  | `waiting` | none | waiting for the browser to come back |
  | `notice` | `text` | one notice a terminal would print on stderr; every notice comes before `saved` or `error` |
  | `saved` | `account`; on a Codex account also `provider` (`"codex"`) and `plan`, left out when the login names none | the account is saved under that name. A Claude account's line is unchanged |
  | `error` | `reason`, and `code` when the refusal is one a caller can name | the login failed; nothing was written |

  The codes:

  | `code` | the refusal |
  |---|---|
  | `codex-fedramp` | the login's workspace is FedRAMP, which tcr cannot pool |
  | `codex-routed` | the workspace check names another backend origin or a routing override |
  | `codex-unconfirmed` | the workspace check could not confirm the workspace |
  | `codex-missing` | no `codex` was found, or `TCR_CODEX_BIN` or `CODEX_BIN` names none |
  | `codex-identity` | the login that came back is not the ChatGPT user and workspace of the `--account` named |
  | `codex-login-failed` | `codex login` ended without leaving a login behind |
  | `account-is-codex` | `--account` names a Codex account, without `--codex` or `--from-codex` |
  | `account-is-claude` | `--codex --account` or `--from-codex --account` names a Claude account |
* **A failure exits non-zero** with that same reason on ONE stderr line, rather than
  anyhow's indented multi-line chain. The 2-minute callback timeout of tcr's own browser flow
  is a failure like any other. `--codex` has no timeout: it waits until `codex login` ends, or
  until the caller sends SIGTERM, which also stops `codex login` and deletes its folder.

It refuses to combine with `--token`, which reads the credential from stdin: that is the
one input this mode has no way to supply. `--from-codex` and `--from-claude-code` honour it: each
sends its import's notice as one `notice` line, a Codex import then the notices that hold, and
then `saved` or `error`.

**A login names the account itself, and never asks on the happy path.** The name is the
profile's email when no other account carries it, and `email/<org-slug>` when one does —
so signing into a second organization of the same person lands beside the first row
instead of on top of it. `<org-slug>` is the organization's name lower-cased with every
run of non-alphanumerics collapsed to `-` (`Henry Token` → `henry-token`), falling back
to the first eight characters of the org uuid when the organization reports no name.
`--name` overrides all of that, and is refused if the name is already in use: taking a
name off an existing row is how a login overwrites the wrong credential.

**`--account <name>` targets one existing account and refuses to fix the
wrong one.** Without it, `login` upserts by whatever identity the browser hands back — a
default-browser OAuth flow carries whatever claude.ai session is already signed in, so
re-logging in a dead account can silently refresh a *different*, already-healthy one and
report success while the broken account stays broken. `--account` resolves
`<name>` against the config with the same rule `tcr enable`/`tcr disable` use
(exact, case-sensitive, no substrings). A re-login keeps the row's existing name rather
than minting a new one — that name is what its pins, group labels and `controlAccount`
key already point at. It also passes the resolved account's email as OAuth's `login_hint`,
which pre-selects that address on a clean login page — this part is ergonomics only, not
a guarantee: it is unverified whether the hint overrides a browser that is *already*
signed in as someone else, which is exactly the case the default-browser flow produces.
The hint is only ever sent when the resolved account's name is address-shaped; an
account named e.g. `work` never produces `login_hint=work`.

The correctness guarantee is a separate check, after the browser round trip and before
anything is written. **An account UUID alone identifies the *person*, not the account**:
the same person routinely holds more than one organization (a corporate Pro org and a
personal Max org), each with its own token and quota, so a UUID can be identical on two
different rows in the config. The check therefore resolves the identity that came back
the same way a write would — UUID *and* org together, through the same resolution
`upsert_account` uses — and only a resolution landing on the SAME row as `--account`
counts as a match. Two rows sharing a UUID but a different org (Corp vs. Personal) are
correctly treated as different accounts, and a re-login intended for one can never be
silently written onto the other. On any other outcome — a different row, an ambiguous
resolution, or an unresolvable one — **nothing is written**, the config is left
byte-identical, and the error names both the account that was requested and the identity
the browser actually authenticated as. A profile with neither an email nor a uuid (a
failed profile fetch) is refused the same way, never treated as a pass. An account with
no stored UUID *and* a non-email display name (e.g. `work`) has nothing on file to check
a returned identity against at all; that case is refused too, with a message that says so
plainly instead of suggesting a browser sign-out that could not help. Omitting `--account`
behaves exactly as before: no identity check runs, and a fresh login is free to land on
whatever account the browser returns — that is the add-a-new-account path.

**A running proxy no longer has to be stopped.** Before opening the browser, `tcr login`
asks the proxy on the configured port whether it can take an account live, by POSTing a
deliberately-invalid body to `/_tcr/accounts` and reading the reply — always, even with
`--force`. What happens next depends on that answer:

| the proxy on the port | what login does |
|---|---|
| answers, and has the route | hands the account to the **running** proxy; it joins rotation immediately and the proxy writes the config itself — wins even under `--force` |
| answers, but rejects the configured api-key | refuses outright — `--force` does not override this |
| answers, but has no such route (an older `tcr`), or answers unusably (wedged or timed out) | refuses, unless `--force`, which writes the config file instead |
| nothing listening | writes the config file, exactly as it always did |

The live path is the one worth having. Restarting the proxy to pick up a new account
discards the session→account pin map, so every live session cold-starts its prompt cache on
its next turn — the most expensive event in this system. Adding an account live costs none of
that. It also removes a second hazard: when the CLI writes the file itself while a proxy is
running, the two can interleave, and because Anthropic's refresh tokens are single-use, a
reverted write is not recoverable by retrying. On the live path only one process writes.

Logging in again as an account already in the pool is the same operation — its credentials
are replaced in place, and it keeps its position, its priority and its learned quota.

**The refusal still exists, and still means what it said.** Against a proxy too old to have
the route, the original hazard is real: that server reads the config at boot and its next
token refresh writes its *boot-time* tokens back over the file, silently clobbering a fresh
login (observed live). The message names the port, the pid, and the ordered remedy — stop the
server, run `tcr login`, then start it again. If the pid belongs to a host application serving
the proxy in-process (TcrBar), it says so and tells you to quit the application rather than
kill the pid: killing it skips shutdown and loses the pin map.

`--force` **overrides a refusal**, not the probe: the probe still runs, and still wins when it
confirms a safe path. It never overrides a confirmed live route — that path is already safe,
so there is nothing to force. It never overrides a rejected api-key either: a rejection is
proof the proxy is alive and answering, which makes it the worst-informed moment to write the
config file beside it — fix `proxy.apiKey` to match the running server, or stop it and log in
offline, instead. Where `--force` *does* apply — no route on the port (an older `tcr`), or an
unusable answer (wedged or timed out) — it makes the unsafe path available anyway: the login
succeeds and the running server's next refresh can overwrite it. It remains only as an escape
hatch for a proxy that answers but misbehaves. Detection is read-only throughout; the server is
never signalled.

The callback server binds a random loopback port, and tokens are never printed or logged.

### `--token`: adding a `claude setup-token` credential

`claude setup-token` mints a long-lived access token for a headless or CI-style login,
without a browser. `tcr login --token` puts that token into the pool instead of running
the browser flow — same downstream config write, same live-proxy-vs-file routing above,
same "tokens are never printed or logged" guarantee, replacing only the browser half.

The token is **read from stdin**, prompted for when stdin is a terminal:

```
claude setup-token | tcr login --token
# or, interactively:
tcr login --token
Paste the token from `claude setup-token`: <paste>
```

It is never accepted as a `--token=<value>` argument, on purpose: an argv value is
visible to every other process on the machine via `ps`, and shells routinely persist
argv into history files. A prompt or a pipe are the only ways in.

A `claude setup-token` credential is not shaped like a browser login, and two
consequences follow directly from that:

**There is no refresh token, ever — not "sometimes missing", genuinely never.** The
mint requests only the `user:inference` scope, not the six scopes a normal `tcr login`
gets, and even though the token endpoint itself does return a refresh token for this
scope, the `claude` CLI that talks to it discards it before you ever see the output.
There is nothing for `tcr` to capture. The account this adds therefore serves until its
access token expires and then goes dead, with nothing to renew it — `tcr login --token`
prints that hazard on every successful add, on both the live and the file route (the
existing live-add warning above only fires through the running proxy's own wire route,
which this offline path never touches). Its expiry is stamped as **one year from now**:
that is `tcr`'s ASSUMPTION about what the mint requested (`expires_in: 31536000`, the
value the Claude CLI asks for), not something read out of the token itself — `tcr
status` shows that stamped date so an operator has something truthful to watch, rather
than "no expiry".

**There is usually no email either.** `/api/oauth/profile` needs more than
`user:inference` to answer, so the profile fetch this add still makes will very likely
come back empty — that is expected, not an error. Name the account with `--name`, or
answer the prompt when it is omitted. Decline the prompt too and it is named `unnamed`,
or the first free `unnamed-2`, `unnamed-3` when that is taken.

That collision scan is load-bearing rather than cosmetic. An account added this way has
no email and no account uuid, so it is the one kind of row `tcr` can only tell apart by
its **name** — every other account is resolved by uuid and org. Two of them sharing one
name would resolve to the same row, and the second `--token` login would overwrite the
first one's credentials instead of adding an account. The browser flow's `account-N`
fallback is derived from the account *count*, which hands out a name already in use as
soon as a row is removed; that is harmless for a credential carrying an identity to be
resolved by, and is exactly the bug here, which is why this path does not share it.

**`--token` refuses outright when combined with `--account`, and writes
nothing.** That flag exists to confirm that the identity a fresh login authenticates as
matches a specific existing row — see `--account`'s own section above. An
inference-only token carries no email and no account id for that confirmation to run
against, so the check can never be evaluated, and an assertion that cannot be evaluated
must fail closed rather than silently pass. Drop `--token` and use the browser flow with
`--account` instead, or use `--token` alone.

**The token is checked before it is written.** `tcr login --token` makes one authenticated
call with it — the same 1-token `POST /v1/messages` the keep-warm sends — and refuses on a
401 or 403, writing nothing: mint a fresh one and try again. A 429 or a 5xx is accepted (an
answer that got past auth proves the credential), and an unreachable upstream stores the
token with a warning rather than blocking an offline add. This check exists because an
account added this way is the one kind nothing else ever validates: with no refresh
token there is nothing to refresh and nothing to probe, so a mistyped or revoked token
used to be stored and reported `active` while every request it served came back 401. The
proxy now closes the other half too — a 401 on an account with no refresh token marks it
`error` on the spot (it cannot be rotation churn, since the token never changes), so a
credential that dies later shows as `error` in `tcr status` after its first failed request
instead of never.

### `--from-claude-code`: importing the login `claude` already has

If you use the `claude` CLI on this machine, you are already logged in to the account you
were about to log in to again. `tcr login --from-claude-code` copies that credential into
the pool instead of opening a browser:

```bash
tcr login --from-claude-code
```

It reads the same store the CLI does — the login Keychain item `Claude Code-credentials`
on macOS, otherwise `~/.claude/.credentials.json`. Set `TCR_CLAUDE_CODE_CREDENTIALS` to a
file path to read that instead of both; a path that does not exist means "no Claude Code
login on this machine", which is how this crate's own tests stay off the real Keychain.
Nothing here ever prints or logs a token.

Unlike `--token`, this credential is a full login: it carries a refresh token, a real
expiry and the `user:profile` scope, so the account renews itself like any browser login,
its email names the row, and `--account` works with it (there is an identity to confirm
against). `--name`, `--force` and the live-proxy add route all behave exactly as they do
for the browser flow, because it is the same finishing path.

**The cost, and it is stated on every import: refresh tokens are single-use.** The first
time tcr refreshes the imported credential, the copy the `claude` CLI is still holding
stops working, and `claude` asks you to log in again in the browser — once. Sessions
started with `tcr run` are unaffected, because `tcr run` never hands `claude` a key of its
own.

**A first run does this by itself.** Any of `tcr status`, `tcr accounts` and the server's
own boot, finding a config with no accounts in it, imports this login before doing its own
work and says so on stderr:

```
[tcr] imported 'you@example.com' (max) from your Claude Code login; this copies the login
the `claude` CLI itself uses, and each refresh token is single-use, …
```

Only when no login is found does the usual `no accounts configured — run \`tcr login\`` line
print instead. It never runs against a config that already has accounts, so an account
removed on purpose stays removed — `--from-claude-code` is the explicit way to redo it. A
Keychain read or profile fetch that fails is one warning line and the verb carries on with
zero accounts: never a non-zero exit, never a retry, and `--json` output is unchanged.

### `--from-codex`: importing the login `codex` already has

A Codex account is a ChatGPT login of OpenAI's `codex` CLI. `--from-codex` imports the login
`codex` already holds; [`--codex`](#--codex-a-codex-login-of-tcrs-own) signs in one of tcr's own instead.

```bash
codex login                 # once, if codex is not signed in yet
tcr login --from-codex
```

It reads `$CODEX_HOME/auth.json`, which is `~/.codex/auth.json` when `CODEX_HOME` is unset or
empty. Set `TCR_CODEX_AUTH` to a file path to read that file instead; a path that does not
exist means "no Codex login on this machine". That file is where `codex` keeps its login by
default, and the only place tcr looks: a login `codex` was told to keep in the system keyring
instead (its `cli_auth_credentials_store` setting) is not read, and the error for a missing file
says to use `tcr login --codex` for it, which needs no file. A file holding an API-key
login, or one with no refresh token, is refused by name, because tcr pools ChatGPT logins
only. Nothing here ever prints or logs a token.

**A workspace tcr can pool.** tcr pools a ChatGPT login only when its workspace is served at
`https://chatgpt.com` with no routing of its own, and refuses any other, with nothing written.
`codex` sends a FedRAMP or data-residency workspace's requests to its own backend, or marks them
with a routing no pooled account shares, so a pooled copy of that login could serve nothing. A
login whose id token carries `chatgpt_account_is_fedramp` is refused first, before anything is
asked. Then the import sends one `GET <codexUpstream>/backend-api/wham/accounts/check` with the
imported bearer, and imports the login only when the answer gives the workspace the backend
origin `NO_CONSTRAINT` or `https://chatgpt.com` and the routing override `NO_CONSTRAINT`. An
answer that names another origin or override is refused, quoting it. One that names neither, and
a check that cannot complete (no connection, a timeout, an HTTP error or a body that is not
JSON), refuse the import too, saying why tcr could not confirm the workspace.

**A second account** is easiest with [`tcr login --codex`](#--codex-a-codex-login-of-tcrs-own), which makes and
deletes the folder by itself. By hand, it is signed in under a `CODEX_HOME` of its own, then
imported from there. `codex` wants that directory to exist already:

```bash
mkdir -p ~/.codex-second
CODEX_HOME=~/.codex-second codex login         # or: CODEX_HOME=~/.codex-second codex login --device-auth
tcr login --from-codex --codex-home ~/.codex-second
```

`--codex-home DIR` reads `DIR/auth.json`. It is refused, with nothing written, while
`TCR_CODEX_AUTH` is set too: the variable would win, and the import would take the first
login again and report success.

**The name.** The account is named by the email in the login's id token. When an account of
either provider already holds that name, it becomes `<email>/codex`, then `<email>/codex-2`,
`<email>/codex-3` and so on, the way a second organization's Claude login becomes
`email/<org-slug>`; so a ChatGPT login under the email your Claude login uses lands beside it
as `alice@example.com/codex`. `--name` overrides all of that, and is refused when another
account holds the name. A login whose id token carries no email needs `--name`. `--account
<name>` takes the name `tcr accounts` prints, or the login's email, and refuses the import, with
nothing written, unless the login is that account's: a Codex account must be the same ChatGPT user
in the same workspace, a Claude account is refused outright, and a name no account holds must be
the login's email. It is a guard against importing from the wrong `CODEX_HOME`.

**The folder.** The row records the folder it was imported from as `codexHome`
([configuration.md](configuration.md#accounts)), so tcr can keep refreshed tokens in sync and
TcrBar can offer to import from it again. A login of tcr's own records none, and signing a row
in with `--codex` removes the one it had. A `TCR_CODEX_AUTH` override whose filename is not
`auth.json` records no `codexHome`: its standalone file is not automatically synchronized.

**Importing the same login again** updates its row in place and keeps the name the row is
saved under, so a `--name` that differs from it is refused. It is refused outright when tcr
already holds a newer credential for the account than the file does, for example when an
earlier write-back could not complete:

```
tcr already holds a newer credential for this account than `codex` does; run `codex login` first if you meant to replace it; nothing was written.
```

**A running proxy.** The import reaches the config the way every `tcr login` does, with the
same live route, file route, refusals and `--force` rules described above. Over the live route
it needs one thing more: the running proxy must name `codex` among the providers it can store,
in the `x-tcr-providers` header on its account-add route's answer. A proxy built before Codex
accounts names none. Handed the login, it would keep it as a Claude account and send its
OpenAI tokens to Anthropic, so the import is refused with nothing written, and `--force` does
not get past it: a confirmed live route is never traded for the file, and that proxy would
refuse the file at its next start, or read it the same way if it predates the
`credentialVersion` check (0.2.45 and older). The message and the fix are in
[troubleshooting.md](troubleshooting.md#tcr-login---from-codex-says-the-proxy-predates-codex-accounts).
The proxy's answer is checked as well, and must report the row stored as a Codex account. If
it does not, the row is already live as a Claude account, and the import fails naming the
`tcr remove` to run at once, followed by a fresh `codex login`.

**Token sharing, stated on every import.** Each import prints one line on stderr carrying two
notices, the first about guarded token updates to the `codex` login, the second about older
proxies, then a line on using the account, then the notices of
[`--codex`](#--codex-a-codex-login-of-tcrs-own) that hold:

```
[tcr] imported as 'alice@example.com' (ChatGPT Pro); this shares the login the `codex` CLI itself uses. Codex refresh tokens are single-use: tcr checks auth.json before writing refreshed tokens and can adopt replacement tokens for the same ChatGPT account. A simultaneous codex refresh can still race this update. Signing in again with `codex login` in that folder revokes this shared login; tcr may then need sign-in. `tcr login --codex` gives tcr a separate login. Update every tcr that may start on this config, TcrBar's bundled one included, before it next starts: an older one refuses to start, or reads this login as a Claude account.
[tcr] start a pooled session with `tcr run --codex`
```

The plan in parentheses is the label `tcr status` shows, left out when the login names no
plan. After a successful refresh of an imported account, tcr tries to update only the recorded
`codexHome/auth.json`, preserving the document's other fields and token layout, including a
refreshed id token when one is returned. tcr serializes its writers with a private advisory
lock, checks the loaded refresh token, then prepares a `0600` temporary file in the same
directory. It compares the complete original file again immediately before the atomic rename
and skips the write if the file changed. A symlinked `auth.json` is refused at import because token sharing cannot update it safely.
If a regular imported file is later replaced by a symlink, write-back preserves that symlink and refuses to overwrite it. A failed rename removes the temporary file when possible and erases its token
contents if the directory prevents removal.

A failed publication retries with exponential backoff while the file still holds the spent
source refresh token. tcr persists the source token's SHA-256 digest and returned id token
beside its current credentials, so a restart resumes publication without another refresh POST.
Text status shows `token_sync=pending`; JSON and TcrBar decode `tokenSyncPending`. Healthy
requests do not queue behind a publication, and cancellation cannot release its refresh lock.
Make the imported folder writable again, or use a separate login.

Codex does not take tcr's advisory lock. A Codex write between that final comparison and the
rename can still be overwritten; these checks cannot make sharing atomic with Codex.
`tcr login --codex` gives tcr a separate login and avoids this shared-file race.

When the file holds a changed token pair for the same ChatGPT account, tcr can recover it
instead. A live access token can be adopted directly; an expired access token must first be
refreshed using that file's newer refresh token. Before refreshing, tcr requires evidence that the file's tokens are newer. After
the current refresh token is permanently rejected, it can also recover a matching changed pair
whose issuance time is equal or unreadable and whose file timestamp is equal or missing. Known
older tokens, a different login, a malformed file or an unreadable file are rejected. If no
matching replacement is available, a rejected refresh still needs a new login.

**Upgrade every `tcr` before the first import.** A Codex row is written at
`credentialVersion: 2` ([configuration.md](configuration.md#accounts)), which a `tcr` that
predates Codex accounts cannot serve, so upgrade every `tcr` that may boot on this config, the
one TcrBar bundles included, before you import a Codex login. What an older build does with
the file, and the recovery once one has refused it, are in
[troubleshooting.md](troubleshooting.md#after-a-codex-import-an-older-proxy-refuses-to-load-the-config).

**No first run imports it.** The first-run import above takes the `claude` CLI's login only. A
Codex login enters the pool through `--codex` or `--from-codex` and nothing else.

**TcrBar** and the terminal dashboard show Codex accounts in sections of their own; see
[Watching it](../README.md#watching-it).

Once imported, the account is refreshed against `auth.openai.com`, probed at
`/backend-api/wham/usage`, never kept warm, and serves the inference requests
of `tcr run --codex` and other Codex connections carrying the proxy marker.
[architecture.md](architecture.md#codex-requests) lists all five pooled model paths.

Model availability can differ between ChatGPT plans. When ChatGPT rejects a model for one
pooled account, tcr tries another available account without holding the rejected account out
of other models. If none can serve it, tcr returns `model_unavailable` with the requested
model and a choice to use another model or make a supporting account available. It does not
automatically change the requested model.


---

## `tcr import`

Lists every Claude and Codex subscription login another tool on this machine holds, each marked
new or already in tcr, and with `--yes` imports the new ones. It is the way in from the tools in
the README's [comparison table](../README.md#compared-with-other-tools).

```bash
tcr import          # list only: reads, never writes
tcr import --yes    # import every login the listing marks new
```

| flag | type | default | effect |
|---|---|---|---|
| `--config <path>` | path | `~/.config/tcr.json` | config to compare against and write into |
| `--yes` | bool | `false` | import every login listed as new; exits non-zero when any of them could not be imported |

One line per login: where it was found, `claude` or `codex`, its email, and one of `new`,
`already in tcr as '<name>'`, `skipped: <reason>`, or `unknown` for a login whose source stores
no identity (`--yes` asks the profile endpoint before writing it, and writes nothing when the
answer is an account tcr already has).

**Where it looks.**

| source | read from | override |
|---|---|---|
| claude-swap | `sequence.json` in claude-swap's folder (`~/.claude-swap-backup` on macOS; `$XDG_DATA_HOME/claude-swap` or `~/.local/share/claude-swap` elsewhere), and each slot's `credentials/.creds-<n>-<email>.enc`, else the Keychain item `claude-swap` / `account-<n>-<email>` (macOS) | `TCR_CLAUDE_SWAP_DIR` |
| CLIProxyAPI, Quotio, ccs | every `*.json` directly inside `~/.cli-proxy-api`, `~/.ccs/cliproxy/auth` and `~/.ccs/cliproxy/auth-paused`; `type` `claude` and `codex` only, every other type is ignored | `TCR_CLIPROXYAPI_DIRS` (folders separated by `:`) |
| Claude Code | the login [`--from-claude-code`](#tcr-login) reads, named by `~/.claude.json`'s `oauthAccount` | `TCR_CLAUDE_CODE_CREDENTIALS`, `TCR_CLAUDE_CODE_ACCOUNT` |
| Codex CLI | the login [`--from-codex`](#tcr-login) reads | `TCR_CODEX_AUTH`, `CODEX_HOME` |

`TCR_NO_KEYCHAIN=1` turns every Keychain read off. A Keychain read waits at most 10 seconds (an
unanswered authorization dialog); one that is refused or times out skips that entry with
`skipped: keychain read refused or timed out`, and the run asks the Keychain nothing more.

**What is skipped, by name.** API-key logins (`skipped: API key, tcr serves subscriptions only`);
a login with no refresh token (a setup-token: `tcr login --token` adds those); a Codex file signed
in through another OAuth client (`signed in with another client`); the claude-swap slot that is
active in Claude Code, whose backup may be older than the live login the Claude Code line covers;
and a file that does not parse, named with the error's position.

**Already in tcr** means: a Claude login whose account UUID and org UUID match a row (one person in
two orgs is two rows), or, when either side has no account UUID, whose email matches a row's; a
Codex login that is the same ChatGPT user in the same workspace as a row. Such a login is never
written, so tcr's own working token is never replaced by another tool's copy, and a claude-swap
slot tcr already has is not even read, so it costs no Keychain dialog. The check runs again
against the config each write is based on, so one login found in two tools is imported once.

**The listing reads only.** `tcr import` without `--yes` never refreshes a token, never fetches a
profile, never writes the config and never talks to a running proxy. It compares by the identity
each source stores.

**`--yes` is a take, not a share.** Refresh tokens are single-use: the first time tcr refreshes an
imported login, the copy the other tool holds stops working, and claude-swap and CLIProxyAPI
refresh their own copies in the background, so leaving them running guarantees one side is logged
out. The run says so once before its first write; remove the imported accounts from those tools.
Claude Code and Codex CLI imports print the same lines `--from-claude-code` and `--from-codex` do.
Each import goes through the path `tcr login` writes by, so naming, the running proxy's add route
and every refusal behave as they do there. A login whose access token has already expired is
refreshed once before the write, and a refused refresh is that login's failure line. One failed
import never stops the others.

**What is never read.** Anything in `~/.claude.json` beyond `oauthAccount`'s `accountUuid`,
`organizationUuid` and `emailAddress`; claude-swap's other files; CLIProxyAPI's config; any
other tool's store. Nothing `tcr import` prints carries a token.

---

## `tcr accounts`

Lists the configured accounts. Offline by construction: it builds its own view from the
file and never asks the server, so its serving counters render as unmeasured rather than as
zeroes.

Each line is the account line [`tcr status`](#tcr-status) prints, a Codex account's
[tokens](#codex-tokens-on-the-account-line) included. An empty fleet gets `tcr status`'s
empty-fleet lines on stderr, the codex-login line among them, and the same exit code.

| flag | type | default | effect |
|---|---|---|---|
| `--config <path>` | path | `~/.config/tcr.json` | config to list |
| `--probe` | bool | `false` | refresh each account's live quota first, a real network call per account |

---

## Account resolution: `remove`, `priority`, `enable`, `disable`, `token`

These five take a positional `<query>` naming one account, and they all resolve it the same
way.

The rule is: **the account's exact `name`**, compared with `==` on the raw string. That
means resolution is **case-sensitive and never a substring**. A name is unique across the
config (see "Account names are unique" below), so an exact name is a complete answer and
there is nothing to narrow it by. Given an account named `alice@example.com/acme`:

```
tcr disable alice@example.com/acme       # matches: exact name
tcr disable alice@example.com            # NO MATCH unless a row is named exactly that
tcr disable acme                         # NO MATCH: the suffix is not a name
tcr disable alice                        # NO MATCH: not a substring
tcr disable alice@                       # NO MATCH: not a prefix
tcr disable ALICE@EXAMPLE.COM/ACME       # NO MATCH: case-sensitive
```

A query matching nothing is an error and the config is left byte-identical: resolution runs
before any mutation, so there is no partial write.

### Account names are unique

`tcr` will not let two accounts share a name, so a name is the whole address every account
verb takes. There used to be an `--org` flag on each of these verbs; it is gone, because a
flag that no longer narrows anything is a lie in `--help`.

It existed because the name was the email, and one person's two organizations — a personal
Max org and a company Team seat — produced two rows carrying the same one. Every by-name
path was a latent bug: `tcr token` refused as ambiguous, `tcr group add` labelled whichever
row came first and reported success, and the menu-bar panel drew one row's numbers on both.

**A config holding duplicates migrates itself, once.** On the next load, one row per
duplicated name keeps the bare email — the row on a personal plan (`claude_max` /
`claude_pro`), else the lowest priority number — and every other row becomes
`email/<org-slug>` from its stored `orgName`, falling back to the first eight characters of
its `orgUuid`. It prints one line per rename:

```
[tcr] renamed account: henry@example.com -> henry@example.com/henry-token (org Henry Token)
```

The loader never refuses a duplicated config — that would take a fleet down on upgrade —
and it writes the result straight back, so the rename happens once rather than on every
boot. `controlAccount` follows if it named a renamed row, group labels ride along
untouched (they are per-entry), and the session-affinity pin file is rewritten in the same
pass so warm sessions keep their prompt cache. If the write itself fails, the server still
boots and says so, and a CLI verb that would edit the config refuses rather than writing
under names that exist nowhere else.

A config edited by hand back into a duplicate is simply migrated again on the next load.

### `tcr remove <query>`

Deletes the account from the config. Flags: `--config`.

This is destructive and there is no confirmation prompt. The entry is removed and the file
is rewritten in place; the access and refresh tokens go with it, so recovering the account
means running `tcr login` again, not editing anything back.

**It disables the account on the running proxy first, then deletes it from the file**, the
same live route `tcr disable` uses. So a removed account leaves rotation at once, with no
restart, and the proxy stops LISTING it the next time it reads the file, which every
`tcr status` makes it do. TcrBar's row goes on its next poll.

Everything the file could refuse (no such account, a name two rows share) is checked before
the proxy is touched, and a write that fails after the disable puts the account back the way
it was, so a failed `tcr remove` changes nothing. The delete edits the file's JSON in place,
so a row the loader sets aside (an API-key row, a foreign `upstream`) does not block it and
survives it byte for byte. Deleting the control account also drops `controlAccount`;
deleting the last account writes `"claudeCodeImport": false`, so the next `tcr status` does
not import this Mac's Claude Code login into the empty fleet.

What happens depends on what the proxy answers:

| the proxy | what `tcr remove` does |
|---|---|
| accepts the disable | deletes the entry from the file; if the proxy predates live removal it keeps listing the account as disabled until it restarts, and `tcr remove` says so on stderr |
| is not running | deletes the entry from the file; there is nothing live to update |
| rejects the api-key, or refuses the query (no match, or more than one) | exits with an error and changes nothing |
| is too old to have the route | deletes the entry from the file and warns that the proxy will keep routing to the account until it restarts |
| answers something unusable, or does not answer | deletes the entry from the file and warns that the proxy may keep routing to the account until it restarts |

The refusals change nothing because a deletion cannot be undone: if the file lost the
account while the proxy kept rotating it, there would be no record left to fix it from.

### `tcr priority <query> [N]`

Sets rotation priority. **Lower value is preferred.** Flags: `--first`, `--last`, `--config`.

| form | effect |
|---|---|
| `tcr priority alice@example.com 5` | writes `5` verbatim |
| `tcr priority alice@example.com --first` | `min(0, existing priorities of the account's own provider) - 1` |
| `tcr priority alice@example.com --last` | `max(0, existing priorities of the account's own provider) + 1` |

The `0` seed in those relative forms guarantees the move crosses the default tier even when
every existing priority sits on the same side of it.

Claude and Codex accounts rotate apart, since neither serves the other's requests, so the
relative forms read only the priorities of the account's own provider; in a fleet with no Codex
account, that is every account. On a Codex account, `--first` and `--last` say which end of
the Codex rotation it now holds: `Set priority of '<name>' to <n>, the front of the Codex
rotation.` (or `the back`). An explicit `N`, and any move of a Claude account, prints
`Set priority of '<name>' to <n>.` as before.

The positional value conflicts with `--first`/`--last`, and `--first` conflicts with
`--last`. There is no default: omitting all three is a runtime error,
`provide a priority value, or one of --first / --last`.

**It changes the running proxy first, then the file**, so the next new session is routed by
the new order with no restart. The value is worked out from the config file (that is what
`--first` and `--last` are relative to), then POSTed to the proxy's
`/_tcr/accounts/priority` route, which sets it in the live rotation. The command then writes
the file. A session already pinned to an account stays on it; priority decides where new
sessions go.

| the proxy | what `tcr priority` does |
|---|---|
| applies it | writes the file |
| is not running | writes the file; there is nothing live to update |
| rejects the api-key, or refuses the query (no match, or more than one) | exits with an error and changes nothing |
| is too old to have the route | writes the file and warns that the proxy keeps the old order until it restarts |
| answers something unusable, or does not answer | writes the file and warns that the proxy may keep the old order until it restarts |

### `tcr enable <query>` and `tcr disable <query>`

`disable` holds an account out of rotation; `enable` clears the flag. Flags: `--config`.

**These act on the running proxy first, not on the file.** A file-only write was the
original bug: the proxy reads `disabled` from the config once, at startup, and never again,
so `tcr disable alice@example.com` exited 0, printed a confident line, and the proxy kept
handing that account live traffic while every surface reported it benched. The command now
POSTs to the proxy's `/_tcr/accounts/disabled` control route and only touches the file when
it has to.

The four outcomes:

- **The proxy applied it.** Done. Any caveat the proxy returned is printed as a warning.
- **Nothing is listening.** The quiet, historical case: the file is written, and there is no
  live rotation to disagree with it.
- **The proxy rejected the key.** `proxy.apiKey` did not match. The command changes
  *nothing* and exits non-zero, on purpose: writing the file here would put the old lie in a
  new place, with the config saying benched while the proxy you could not reach keeps
  rotating the account.
- **The route is missing.** An older `tcr` is serving. The file gets written, and the command
  says loudly that this is only half a disable.

If you have set `proxy.apiKey`, these two commands need it; they go through the same
loopback-plus-key gate as every other `/_tcr/` route, with no loopback exemption. See
[configuration.md](configuration.md#proxyapikey-is-a-security-control-not-a-convenience).

### `tcr token <query>`

Prints the account's current access token to stdout — one line, nothing else — so it can
be piped or copied. Flags: `--config`. Read-only; a non-matching query exits
non-zero with the file untouched. It reads the file rather than the running proxy, which is
current enough: every refresh the proxy performs is written straight back to the file.

The token is a credential. Pipe it (`tcr token alice@example.com | pbcopy`); do not paste
it into a chat, a ticket, or a command line that lands in shell history. TcrBar's
right-click **Copy Access Token** runs exactly this and puts stdout on the pasteboard.

---

## `tcr request-key`

Prints the request key ([`proxy.requestKey`](configuration.md#proxyrequestkey-who-launched-this-caller))
to stdout, one line and nothing else, for a launcher outside tcr that needs to present it: a
container forwarder, or a script that sets `HTTPS_PROXY=http://tcr:<key>@127.0.0.1:3456`.
Flags: `--config`.

It never writes a key. The server writes one into the config the first time it starts without
one, so the key this prints and the key the server holds are always the same. With no key in
the file it exits 1 and says so:

```
Error: no request key in /Users/example/.config/tcr.json: the tcr server writes one there when it starts
```

It reads the config file without loading the rest of it, and changes nothing. The key is a
secret, like a `tcr token`: pipe it, do not paste it anywhere it is kept.

---

## `tcr mint`

Mints a long-lived (365-day) token for an existing Claude account, or for every account
carrying a group label, and puts the result on the clipboard. Nothing is stored: the config
file is read and never written, so this command exports a token and keeps nothing.

| flag | type | default | effect |
|---|---|---|---|
| `--account <name>` | string | none | mint for one account, by its exact name (see "Account resolution" above) |
| `--group <label>` | string | none | mint for every account carrying this group label, one at a time |
| `--config <path>` | path | `~/.config/tcr.json` | config to read |

Exactly one of `--account` and `--group` is required; a group no account carries is an error.
For each account, `tcr mint` prints an authorize URL on stderr; you sign in as that account
and paste back the `code#state` the page shows. The exchange is checked against the account
it was minted for: a token issued for a different email, or for any lifetime other than the
365 days asked for, is discarded and never reaches the clipboard. The clipboard gets one
`<name>`, tab, `<token>` line per account that minted.

stdout is a report, never a token: one line per account, `<name>`, tab, `OK`, tab, `<days>d`,
tab, `<org>` for a mint, or `<name>`, tab, `REFUSED`, tab, `<reason>`. A last line counts them:
`<minted>/<n> minted`. The exit code is 0 when every Claude account it targeted minted, and 1
otherwise, so a program driving it can tell.

**A Codex account cannot mint.** Only a Claude account has a long-lived token to mint.
`--account` naming a Codex account is refused before any URL is printed, and changes nothing.
A `--group` mint skips its Codex members: each gets its own line, `<name>`, tab, `REFUSED`,
tab, `Codex account (tcr mint is Claude-only)`, none of it reaches the clipboard, and the
tally counts it apart, as in `2/2 minted · 1 Codex account skipped`. A skipped member is not
a failed mint, so it does not change the exit code.

---

## `tcr status`

Probes every account's live quota and prints the fleet.

| flag | type | default | effect |
|---|---|---|---|
| `--config <path>` | path | `~/.config/tcr.json` | config to read |
| `--json` | bool | `false` (text) | emit a JSON array instead of greppable text |
| `--check` | bool | `false` | exit **4** when an enabled account's last probe failed; output unchanged |

It asks the running proxy where there is one and falls back to an offline read where there
is not; the output labels which it got, so a fallback is never silently presented as a live
measurement.

With no accounts configured, it first tries to import this machine's Claude Code login
(see `tcr login --from-claude-code`). When there is none to import, it prints the line
`no accounts configured — run \`tcr login\` to
add one` on stderr and exits **0**: stdout stays the ordinary empty table (`[]` under `--json`), so a
caller piping it into `jq` — TcrBar's panel among them — decodes an empty fleet rather than a
failure. The same line and the same exit code come from `tcr accounts`.

When `codex` has a login of its own on this machine, at `$CODEX_HOME/auth.json`, else
`~/.codex/auth.json`, a second line follows that one:

```
[tcr] found a codex login at /Users/example/.codex/auth.json: `tcr login --codex` adds a Codex account of tcr's own, and `tcr login --from-codex` imports that one with guarded token updates shared through auth.json
```

It prints on an empty fleet only, and only on that evidence: the file is checked for existence
and never read, and tcr never imports it on its own. A config holding `"claudeCodeImport": false`
gets the line too, since that key stops only the Claude Code import. With no such file the
output is what it always was.

**Above the header,** the text form prints a verdict line for each route that has a problem,
Claude's first:

```
verdict: this proxy is not on Claude's route: /Users/example/.claude/settings.json sets https://gateway.example.com
codexVerdict: codex is not pooled: the proxy on :3456 is running without MITM: its TLS material failed to load at boot — see its log
```

- **Claude's `verdict:`** prints when Claude Code is routed elsewhere, or routed here with
  nothing answering ([`tcr doctor`](#tcr-doctor)'s exit 2 or 3), and only while the fleet holds a
  Claude account or no account at all: a fleet of Codex accounts alone is not told about a route
  none of its accounts uses.
- **The Codex line** prints when the fleet holds a Codex account, in any state, and
  `tcr run --codex` would not pool `codex`. It reads `codex is not pooled:` and the reason
  `tcr run --codex` prints, word for word
  ([the reasons](#--codex-launching-codex-through-the-pool)), so read offline it says that no
  proxy answered. It is keyed `codexVerdict:` in a fleet that also holds a Claude account, and
  `verdict:` in a Codex-only fleet, where it is the only verdict.

**On stderr,** in the text form only, a fleet that holds a Codex account gets a note for each
thing the running proxy did with `codex` in the last ten minutes, each only while its count is
above zero: the 400 or 429 pool refusals tcr answered `codex` with itself, with the latest one's
type, and the requests it relayed on the client's own credential because they named a routing
of their own.

```
[tcr] note codex-refusals: refused_10m=12 last_type=usage_limit_reached — tcr refused codex because no Codex account could serve; see the Codex rows above
[tcr] note codex-unpooled: relayed_10m=3 — requests from a FedRAMP or residency-routed ChatGPT login ran on their own credential, unpooled
```

The counts are `codexTraffic`'s ([Codex rows on `--json`](#codex-rows-on---json)). The verdict
lines and the notes print the same with and without `--check`, and never change its exit code:
`codex` not being pooled is not a failed probe.

### `--check`: an exit code for scripts

Without the flag, `tcr status` exits **0** whatever the probes said. With it, the output is the
same bytes and only the exit code changes:

| exit | meaning |
|---|---|
| **0** | no enabled account's last probe failed |
| **1** | the command could not run (an unreadable config, say); not a probe verdict, so a script tells "tcr failed" from "a probe failed" |
| **4** | at least one enabled account's last probe ended in `error`, `timeout` or `upstream-down` |

These do not fail the check: `never` (no probe has run, so there is nothing to judge),
`rate-limited` (the usage endpoint throttled the probe itself; the account's serving quota is
untouched and its last bar is kept), and any account that is disabled, whatever its probe says.

A Codex row counts exactly as a Claude row does: an enabled one whose last probe failed makes the
exit 4. The [re-login line](#codex-tokens-on-the-account-line) a signed-out Codex row puts on
stderr is not a probe verdict, and never changes the exit.

The code is 4 rather than 1 for the reason `tcr doctor` uses 2 and 3: 1 is what any verb exits
with when it could not run at all (an unreadable config, say), and a script should be able to tell
"the fleet has a failed probe" from "the command failed". 2 and 3 are doctor's route verdicts,
which `tcr status` also prints, so they stay out of the way.

The check is opt-in because the default exit is a contract. TcrBar runs `tcr status --json` and
reads any non-zero exit as a failed poll (`StatusPoller.classify`), which would blank the panel
every time one account's token was rejected.

```sh
tcr status --check >/dev/null || echo "a probe failed"
```

### Codex tokens on the account line

A Codex account's line is a Claude account's line with a few more tokens. A Claude line carries
none of them, so a fleet without a Codex account prints exactly what it always did. With the
weekly reading elided:

```
account bob@example.com/codex provider=codex priority=0 plan=ChatGPT Pro 5h=none wk=… cache=n/a state=ok status=active probe=ok stream_errors=0 credits=12.50 workspace=1a2b3c4d
```

- **` provider=codex`** follows the name.
- **`5h=none` and `wk=none`** say the account's plan reports no such window: measured on ChatGPT
  Pro logins, Codex reports a weekly window only, so a Pro row reads `5h=none`. That is a fact
  about the plan; `n/a` keeps meaning only that a window has no reading yet.
- **The tail**, after the group tokens, holds these tokens in this order, each only when it has
  something to say:

| token | what it says |
|---|---|
| `login_error=<code>` | why the login died, OpenAI's error code verbatim (`refresh_token_reused`, …): `loginError` on `--json` |
| `limit_reached=<kind>` | why ChatGPT refused the account, verbatim (`rate_limit_reached`, …), while that refusal stands: `limitReachedType` |
| `spend_control=reached` | the quota probe says the account's spend control is reached |
| `credits=unlimited`, `credits=<balance>` | the account's credits: `unlimited`, else the balance as reported while the account has credits |
| `reachable=no` | the account's gate is `reserved`: it is in a reserved group, which no Codex request can ask for, so nothing reaches it |
| `workspace=<8 hex>` | the first 8 hex digits of the account's ChatGPT workspace id (`orgUuid`), dashes skipped, as TcrBar shows it |

**The header** of a fleet that holds both kinds of account ends with each pool's count of
eligible accounts, Claude first:

```
status source=live … eligible=claude:2/3,codex:1/2
```

The second number of each pair is every account of that provider, and the first the ones whose
gate is `ok` and that are not disabled, as the terminal dashboard counts them. So an account held
by an upstream 429 or 403 is not eligible until its hold ends. A Claude-only or Codex-only
fleet's header has no `eligible=` token.

**A signed-out Codex account** (gate `login`) gets one more line on stderr, in the text form only,
after the account lines and before the Codex notes above. It says why the login died, when tcr
knows, and the command that signs it in again:

```
[tcr] carol@example.com/codex needs re-login: the codex CLI in ~/.codex refreshed this shared login first — tcr login --codex --account carol@example.com/codex
```

The why is the dashboard's, word for word. What to do about it is in
[troubleshooting.md](troubleshooting.md#a-codex-account-says-it-needs-sign-in).

A Codex account held after an upstream 403 reads `refused` as its `--json` `gate`, as a Claude
account does; [the gate table](troubleshooting.md#what-the-gate-value-means) says what each value
means on a Codex row.

### The weekly quota pair on `--json`

Each row carries `sevenDay`/`sevenDayState`/`sevenDayResetAtMs` for the shared weekly bucket, and
the same three keys again under an `Oi` suffix for the Fable weekly (`unified-7d_oi`) bucket. No
per-row table exists for the row's other quota fields, so this documents only the pair this change
adds:

| key | shape | what it is |
|---|---|---|
| `sevenDayOi` | fraction or `null` | Fable weekly utilization (0.0–1.0), evaluated live. `null` when never learned |
| `sevenDayOiState` | `"ok"` / `"near"` / `"spent"` or `null` | this window's own state against the account's threshold, `null` when `sevenDayOi` is |
| `sevenDayOiResetAtMs` | Unix ms or `null` | when this window resets, `null` once it has already elapsed with nothing learned since, or if it was never learned |

Unlike the shared `sevenDay` bucket, this window gates **Fable requests only** — a non-Fable
request never checks it, and `held[]`/the general `quotaState` never reflect it either. It exists on
the wire so a Fable-scoped caption and tint have something to read.

### How old the quota reading is on `--json`

| key | shape | what it is |
|---|---|---|
| `lastProbeMs` | Unix ms or `null` | when the background usage probe last finished, whether it succeeded or failed |
| `quotaUpdatedAtMs` | Unix ms or `null` | when a quota reading was last taken from either source: a successful probe, or a served response that carried rate-limit headers. `null` when nothing has been read since the server started |

Use `quotaUpdatedAtMs` to judge whether the bars are current. `lastProbeMs` cannot: a busy
account's bars move from response headers between probes, and a failed probe moves
`lastProbeMs` while reading nothing, so an idle account with a broken probe shows frozen bars
next to a recent probe time. A server built before this key existed omits it.

```sh
tcr status --json | jq -r '.[] | "\(.name)\t\(((now * 1000 - (.quotaUpdatedAtMs // 0)) / 60000) | floor)m"'
```

### The group keys on `--json`

Each row carries its group labels plus the subsets that change what the row can serve. All
three are always arrays — `[]`, never `null`: "this account has no groups" is a known fact,
not an unmeasured one.

| key | shape | what it is |
|---|---|---|
| `groups` | array of strings | every label this account carries |
| `reservedGroups` | array of strings | the subset marked `reserved` — the row serves only traffic that asked for one of its groups |
| `parkedGroups` | array of strings | the subset marked `parked` (`tcr group park`) — non-empty means the row is out of rotation entirely, and names which group did it |

A parked row's `gate` reads `"parked"`. Every `gate` value, what it means and what to do
about it is in [What the `gate` value means](troubleshooting.md#what-the-gate-value-means).
A row that is BOTH parked by its group and disabled on its own
reports `"disabled"`: the row's own state outranks the group's, so the output never
attributes a bench to a group when a person did it.

```
tcr status --json | jq -r '.[] | select(.parkedGroups | length > 0) | "\(.name)\t\(.parkedGroups | join(","))"'
```

### The plan and org keys on `--json`

Every row carries which plan the account is on, and which org it belongs to — facts to SHOW
beside a row, not a way to address it. The `name` is the address, and it is unique. These
answer the question a reader looking at two rows of one person's two organizations still
has: *which* of these is the company seat?

| key | shape | what it is |
|---|---|---|
| `plan` | string or `null` | the customer-facing label: `Max 20x`, `Team 5x`, `Team Standard`, `Pro`, … `null` for an account never profiled — never a fabricated default |
| `organizationType` | string or `null` | the provider's own word, verbatim: `claude_max`, `claude_team`, `claude_pro`, `claude_enterprise` |
| `rateLimitTier` | string or `null` | verbatim, e.g. `default_claude_max_20x` — the rate-limit multiplier, when it names one |
| `seatTier` | string or `null` | verbatim, e.g. `team_standard` / `team_tier_1` — which seat this row holds. `null` on Max and Pro, which have no seats |
| `orgUuid` | string or `null` | the org this account is scoped to |
| `orgName` | string or `null` | the org's display name. Two orgs can share one, which is why `orgUuid` is the exact fact and this is the readable one |

`plan` is DERIVED from the three raw keys, in one place, so the JSON, the plain text (`plan=`), the
TUI and the macOS panel cannot disagree. The plan word comes from `organizationType`; the suffix
after it is the `rateLimitTier` multiplier (`_20x` / `_5x`) when there is one, otherwise — on Team
and Enterprise only — the seat (`team_standard` → `Standard`, `team_tier_2` → `Tier 2`). The
multiplier wins because it is the stronger statement: a premium Team seat reads `team_tier_1` with a
`_5x` tier, and `Team 5x` says what the account can do where `Team Tier 1` says nothing. An
`organizationType` we do not recognize is passed through verbatim rather than bucketed into a plan we
do — a new plan name shows as itself:

```
tcr status --json | jq -r '.[] | "\(.name)\t\(.plan // "unprofiled")\t\(.orgUuid // "-")"'
```

The plan is learned at login, and backfilled for accounts that logged in before these keys existed:
the background quota probe fetches the profile once for any account that has no `organizationType`,
records it, and writes it to the config. So a fresh checkout shows `null` until each account's first
probe, not forever.

### Codex rows on `--json`

The output stays a bare array of rows. A Codex account's row carries `"provider": "codex"`; an
Anthropic row carries no `provider` key at all, never `null`, so a client written before the key
reads every Anthropic row exactly as before. On a Codex row `organizationType` is the ChatGPT plan,
verbatim (`pro`, `plus`, `team`, …), `plan` is its label (`ChatGPT Pro`), and
`rateLimitTier`, `seatTier` and `orgName` are `null`. `orgUuid` is the ChatGPT workspace id, the one
tcr sends as `chatgpt-account-id`, and is `null` on a row whose `accountUuid` still holds the
workspace ([configuration.md](configuration.md#accounts) has that shape). A plan word tcr does not know is labelled `ChatGPT` and the word as ChatGPT wrote
it (`edu` reads `ChatGPT edu`), unless the word already starts with `ChatGPT`. That plan is read
from the login's token claims at import and kept current by the quota probe, whose `plan_type`
names it on every probe: when the plan changes at ChatGPT (Plus to Pro, say), the new one is written
to that row's `organizationType` in the config, and no other key is touched. A probe whose
`plan_type` is missing or blank names no plan, and leaves the one the row has. The plain text
carries the same label (`plan=ChatGPT Pro`), and the terminal dashboard lists Codex rows in a
section of their own, with the plan's short form (`Pro`) after the name.

```
tcr status --json | jq -r '.[] | select(.provider == "codex") | "\(.name)\t\(.plan // "unknown")"'
```

A Codex account's quota windows fill the same `fiveHour` and `sevenDay` keys an Anthropic
account's do, matched by window length rather than by the slot Codex reports a window in.
Measured on ChatGPT Pro logins, Codex reports a weekly window only, so such an account's
`fiveHour` stays `null` while `sevenDay` moves; the mapping is in
[architecture.md](architecture.md#codex-requests).

A Codex row also says which windows its plan has, and what Codex reported about the account
beside them. Each key is on Codex rows only, and absent rather than `null` when there is nothing
to say:

| key | shape | what it is |
|---|---|---|
| `quotaWindows` | array of `"fiveHour"` / `"sevenDay"`, in that order | which usage windows the account's plan reports. Absent: not known yet, so a reader draws both. `[]`: the plan reports no usage window at all, which is a fact, not a gap. Once the quota probe has answered, its answer outranks the response headers', which may carry a five-hour slot the plan does not have |
| `fiveHourWindowMinutes` | integer | how long the window in the five-hour bucket is, in minutes, as reported (`300`). Absent when no length was reported |
| `sevenDayWindowMinutes` | integer | how long the window in the weekly bucket is, in minutes: `10080` is seven days, and a 30-day window reads `43200`. Absent when no length was reported |
| `limitReachedType` | string | why ChatGPT refused the account, verbatim: `rate_limit_reached`, or a credits or spend-cap kind. Present exactly while that refusal stands |
| `spendControlReached` | `true`, or absent | the quota probe says the account's spend control is reached |
| `credits` | `{hasCredits, unlimited, balance?}` | the account's credits, from the last response or probe that reported them. `balance` is verbatim (`"12.50"`), and absent when none was reported |
| `loginError` | string | why the account's login died, from its last refresh: OpenAI's error code verbatim (`refresh_token_reused`, `refresh_token_expired`, `refresh_token_invalidated`, `invalid_grant`, …), or `http_401` for a 401 that named none. Present while the login is known dead; a successful refresh or a fresh login clears it |
| `codexHome` | string, an absolute path | the folder the account's login was imported from (`--from-codex`). Absent for a login of tcr's own (`--codex`) and for a row imported before tcr recorded it |
| `codexRoute` | `{pooled, code?, reason?, upstream?}` | whether `tcr run --codex` would pool `codex` through the running proxy, the same on every Codex row (below). Absent when the read was not live |
| `codexTraffic` | `{refused10m, lastRefusal?, lastRefusalAtMs?, unpooled10m}` | what the running proxy did with `codex` in the last ten minutes, the same on every Codex row (below). Absent when the read was not live, or the proxy predates the counts |

`codexRoute` is the decision `tcr run --codex` acts on, worked out by this command from the
running proxy's own report of what it intercepts and from tcr's CA on disk. `pooled: true`
carries no `code` and no `reason`. Otherwise `code` says why, and `reason` is the sentence the
launch prints, verbatim:

| `code` | `reason` |
|---|---|
| `no-proxy` | `no proxy answered on 127.0.0.1:<port>` |
| `status-unreadable` | `could not read the running proxy's status (<why>)` |
| `predates` | `the proxy on :<port> (build <sha>) predates Codex interception — restart it on this build at a quiet moment` |
| `mitm-off` | `the proxy on :<port> is running without MITM: its TLS material failed to load at boot — see its log` |
| `leaf-missing-host` | `the proxy on :<port> serves a leaf it minted but does not intercept chatgpt.com (it intercepts [<hosts>])` |
| `unknown-leaf` | `the proxy on :<port> (build <sha>) reports a leaf this build does not know ("<leaf>") — run tcr run --codex from the same build as the proxy` |
| `no-codex-account` | `` the proxy on :<port> holds no Codex account, so it blind-tunnels chatgpt.com and codex keeps its own login — add one with `tcr login --codex` `` |
| `no-ca` | `no readable CA at <path>` |

They are tried in that order, and the first that holds wins; what each needs is under
[`--codex`](#--codex-launching-codex-through-the-pool). `--json` carries a route only from a
live read, so `no-proxy` and `status-unreadable` appear only in the text form's
[`codexVerdict:`](#tcr-status) line. `upstream` is present when the proxy sends Codex requests
somewhere other than the default `https://chatgpt.com` (`codexUpstream`), pooled or not.

`codexTraffic` counts, over the last ten minutes, the 400 or 429 pool refusals tcr answered a
Codex request with itself (`refused10m`), whatever their type, and the requests it relayed on
the client's own credential, unpooled, because they named a FedRAMP or data-residency routing of their own
(`unpooled10m`). `lastRefusal` is the latest pool refusal's `error.type` (`no_codex_account`,
`usage_limit_reached` or `pool_unavailable`) and `lastRefusalAtMs` when it was sent; both are
present exactly while `refused10m` is above zero. The counts live in the proxy's memory, so a
restart empties them until the next refusal.

A client that does not recognize a newer server's refusal kind keeps the counts and timestamp
and skips that optional label.

On a Codex row, `held` labels each window by that reported length: whole days read `{n}d`, whole
hours `{n}h`, anything else `{n}m`, so a 30-day weekly window is held as `30d`. Without a length,
and on every Anthropic row, the labels stay `5h` and `7d`.

A Codex account at its usage limit reports the window that refused it as its `gate`:
`"seven-day"` or `"five-hour"`, with `freeAtMs` and `secondsUntilFree` naming when that window
rolls. With both windows full, the later of the two binds, since both must roll before the account
serves. Once that reset has passed, the row keeps the window's `gate`, with a `null` `freeAtMs`,
while the refusal stands: until a usage probe reads the new window, or a request tcr sends the
account is answered with a window. It is back in the pool once its hold lapses. A refusal
that no full window explains, such
as one for credits or a spend cap, still sets a hold: tcr holds every Codex quota refusal, to its
windows' reset (at most an hour), else to the 429's `retry-after`, else for 60 seconds. While that
hold lasts the row reads `"hold"` with `rateLimitedUntilMs`, because the account really is held
until then; once it lapses, the refusal that still stands reads `"rejected"` with a `null`
`freeAtMs`. A refused account is back in the pool once that hold lapses, and if an upstream 403
then holds it, the row reads `"refused"` until that hold ends, as on any other row. These readings
are display only. They change no routing, and whichever of them the row shows, a refused account
is probed around half the cadence, as every account upstream answered `rejected` is
([configuration.md](configuration.md#both-cadences-are-random-per-account-not-a-fleet-sweep)).

A usage probe ends a Codex refusal when it sees one of the account's windows roll, or when it reads
the account allowed: not at its limit, with its spend control not reached and no refusal kind. The
second is how a probe ends a refusal for credits or a spend cap, which no window roll explains. An
Anthropic row's `"rejected"` is unchanged: a probe ends it only on a window roll.

Under the pool, `codex`'s own limit display disagrees with these rows, and that is expected. A
pooled response has its per-account `x-codex-*` quota headers stripped, because they describe
whichever account served that one request, so `codex`'s in-session limit line goes blank.
`codex`'s own usage call (`/backend-api/wham/usage`) is relayed on the client's own login, so its
`/status` shows that login's limits, not the serving account's. The pool's limits are in TcrBar
and `tcr status`.

### The `usage` object on `--json`

Every row carries `usage`: what that account spent, aggregated by the proxy as it served each
request rather than by re-reading transcripts afterwards.

```
tcr status --json | jq '.[] | {name, today: .usage.today.costUsd, hour: .usage.lastHour.costUsd}'
```

| key | shape | what it is |
|---|---|---|
| `usage.today` | totals | the **local calendar day** of the machine the server runs on |
| `usage.window` | totals + `since` | this account's current 5-hour quota window, read from its provider's own reset header: Anthropic's for a Claude account, Codex's for a Codex one. `null` when that reset is unknown, because then the window's start cannot be named |
| `usage.sevenDayWindow` | totals + `since` | a Codex account's current weekly quota window. `since` is the weekly reset minus the window's length, which is seven days unless the plan reports another, and the totals are what the account spent since. Absent on a Claude row, when the weekly reset is unknown, and when the window opened more than seven days ago, because tcr keeps only a week of a Codex account's per-minute usage: a 30-day window carries the key only in its first seven days. `usage.window` stays the 5-hour window |
| `usage.lastHour` | totals | the trailing 60 minutes — burn rate is `lastHour.costUsd` per hour, by definition |
| `usage.todayByModel` | `{model: totals}` | `today`, split by model id |

Each totals object carries `requests`, `inputTokens` (BASE input — see below), `cacheCreationTokens`,
`cacheCreation1hTokens`, `cacheReadTokens`, `outputTokens`, `costUsd` and `unpricedRequests`, and
`reasoningTokens`: a Codex turn's reasoning tokens, already inside `outputTokens`, absent when zero.

`cacheCreationTokens` is ALL cache creation, both TTLs — the same quantity the row-level key of that
name carries, so the two never mean different things in one row. `cacheCreation1hTokens` is the
subset of it written under the extended 1-hour window (which bills at 2x base input rather than
1.25x); the 5-minute part is the difference between the two.

**`usage.today.inputTokens` is not the row's `inputTokens`.** The row-level one is the QUOTA counter
and folds cache creation and cache reads into a single number, which is what quota is charged on. The
usage object keeps the four dimensions apart because they bill at four different rates, so they have
to be separable to be priced at all. The new row-level `cacheCreationTokens` is the companion
`cacheReadTokens` never had: with both, a reader can see how much input was served FROM cache and how
much was spent WRITING it.

**`costUsd` is API list price, not a bill.** These accounts are subscriptions; nothing here is
charged. The figure answers "what would this traffic have cost on the API", which is the only unit
that compares across accounts, models and days. The price is the model's own provider's list price.
No OpenAI model has a built-in rate, so a Codex account's buckets read `costUsd: null` until `pricing`
names its model. When a model has no published rate in this build,
`costUsd` is `null` and `unpricedRequests` counts the requests missing from the figure — a partial
total says so rather than passing itself off as the whole. Add a rate with `pricing` in the config
([configuration.md](configuration.md#pricing-and-usageretentiondays)).

`costUsd: null` means exactly that one thing: the bucket served requests and not one of them could be
priced. A bucket that served NOTHING reports `costUsd: 0.0` — an idle account is a measured zero, in
`today` and `lastHour` alike.

**`usage: null` means not measured, never "spent nothing".** Two cases produce it: the row came from
the offline path (no serving process, so there is nothing to aggregate), or the proxy that answered
was built before this field existed — which is the ordinary state here, since the binary on disk is
rebuilt on merge while the live process keeps serving until someone restarts it. `source` on the same
row says which.

Totals survive a restart: each served request is appended to `~/.cache/tcr/usage/`, and boot
replays the day back in, and the last seven days when the fleet holds a Codex account, so the weekly
window survives a restart too. Fleet-wide totals are not a field — sum the rows.

### The `sessions` array on `GET /_tcr/status`

Reachable via the raw `/_tcr/status` endpoint, not the CLI's own array output: one entry per Claude
Code or Codex session seen in the last hour, each carrying its tool-call stats (`tools.running`, the
pending calls; `tools.slowest`, the ten slowest completed ones). A session leaves the list an hour
after its last request. A Codex session stays until its last usage, recorded when that turn's
stream ended, has also left the Codex rows' `usage.lastHour`, at most two minutes past the hour
after the stream's end, so while `lastHour` still counts a session's turn, the session is listed. `tools.running` entries for an
`Agent` or `Task` tool call carry the subagent's description in `commandHead` (prefixed with its
type when Claude Code sent one), and `tools.subagentsRunning` is that count precomputed, so a panel
can show "2 subagents" without scanning the list itself.

Each session also carries `tools.byTool` (a per-tool-name breakdown: calls, errors, a median
duration over a bounded reservoir, and `overOneMinute`) and `reqPerMinute` (30 wall-clock minutes
of request counts, oldest first, decaying to zeros as the session goes idle); the payload's
top-level `sessionsSummary` is the same `byTool` shape summed across every session server-side, so
a panel's fleet-wide headline never disagrees with the per-session rows by re-summing them itself.

A Codex session's id is the `session-id` header codex sends on every turn, and its row carries
`provider: "codex"`, a key absent on a Claude Code session. `unpricedRequests` counts its requests
no price covers, a turn still streaming or one that ended without reporting usage among them, and
`costUsd` stays the priced part (0.0 when none was priced); `reasoningTokens` counts its reasoning
tokens, which are already inside `outputTokens`. Both keys are absent at 0. Its tool calls fill
the same `tools` keys: each runs from the end of the response that asked for it until tcr serves
the next request on the same session, and a running call carries `timeoutSeconds` when it named
its own timeout. A request tcr answers itself is not counted and closes nothing. After
an upstream 403, tcr holds that account out for 120 seconds, and while no other Codex account can
serve, tcr answers the session's requests itself, except that a request arriving in the hold's last
20 seconds waits until the account frees. Either way the calls keep running, and their time
includes that wait. A Codex call is never counted as an error or a timeout, because the request
that closes it carries its output in a body tcr does not read.

### The `mitm` object on `GET /_tcr/status`

Also on the raw `/_tcr/status` document only, never in `tcr status --json`'s array: what the
serving process's forward-proxy listener can intercept, decided once at boot from its policy and
the TLS material that process loaded. `interceptedHosts` lists the hosts that policy and leaf let
it terminate TLS for, and `leaf` names the certificate it presents: `minted` (a leaf tcr minted
under `~/.config/tcr-ca.pem`) or `none` (MITM off: the TLS material failed to load,
and every `CONNECT` is tunnelled). `chatgpt.com` in `interceptedHosts` says what the leaf allows,
not what happens on every tunnel: the listener requires the Codex marker on CONNECT and a Codex
account in the pool, counted on every connection, and tunnels it untouched otherwise. The same document's
account rows show whether it holds one: a Codex row carries `"provider": "codex"`.

```
"mitm": {
  "interceptedHosts": ["api.anthropic.com", "platform.claude.com", "chatgpt.com"],
  "leaf": "minted"
}
```

A server that has the key always sends it, MITM off included, and a server built before it leaves
it out. That difference is how `tcr run --codex` tells an older proxy from one running without
MITM.

### The `heat` object on `GET /_tcr/status`

How hot the serving Mac's CPU runs and how hard its fans work, read by the proxy through AppleSMC
without root (`crates/tcr-smc`). It is on the raw `/_tcr/status` document and on `tcr sessions
--json`'s object (see [`tcr sessions`](#tcr-sessions)), never in `tcr status --json`'s array: it
describes the machine, not an account.

```
"heat": {
  "cpuCelsius": 61.3,
  "fans": [
    {"index": 0, "actualRpm": 2401.5, "minRpm": 1350.0, "maxRpm": 5777.0, "manual": false, "targetRpm": 0.0},
    {"index": 1, "actualRpm": 2398.0, "minRpm": 1350.0, "maxRpm": 5777.0, "manual": false, "targetRpm": 0.0}
  ],
  "ftst": false
}
```

- `cpuCelsius` is the hottest CPU sensor: `TCMz` where the Mac has it, otherwise the hottest `Tp*`
  sensor. Readings outside 0.5 to 150 °C are dropped as sensors that are not fitted or not powered.
  The key is absent when no CPU sensor read.
- `fans` has one entry per fan whose speed read, and is empty on a Mac with no fans. `index` is
  0-based, the `n` in the SMC's `F{n}Ac`. `minRpm` or `maxRpm` is 0 when it did not read. `manual`
  says the fan's mode key reads "forced": a program fixed the speed, so it no longer follows the
  heat. A mode key that did not read is `false` here; fan control itself treats it as unknown
  and writes nothing. `targetRpm` is `F{n}Tg`, the speed the fan is told to hold, absent when it
  did not read.
- `ftst` is the SMC's `Ftst` unlock flag, `true` while a fan writer holds it raised. Absent on a
  Mac without it.
- An SMC that opened and read nothing gives `{"fans": []}`. One that refused to open gives the
  same, and the proxy tries to open it again a minute later.

The whole key is absent where there is no AppleSMC (a virtual machine, Linux), and from a server
built before it. One reading answers for 2 seconds, so a burst of status requests costs one SMC
read. A request never queues behind a read that is still running: it waits at most 500 ms, and
otherwise answers with the last reading, or with no key when there has never been one. A client
that cannot parse the object (one built for a later shape, say) leaves the heat out and keeps the
rest of the payload. `tcr sessions` says so on stderr.

### The request key on `GET /_tcr/status`

Also on the raw `/_tcr/status` document only, like `mitm`: whether the serving process holds a
request key ([`proxy.requestKey`](configuration.md#proxyrequestkey-who-launched-this-caller)), and
who has reached it without one.

```
"requestKeyMode": "log",
"unkeyedConnects": [{"class": "other", "userAgent": "curl", "count": 3}],
"unkeyedRequests": [{"class": "api", "userAgent": "bun", "count": 1}]
```

`requestKeyMode` is present whenever the server holds a key, which it does after its first boot on
this version, and absent on a server that holds none or predates keys. Its presence is what a
launcher reads before it adds the key to a launch. `log` is the only mode: a caller without the
key is counted and served exactly as before, and nothing is refused.

`unkeyedConnects` counts `CONNECT` tunnels that carried no key, by host (`api.anthropic.com`,
`platform.claude.com`, `chatgpt.com`, or `other` for a blind tunnel) and user-agent family
(`claude-cli`, `codex`, `bun`, `curl`, `python`, `node`, `other`, or `none` when the caller sent
no `User-Agent`). `unkeyedRequests` counts plain requests the same way, by path: `messages`
(`/v1/messages` and below), `v1-other`, `codex`, `api` (where Claude Code's `HEAD /api/hello`
lands) or `other`. A request decrypted from a tunnel is not counted again; its `CONNECT` was.
The counts reset when the server restarts.

The first unkeyed caller of each kind also gets one INFO line in the server log, with its source
port, so `lsof -nP -iTCP:<port>` can name the process while it is still connected:

```
INFO request key: first unkeyed caller of this kind (log mode, nothing refused) kind="connect" class="other" user_agent="curl" peer_port=50123
```

There is no line per request. The key itself never appears in the document or the log.

### `GET /_tcr/quota`, the light read for a status line

A Claude Code status line ticks every turn and needs only the quota, so it can call this instead of
`/_tcr/status`. It answers from the running proxy's memory: no probe and no upstream call. The only
disk access is the one every snapshot makes, a `stat` of the config file, re-reading it only when it
changed so group gates stay current. Same gate as `/_tcr/status` (loopback only, and `x-api-key` when `proxy.apiKey` is set); any
verb but `GET` is a local 405.

```json
{"kind":"tcr.quota.v1","accounts":[
  {"name":"alice","gate":"ok","fiveHour":0.5,"fiveHourResetMs":1767225600000,
   "sevenDay":0.2,"sevenDayResetMs":1767830400000}]}
```

One row per Claude account in the server's order. `gate` is the same token `tcr status --json` reports
(`ok`, `hold`, `five-hour`, `seven-day`, ...); utilizations are fractions and resets are Unix
milliseconds, `null` when the window has no reading yet. Check `kind` before trusting a body: a tcr
too old to have the route answers it with a local 404.

The v1 route stays Claude-only, so existing status lines cannot count ChatGPT quota as Claude
capacity. Use `/_tcr/status` or `tcr status --json` for provider-aware Claude and Codex rows.

---

## `tcr doctor`

Answers one question: **is Claude Code actually reaching this proxy, and if it is not, what decided
otherwise.** Every other verb here reports the fleet this proxy holds; this one reports whether
anything is pointed at it. When the fleet holds a Codex account, it answers the same question for
`codex` as well ([The Codex block](#the-codex-block)).

| flag | type | default | effect |
|---|---|---|---|
| `--config <path>` | path | the per-user config path | config to read |
| `--json` | bool | `false` (text) | emit one JSON object instead of greppable `key: value` lines |
| `--codex` | bool | `false` | report only whether `codex` is pooled, whatever the fleet holds, and exit with that verdict's code ([below](#the-codex-block)) |

```
$ tcr doctor
baseUrl: https://gateway.example.com
baseUrlSource: /Users/example/.claude/settings.json
proxyPort: 3456
portHolder: pid 4242 tcr
portHolderIsTcr: yes
requests10m: 0
shellRoute: not installed
verdict: this proxy is not on Claude's route: /Users/example/.claude/settings.json sets https://gateway.example.com
```

| exit | meaning |
|---|---|
| **0** | Claude Code is routed to this proxy and the proxy answered |
| **2** | Claude Code is routed somewhere else; the verdict names the file or variable that set it |
| **3** | the route points here and nothing answered on the port |

These codes answer for Claude's route. Beside a Codex block, the fleet's shape decides whose
verdict sets the exit code ([The Codex block](#the-codex-block)).

"Routed here" means the base URL names a loopback address (`127.0.0.1`, `localhost`, `[::1]`) on the
port this config uses. Another machine's proxy on the same port is not this one.

The `shellRoute:` line names the startup file holding [`tcr setup shell`](#tcr-setup)'s block for your
login shell, or says `not installed`. With the block in place and no settings file naming a base URL,
a `claude` typed in a terminal runs `tcr run`, so doctor counts that as routed here and the verdict
says so. A settings file that names another URL still wins, because Claude Code applies it over what
`tcr run` exports.

### Why the base URL can come from a file rather than the variable

`tcr run` exports `ANTHROPIC_BASE_URL` onto the `claude` it launches. Claude Code then applies its
settings files' `env` block **on top of** the environment it inherited, so a base URL written in a
settings file wins over the one the launcher exported, silently. `doctor` resolves the four sources
in Claude Code's own order and reports the winner:

1. `.claude/settings.local.json` in the working directory
2. `.claude/settings.json` in the working directory
3. `.claude/settings.json` in the home directory
4. the process environment's `ANTHROPIC_BASE_URL`
5. failing all of those, `https://api.anthropic.com`

A settings file that exists and cannot be parsed is reported on its own `problem:` line rather than
read as "sets nothing": it is the likeliest place for the answer to be hiding, and skipping it
quietly would let `doctor` name the wrong source with confidence.

`requests10m` is how many requests Claude Code sent through this proxy in the last ten minutes,
summed from the per-minute sparklines of the Claude sessions on `/_tcr/status` (`reqPerMinute`,
documented under `tcr status`). Codex sessions are left out, because a proxy busy with codex says
nothing about whether Claude Code reaches it. It reads `none` when no proxy answered, which is a
different fact from `0`: a proxy that is up and idle is healthy, and only the first of those is
exit **3**.

`portHolder` is whoever is listening on the port, by pid and process name, from the same listener
enumeration `tcr server` uses to decide a takeover. It answers the case the verdict line cannot:
the route is right, nothing answers, and the port is held by something that is not a `tcr`.

`--json` emits the same decision as one object, with `verdict` and `exitCode` written out so a
caller reads one document instead of re-deriving either.

### Config keys tcr does not recognise

`doctor` also names every top-level key in the config that this build does not recognise, with the
known key it is closest to. A misspelled key is kept in the file as written and has no effect, so
without this line `quotaProbeSecs` looks exactly like a config that sets the probe cadence:

```
unknownKey: quotaProbeSecs (did you mean quotaProbeSeconds?)
```

In `--json` the same list is `unknownKeys`, an array that is `[]` when every key is known:

```
"unknownKeys": [{ "key": "quotaProbeSecs", "nearest": "quotaProbeSeconds" }]
```

`nearest` is `null` when no known key is close enough to be a likely typo. A key counts as known when
it is a field tcr models, one of the keys read from the unmodelled map
([configuration](configuration.md#keys-read-from-the-unmodelled-map)), or the legacy `throttle`. The list is reported and never
enforced: the file is not changed, the exit code above still describes routing alone, and the server
logs the same keys once at boot. Only top-level keys are checked; per-account and `proxy` keys are
not.

`tcr status` prints this verdict line **first**, and only when it is exit 2 or 3, so a glance at the
fleet cannot show healthy accounts while Claude Code talks to something else. It prints only while
the fleet holds a Claude account or no account at all, and in a fleet that also holds a Codex
account a Codex verdict can follow it ([`tcr status`](#tcr-status)). The `--json` form of
`status` is untouched: it stays a bare array.

### The Codex block

When the fleet holds a Codex account, in any state, `doctor` also answers the other routing
question: would a `codex` started from this shell be pooled, and if not, why. `--codex` asks only
that, whatever the fleet holds. The verdict is the decision `tcr run --codex` acts on and
`tcr status` prints, from the same function, so the three never word it differently
([the reasons](#--codex-launching-codex-through-the-pool)). A fleet with no Codex account, run
without `--codex`, prints exactly what it always did, text and `--json` alike.

```
$ tcr doctor
baseUrl: http://127.0.0.1:3456
baseUrlSource: ANTHROPIC_BASE_URL
proxyPort: 3456
portHolder: pid 4242 tcr
portHolderIsTcr: yes
requests10m: 3
shellRoute: not installed
codexAccounts: 2 (1 in rotation, 1 in the weekly window)
codexIntercept: yes
codexCa: /Users/example/.config/tcr-ca.pem (readable)
codexUpstream: https://chatgpt.com (default)
codexBin: /opt/homebrew/bin/codex (PATH)
codexEnv: HTTPS_PROXY unset · NO_PROXY unset · SSL_CERT_FILE unset
codexLogin: /Users/example/.codex/auth.json exists (existence only; never read)
codexRequests10m: 2
codexRefused10m: 0
codexUnpooled10m: 0
codexVerdict: codex is pooled: `tcr run --codex` routes chatgpt.com through this proxy, which served 2 Codex requests in the last 10 min
verdict: Claude is routed to http://127.0.0.1:3456 (ANTHROPIC_BASE_URL); this proxy has served 3 Claude requests
```

The block's keys, in order:

| key | what it reads |
|---|---|
| `codexAccounts` | the Codex accounts, counted by why each is out, in the words tcr's own pool refusal gives `codex`: `in rotation`, `on an upstream hold`, `refused upstream (403)`, `in the 5-hour window`, `in the weekly window`, `needing re-login`, `refused by ChatGPT`, `disabled`, `unreachable in a reserved group`, `in a parked group`, in that order. An account held by an upstream hold or 403 while ChatGPT's refusal stands (its row carries `limitReachedType`) counts as `refused by ChatGPT`, since that refusal outlasts the hold; one that a full window refused keeps that window's phrase (`in the 5-hour window` or `in the weekly window`). The rows are the running proxy's, which leave out an account deleted while it runs; with no proxy answering they are the config's, which carry no gate: `2 (no proxy answered)`. `0` with none |
| `codexIntercept` | whether the proxy can terminate marked Codex connections to `chatgpt.com`: `yes` when `codex` is pooled, and when only tcr's CA file is missing; `no` when the proxy's leaf cannot cover `chatgpt.com`, it runs without MITM, or it holds no Codex account; `unknown` when no proxy answered, its status could not be read, or it is another build, one that predates the report or names a leaf this build does not know |
| `codexCa` | tcr's CA, `~/.config/tcr-ca.pem`, and whether it is a readable file. It is never loaded |
| `codexUpstream` | where Codex requests go, `(default)` or `(set by codexUpstream)`: the running proxy's, or the config's when no proxy answered |
| `codexBin` | the `codex` that `tcr run --codex` would launch and the rule that found it (`TCR_CODEX_BIN`, `CODEX_BIN`, `PATH`, `TcrBar's directories` or `npm's global prefix`), or `not found` and why, by [the one search](#--codex-a-codex-login-of-tcrs-own) |
| `codexEnv` | what this shell would hand a `codex` started here: the proxy variable it reads for `https://` (the first set of `HTTPS_PROXY`, `https_proxy`, `ALL_PROXY` and `all_proxy`, any credential in it shown as `…`, marked `(points at tcr)` when it names this proxy), `NO_PROXY` (marked `(exempts chatgpt.com)` when an entry does) and `SSL_CERT_FILE` |
| `codexLogin` | the login `codex` keeps, `$CODEX_HOME/auth.json`, else `~/.codex/auth.json`, and whether it exists. Existence only: it is never read |
| `codexRequests10m` | the Codex requests this proxy served in the last ten minutes, summed from the Codex sessions' `reqPerMinute` |
| `codexRefused10m` | the 400 or 429 pool refusals tcr answered `codex` with itself in the last ten minutes (`codexTraffic`'s `refused10m`) |
| `codexUnpooled10m` | the Codex requests relayed unpooled, on a login's own credential, in the last ten minutes (`unpooled10m`) |

The three counts read `none (no proxy answered)` offline, and the last two
`n/a (the proxy does not report it)` from a proxy that predates them. `requests10m` above them
counts Claude sessions only, and `codexRequests10m` Codex sessions only.

A `codexProblem:` line follows for each problem, in this order:

- an unmarked proxy URL points at tcr: `chatgpt.com` is tunneled untouched, so start with
  `tcr run --codex` to join the pool for that launch instead of marking the whole shell;
- a marked proxy URL points at tcr, `NO_PROXY` does not exempt `chatgpt.com`, and the CA
  `codex` reads first (`CODEX_CA_CERTIFICATE`, else `SSL_CERT_FILE`) is not tcr's, so its
  TLS handshake would fail. Left out while `codexIntercept` is `no`;
- one line for each Codex account in a reserved group: a Codex request cannot ask for a group, so
  the account serves nothing;
- `codex` has no login of its own, which it needs to start even when pooled: `codex login` once;
- last, why `codex` is not pooled, when it is not.

The verdict reads `codex is pooled: …`, with the Codex requests it served lately, or
`codex is not pooled: <reason>`. The fleet's shape decides its key, the last line and the exit
code:

| fleet | what prints | last line | exit code |
|---|---|---|---|
| no Codex account, no `--codex` | Claude's keys and verdict, unchanged | Claude's `verdict:` | Claude's |
| Claude and Codex accounts | Claude's keys, the block, `codexVerdict:` | Claude's `verdict:` | Claude's |
| Codex accounts only | Claude's keys, the block | `verdict:`, Codex's | Codex's |
| `--codex`, any fleet | the block, then the config's `unknownKey:` lines | `verdict:`, Codex's | Codex's |

Codex's exit codes mirror Claude's: **0** when `codex` is pooled, **2** when a proxy answered and
`codex` would not be pooled (the verdict says why), **3** when no proxy answered or its status
could not be read. A mixed fleet keeps Claude's, so a script that asks `doctor` whether Claude Code
reaches this proxy gets the answer it always got. Its Claude verdict, when the proxy served,
names the requests as Claude's (`this proxy has served 3 Claude requests`), whether the base URL
or the [shell route](#tcr-setup) is Claude's route, since the Codex count sits right above it; a
fleet without the block keeps `this proxy has served 3 requests`. Its verdicts that name no count
already name Claude's route, and keep their words.

**Unknown config keys.** In mixed and Codex-only text the `unknownKey:` lines sit with Claude's
keys: after `shellRoute:` and any `problem:` line, before `codexAccounts:`. Under `--codex` they
come after the block, right before the verdict, since a misspelled `codexUpstream` matters most to
a reader asking about `codex`: the block would read `codexUpstream: https://chatgpt.com (default)`
and nothing else would say why. They never change an exit code.

**`--json`.** Beside the block, Claude's object gains a `codex` object, with these keys: `accounts`
(`total`, and `byGate`, the rows counted by their `--json` gate, `{}` when no proxy answered),
`intercept`, `route` (as `tcr status --json` carries [`codexRoute`](#codex-rows-on---json)), `ca`
(`path`, `readable`), `upstream` (`url`, `isDefault`), `bin` (`path` and `source`, or `{}` when no
`codex` was found), `env` (`httpsProxy` when one is set, credential masked; `pointsAtTcr`;
`noProxyKeepsChatgpt`, true when `NO_PROXY` exempts `chatgpt.com`; `sslCertFile` when set),
`login` (`path`, `exists`), `requests10m`, `refused10m` and `unpooled10m` (`null` when not known),
`problems`, `verdict` (the bare sentence) and `exitCode`. A mixed fleet keeps Claude's top-level
`verdict`, in the Claude-naming words above, and its `exitCode`; a Codex-only fleet's top-level
`verdict` and `exitCode` are Codex's. The top-level `unknownKeys` stays where it was.
`--codex --json` prints the `codex` object alone, with the config's `unknownKeys` added.

---

## `tcr config show [--json]`

Prints the rotation and limit settings a server started on this config file would use, with
every default applied: the saved values, which the next start uses. When a proxy answers on the
config's port it also asks it, through `GET /_tcr/config`, which values it is running. Every key
here except `controlAccount` is read when the proxy starts, so a saved value it is not running yet
is waiting for a restart, and the last line names those keys.

```
$ tcr config show
config                 /Users/example/.config/tcr.json
switchThreshold        95%
controlReserve         5%
fableWeeklyThreshold   95%
resetUrgencyTierHours  24 h
sessionAffinity        on
controlAccount         none
controlPooled          off
accountThrottle        1 per 350 ms, bursts of 24
fleetThrottle          1 per 100 ms, bursts of 32
pacing                 off
usageRetentionDays     90 days
http1Only              off

Waiting for a proxy restart: switchThreshold.
```

With no proxy on the port the last line says nothing is waiting. A proxy too old to report its
settings, or one that cannot be read, gets a line on stderr and is treated as no proxy.

`--json` prints one object with the same settings as values: `configPath`, `switchThreshold`,
`controlReserve` and `fableWeeklyThreshold` as fractions (`0.95`), `fableWeeklyThresholdFollows`
(`true` when the file sets no Fable override, so the value is the switch threshold's), `resetUrgencyTierHours`,
`sessionAffinity`, `controlAccount` (the name, or `null`), `controlPooled`, `accountThrottle` and
`fleetThrottle` (`{minSpacingMs, burst}`, or `null` when off), `pacing`
(`{maxInFlightPerAccount, minSpacingMs}`, either `null`, or the whole key `null` when off),
`usageRetentionDays` and `http1Only`. Each value is the one the server applies: `controlReserve`
clamped to `[0, 0.5]`, `fableWeeklyThreshold` falling back to `switchThreshold`, a throttle with
no spacing reported as off, an absent `burst` as `1`. TcrBar's Settings window reads this object.

When a proxy answered, the object also carries `running`: an object of the same keys holding the
values that proxy booted with. It is absent, not `null`, when no proxy answered, because "nothing
is running" and "the running proxy matches" are different facts. A key is waiting when its saved
and running values differ; `fableWeeklyThreshold` waits only when its own override changed, since
while it follows `switchThreshold` a switch-threshold change is that row's.

```
tcr config show --json | jq -c '{saved: .switchThreshold, running: .running.switchThreshold}'
```

---

## `tcr config set <key> <value>`

Saves one rotation or limit setting, under its config-file name:

| key | value |
|---|---|
| `switchThreshold` | a share above 0 and at most 100%: `90%` or `0.9` |
| `controlReserve` | a share from 0 to 50%: `5%` or `0.05` |
| `fableWeeklyThreshold` | a share like `switchThreshold`, or `default` to follow `switchThreshold` again |
| `resetUrgencyTierHours` | whole hours; `0` turns the tier off |
| `sessionAffinity`, `controlPooled`, `http1Only` | `on` or `off` |
| `accountThrottle`, `fleetThrottle` | two whole numbers of at least 1, the milliseconds between sends and the burst (`350 24`), or `off` |
| `pacing` | the most requests one account may have in flight, at least 1, or `off` |
| `usageRetentionDays` | whole days |

The control account is not here; it has its own verb, `tcr control <account>`.

```
$ tcr config set switchThreshold 90%
Saved switchThreshold = 90%. The proxy on :3456 keeps its current value until it restarts.
$ tcr config set controlReserve 0.6
Error: controlReserve takes a share of quota from 0 to 50%, as 5% or 0.05, not '0.6'. Nothing was changed.
```

A value the server would quietly reinterpret is refused rather than saved: a reserve outside
`[0, 0.5]` (the server clamps it), a throttle spacing or burst of `0` (the server reads them as off
and `1`), a pacing cap of `0` (no cap). Only the named key changes in the file; every other key,
and every key tcr does not know, is left as it was. `pacing` keeps a `minSpacingMs` already in the
file; `off` removes the `pacing` key, which is its default.

**Who writes the file.** With a proxy on the config's port, `config set` hands the setting to it
(`POST /_tcr/config`, behind the same api-key and loopback check as `tcr control`), and the proxy
writes the key under the lock its own token and `tcr disable` writes hold, so neither can drop the
other's change. With nothing listening, `config set` writes the file itself. A rejected api-key,
or a refusal from the proxy, changes nothing; a proxy too old for the route, or one that answers
something unusable, gets the file written beside it with a warning on stderr.

Either way the running proxy keeps the value it booted with until it restarts, which is what
`running` in `tcr config show --json` reports. TcrBar's Settings window saves each edit through this
verb and counts the waiting keys.

---

## `tcr sessions`

`tcr sessions [--json]` — the sessions the RUNNING proxy has seen in the last hour. This is the
channel TcrBar's panel reads for its Sessions and Tools tabs: the rows live only on the proxy's own
`/_tcr/status` response, behind the proxy api-key, and the app may not read that key, so it shells
out here the way it does for every other fact.

Live only, with no offline fallback. A session is the running proxy's own state: the copy it keeps
on disk, `~/.cache/tcr/session-wire.json`, is a cache it reads back at boot, not a record of
what is live, so an offline rendering could only be a stale list dressed up as a measurement. With
nothing listening the command exits non-zero and says so on stderr.

```
tcr sessions --json | jq -c '{supported, n: (.sessions|length)}'
```

`supported` answers **did this server's payload carry a `sessions` key**, not whether the array has
anything in it. A proxy built before sessions existed and a completely idle one both report zero
rows, and the panel has a different sentence for each, so the two must stay distinguishable. The
rows themselves are the same `SessionRow` shape `tcr status --json`'s payload documents above,
verbatim.

The object also carries the server's [`heat`](#the-heat-object-on-get-_tcrstatus) reading when
there is one, under a `heat` key. The key is absent when the server read none. TcrBar's machine
line on the Tools tab reads its CPU and fan figures from here, because the app reads no SMC
itself. Beside it, a `fanControl` key carries the server's fan controller
([`tcr fan`](#tcr-fan)), absent from a server that runs none.

```
$ tcr sessions
supported=true sessions=2
session=a1f3c2d4-0000-4000-8000-000000000001 account=alice@example.com model=claude-opus-5 requests=24 inputTokens=180000 outputTokens=9200 cacheReadTokens=160000 toolCalls=12 costUsd=1.2034
session=019a3c5e-0000-7000-8000-000000000002 account=bob@example.com/codex provider=codex model=gpt-5.1-codex requests=7 inputTokens=95000 outputTokens=4100 cacheReadTokens=80000 toolCalls=3 costUsd=n/a reasoningTokens=1200
```

A Codex line carries ` provider=codex` after the account. No built-in price covers an OpenAI model,
so its `costUsd=` reads `n/a` until a [`pricing`](configuration.md#pricing-and-usageretentiondays)
entry covers one of its requests, then the priced part with the count it leaves out
(`costUsd=0.4200 unpricedRequests=3`). ` reasoningTokens=` prints when the model reasoned. A Claude
line keeps its figure as it always has, `0.0000` for a model this build has no price for included.

`tcr status --json` is unchanged and still emits a bare array of accounts. The `supported` flag has
nowhere to live in an array, which is why this is a separate verb rather than a flag on `status`.

The build-skew warning goes to **stderr** in both modes, as `status`'s does, so `--json`'s stdout
stays a single JSON object a script can pipe into `jq`.

---

## `tcr list`

`tcr list --json` prints every configured Claude account as one JSON object in claude-swap's list
schema v1, the document [CodexBar](https://github.com/steipete/CodexBar)'s "Read accounts from
claude-swap" setting parses. It exists so CodexBar can show tcr's accounts with no change on its
side. The projection and its rules live in `crates/tcr-status-wire/src/cswap_v1.rs`; the committed
example is `tests/fixtures/cswap-list-v1.json`.

```
tcr list --json | jq -c '{v: .schemaVersion, n: (.accounts|length)}'
```

What each field carries:

| field | value |
|---|---|
| `schemaVersion` | `1` |
| `activeAccountNumber` | `null`: tcr pins per session and has no one active account |
| `supportsAccountSwitching` | `false`: tcr routes per request, so CodexBar hides Switch |
| `accounts[].number` | the account's 1-based position among the Claude accounts of the config file's `accounts` list |
| `accounts[].active` | `false` on every row |
| `accounts[].email` | the account name before any `/org` suffix |
| `accounts[].organizationName` | the org's name, or `""` |
| `accounts[].usageStatus` | `relogin_required` when the account needs a login; `ok` when the live server's last quota probe succeeded; `unavailable` otherwise, including every row when no server answers and any gate this build does not know |
| `accounts[].usage.fiveHour`, `.sevenDay` | `{pct, resetsAt}`: percent used (0 to 100) and the reset in ISO-8601 UTC; omitted when unknown |
| `accounts[].usage.scoped` | the Fable weekly window as `{"name": "Fable", ...}`; omitted when unknown |
| `accounts[].usageFetchedAt` | when the bars were last read (`quotaUpdatedAtMs`: a successful probe or a served response's quota headers); omitted when unknown. Not `lastProbeMs`, which a failed probe also stamps |
| `accounts[].disabled` | `true` when the account is disabled or in a parked group |

Not sent: `alias`, `usage.spend` (tcr's cost is a list-price equivalent with no limit) and
`lastGoodUsage` (tcr keeps no quota reading on disk). An API-key account is never listed: tcr
serves subscription logins only, so `apikey` rows are quarantined when the config loads. A Codex
account is not listed either: claude-swap's list is CodexBar's list of Claude accounts, with no
field to say a row is anything else, so a Codex row there would be drawn as a Claude account.

**It never probes.** CodexBar runs this on every refresh, so it reads the running proxy and
nothing else. With no proxy answering it still lists every configured account, each as
`unavailable`, where `tcr status` would fall back to probing every account over the network.
A slot follows the config file even when the running proxy holds its accounts in another order
(the file was reordered after it started); an account the proxy still serves but the file no
longer lists is placed after the file's accounts.

A handled failure (a config file that cannot be parsed) prints
`{"schemaVersion":1,"error":{"type":"config_unreadable","message":"..."}}` on **stdout** and exits
1, because CodexBar reads stdout whatever the exit code. Bare `tcr list` without `--json` is a
usage error, exit 2, pointing at `tcr status` for the readable view.

CodexBar runs its configured path as `<path> --list --json` and `<path> --version`, with no
shell, so point it at a shim that forwards both:

```sh
#!/bin/sh
case "$1" in --version) exec tcr --version ;; *) exec tcr list --json ;; esac
```

Save it as an executable file (`chmod +x`) and set it as CodexBar's claude-swap path. The cards
are labelled "claude-swap" until CodexBar learns about tcr as a source of its own.

---

## `tcr wrap`

`tcr wrap [--days N] [--json]` — a usage report for the last `N` days (default 7, ending
today UTC), read straight off the usage ledger in `~/.cache/tcr/usage/`. Unlike `tcr
status`, this needs no running proxy: the ledger is a durable, append-only record on disk, and
`tcr wrap` just reads it.

It reports, in order: totals (requests; uncached input, output, cache-write and cache-read
tokens; cost; and the cache-hit ratio, the share of all prompt tokens read from cache), a per-model breakdown sorted by cost, a per-account breakdown sorted by requests, a
per-day breakdown with the busiest day marked, the count of distinct sessions plus the three
longest-running ones by request count, and a comparison against the previous `N`-day period
(`+12%` / `-3%`). `--config` (default `~/.config/tcr.json`) is consulted only for
pricing overrides — never for accounts, so a removed account's traffic still shows up under
the name it was recorded under. `--json` emits the same numbers as one object instead of the
plain-text layout.

When the period holds a Codex ledger line, the report also prints a second totals line, which counts
the Codex requests no price covers, and a `by provider` block (`anthropic`, `codex`) before the
per-model breakdown. No OpenAI model has a built-in price, so Codex traffic is unpriced until
`pricing` names its model ([configuration.md](configuration.md#pricing-and-usageretentiondays)).
Beside every cost that a bucket's unpriced requests leave out, the report says so:
`cost=n/a (42 unpriced)` when none of the bucket's requests was priced, and
`cost=$7.20 (+12 unpriced)` when some were, never `cost=$0.00` for Codex traffic. The comparison's
cost then says `(priced requests only)`.
A Claude-only ledger prints exactly what it always has, and an unpriced Claude model still shows
`$0.00`.

"Day" here is the UTC calendar day a ledger line's own file is named after — the same day
`tcr status`'s `--json` `usage` object writes to and `attach_ledger`'s boot replay reads — not
the local day `today` on that object uses, and cost is the same API list-price figure `tcr
status` reports (see the note there: no dollar here is ever billed, every account in this
fleet is a subscription).

Every number is reproducible straight from the files, without running `tcr` at all — for
example, total requests and cost for one day:

```
$ jq -s '{requests: length, inputTokens: (map(.i) | add)}' ~/.cache/tcr/usage/2026-09-10.jsonl
{
  "requests": 5,
  "inputTokens": 812000
}
```

A line a Codex account served also carries `"p":"codex"`, and `rt`, its reasoning tokens (already
inside `o`), when there are any; an Anthropic line carries neither key. A line written before tcr
recorded `p` is filed by its model id: `gpt-` and `codex-` ids go to codex, every other to claude. A
`note: N older ledger line(s) carry no provider; filed by model id` line says how many lines were
filed that way.

`--json` carries `unpricedRequests` on the totals and on every bucket, absent at 0, and `byProvider`
only when the period holds a Codex line. `costUsd` stays the priced floor: the unpriced requests are
counted beside it, never added to it as zero.

Example (fake data):

```
$ tcr wrap --days 7
tcr wrap: last 7 day(s), 2026-09-06 to 2026-09-12 (UTC)

totals requests=1200 input=812000 output=241500 cacheWrite=90000 cacheRead=98000 cost=$42.17 cacheHitRatio=9.8%

by model (sorted by cost):
  claude-opus-5                requests=600      cost=$31.80
  claude-sonnet-5              requests=600      cost=$10.37

by account (sorted by requests):
  alice@example.com            requests=800      cost=$28.44
  bob@example.com              requests=400      cost=$13.73

by day (busiest marked *):
  2026-09-06   requests=140      cost=$4.90
  2026-09-07   requests=210      cost=$7.35
  2026-09-08 * requests=300      cost=$10.68
  2026-09-09   requests=180      cost=$6.30
  2026-09-10   requests=140      cost=$5.08
  2026-09-11   requests=120      cost=$4.20
  2026-09-12   requests=110      cost=$3.66

sessions: 38 distinct; top 3 by requests:
  session 1234567890123456789 requests=210
  session 9876543210987654321 requests=140
  session 5566778899001122334 requests=95

compared with the previous 7 day(s): cost=+12% requests=-3%
```

A mixed ledger (fake data). The `#` annotations are not part of the output: each gives its line's
own arithmetic. The cache-hit ratio is over all prompt input, which is uncached input plus cache
writes plus cache reads, and each change is against the previous period's figures.

```
$ tcr wrap
tcr wrap: last 7 day(s), 2026-10-07 to 2026-10-13 (UTC)

totals requests=2200 input=14100000 output=1200000 cacheWrite=3000000 cacheRead=41000000 cost=$105.06 cacheHitRatio=70.6%  # 41000000 cache-read tokens of 58100000 prompt tokens
totals unpriced=400 (Codex requests with no price; the cost above covers priced requests only)

by provider:
  anthropic                    requests=1800     cost=$105.06
  codex                        requests=400      cost=n/a (400 unpriced)

by model (sorted by cost):
  claude-opus-5                requests=1200     cost=$96.00
  claude-sonnet-5              requests=600      cost=$9.06
  gpt-5.1-codex                requests=400      cost=n/a (400 unpriced)

by account (sorted by requests):
  alice@example.com            requests=1200     cost=$76.53
  frank@example.com            requests=600      cost=$28.53
  bob@example.com/codex        requests=400      cost=n/a (400 unpriced)

by day (busiest marked *):
  2026-10-07   requests=280      cost=$13.06 (+60 unpriced)
  2026-10-08   requests=300      cost=$14.01 (+60 unpriced)
  2026-10-09   requests=340      cost=$16.56 (+60 unpriced)
  2026-10-10 * requests=420      cost=$20.06 (+80 unpriced)
  2026-10-11   requests=310      cost=$14.96 (+50 unpriced)
  2026-10-12   requests=260      cost=$12.41 (+40 unpriced)
  2026-10-13   requests=290      cost=$14.01 (+50 unpriced)

sessions: 31 distinct; top 3 by requests:
  session 1111111111111111111 requests=240
  session 2222222222222222222 requests=180
  session 3333333333333333333 requests=150

compared with the previous 7 day(s): cost=+12% (priced requests only) requests=+30%  # cost (105.06 - 93.86)/93.86, requests (2200 - 1692)/1692
note: 120 older ledger line(s) carry no provider; filed by model id
```

---

## `tcr update`

Self-update. One flag, `--force`: rebuild or reinstall even when the source reports it is
already up to date.

What it does depends on how `tcr` was installed, which it classifies at runtime. From a git
checkout it runs `git pull --ff-only` and `cargo build --release` in that checkout, with git
and cargo output inherited so you see progress live. From an installed copy it fetches the
newest published release's installer and runs it against the directory the running binary is
in, so the update replaces the copy on your `PATH` instead of adding a second one. From
inside a `.app` bundle it hands the request to the app's own updater, falling back to printed
manual instructions when that handoff cannot be made.

Updating the binary does not update a running proxy. The process that is serving traffic
keeps its own image until it is restarted, and `tcr status --json` reports the running
build's SHA if you need to know which one is live.

---

## `tcr demo`

Takes no flags. Renders the TUI against fake accounts, which is how the sanitized README
screenshots are produced. It touches no real config and makes no network calls.

Its fleet holds six Claude accounts and three Codex ones: a ChatGPT Pro row with the weekly
window only, a Plus row with both windows, and a Pro row at its weekly limit. It also seeds one
Codex session and one `…/codex/responses` log line, so the dashboard's Codex sections show
without a real account. The demo draws as if codex were pooled and reads nothing from this
machine, so a screenshot of it shows no path of yours.

---

## `tcr ui`

Takes no flags. Opens TcrBar, the macOS menu-bar app, by asking LaunchServices for the
bundle id `io.github.dhkts1.tcrbar`.

It exists for discoverability, since `open -a TcrBar` already worked. Without the
subcommand, nothing in `tcr --help` reveals that a UI exists at all, so the app was only
findable by already knowing about it. It deliberately does not build the app or know where
your checkout is; when the bundle id is not registered it says TcrBar is not installed and
names the install script, rather than surfacing LaunchServices' exit code. On non-macOS
builds the subcommand still exists so `--help` is identical everywhere, and fails with that
reason.

---

## `tcr fan`

```
tcr fan [--json]
tcr fan auto [--force]
tcr fan fixed <RPM>
tcr fan follow <QUIET_C> <FULL_C>
```

Fan control through the running server. The server holds the one connection to the root fan
helper and runs the controller on its own thread; this command asks it through
`POST /_tcr/fan` and reads the result from `GET /_tcr/status`. It needs the `tcr` bundled in
TcrBar.app (signed for fans) running as the server, and the helper registered and approved
([below](#tcr-fan-probe---label-label-hidden)). Fans go back to automatic when the server stops,
crashes or updates.

- `auto` hands the fans back to macOS. `--force` hands back even when this server holds no record
  of having set them: the recovery for fans left manual by a run nobody remembers. Neither writes
  while Macs Fan Control runs; Auto then waits ("fans go back to automatic when Macs Fan Control
  quits").
- `fixed <RPM>` holds every fan at exactly that speed, 1 to 20000, clamped by the helper to each
  fan's own range, however hot the CPU runs, as Macs Fan Control does.
- `follow <QUIET_C> <FULL_C>` runs each fan at its minimum at or below `QUIET_C` and at full speed
  from `FULL_C`, a straight line between. Whole degrees; `QUIET_C` at least 20, `FULL_C` at least
  `QUIET_C + 5` and at most 100.
- In `follow` a hot guard runs the fans at full speed from 100 °C until the CPU is below 90 °C
  (starting values, not yet measured). `fixed` has no hot guard, and `hotGuard` is always `false`
  in it.

A mode command is never optimistic: it sends the mode, then reads the status until the state
leaves `entering` and `releasing` (at most 49 s: the version call at its 15 s worst, which waits
out launchd starting the helper up to 10 s late, two set-manuals at their 15 s worst, and two
set-targets), and exits 0 only when the wish is still the mode it sent and
the state is `holding` for `fixed` and `follow`, or `off` for `auto`. Otherwise it prints the
state and exits 1 with the reason, for example
`tcr: the fans are foreign (macsFanControl), not holding`; a mode another command set in the
meantime is `tcr: another command changed the fan mode before this one settled`, and a
controller that never acted on the mode in that time is
`tcr: the fan controller had not acted on this command after 49 s`.

With no action it prints the wish, the state and the reason, then each fan as the server reads it
(`actual`, `min`, `max`, the mode key, `F{n}Tg` as `target`, and what this server last `written`),
and `ftst`. `--json` prints the `fanControl` object below.

### What the controller does

Whose the fans are is judged on every 3 s tick, never remembered: a fan is this server's only
while its `F{n}Tg` still reads the target it wrote (within 1 rpm). If Macs Fan Control runs, or
another program moved a target, the state is `foreign` and nothing is written; a read that fails
is `unknown` and nothing is written, and so is a reading that lost a fan the server holds (when
the fan count does not read, no fan does). When every fan reads automatic again (a wake from
sleep, or someone handed back), or one held fan does while the rest keep their targets, the
controller takes them again; more than 3 such re-entries in 10 minutes is
`unavailable / contended`. An entry that stops partway (Macs Fan Control starts, its probe fails,
a call fails, or Auto or a stop arrives) hands back any fan it already set manual before it
reports why. Follow CPU writes a new target only when it moved at least 5 % of
the fan's range, reached an end, or the hot guard flipped.

The wish and the hold record live in `~/.cache/tcr/fan.json`, never in the config file. A
graceful stop hands back within 3 s; the next start re-applies the wish. A missing or unreadable
file means Auto.

### The `fanControl` object

```
"fanControl": {
  "desired": {"mode": "fixed", "rpm": 5000},
  "state": "holding",
  "reason": null,
  "hotGuard": false,
  "targets": [{"fan": 0, "rpm": 5000.0}, {"fan": 1, "rpm": 5000.0}],
  "detail": null,
  "sinceMs": 1760000000000
}
```

- `desired`: `{"mode": "auto"}`, `{"mode": "fixed", "rpm": 5000}` or
  `{"mode": "followCpu", "quietBelowC": 60, "fullAtC": 90}`.
- `state`: `off`, `entering`, `holding`, `releasing`, `foreign`, `unknown`, `unavailable`, `stuck`.
- `reason`, for `foreign` and `unavailable`, `null` otherwise: `macsFanControl`, `anotherApp`,
  `notSignedForFans`, `noSmc`, `noFans`, `helperUnreachable` (not registered, not approved, or not
  answering; retried after 5, 10, 20, 40, then every 60 s, stopping after three failed tries in a
  row until the next command), `helperNoReply`, `helperSignature` (never retried on its own),
  `{"helperVersion": {"installed": 2}}`, `{"modeLocked": 1}` (that fan would not leave
  automatic), `contended`, `helperRefused`, `controllerStopped` (the controller's thread stopped on
  an internal error; a restart starts it again). A reason that carries data is a one-key object.
- `targets`: what this server last wrote to each fan it holds.
- `detail`: a sentence for a person, or `null`.
- `sinceMs`: when the state last changed, or the controller last acted on a command.

`desired` and `sinceMs` move together, when the controller acts: a mode just sent does not show
until the controller has acted on it, so every object pairs a state with the wish it was reached
for. `tcr fan` waits while the object still shows the wish from before its command, and for a
`sinceMs` at or after the moment it sent.

The committed samples are `tests/fixtures/fan-control-contract.json`; TcrBar's popover decodes
the same file.

### `POST /_tcr/fan`

Gated like `/_tcr/config` (loopback, the proxy key when one is set, a loopback host, not
cross-site, `application/json`). The body is exactly one of `{"mode": "auto"}`,
`{"mode": "auto", "force": true}`, `{"mode": "fixed", "rpm": 5000}`,
`{"mode": "followCpu", "quietBelowC": 60, "fullAtC": 90}`; any other key or value is a 400 in one
sentence. Accepted is 202 with the `fanControl` object as it stood when the command went in, before the
controller acted on it (`tcr fan` keeps its `desired` to tell "not taken yet" from "replaced"); a server running no fan controller
answers 503. `GET` is 405. Any local caller that passes that gate can ask for a fan mode, which
is the same class of caller that can already save a setting or add an account; what it gets is a
validated mode, clamped by the helper, with the hot guard on in Follow CPU.

## `tcr fan probe [--label <label>]` (hidden)

A diagnostic for the root fan helper. It sends the helper its version op straight from this CLI process and prints the reply
(`helper answered: protocol 1, build <sha>`) or the XPC error, and exits non-zero on anything
but a protocol-1 reply. It writes no fan.

Which helper it dials comes from this binary's own signature: the `tcr` bundled in TcrBar.app
(signed `io.github.dhkts1.tcr`, or `.dev` in any build but a shipping Developer ID one, the same
rule that names the helper) dials the matching helper, and any
other `tcr` refuses to dial and says so. `--label` names the helper to dial instead
(`io.github.dhkts1.tcrbar.fanhelper` or its `.dev` twin), so an unsigned build can be pointed
at a real helper to watch the helper refuse it: the helper's own signing requirement still
decides.

The helper is registered with `TcrBar --fan-helper register` and approved in System Settings ▸
General ▸ Login Items; `TcrBar --fan-helper status` reads the registration back.

**The recovery limit.** When the connection holding the fans goes away, the helper hands them
back to macOS if they still carry the targets it wrote, retrying for about a minute (the first
attempt at once, then six more after waits of 1, 2, 4, 8, 16 and 32 s) and logging each failure
at `LOG_CRIT`. If every attempt fails the fans stay manual at the last target until the next
proxy start, `tcr fan auto --force`, or a reboot. Whether a reboot resets fan
modes on Apple Silicon has not been measured yet.

---

## `tcr peer`

Finds other Macs on the network, trusts them, and lets trusted Macs share Claude accounts
with each other. A walkthrough of the whole flow, Find, Trust, Share, the network key, the
share link, and exactly what each act sends over the network, is in
[peers.md](peers.md); this section is the flag reference for every `tcr peer` subcommand.
None of it is on by default: a fresh install discloses nothing, opens no port and answers
nobody.

Every subcommand takes `--peers <path>`, defaulting to the peers file in the operator's
config directory, for the same reason `--config` is on every account verb: a test (or a
second identity on one Mac) points the whole peer surface at a different file with one flag.

### `tcr peer id`

Prints this node's own peer id, minting the keypair on first use.

| flag | type | default | effect |
|---|---|---|---|
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file (and its node-key directory) to use |
| `--regenerate` | bool | `false` | mint a NEW keypair; every peer that pinned the old one is evicted and its next handshake fails the pin check. Requires `--yes` |
| `--yes` | bool | `false` | confirms `--regenerate`; has no effect alone |

### `tcr peer ls`

Lists pinned peers, what each may do, what is in flight, and every Mac heard announcing on
the LAN in the last minute and not yet trusted, a found row.

A found row never comes from this command running its own scan: it reads
`peer-state.json`, which a running server's own browse task, on the same cadence as its
beacon, writes to. With no server running, `tcr peer ls` still answers, and simply shows no
found rows, the same honest reading a stale peers file with no proxy behind it already gives.

Trusted rows print first, found rows after, newest first within the found group. A found
row's line is `<address>  <name>  found <age>s ago, not trusted`, with `no name announced` in
the name column when the beacon carried none. The summary line carries `found=<n>` (how many
are shown) and `not-shown=<n>` (how many this Mac is holding back past the twelve-row cap, the
`abuse-resistance.md` display limit).

A Codex account whose exit lock names a peer gets a line of its own, after the found rows and
before the summary: `<account>  exits=<peer short id>`, then why the pin does what it does. A
Codex request always leaves from this Mac, so a pin that is not strict changes nothing, and
the line ends ` (inert: Codex accounts always leave from this Mac)`. A strict pin (`--must`)
is not inert: tcr skips that account during selection until the pin is cleared, while other
usable Codex accounts can still serve. The line names the command that clears it, `tcr peer
account <name> --exits-from local --no-must`. If no usable Codex account remains, requests
fail immediately with a Codex configuration error. `tcr peer account` no longer writes either
pin on a Codex account (see
[peers.md](peers.md#exit-lock-keeping-an-account-on-one-ip)); these lines are for one written
before it refused. The `--json` document is unchanged, and its `lentTo` block never names a
Codex account, because no lease draws on one.

| flag | type | default | effect |
|---|---|---|---|
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to read |
| `--json` | bool | `false` | machine-readable output. A sibling document to `tcr status --json`, never merged into it: that is a bare array of accounts, and clients depend on exactly that shape. `peers` carries both kinds of row: a trusted one has `node`; a found one has `name`, `address`, `trusted: false`, `lastSeenMs`, and no `node` at all, because a found row carries no key to pin |
| `--config <path>` | path | `~/.config/tcr.json` | main config to read: `--json` reads it for the account labels the `lentTo` block is keyed by, each account's exit lock (`exits`) and the expiry a handed-over key ends at (`handedKeyUntil`), and the text form for the Codex exit pins it lists. A config that is missing or unreadable reads as one with no accounts, so `lentTo` and `exits` are empty and no pins are listed, instead of failing the listing |

### `tcr peer find <on\|off>`

Turns discovery on or off: announcing this Mac's presence and looking for others.

| flag | type | default | effect |
|---|---|---|---|
| `<on\|off>` | positional | | turn discovery on or off |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |
| `--announce-name <on\|off>` | enum | `off` | whether the beacon includes this Mac's display name. Off either way, the beacon never carries a key, a peer id, or any other identity material |

**This verb writes a flag; the running server does the announcing AND the browsing.** It
re-reads the flag about every twenty seconds, so `on` starts the beacon within that and `off`
stops it within a minute, neither one needing a restart. The beacon carries the announcing
process's per-boot instance id, which is why the CLI cannot announce on the server's behalf: a
neighbour's knock names the id it saw, and an id from a CLI that has already exited matches
nothing.

**Running `tcr peer find on` in a terminal also runs one scan of its own, right away, and
prints what it saw**: an address and a name (or `no name announced`) per line, `no Macs found`
when there was nothing. That scan is a diagnostic readout for an operator at a prompt; it
writes nothing to disk. The list `tcr peer ls` and the Peers tab read comes from the running
server's own browse task, never from this command.

### `tcr peer name [name]`

Sets, or with no argument prints, this Mac's display name, what another Mac shows for it.
Refused if it carries an `@`, a UUID shape, or an organization name, because a name reaches
other machines. With nothing set, the name shown is the host name.

| flag | type | default | effect |
|---|---|---|---|
| `[name]` | positional | prints current | the name to show |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |

### `tcr peer share <on\|off>`

Turns account sharing on or off for every currently trusted peer, at a default lend amount.

| flag | type | default | effect |
|---|---|---|---|
| `<on\|off>` | positional | | turn sharing on or off |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |
| `--window <5h\|7d\|7d_oi>` | enum | `7d` | which rate-limit window is shared. `5h` and `7d` are untiered; `7d_oi` is the only window upstream reports per model |
| `--fraction <f>` | float | `0.1` | ceiling on any one lease, as a fraction of the window. Clamped to `0.0..=0.5` |
| `--ttl <secs>` | int | `600` | how long a granted lease lives before it needs renewing |
| `--max-inflight <n>` | int | `2` | how many borrowed requests may be in flight at once against one lease. `0` mints leases that refuse every request, which is what writing `0` asks for |
| `--scope <scope>` | string | `all` | what the DEFAULT lease draws from: `all`, `group:<name>`, or `account:<label>[,<label>]`. Written onto every pinned Mac and recorded as `defaultLend`, which the Sharing defaults sheet shows |
| `--config <path>` | path | `~/.config/tcr.json` | main config, read only when `--scope account:` names accounts, to refuse a label that is a Codex account's. A config that is missing or unreadable skips that check |

`tcr peer share on` with no flags mints exactly one grant per trusted peer: 10% of the weekly
window (`7d`), a 600-second lease, at most 2 requests in flight, drawn from every account.

A scope that names a Codex account (`--scope account:<label>`, with the label `tcr status`
prints for it) is refused before anything is written: Codex accounts stay on this Mac, so
only a Claude account can be lent. `--fraction 0` is still allowed, so an old grant that names
one can be removed.

Run again over a grant that already exists for the same window and scope, it **keeps that
grant's mode, its end date and its daily hours** and changes only what you passed. Those three
are decisions taken per peer: turning a `hand` grant back into a `serve` grant is a different
disclosure than the one that was taken, and it used to happen silently on every re-run.

`--fraction 0` **removes** the grant for that window and scope rather than minting a zero one,
and prints how many it removed. Every Mac that still holds `inspect` may still open a serve
stream; each request on it is then refused for want of a grant, and `tcr peer share off`
closes the streams too.

### `tcr peer pair <addr> [code]`

Trusts a Mac interactively: both screens show six digits, both operators compare them and
confirm.

| flag | type | default | effect |
|---|---|---|---|
| `<addr>` | positional | | `host:port` of the Mac to pair with |
| `[code]` | positional | starts pairing | the six digits the other screen is showing. Omitted, this starts the pairing and prints this side's digits instead |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |

The six digits are built from a **nonce contributed by each side**, and the answering Mac
commits to its nonce before it sees the other's, so neither end can steer the digits after it
knows what the other picked. A Mac on a build that predates this sends a shorter message and
is **refused by name**: the error says an older build reached here and to update it and pair
again, rather than falling back to digits one side could have chosen.

### `tcr peer invite`

Mints a one-line join key for a Mac with no screen to compare digits on: the headless path.

| flag | type | default | effect |
|---|---|---|---|
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |
| `--label <name>` | string | none | a name for the joining Mac. Refused if it carries an `@`, a UUID shape, or an organization name |
| `--ttl <secs>` | int | `600` | how long the key stays usable. Short on purpose: anything that can read the peers file can use an outstanding key while it exists |
| `--uses <n>` | int | `1` | how many Macs may join with this one key |
| `--revoke <id>` | int | none | revoke an outstanding key by id instead of minting one |
| `--plain` | bool | `false` | mint the older, readable key (`tcr-join:v2:…`) instead of the opaque one this build mints by default |
| `--sealed` | bool | `false` | mint an ask (`tcr-invite:v1:…`) instead of a key: a one-time public key that names no address and grants nothing. Good for ten minutes |
| `--reply` | bool | `false` | open a reply to an ask this Mac minted, and join with the key it carried. Requires `--stdin` |
| `--stdin` | bool | `false` | read the reply from standard input, for `--reply`. The only path: a reply is a live answer to a live ask and does not belong in argv any more than a join key does |

The key carries **every address this Mac can be reached at**, best first, and the joining Mac
tries them in order: the tailnet address, then the external address a port mapping published,
then each address a real interface holds. A bind address is never one of them: a Mac
listening on `0.0.0.0` used to mint a key reading `0.0.0.0`, which sent the friend's `tcr peer
join` at its own machine.

By default the key is opaque, one run of base32 (`tcr-join:v3:<...>`), so nobody reading it
over a shoulder or in a screenshot sees an address. It encodes the same three fields the
readable key does; it is not encrypted, and whoever holds it can still spend it. `tcr peer
invite` also prints one line about older builds: a Mac on 1.1.9 or earlier refuses a v3 key
by name, and `--plain` mints the key that build reads
(`tcr-join:v2:<addr,addr,...>:...`).

`tcr peer invite` runs as its own short-lived process, so the external address is read off the
state file a serving process's keeper wrote, not asked of the router by this command itself
(the same reader `tcr peer moved mint` uses). A mapping past its deadline, or one whose router
would not name its own external address, is left out and said out loud: `peer invite: the
router mapped a port and would not name its own external address, so this key carries what
this Mac answers on here alone`.

Under the key, one line per address says which kind it is:

```
peer invite: tailscale 100.64.0.1:7755
peer invite: lan 192.0.2.10:7755
```

When none of them is reachable from outside this network, one more line says so: over the
internet the friend needs this Mac's router to forward the port, and `tcr peer reach` reports
where that stands. When `listen` names one specific address rather than `0.0.0.0`, the key
carries that address alone, because somebody chose it.

**The sealed exchange, `--sealed` and `--reply --stdin`.** A key, opaque or plain, still shows
its holder an address once they parse it. For a friend who does not want an address in the
chat at all, `tcr peer invite --sealed` mints an ask instead: a one-time public key that names
nothing and grants nothing. The friend answers it with `tcr peer join --stdin` (below), which
mints its own one-use join key and seals it to the ask, printing a reply
(`tcr-reply:v1:…`). Pasting that reply back with `tcr peer invite --reply --stdin` opens it and
runs the join immediately: pinned and trusted on both sides the moment the handshake
completes, the same `Enrol` (`IKpsk1`) path a pasted key already takes. There is no knock, no
six-digit compare and no further command, because the reply carried a real key rather than a
bare address, and `src/peer/listener.rs`'s off-LAN admission answers that path even when the
friend is not on this Wi-Fi, which a knock never would be.

### `tcr peer join [key]`

Joins another Mac using a key, a link, or an ask it printed.

| flag | type | default | effect |
|---|---|---|---|
| `[key]` | positional | | the `tcr-join:…` key, the `tcr://peer/join?…` link, or the `tcr-invite:…` ask the other Mac printed. **Visible in `ps` and shell history**: use `--stdin` to avoid that |
| `--stdin` | bool | `false` | read the key, link or ask from standard input instead of argv. The only path the panel and the `tcr://` URL handler use, and the one that never leaks the secret to another process |
| `--label <name>` | string | none | this Mac's name, as the other one will show it |
| `--replace` | bool | `false` | accept a link's network key when this Mac already has one |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |

A link that carries a network key is **refused when this Mac already has one**, and the
refusal names what replacing it would cut this Mac off from. It is the same refusal
`tcr peer network-key join` gives, for the same reason: a second office's key pasted over the
first is the commonest way a Mac disappears from its own mesh. `--replace` means it.

Fed an ask (`tcr-invite:v1:…`) instead of a key or a link, `tcr peer join --stdin` mints a
one-use, ten-minute join key of its own (the same key `tcr peer invite` would mint) and seals
it to the ask, printing a reply (`tcr-reply:v1:…`) rather than joining anything itself: it is
the friend's side of the sealed exchange `tcr peer invite --sealed` describes above.

### `tcr peer forget <peer>`

Stops trusting a Mac. One deleted line; the next handshake from it fails.

| flag | type | default | effect |
|---|---|---|---|
| `<peer>` | positional | | the peer id to forget, as `tcr peer ls` prints it |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |

Does **not** revoke egress still reachable through a peer that holds the `forward` grant; the
output says so whenever one does.

### `tcr peer allow <peer> <grant> <on\|off>`

Grants or revokes one thing for one peer.

| flag | type | default | effect |
|---|---|---|---|
| `<peer>` | positional | | the peer id, in its full wire form: the `node` field of `tcr peer ls --json`, never the short `tcr-…` form the text output prints |
| `<grant>` | enum | | `gateway` (carry this peer's bytes out, blind), `forward` (relay to peers this Mac has pinned, transitive), `inspect` (accept this peer's requests and serve them here, reading them in full), `disclose` (send requests to this peer, letting it read them in full), `accept-move` (accept an account this peer moves here, the only grant under which a credential crosses a host boundary), `control-briefs` (tell this peer about this Mac's other peers, one hop out), `control-lendable` (tell this peer how much this Mac could lend), `control-diag` (tell this peer this Mac's build and boot id), `control-drop` (leave this peer this Mac's current address at the dead drop, and apply the address it leaves back; read on both sides, so a peer this Mac does not publish for does not get to write endpoints onto its row either) |
| `<on\|off>` | positional | | grant or revoke |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |

### `tcr peer lend <peer>`

Sets what one peer may borrow, on one window and scope, or takes it away. A Mac may hold
several leases at once, one per scope: lending a new scope adds a lease rather than replacing
the ones already granted. `--list`, `--revoke` and `--relend` manage that set; on the panel
this is the per-Mac sheet's **Lend from** list.

| flag | type | default | effect |
|---|---|---|---|
| `<peer>` | positional | | the peer id in full wire form, same rule as `tcr peer allow` |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |
| `--window <5h\|7d\|7d_oi>` | enum | `7d` | which window this grant is against |
| `--fraction <f>` | float | `0.1` | ceiling on any one lease, as a fraction of the SCOPE's headroom. Clamped to `0.0..=0.5`. `0` removes the grant |
| `--ttl <secs>` | int | `600` | how long a granted lease lives before it needs renewing |
| `--max-inflight <n>` | int | `2` | how many borrowed requests may be in flight against one lease |
| `--scope <scope>` | string | `all` | what this lease draws from: `all`, `group:<name>` (one `tcr group` group, pooled), or `account:<label>[,<label>]` (one or more accounts, by the sanitized label `tcr status` prints, never an email or a uuid) |
| `--for <duration>` | string | no end | lend for a duration (`2h`, `90m`, `3d`), after which this Mac stops renewing the lease. `none` clears an end |
| `--until <time>` | string | no end | lend until a time of day (`18:00`), today or tomorrow, whichever comes next |
| `--between <HH:MM-HH:MM>` | string | no restriction | only open the lease inside this daily window, in this Mac's local time. `22:00-08:00` crosses midnight and is charged to the day it starts, so a Friday-only schedule with that window is open Friday 22:00 through Saturday 08:00 |
| `--days <mon,tue,...>` | string | every day | only open the lease on these days of the week, read against the day the window starts |
| `--mode <serve\|hand>` | enum | keeps the replaced grant's mode, or `serve` for a new one | `serve` sends the borrower's requests over this Mac and out on this Mac's IP; this Mac reads them. `hand` gives the borrower a short-lived access token instead, over the paired session, so the borrower sends the request on its own IP and this Mac never reads it. Omitted on a replace keeps the mode already in force, so editing a hand grant's fraction cannot quietly turn it back into `serve` |
| `--list` | bool | `false` | list this peer's leases instead of changing them, one greppable line each, with the lease id `--revoke` and `--relend` take |
| `--config <path>` | path | `~/.config/tcr.json` | main config, read by `--mode hand` to check the scope's exit locks, and when `--scope account:` names accounts, to refuse a label that is a Codex account's. A config that is missing or unreadable skips the second check |
| `--revoke <lease-id>` | string | none | take one lease away, by the id `--list` printed. The other leases this Mac holds are untouched |
| `--relend <lease-id>` | string | none | put an ended lease back to work, with a new `--for`/`--until` or with no end at all |

A lease asked for outside its own `--between`/`--days` window is refused: the borrower gets
`OutsideSchedule` back instead of a grant, and nothing is served. A lease with no schedule set
behaves exactly as it does today, open at every hour.

**What `--mode hand` changes.** A `serve` grant is a proxy: Mac B's request travels to Mac A,
Mac A sends it to the account's real destination and hands the reply back, so Mac A's `tcr`
sees every prompt. A `hand` grant is different in kind: Mac A mints the account's own
short-lived access token and sends it to Mac B once, over the already-paired, already-encrypted
session. From then on Mac B talks to the account directly, on Mac B's own IP, and Mac A reads
nothing. Mac A still holds the refresh token and still decides when the lease ends; letting it
expire, or `--revoke`, is the only way to take a hand grant back, since there is no request
passing through Mac A to refuse.

A `hand` grant is refused outright, before anything is written, when every account the scope
covers has `egressStrict` on: that pin says the account's requests leave through one named Mac
or not at all, and a borrower sending on its OWN IP can never be that Mac. Lend it as
`--mode serve` instead, or clear the pin on an account the scope covers first.

A scope that names a Codex account (`--scope account:<label>`) is refused too, before anything
is written: Codex accounts stay on this Mac, so only a Claude account can be lent. A zero
fraction, `--list`, `--revoke` and `--relend` are still allowed, so an old lease that names one
can be cleared.

### `tcr peer status`

Asks the RUNNING proxy what it holds for each pinned Mac: what is in flight, what each lease
has spent, and which paths have been measured. Nothing here is read off the peers file, so a
row appears only while a server is up.

| flag | type | default | effect |
|---|---|---|---|
| `--config <path>` | path | `~/.config/tcr.json` | main config to read the port and api-key from |
| `--json` | bool | `false` | emit the peers block as JSON instead of greppable text |

Each row carries the pinned key twice: **`id`** is the full wire form, the only spelling
`tcr peer lend --revoke` and `--relend` read back, and **`display`** is the short `tcr-…` form
for a person reading one row aloud. Nothing parses `display` back. A row's `name` is the
operator's label through the same sanitizer `tcr peer ls` uses, so a label that is an email or
a UUID comes out `[masked]`.

### `tcr peer via <target>`

Chooses how this Mac reaches the internet when its own connection is down.

| flag | type | default | effect |
|---|---|---|---|
| `<target>` | positional | | `auto` to fall back to a trusted, willing Mac automatically, `off` to never route out through a peer, or a peer id to pin one specific Mac |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |

### `tcr peer internet <on\|off>`

Opts this Mac into being reached from off its own network. Off by default: a fresh install
answers nobody outside its LAN, the same as it answers nobody at all before `tcr peer listen`
is set.

| flag | type | default | effect |
|---|---|---|---|
| `<on\|off>` | positional | | turn internet reachability on or off |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |

Turning it on asks the router for a mapping on the peer listener's port and keeps it open,
renewed every 30 minutes on a 2-hour lifetime; turning it off, or shutting down, deletes the
mapping instead of leaving it on the router. See [peers.md](peers.md) § "Reaching a Mac off
your network" for the fallback path when the mapping itself is unreachable.

**A running server honours both settings without a restart.** It re-reads the flag about every
five seconds, the way it re-reads `peer.find`: `off` ends the keeper and deletes through
whichever protocol granted the mapping, and `on` starts a keeper and asks the router again. The
flag used to be read once at boot, which meant `off` ran in the CLI, deleted over NAT-PMP, and
the server's next renewal simply created the mapping again; `on` then reached nothing at all
until the next restart.

### `tcr peer drop <on\|off>`

Turns the dead drop on or off: leaving a friend that has also moved this Mac's current address
on an HTTPS surface you own, and reading theirs back. Off by default, the same shape as `tcr peer
internet <on\|off>`.

| flag | type | default | effect |
|---|---|---|---|
| `<on\|off>` | positional | | turn the dead drop on or off |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |

Turning it on with no store configured prints what to run first and changes nothing, the shape
`tcr peer find on` uses when it has nothing to open a port with. Set a store first with `tcr peer
drop-store set`, and grant each peer `control-drop` with `tcr peer allow <peer> control-drop on`
(both Macs; see [peers.md](peers.md) § "Reaching a Mac off your network"). The running server
re-reads this flag whenever the peers file's mtime moves, so no restart is needed.

### `tcr peer drop-store <set\|show\|clear> [value]`

Points the dead drop's store at an HTTPS surface, or shows or clears the one already set.

| flag | type | default | effect |
|---|---|---|---|
| `<set\|show\|clear>` | positional | | what to do |
| `[value]` | positional | | `set` only: the URL template containing `{name}`, used for both the publish and the fetch |
| `--token <token>` | string | none | `set` only: the bearer token. Give the flag with no value to read it from standard input instead, so it never enters this process's argv or the shell history |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |

`set` refuses a template with no `{name}` placeholder at the command line, before it is ever
written to the peers file. `show` prints the kind and the template with the token redacted as
`set` or `not-set`, never the token itself. The store credential lives in `tcr-peers.json`, mode
0600, the same as `networkKey` and every `rendezvousSecret`.

### `tcr peer pending`

Shows the Macs asking to connect with this one, and nothing else about them: a request is a
row, never a trust decision.

| flag | type | default | effect |
|---|---|---|---|
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file (and the runtime-state file beside it) to use |
| `--json` | bool | `false` | machine-readable output |

### `tcr peer accept <target>`, `tcr peer ignore <target>`, `tcr peer block <target>`

Answer one pairing request. `<target>` is the instance id or address, as `tcr peer pending`
prints it, on all three.

| verb | effect |
|---|---|
| `accept` | opens a two-minute window during which the six-digit compare can complete for that one Mac |
| `ignore` | turns the request down and stays quiet to that address for an hour |
| `block` | refuses permanently: bans the address, and the peer's static key too once the handshake has learned one, so a new address does not help it |

Both take `--peers <path>` (default: the peers file in the operator's config directory).

### `tcr peer unblock <addr>`

Lifts a block.

| flag | type | default | effect |
|---|---|---|---|
| `<addr>` | positional | | the address to unblock, as `tcr peer ls --json` lists it under `blocked` |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |

### `tcr peer network-key <set\|join\|clear\|show> [key]`

The opt-in office network key (52 characters, 32 bytes, Crockford base32): mint one, paste one
in, clear it, or check whether one is set.

| flag | type | default | effect |
|---|---|---|---|
| `<action>` | enum | | `set` mints a key and prints it once; `join` pastes in a key another Mac printed; `clear` forgets it, making this Mac visible and reachable to every `tcr` on the network again; `show` says whether one is set, without printing it |
| `[key]` | positional | | for `join`: the key string. Omit with `--stdin` |
| `--stdin` | bool | `false` | for `join`: read the key from standard input, so it never enters argv or shell history |
| `--replace` | bool | `false` | required by `set` and `join` when a key is already set, because replacing it cuts this Mac off from every Mac still holding the old one. Without it the verb refuses and prints what it would have cut off |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |

### `tcr peer link`

Prints one link to share, which brings another Mac onto this mesh. Refuses if no network key
is set yet (`tcr peer network-key set` first): a link with nothing to carry names `tcr peer
invite` as the headless alternative instead.

| flag | type | default | effect |
|---|---|---|---|
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |
| `--invite` | bool | `false` | also mint a one-use join key and carry it in the link, so opening it also completes pairing. **Turns the link into a live bearer secret with ten minutes on it**: without this flag it carries only the network key |
| `--label <name>` | string | none | a name for the joining Mac, when `--invite` is given |

### `tcr peer moved <mint\|open> [peer\|link]`

The link a Mac sends one friend after it changed networks: it carries this Mac's current
addresses, sealed so only that one friend's Mac can read them. It joins nothing, grants nothing
and pairs nothing. See [peers.md](peers.md) § "When both Macs moved".

| flag | type | default | effect |
|---|---|---|---|
| `<action>` | enum | | `mint` prints one link for one already-trusted Mac; `open` reads a link somebody sent and says which of this Mac's peers it is from and where that Mac now is |
| `[peer\|link]` | positional | | for `mint`: the peer id to seal for, in its full wire form, the `node` field of `tcr peer ls --json`. For `open`: the `tcr://peer/moved?…` link. **A link typed here is visible in `ps` and in shell history**, and `open` says so on stderr: use `--stdin` |
| `--stdin` | bool | `false` | for `open`: read the link from standard input, one line, so it never enters argv. The only path the panel and the `tcr://` URL handler use |
| `--yes` | bool | `false` | for `open`: actually write the addresses. Without it, `open` reads the link, says what it would add, and changes nothing. No effect on `mint` |
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use (and the runtime-state file beside it, which is where the router mapping `mint` advertises is read from) |

`mint` seals for one Mac at a time, under a key derived from the secret that pair already
shares. It refuses a Mac this one has never completed a session with since that secret existed,
and the refusal carries the fix: the two have to talk once, over any address that works, before
either can seal for the other.

What the link carries is every address a friend could dial this Mac at, ranked the way an invite
key ranks them: the tailnet address first, then the external socket a router mapping published,
then the addresses this Mac's own interfaces hold. A listen socket bound to every interface,
`0.0.0.0:7755`, is not itself one of them: it is what the listener is BOUND to, and a friend
still has to reach this Mac at one particular interface. A Mac with none of the three, and no
router mapping, gets a refusal naming `tcr peer reach` and `tcr peer internet on` rather than a
link with nothing dialable in it. The link never carries the address that peer last said it sees
this Mac at, which is the address this Mac had *before* it moved.

`open` without `--yes` writes nothing, the same split `tcr peer id --regenerate` makes with its
own `--yes`. What it can add is bounded: at most two addresses, on a row this Mac has already
pinned, at the lowest confidence band there is. It never creates a row, never un-forgets a Mac,
never touches a network key and never turns a switch on. Pasting the same link twice writes the
file once.

When `open --yes` writes nothing it says which of the four reasons applies, because they need
different things from the person reading: the Mac is already reachable at everything the link
says (`already-known`); the row's slots for a link are full; nothing in the link is an address
anything can dial, so the Mac that sent it has to mint a new one; or this Mac holds no row for
that peer at all, which means the pairing is gone and a link never brings one back.

```
$ tcr peer moved mint 0W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3GE1R70W3G
tcr://peer/moved?v=1&r=6YFN2TQ8ZKW...
peer moved: sealed for studio-mac; only that Mac can read it, and it goes stale in 24 hours
peer moved: it carries 2 address(es) and nothing else: it joins nothing, grants nothing and pairs nothing

$ tcr peer moved open --stdin < link.txt
peer moved: from studio-mac (tcr-0W3GE1R70W), sealed 372s ago
peer moved: would add 192.0.2.7:41234
peer moved: would add [2001:db8::1]:41234
peer moved: nothing written; pass --yes to keep these

$ tcr peer moved open --stdin --yes < link.txt
peer moved: added 2 address(es) to studio-mac; nothing else changed
```

Every refusal exits non-zero except `already-known`, which is the ordinary outcome of pasting a
link twice. A refusal about a link that arrived names nothing: not the peer it might have been
for, not an address, not any part of what was pasted, because a link forwarded into the wrong
group chat must teach its reader nothing about who this Mac knows.

### `tcr peer reach`

Prints what this Mac can be reached on from off the local network. Read-only: nothing here
dials a peer or touches the running proxy.

| flag | type | default | effect |
|---|---|---|---|
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |
| `--map` | bool | `false` | also ask the router for a NAT-PMP mapping on the peer listener's port, held for two minutes |
| `--json` | bool | `false` | machine-readable output |

It prints, in order: this Mac's global IPv6 addresses (none, if it has none); the router's
answer, meaning the gateway found, the external address, and the mapping outcome when
`--map` was passed (a refusal is an ordinary outcome, not an error); the local listen port;
the current time-derived port slot; and, per pinned peer, that peer's derived port for this
slot and a punch line: the address that peer last said it sees this Mac at, and whether a
punch between the two is possible right now. The answer is `yes` when either that peer's
last direct arrival or the row's own recorded endpoints give an address to aim at, and `no`
when neither does. A refusal from the router, or a peer row with no derived port yet, still
exits 0.

The external address is asked over NAT-PMP first and UPnP IGD when NAT-PMP refuses, the same
order the mapping itself falls back through, and the line names which protocol answered:
`reach: external-address: <addr> (nat-pmp)` or `reach: external-address: <addr> (upnp)`. A
router that answers neither prints `reach: external-address: unavailable: <nat-pmp refusal>
(nat-pmp), <upnp refusal> (upnp)`.

It also prints the dead drop: `off` when `tcr peer drop` is off, or `on` with the current drop
slot and, per pinned peer that holds `control-drop`, that peer's drop name for this slot (the
name this Mac reads from, not the one it writes to) and the last time a fetch actually wrote a
new address from it. There is no separate `tcr peer drop-status` verb; this readout is where it
lives, on `--json` under a `deadDrop` key. Publishing itself leaves nothing on this Mac to read
back: the write lands on the store, never on the peers file, so no last-publish time is printed.

### `tcr peer graph <--json\|--serve>`

Prints, or serves, the mesh as this Mac currently sees it: pinned peers, the paths to each,
and each path's RTT and loss. Read-only, same as `tcr peer reach`.

| flag | type | default | effect |
|---|---|---|---|
| `--peers <path>` | path | `~/.config/tcr-peers.json` | peers file to use |
| `--json` | bool | `false` | print the graph once, machine-readable, and exit |
| `--serve` | bool | `false` | bind loopback only and serve one page redrawing the graph from the same data every 5 seconds, nodes and edges coloured by RTT and loss. Refuses to bind anything but loopback: this is a page for the Mac it runs on, not the mesh. See [peers.md](peers.md) § "Seeing the mesh's paths (not in this release)" for why there is no mesh-wide version yet |

`--json` and `--serve` are mutually exclusive; one of the two is required.
