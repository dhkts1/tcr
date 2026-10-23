---
name: tcr
description: "Run OpenClaw on tcr, a local proxy that pools your own Claude and ChatGPT subscriptions: check the pool, start the Gateway on it, and confirm turns go through it."
homepage: https://dhkts1.github.io/
disable-model-invocation: true
metadata:
  {
    "openclaw":
      {
        "emoji": "🔀",
        "os": ["darwin", "linux"],
        "requires": { "bins": ["tcr", "openclaw"] },
      },
  }
---

# tcr

[tcr](https://github.com/dhkts1/tcr) is a local proxy that pools your own Claude and ChatGPT
subscriptions. For each request it picks the account with quota left, and it keeps a conversation
on the account whose prompt cache is already warm. This skill starts OpenClaw on that pool. Claude
turns go through the real `claude` binary and ChatGPT turns through tcr's Codex accounts, with no
change to OpenClaw.

Use it for your own subscriptions on your own machine. Anthropic's terms allow the unmodified
`claude` binary on your own subscription and do not allow routing other people's requests through
plan credentials.

## What tcr sees

Read this before you route anything through tcr.

- **tcr runs on your machine and listens on `127.0.0.1` only.**
- **It decrypts the Claude and ChatGPT traffic it pools.** To pick an account and put that
  account's credential on a request, tcr terminates TLS for `api.anthropic.com`, and for
  `chatgpt.com` on connections marked `tcr-codex`, using its own CA. The launcher below makes
  OpenClaw trust that CA, so tcr can read those requests and replies, prompts included. Traffic to
  every other host passes through tcr's tunnel still encrypted.
- **It keeps on disk:**
  - your account tokens, in its config (`~/.config/tcr.json`);
  - a usage ledger of token counts, model and cost per request, without prompt text, for 90 days;
  - a list of recent sessions with up to 120 characters of each slow or running tool command;
  - a server log that names accounts by email (`~/.cache/tcr/`).
- **It sends requests only to Anthropic and OpenAI**, unless you turn on tcr's peer sharing with
  your other Macs, on your network or over your tailnet, which is off by default.

## What this skill runs

It runs only when you invoke it (`/tcr`); the agent does not load it on its own. It uses the `tcr`
and `openclaw` commands shown here and its two scripts. Ask the user before installing tcr, before
editing `openclaw.json`, and before starting the Gateway through the launcher.

## Install tcr

Download a release archive and its checksum from the
[release page](https://github.com/dhkts1/tcr/releases/latest), check the archive, then extract it.
This example uses Apple silicon. The archives are `tcr-aarch64-apple-darwin`,
`tcr-x86_64-apple-darwin`, `tcr-aarch64-unknown-linux-musl` and `tcr-x86_64-unknown-linux-musl`.

```bash
a=tcr-aarch64-apple-darwin.tar.xz
curl -LO "https://github.com/dhkts1/tcr/releases/latest/download/$a"
curl -LO "https://github.com/dhkts1/tcr/releases/latest/download/$a.sha256"
shasum -a 256 -c "$a.sha256"         # expect "OK"; on Linux: sha256sum -c "$a.sha256"
tar -xf "$a"
mv tcr-aarch64-apple-darwin/tcr ~/.local/bin/tcr   # any directory on your PATH
```

The checksum catches a corrupted or substituted download. It can't catch a tampered release,
because tcr's CLI releases publish no signatures. Then add accounts and start the proxy:

```bash
tcr login            # add each Claude account
tcr login --codex    # add each ChatGPT account
tcr server           # or open TcrBar on macOS, which runs it for you
```

On macOS, TcrBar is the menu-bar app that runs the proxy and shows every account. Get it from
[the website](https://dhkts1.github.io/) or as
[`TcrBar.dmg`](https://github.com/dhkts1/tcr/releases/latest/download/TcrBar.dmg), signed and
notarized by Apple. It runs its own copy of tcr, so this skill still needs the CLI above on your
PATH.

## Check the pool

```bash
tcr status
```

Lists every account with what it has left in each quota window. If the proxy is not running,
start it with `tcr server`.

## Configure OpenClaw

Run Anthropic models on the `claude-cli` runtime, which starts `claude` itself, and allow the
OpenAI models, in `openclaw.json`:

```json
{
  "agents": {
    "defaults": {
      "models": {
        "anthropic/*": { "agentRuntime": { "id": "claude-cli" } },
        "openai/*": {}
      }
    }
  }
}
```

OpenClaw still needs one ChatGPT sign-in of its own before a ChatGPT turn
(`openclaw models auth login --provider openai`). tcr then serves the request from a pool account.

## Start the Gateway on the pool

```bash
{baseDir}/scripts/openclaw-on-tcr gateway run
```

The launcher prints what it is about to route, then sets four variables and runs `openclaw` with
your arguments:

- `HTTPS_PROXY` points at tcr with the `tcr-codex` marker and tcr's request key. The request key
  only tells your own tcr that it launched this process; it is not an account token. OpenClaw
  passes the variable to every `claude` it starts, and the marker opts `chatgpt.com` into pooling.
- `NODE_EXTRA_CA_CERTS` trusts tcr's CA (`~/.config/tcr-ca.pem`).
- `NODE_USE_ENV_PROXY=1` sends OpenClaw's own `fetch` calls, including ChatGPT turns, through the
  same proxy (Node 24 and later).

These have to be in place when the Gateway starts. Node reads `NODE_USE_ENV_PROXY` at startup, so
`env.vars` in `openclaw.json` is too late for the ChatGPT side. Set `TCR_PORT` if tcr listens
somewhere other than 3456.

## Confirm it worked

```bash
{baseDir}/scripts/check
```

Sends one Claude turn and one ChatGPT turn through the running Gateway, then reads tcr's own
records. It prints the tcr session that served the Claude turn and how far the Codex accounts'
request count moved, and exits non-zero if tcr did not serve both.

## Things to know

- The proxy covers the whole OpenClaw process, not only its model calls. A request tcr does not
  pool goes through its tunnel unchanged, on OpenClaw's own credential.
- OpenClaw's memory feature calls OpenAI's paid embeddings API, which a ChatGPT subscription does
  not cover. Give memory its own embedding provider or turn it off.
- Use a model ChatGPT accounts accept. `openai/gpt-5.5` is refused on a ChatGPT account; use a
  model your `codex` CLI runs.

More detail: https://github.com/dhkts1/tcr/blob/main/docs/openclaw.md
