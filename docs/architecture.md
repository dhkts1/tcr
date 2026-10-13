# How `tcr` works

The request path, the two entry modes, how a Codex request travels, how an account gets
picked, and how quota is kept fresh. Every tunable named here has its type, default and
source citation in [`configuration.md`](configuration.md); this document describes the
mechanism rather than the values.

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
Anthropic's own API hosts, and for `chatgpt.com` while the pool holds a Codex account (see
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
trusts tcr's leaf through `CODEX_CA_CERTIFICATE`; `tcr run --codex` sets both
([`cli.md`](cli.md#--codex-launching-codex-through-the-pool)). A request's provider is
decided by its path: everything under `/backend-api/` is Codex, everything else is Anthropic,
and a request tcr pools is served only by an account of its own provider. An intercepted host
has to agree: a request for `chatgpt.com` outside `/backend-api/`, or for `api.anthropic.com`
or `platform.claude.com` under it, is answered 421, naming the host and the path, and nothing
is forwarded. Inside a tunnel the host is the `CONNECT` target: a request that declares any
other host, a loopback one included, or a `Host` that cannot be read, is answered 421 too and
forwarded nowhere, so a request for one provider's host never reaches the other's upstream.
Only a base-URL request, which comes through no tunnel, is judged by the host it names, and one
naming a loopback host, or none, keeps its path's provider.

tcr terminates `chatgpt.com` only while the pool holds a Codex account, counted again on
every `CONNECT`. With none it tunnels `chatgpt.com` untouched, and `codex` runs on its own
login, as it does whenever `chatgpt.com` is in its `NO_PROXY`. A login whose ChatGPT
workspace is FedRAMP, or routed to a data residency or to a backend of its own, is neither
pooled nor importable: `tcr login --from-codex` refuses it, another backend is tunnelled
untouched, and a request that names such a routing (`x-openai-account-routing-override` or
`x-openai-fedramp`) is relayed on the client's own credential, inference included, and never
pooled.

```
codex   HTTPS_PROXY=http://127.0.0.1:<port>   CODEX_CA_CERTIFICATE=~/.config/tcr-ca.pem
  |
  |-- CONNECT chatgpt.com:443         on the allowlist, covered by the leaf, and a Codex
  |                                   account in the pool: TLS terminated (else a blind tunnel)
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
  serving account's token, and the header with that account's id, its `accountUuid`. It
  inserts the header when the client sent none, and drops it when the row has no id.
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

**When no Codex account can serve,** the 429 is shaped for `codex` rather than for Claude Code,
on the principle the Claude 429 follows: a quota is the user's limit, and a hold is not. When
the Codex account that frees soonest is out on a usage limit, a five-hour or weekly window or a
refusal from the backend, or none reports when it frees and every one is out on such a limit, it
is `usage_limit_reached`, which `codex` shows as a usage limit and on which it ends the turn.
Its `resets_in_seconds` is the wait until the soonest Codex account frees, uncapped, beside the
capped `retry-after` that [When every account is out](#when-every-account-is-out) describes.
When some Codex account reports when it frees, `resets_at` is the instant that wait ends, in
Unix seconds, and `codex` names it as the time to try again; when none does, the wait is a flat
60 seconds that nothing measured, so there is no `resets_at`, and `codex` says to try again
later. When they are out for anything else, a hold on an upstream `retry-after`, a dead login,
or an account disabled, parked or reserved, it is `pool_unavailable`: the same
`resets_in_seconds` and capped `retry-after`, never a `resets_at`, and `codex` retries it after
that `retry-after`, up to five times, instead of ending the turn. With no Codex account in the
config at all it is `no_codex_account`, whose message names `tcr login --from-codex`, and it
carries no `retry-after`, because waiting changes nothing; tcr intercepts no `chatgpt.com`
tunnel then, so a `codex` meets it only on a tunnel opened while the pool still held an account,
or by sending to the listener directly. `codex` shows a 429 of that type only as a generic
retry-limit error, without the message; tcr's log says that no Codex account is configured. A
Claude request never spills onto a Codex account, nor a Codex request onto a Claude one.

## Account selection

Account selection is not one sort. On the normal path, `tcr` picks the eligible account
minimising `(priority ascending, least-recently-selected, soonest weekly reset)`, so
`priority` is a hard tier and rotation breaks ties within it. Eligibility means: not
disabled, not in an error state, not under a live rate-limit hold, under the switch
threshold, and not blocked for this request's model class. Four things change that picture:
`lockAccount` short-circuits every pooled request of its account's provider to that account
(the other provider's pooled requests get a 429), an honoured session-affinity pin is served
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
