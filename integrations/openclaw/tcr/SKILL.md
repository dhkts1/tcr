---
name: tcr
description: "Run OpenClaw on tcr, a local proxy that pools your own Claude and ChatGPT subscriptions: check the pool, start the Gateway on it, and confirm turns go through it."
homepage: https://github.com/dhkts1/tcr
metadata:
  {
    "openclaw":
      {
        "emoji": "🔀",
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

## Install tcr

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/dhkts1/tcr/releases/latest/download/tcr-installer.sh | sh
tcr login            # add each Claude account
tcr login --codex    # add each ChatGPT account
tcr server           # or open TcrBar on macOS, which runs it for you
```

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

The launcher sets four variables and runs `openclaw` with your arguments:

- `HTTPS_PROXY` points at tcr with the `tcr-codex` marker and tcr's request key. OpenClaw passes
  it to every `claude` it starts. The marker also opts `chatgpt.com` into pooling.
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
request count moved.

## Things to know

- The proxy covers the whole OpenClaw process, not only its model calls. A request tcr does not
  pool goes through its tunnel unchanged, on OpenClaw's own credential.
- OpenClaw's memory feature calls OpenAI's paid embeddings API, which a ChatGPT subscription does
  not cover. Give memory its own embedding provider or turn it off.
- Use a model ChatGPT accounts accept. `openai/gpt-5.5` is refused on a ChatGPT account; use a
  model your `codex` CLI runs.

More detail: https://github.com/dhkts1/tcr/blob/main/docs/openclaw.md
