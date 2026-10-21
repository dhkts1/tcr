<div align="center">

<img src="assets/tcrbar-icon.png" alt="" width="116">

# tcr

tcr (Team Capacity Router) is a local proxy that runs Claude Code and Codex across multiple subscription accounts.

Point Claude Code, OpenAI's `codex` CLI, or a tool built on them such as OpenClaw, at it. For each
request it picks an account by looking at what every account has left in every quota window. It
keeps a conversation on the account whose prompt cache is already warm, and it shows you what the
Claude traffic would have cost.

[![Latest release](https://img.shields.io/github/v/release/dhkts1/tcr)](https://github.com/dhkts1/tcr/releases/latest)
[![License: PolyForm Noncommercial](https://img.shields.io/badge/License-PolyForm%20Noncommercial-yellow.svg)](LICENSE)
![Rust](https://img.shields.io/badge/rust-stable-orange.svg)
![macOS and Linux](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey.svg)

[Website](https://dhkts1.github.io/) · [Install](#install) · [Usage](#usage) · [Compared with other tools](#compared-with-other-tools) · [FAQ](#faq) · [Configuration](docs/configuration.md) · [CLI](docs/cli.md) · [Security](#security)

[![Download TcrBar for macOS](https://img.shields.io/badge/Download-TcrBar%20for%20macOS-blue?style=for-the-badge)](https://github.com/dhkts1/tcr/releases/latest)
[![Install the CLI](https://img.shields.io/badge/Install-the%20CLI-lightgrey?style=for-the-badge)](#install)

<sub>TcrBar is a `.dmg` on the release page</sub>

<img src="assets/tcrbar-panel-healthy.png" alt="TcrBar menu-bar panel showing a healthy pool of accounts" width="400">

</div>

## Why tcr

- **It chooses by quota, not by turn.** It reads the five-hour, weekly and per-model weekly windows
  on every account, and spends the quota that resets soonest first, so less of it expires unused.
- **It keeps the prompt cache warm.** A conversation that moves to another account starts cold, and
  its next request pays at least 12.5 times as much for the part of the prompt that was cached. So
  each conversation is pinned to one account and stays there, and on subscriptions the cache is kept
  for an hour instead of five minutes. Over 30 days of the author's own traffic, ending 2026-10-13,
  97.6% of prompt tokens were read from cache.
- **One proxy serves every session.** Each Claude Code session on the machine has its own pin, and
  you can add, disable or remove accounts while the proxy runs.
- **You can watch it.** A macOS menu-bar app and a terminal dashboard show every window and every
  session, and the app shows what the traffic would have cost at list price.
- **It stays on your machine.** No telemetry and no crash reporting. Tokens, logs and pins are
  files on your own disk.

## Install

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://raw.githubusercontent.com/dhkts1/tcr/main/install.sh | sh
```

This installs the `tcr` CLI. On macOS it also installs TcrBar, the menu-bar app, from the `.dmg` on
the [release page](https://github.com/dhkts1/tcr/releases/latest); set `TCR_SKIP_UI=1` to skip
that. The installer then opens TcrBar, so look for its icon at the top right of the screen; set
`TCR_NO_LAUNCH=1` to stop that (it is skipped anyway under CI and SSH).

To install only the `tcr` CLI, on any platform, use the release's own installer:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/dhkts1/tcr/releases/latest/download/tcr-installer.sh | sh
```

Or build from source with Rust 1.99 or newer (`rust-toolchain.toml` pins the exact version, and
rustup installs it on the first build). Install the binary with the script and not with `cp`,
because [macOS kills a running `tcr` that `cp` writes over](CONTRIBUTING.md#installing-it-onto-your-path):

```sh
cargo build --release
scripts/install-cli.sh          # places `tcr` at ~/.local/bin/tcr by default
```

| Platform | `tcr` CLI | TcrBar menu-bar app |
|---|---|---|
| macOS, Apple silicon (`aarch64-apple-darwin`) | prebuilt | yes |
| macOS, Intel (`x86_64-apple-darwin`) | prebuilt | yes |
| Linux x86_64, musl (`x86_64-unknown-linux-musl`) | prebuilt | no |
| Linux aarch64, musl (`aarch64-unknown-linux-musl`) | prebuilt | no |
| Anything else | build from source | no |

The Linux builds are static musl binaries, so they run on both Alpine and glibc systems. TcrBar and
`tcr ui` are macOS-only.

To make a plain `claude` go through tcr, run one of these. Both refuse to overwrite a `claude`
alias, function or base URL you already have, and both take `--remove`.

- `tcr setup shell` adds a `claude` function to your shell's startup file, for terminals.
- `tcr setup claude-settings` sets one key in `~/.claude/settings.json`, so every launch goes
  through tcr. The cost is that `claude` cannot reach Anthropic while tcr is stopped.

TcrBar asks once which one you want, when the pool has a Claude account.
Details: [`tcr setup`](docs/cli.md#tcr-setup).

### Verify a download

Get `tcr` only from this repository's [release page](https://github.com/dhkts1/tcr/releases)
or `install.sh` above, because it will hold your account tokens. Each CLI archive has a `.sha256`
file beside it:

```sh
shasum -a 256 -c tcr-aarch64-apple-darwin.tar.xz.sha256
```

TcrBar is signed with a Developer ID certificate and notarized by Apple. The commands that check
it, and what the checksum does not prove, are in
[`docs/architecture.md`](docs/architecture.md#verifying-a-download).

### Uninstall

Stop the proxy, then delete the binary: `~/.cargo/bin/tcr` if you used either installer, or
`~/.local/bin/tcr` if you used `scripts/install-cli.sh`. On macOS,
`apps/macos/scripts/uninstall.sh` removes TcrBar and its login item.

Nothing deletes your data for you. `~/.config/tcr.json` holds working credentials for every
account in the pool, so delete it too, along with `~/.cache/tcr/`. Also remove any
`ANTHROPIC_BASE_URL`, `HTTPS_PROXY`, `NODE_EXTRA_CA_CERTS` or `CODEX_CA_CERTIFICATE` you exported,
and any `chatgpt.com` entry you added to `NO_PROXY`. Every file `tcr` keeps is listed in
[`docs/architecture.md`](docs/architecture.md#files-on-disk), and the full steps are under
[Uninstalling](docs/architecture.md#uninstalling).

## Usage

```sh
tcr login          # PKCE browser flow, once per account you want in the pool
tcr login --codex  # the same for a Codex (ChatGPT) account, for codex through the pool
tcr                # start the proxy with the live TUI (q quits)
tcr run -- <args>  # launch Claude Code already pointed at the proxy
```

If you already use the `claude` CLI on this machine, you can skip `tcr login` the first time.
When `tcr status`, `tcr accounts` or the proxy starting up finds no accounts configured, it imports
that existing login, and `tcr login --from-claude-code` does the same import on request. A refresh
token works only once, so the first time tcr refreshes the imported login, `claude` asks you to
log in through the browser once. tcr warns you about this on every import, and
[`docs/cli.md`](docs/cli.md) has the details.

To point a client at the proxy yourself instead of using `tcr run`:

```sh
export ANTHROPIC_BASE_URL=http://127.0.0.1:3456   # base-URL mode
# or
export HTTPS_PROXY=http://127.0.0.1:3456          # forward-proxy mode
export NODE_EXTRA_CA_CERTS=<the CA path tcr logs at boot>
```

Forward-proxy mode decrypts TLS with a certificate tcr makes on your machine, so the client has to
trust it. When `tcr` starts it prints the CA path to use and the variable to put it in:
`NODE_EXTRA_CA_CERTS`, and also `CODEX_CA_CERTIFICATE` once the config holds a Codex account.

With the certificate tcr made itself (CA `~/.config/tcr-ca.pem`), it decrypts `chatgpt.com` only
on a connection that uses `HTTPS_PROXY=http://tcr-codex@127.0.0.1:<port>`, and only while the pool
holds a Codex account. `tcr run --codex` sets that marker and the CA for you. Without the marker,
`chatgpt.com` passes through untouched and the client keeps its own login, and that includes the
`HTTPS_PROXY` a Claude `tcr run` session hands down. On a marked connection, paths outside
`/backend-api/` go out under the client's own credential. Keep the marker inside
`tcr run --codex`; do not export it in a shell.

Each client takes its CA from a different place:

- `codex` ignores `NODE_EXTRA_CA_CERTS`. It reads `CODEX_CA_CERTIFICATE`, falls back to
  `SSL_CERT_FILE` while that is unset, and also accepts a CA already in the system trust store.
- A Claude launch sets only `NODE_EXTRA_CA_CERTS`, so a Codex client started inside it keeps its
  own CA setting.
- A pooled Codex launch sets `CODEX_CA_CERTIFICATE` only if you have not, and leaves an explicit
  `NODE_EXTRA_CA_CERTS` alone. The shell tools it starts get a proxy URL without the marker, so
  their ChatGPT requests keep their own TLS and login, unless you set their environment yourself.

The config lives at `~/.config/tcr.json`. `name` and `accessToken` are the only required keys.
Every other key, its default and the file's permissions are in
[`docs/configuration.md`](docs/configuration.md).

### Managing the pool

Changes to the pool reach a running proxy without a restart. `enable`, `disable`, `remove`,
`priority` and `control` apply to it at once, and the proxy picks up `group` edits from the
config file on its next request. Every flag is in [`docs/cli.md`](docs/cli.md).

| Command | What it does |
|---|---|
| `tcr accounts [--probe]` | List the pool. `--probe` refreshes quota live instead of reading the file. |
| `tcr status [--json]` | Probe every account and print the pool. `--json` is what the panel reads. |
| `tcr priority <account> [N \| --first \| --last]` | Set the rotation tier. |
| `tcr enable` / `tcr disable <account>` | Take an account in or out of rotation. |
| `tcr remove <account>` | Delete an account, disabling it live first. |
| `tcr control <account> [--clear \| --show]` | Name the Claude account that answers the calls a client makes besides its prompts. |
| `tcr group ls \| add \| rm \| reserve \| color` | Label accounts, and hold a labelled set back for traffic that asks for it. |
| `tcr run --group <name>` | Start a session that prefers one group. |
| `tcr sessions [--json]` | List the sessions the running proxy has seen in the last hour. The panel's Sessions and Tools tabs read the same feed. |
| `tcr list --json` | Print every Claude account in claude-swap's list format, so CodexBar can show them. Never probes. |
| `tcr wrap [--days N]` | A usage report from the local ledger: cost, tokens and cache-hit ratio, broken down by model, account and day, and by provider when the period holds Codex traffic. Needs no running proxy. |
| `tcr token <account>` | Print an account's access token to stdout, for piping. |
| `tcr update` | Update `tcr` in place, from the checkout or the published installer. |

## What it does

Every Claude account has several usage limits running at once: a rolling five-hour session window,
a weekly window, and a weekly window for a single model family. Each window tracks how much you
have used and when it resets, and different accounts reset at different times. `tcr` learns every
window on every account and schedules requests against them.

**It does not round-robin.** Within a priority tier it prefers the account whose quota window
resets soonest, because weekly quota you have not used is lost once that window resets. Ties go to
the account used least recently, so requests still spread out instead of piling onto one account.
It skips an account that is disabled, returning errors, waiting out a rate limit, too close to a
limit, reserved for a group this request did not ask for, or out of the per-model window this
request needs. The full order is in
[`docs/architecture.md`](docs/architecture.md#account-selection). The soonest-reset rule, and how
to turn it off, is in
[`docs/configuration.md`](docs/configuration.md#reseturgencytierhours-spend-the-quota-that-is-about-to-expire).

**The prompt cache is the expensive part.** Anthropic keeps it per account, so moving a live
conversation to another account rebuilds its whole cache. That is why each session is pinned to
one account and stays there. If one request is sent elsewhere to get around a brief fault, the pin
stays put; only a failure of the account itself moves it. Pins are saved to disk continuously and
restored when the proxy starts, so a restart does not send every live conversation to a cold
cache. Anthropic keeps a cache for five minutes, or an hour if the client asks for it, and a
session that asked for the hour keeps its pin longer to match.

**Outgoing traffic is paced per organization**, because that is the unit Anthropic limits: two
accounts in one org share that org's single rate. A looser limit for the whole pool sits behind
it. Quota probes are reads that spend nothing and keep an idle account's numbers current. Each
account runs them on its own random schedule, so the pool never sends them to Anthropic all at
once on a fixed beat, and a restart scatters them again instead of lining them up.

**Everything is priced.** Every request served goes into a local ledger, priced at the API list
price of its model's own provider, per account and per model, split into input, output, cache
reads and cache writes. No OpenAI model has a built-in price, so a Codex request stays unpriced
until [`pricing`](docs/configuration.md#pricing-and-usageretentiondays) names its model. The
panel shows today's spend, the burn rate over the last hour and the cache hit rate; the terminal
dashboard shows no spend. The ledger reloads from disk when the proxy starts, so a restart does
not reset the day. None of this is a bill: these accounts are subscriptions, and list price is the
one unit you can compare across accounts, models and days. Traffic that could not be priced shows
no figure rather than a zero, an account that served nothing shows a real zero, and a window
holding both shows the priced part next to the count it could not price.

OAuth tokens refresh in the background, so accounts do not expire while you work. Accounts can
carry labels, and a labelled set can be *reserved* so only traffic that asks for it goes there. One
account can be named to answer the identity checks and other calls a client makes besides its
prompts, and prompts never go to that account. You can add, enable, disable and remove accounts
while the proxy runs, which matters because a restart is what risks your warm pins.

**Codex accounts rotate too.** The pool can also hold ChatGPT logins for OpenAI's `codex` CLI,
kept apart from the Claude accounts: only a Codex account ever serves a Codex request, and a Codex
account never serves a Claude one. `tcr login --codex` signs a Codex account in with tcr's own
login, so the `codex` CLI keeps the one it has. `tcr login --from-codex` imports the login `codex`
already holds instead. tcr then shares refreshed tokens with `codex` through guarded writes to its
`auth.json`, and can recover tokens for the same ChatGPT account. That sharing is best effort: if
`codex` refreshes at the same moment, the two writes can collide. `tcr login --codex` avoids the
shared file. `tcr run --codex -- exec "…"` launches `codex` through the pool, where Codex accounts
are picked, refreshed and probed the same way Claude's are, and `tcr doctor --codex` says whether
`codex` is going through the pool and, if not, why. The import and how it guards the shared tokens
are in [`docs/cli.md`](docs/cli.md#--from-codex-importing-the-login-codex-already-has); what tcr
does with a Codex request is in [`docs/architecture.md`](docs/architecture.md#codex-requests).

**OpenClaw runs on the pool too.** Started with the proxy settings tcr gives its own launches,
[OpenClaw](https://github.com/openclaw/openclaw) sends its Claude turns through the real `claude`
binary and its ChatGPT turns through tcr's Codex accounts, with no change to either project. The
settings and what was tested are in [`docs/openclaw.md`](docs/openclaw.md).

## Watching it

`apps/macos` holds TcrBar, a native app built on `tcr status --json`; the terminal dashboard reads
the running proxy's own state instead. TcrBar lives entirely in the menu bar, with no Dock icon
and no window. Its icon shows the capacity left across the whole pool rather than the worst
account, because in a rotating pool one used-up account is the rotation working, not an alarm.

TcrBar and the terminal dashboard both show Claude and Codex separately. TcrBar counts each
provider on its own: the menu-bar count reads Claude first ("3/4 · 1/2"), the menu-bar icon follows
whichever provider has less left unless Settings pins it to one, the strip under the tabs gives
each provider its own row, and the Accounts tab has a CLAUDE and a CODEX section. The dashboard's
banner gives each provider its own segment (`CLAUDE 1/3 eligible │ CODEX 2/4 eligible`), and its
account list a section each. `tcr status --json` marks each Codex row `"provider": "codex"`.

The panel has four tabs. **Accounts** has one row per account, with a line for each quota window
showing a bar, a percentage and the countdown to that window's reset. Each row also shows whether
its quota probes are working and its group tags, and its right-click menu runs `tcr` for you, so
you can manage the pool from the panel. **Sessions** lists the proxy's live sessions by project,
and each one names the account it runs on. **Tools** shows what is running now, today's slowest
calls and totals per tool. **Peers** shows the other Macs `tcr` has found and what is lent (see
[Peers](#peers)). Sessions and Tools read from the running proxy and keep a copy of what they last
saw, so after a proxy restart they show their last known state instead of going blank.

Account rows also show the per-model weekly window (Fable's, on current plans) once the proxy has
learned it for that account; until then the spot stays empty rather than showing a zero. The proxy
enforces that window as well as showing it: a request for that model skips an account that has
used it up, and requests for any other model ignore it. The header and each card show the spend
figures, and the footer holds a keep-awake switch, Check for updates and Quit. TcrBar can also run
the proxy for you, keep the Mac awake while it does, and update itself through
[Sparkle](https://sparkle-project.org).

Install it from the [latest release](https://github.com/dhkts1/tcr/releases/latest), or run
`tcr ui`. To build it from this repo, run `apps/macos/scripts/install.sh`. How releases are made is in [`docs/RELEASING.md`](docs/RELEASING.md).

The panel never shows a blank list. It tells four cases apart, because each needs a different
response: `tcr` is missing, a poll is failing, the pool is empty, or no proxy is running and the
numbers come from disk.

<p>
  <img src="assets/tcrbar-panel-offline.png" alt="TcrBar panel reading from disk with no proxy running" width="400">
  <img src="assets/tcrbar-panel-no-capacity.png" alt="TcrBar panel with no capacity left" width="400">
</p>

<details>
<summary>The full pool view, and the terminal dashboard</summary>

<p>
  <img src="assets/tcrbar-panel-fleet.png" alt="TcrBar panel listing a full pool of accounts, one line each under a strip of every account" width="400">
  <img src="assets/tcrbar-panel-hover.png" alt="TcrBar panel with an account's card open on hover" width="400">
</p>

Every account is one line. Point at a line, or at its segment in the strip, to open its card; right-click an
account and choose Keep Card Open to leave it open.

![tcr live TUI](assets/tui-demo.gif)

The terminal dashboard runs on both macOS and Linux. It shows the accounts, their quota windows and
why each one is out, the live session tree (which conversation is pinned to which account, and
which ones moved) and the request log. The spend figures are the panel's.

`tcr demo` runs the real dashboard against fake accounts, which is how these screenshots were made.
It needs no config and contacts nothing.

</details>

## How it works

One TCP listener serves both modes. It looks at the first eight bytes of each connection, without
consuming them, and decides from those:

- A `CONNECT` to an Anthropic API host, or to `chatgpt.com` on a marked Codex connection while the
  pool holds a Codex account, is decrypted with a leaf certificate tcr generates locally.
- Any other `CONNECT` is copied through as raw bytes.
- Plain HTTP is base-URL mode.

Each request then runs a bounded rotation loop: pick an eligible account, refresh its token if it
is about to expire, swap the client's credentials for the pooled one, send, and move to another
account on a 401, 429, 529 or connection failure.

Quota numbers come from the response headers of traffic `tcr` already serves. Between requests, a
probe that spends nothing keeps them fresh: Anthropic's OAuth usage endpoint for a Claude account,
Codex's `/backend-api/wham/usage` for a Codex one. The probe never calls `/v1/messages`, so an
idle account's bars stay accurate instead of freezing at their last served value. A window whose
reset time has passed counts as unused again, checked against the clock each time it is read, so
neither the display nor the scheduler can act on a stale bar. The request-flow diagram, the
selection order and the probe schedule are in [`docs/architecture.md`](docs/architecture.md).

## Peers

`tcr` can find other Macs on your network that run `tcr`, trust them, and lend a trusted Mac part
of an account: 20 % of a group's weekly window, for example, or one account until 18:00. It is off
by default. Finding and sharing each have their own switch, and nothing is sent before you turn
one on.

The design rests on three rules. The announcement each Mac broadcasts carries a random id that
changes every boot and a port; it never carries a key, and carries a name only if you allow it.
Nobody is trusted until a person at each Mac has compared the same six digits on both screens and
pressed Trust. A borrowed request is served from the lender's own account, so the borrower's
credential never leaves the borrower's Mac and the lender never learns it.

Pairing and every peer stream run over the
[Noise Protocol Framework](https://noiseprotocol.org/noise.html). A first pairing uses `XX` (both
sides learn each other's key, and the six digits come from the handshake hash), a return visit to
a Mac you already trust uses `IK`, and `IKpsk1` is for when a join key has been passed around.
WireGuard is built on the same handshake family, and if you want to understand how any
Diffie-Hellman based authentication works, that one spec is the best hour you can spend.
[`docs/peers.md`](docs/peers.md) covers finding, trusting, lending and what each step sends.

## Compared with other tools

Several tools let Claude Code use more than one account. They differ in where they sit: a proxy in
the request path chooses an account for every request, while a switcher changes which login Claude
Code itself holds.

| Tool | How it switches | How it picks an account | Keeps each conversation on one account | Runs on | License |
|---|---|---|---|---|---|
| **tcr** | Local proxy in the request path | Every request, by what is left in the five-hour, weekly and per-model weekly windows | Yes, always; moves only when that account cannot serve | macOS, Linux | PolyForm Noncommercial |
| [claude-swap](https://github.com/realiti4/claude-swap) | Swaps Claude Code's stored login, for the whole machine or one terminal | By hand, next in turn, or automatically at a five-hour or seven-day threshold | Per terminal with `cswap run`; a switch moves the running session, which its README says rebuilds the cache | macOS, Linux, Windows | MIT |
| [teamclaude](https://github.com/KarpelesLab/teamclaude) | Local proxy in the request path | Rotates when the five-hour or weekly window reaches a threshold, and tracks the per-model weekly cap | Opt-in with `distributeSessions`, off by default | Node.js 20+ | MIT |
| [ccs](https://github.com/kaitranntt/ccs) | Launches Claude Code with a chosen profile; account pools go through a local proxy | Profile chosen at launch; the proxy rotates round-robin by default, or fills one account first with pool routing on | Opt-in: [pool routing](https://docs.ccs.kaitran.ca/features/pool) holds a conversation on one account for up to an hour | macOS, Linux, Windows | MIT |
| [Quotio](https://github.com/nguyenphutrong/quotio) | macOS app that manages a local proxy (CLIProxyAPI) | Round-robin or fill-first | Not stated in its README | macOS app; CLI on macOS and Linux | MIT |
| [cux](https://github.com/inulute/cux) | Swaps the login, restarts Claude Code and resumes the conversation | On a rate limit by default, with optional five-hour and seven-day thresholds | No: moving the conversation is how it works | Linux, macOS, Windows | GPL-3.0 |
| [cc-account-switcher](https://github.com/ming86/cc-account-switcher) | Swaps the stored login while Claude Code is closed | By hand | Not stated in its README | macOS, Linux, WSL | MIT |

Each row comes from that project's own README and docs, read on 2026-10-13. "Not stated" means its
README does not say, not that the tool cannot.

Another tool fits better if you need Windows (claude-swap, ccs, cux), want Gemini or other
providers behind the same tool (teamclaude, ccs, Quotio), want no proxy in the request path
(claude-swap, cux, cc-account-switcher), or need an open-source license for commercial use (every
other row).

Moving to tcr from claude-swap, Quotio, ccs or CLIProxyAPI: `tcr import` lists the logins they
hold, and `tcr import --yes` takes the new ones ([docs/cli.md](docs/cli.md#tcr-import)).

## FAQ

### How do I use Claude Code with more than one Claude account?

Install `tcr`, run `tcr login` once for each account, start the proxy with `tcr` (or let TcrBar run
it), and launch Claude Code with `tcr run`. Every request then goes through the proxy, which picks
an account from the pool. If you are already logged in to Claude Code, the first run imports that
login, so you only log in to the others.

Claude Code has its own answer: `CLAUDE_CONFIG_DIR` moves its configuration directory, and
[Anthropic's docs](https://code.claude.com/docs/en/env-vars) suggest it for running accounts side
by side, as in `CLAUDE_CONFIG_DIR=~/.claude-work claude`. There you pick the account by picking
the directory when you start a session. `tcr` keeps one Claude Code setup and picks the account
for every request instead. `tcr` does not read `CLAUDE_CONFIG_DIR`: the import takes Claude Code's
default login, and [`docs/cli.md`](docs/cli.md#--from-claude-code-importing-the-login-claude-already-has)
says how to point it at another file.

### Does switching accounts lose the prompt cache?

Yes. Anthropic keeps the prompt cache per account, so a conversation that moves arrives cold and
writes its whole cache again. A cache write bills at 1.25 times the input price and a cache read at
a tenth of it or less, so the first request after a move pays at least 12.5 times as much for the
cached part (25 times on Opus 5.5), and on a long conversation the cached part is nearly all of
the request. That is why `tcr` keeps each conversation on one account, even past the point where
it would send new conversations elsewhere. It moves only when that account cannot serve the
request. Pins are saved to disk, so a restart keeps them for conversations still within the cache
lifetime.

### How long does the cache stay warm?

Five minutes after its last use, unless the request asks for an hour. From 1.1.19 `tcr` asks for
the hour on every Claude request, so a conversation you come back to within the hour resumes warm
instead of writing its cache again. On a subscription the hour costs the five-hour window the same
as five minutes does (measured 2026-10-06). `cacheTtlRewrite` in
[`docs/configuration.md`](docs/configuration.md) turns it off.

### What happens when an account runs out?

New conversations skip any account that is close to a limit, waiting out a rate limit, or out of
the per-model window a request needs, and go to the account the quota rules pick next. A
conversation already on that account moves once, and pays one cache rebuild.

### Does it run on Linux, and with API keys?

The `tcr` CLI and terminal dashboard run on Linux as static builds for x86_64 and aarch64. TcrBar,
the menu-bar app, is macOS only. Accounts are Claude subscriptions logged in through the browser,
or ChatGPT logins for Codex, signed in with `tcr login --codex` or imported with
`tcr login --from-codex`. `tcr` does not serve Anthropic API keys, and refuses a Codex login made
with an API key too ([`docs/cli.md`](docs/cli.md#--from-codex-importing-the-login-codex-already-has)).

## Documentation

| Document | What is in it |
|---|---|
| [`docs/configuration.md`](docs/configuration.md) | Every config key, its type, default and source citation. |
| [`docs/cli.md`](docs/cli.md) | Every command and flag, exit codes, account resolution. |
| [`docs/peers.md`](docs/peers.md) | Finding, trusting and sharing accounts with other Macs on your network. |
| [`docs/architecture.md`](docs/architecture.md) | Request flow, entry modes, Codex requests, account selection, quota probes, files on disk, verifying a download, uninstalling. |
| [`docs/troubleshooting.md`](docs/troubleshooting.md) | Symptoms and what they mean. |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | Development setup, test and lint gates, what `main` requires. |

## Security

Read this before you run it.

**Listening only on localhost does not keep other programs out.** `tcr` listens on `127.0.0.1`,
but every process and container on the host can reach it there. The proxy does not ask local
callers for an API key when it forwards, and nothing creates a `proxy.apiKey` for you, so on a
default install the only check is being on this host. Set a key if you want a tighter boundary
than that.

**The forward proxy is an open tunnel on purpose.** It may decrypt only three hosts:
`api.anthropic.com`, `platform.claude.com` and `chatgpt.com`. Every other `CONNECT` target is
copied through as raw bytes and is never decrypted or filtered, which makes `tcr` an unrestricted
forward proxy to any host for any local process. Claude Code needs it to work that way, so treat
it as a tunnel and not as a firewall. Which hosts are actually decrypted depends on the leaf
certificate in use, and [`MITM-DESIGN.md`](MITM-DESIGN.md) works through that. `chatgpt.com` is
decrypted only on a marked Codex connection while the pool holds a Codex account, and passed
through untouched otherwise.

**Credentials.** On the paths that carry prompts, `tcr` drops the client's own `authorization` and
`x-api-key` headers before adding the pooled Bearer token, so a client credential is never sent
alongside ours. Those paths are Anthropic's `/v1/messages` and `/v1/messages/count_tokens`, and
Codex's `responses`, `images/generations`, `images/edits`, `memories/trace_summarize` and
`alpha/search` under `/backend-api/codex/`. On Codex's paths it also replaces the client's
`chatgpt-account-id` header with the serving account's.

The one exception is a Codex request that names its own workspace routing
(`x-openai-account-routing-override` or `x-openai-fedramp`, which `codex` sends for a
data-residency or FedRAMP workspace). That request is relayed under the client's own bearer and
account id, prompts included, and never pooled, and `tcr login --from-codex` refuses to import
such a login. As a backstop, `tcr` also drops both headers from every Codex request it pools.

Everything else a client sends under its own bearer (connector list, plugins, settings,
bootstrap) is relayed under that bearer unchanged, because those calls are about the client's own
identity. The proxy checks an Anthropic client's identity against `controlAccount` (see
`controlIdentity` in [`docs/configuration.md`](docs/configuration.md)). It never checks a Codex
client's, because resolving a ChatGPT bearer through Anthropic's profile endpoint would hand that
token to Anthropic.

`git config core.hooksPath .githooks` turns on a pre-commit secret scan and the other gates
listed in [CONTRIBUTING.md](CONTRIBUTING.md#git-hooks). Treat it as a backstop: it only sees what
you stage, and `--no-verify` exists.

**Peers are opt-in twice and rate-limited.** Nothing is announced until `find` is on, and nothing
is served until `share` is on. A Mac that wants to pair knocks with temporary keys only and waits
in a list until you accept it. Knocks are capped per address (one every 10 s, a burst of 3, two
unauthenticated connections at once), and the list holds eight. An address you ignore is muted.
One you block is banned by address and, once its key is known, by key. A lender sees the plaintext
of the requests it serves, by design, and lends at most half of any window.
[`docs/peers.md`](docs/peers.md#what-leaves-this-mac) has the full table of what each step sends.

Found a security issue? Please report it privately through GitHub's security advisories rather
than in a public issue.

## It does not phone home

`tcr` has no telemetry and no crash reporting. Your config, logs, session pins and OAuth tokens are
files on your own disk. It makes two kinds of outbound call: to Anthropic on your behalf (and,
for Codex accounts, to OpenAI at `chatgpt.com` and `auth.openai.com`), and to GitHub to check for
a newer version.

The cost falls on whoever hits a bug. There is no error stream to search and no session to replay,
so a good bug report is worth more here than in a project that watches its users. #323 was
diagnosed from a screenshot and four log lines someone pasted.

Instead of watching you, it signs things. The app is codesigned with a Developer ID certificate,
notarized by Apple and stapled, and every update is signed again with a Sparkle EdDSA key that your
installed copy checks before it runs anything. Counting them all, there are seven signing secrets
for the release, all deliberately removed from this repository on 2026-08-09 and now kept on one
Mac and in one 1Password item. On top of those come a local CA on your machine that makes a fresh
leaf certificate every time the proxy starts, your per-account OAuth tokens, and the proxy's own
API key.

That is a lot of keys for a tool that rotates Claude accounts. The reasoning is in
[docs/RELEASING.md](docs/RELEASING.md): this repository is public, collaborators have push access,
and Sparkle's private key does more than protect a build artifact. It decides what every copy
already installed will run. A key that is not stored here cannot be stolen from here.

## License

Licensed under PolyForm Noncommercial 1.0.0; see [`LICENSE`](LICENSE).
