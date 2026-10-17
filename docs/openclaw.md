# Using tcr with OpenClaw

[OpenClaw](https://github.com/openclaw/openclaw) can run on a tcr pool with no change to OpenClaw
or to tcr. You start OpenClaw with one proxy address and tcr's CA, and both kinds of turn reach the
pool: Claude turns through the real `claude` binary OpenClaw runs, and ChatGPT turns from
OpenClaw's own HTTP client. Tested on 2026-10-17 with OpenClaw 2026.10.4 and tcr 1.2.10.

This is for one person pooling their own subscriptions. Anthropic's terms allow the unmodified
Claude Code binary on your own subscription and do not allow routing other people's requests
through plan credentials
([Claude Code legal and compliance](https://code.claude.com/docs/en/legal-and-compliance)).

## Config

In `openclaw.json`, run Anthropic models on OpenClaw's `claude-cli` runtime, which starts `claude`
itself, and allow the OpenAI models:

```json
{
  "agents": {
    "defaults": {
      "model": "anthropic/claude-opus-5-5",
      "models": {
        "anthropic/*": { "agentRuntime": { "id": "claude-cli" } },
        "openai/*": {}
      }
    }
  }
}
```

OpenClaw also wants one ChatGPT sign-in of its own before it starts a ChatGPT turn
(`openclaw models auth login --provider openai`). tcr then serves each request from a Codex
account in the pool ([Codex requests](architecture.md#codex-requests)).

## Starting it

Start the Gateway, or any `openclaw` command, with these four variables:

```sh
export NODE_USE_ENV_PROXY=1
export HTTPS_PROXY="http://tcr-codex:$(tcr request-key)@127.0.0.1:3456"
export https_proxy="$HTTPS_PROXY"
export NODE_EXTRA_CA_CERTS="$HOME/.config/tcr-ca.pem"
openclaw gateway run
```

Each variable does one job:

- `HTTPS_PROXY` and `NODE_EXTRA_CA_CERTS` pass through to every `claude` OpenClaw starts, so Claude
  turns reach tcr the way a `tcr run` launch does.
- `NODE_USE_ENV_PROXY=1` makes Node 24 and later send OpenClaw's own `fetch` calls through the
  same proxy, which is how ChatGPT turns reach tcr.
- The `tcr-codex` user in the address is the marker that opts `chatgpt.com` into pooling
  ([the request key on a launch](cli.md#the-request-key-on-a-launch)). Claude traffic under the
  same marker is pooled as Claude.

The port is 3456, tcr's default, and the CA is the one tcr keeps at `~/.config/tcr-ca.pem`.

## As an OpenClaw skill

[`integrations/openclaw/tcr`](../integrations/openclaw/tcr/SKILL.md) packages the steps above as
an OpenClaw skill. It has two scripts:

- `scripts/openclaw-on-tcr` sets the four variables and runs `openclaw` with your arguments, for
  example `openclaw-on-tcr gateway run`. It refuses to start when tcr is missing, not running, or
  has no CA yet.
- `scripts/check` sends one Claude turn and one ChatGPT turn through the running Gateway. It then
  confirms in tcr's own records that tcr served both, and exits non-zero when it did not.

## What the test showed

One Gateway process ran three Claude turns and three ChatGPT turns, alternating:

- **Claude:** all three answered. tcr showed one session with three requests, and its cache reads
  equal the ones OpenClaw reported for turns 2 and 3. OpenClaw keeps one `claude` process per
  conversation with a stable session id, so tcr keeps the conversation on one account and its
  prompt cache stays warm.
- **ChatGPT:** all three answered on `openai/gpt-6.1-sol`, and the pool's Codex account count rose
  by three.

## Things to know

- **The proxy covers the whole OpenClaw process.** All of OpenClaw's own HTTPS goes through tcr,
  not only its model calls. A request tcr does not pool goes through its tunnel unchanged on
  OpenClaw's own credential.
- **OpenClaw's memory feature calls OpenAI's paid embeddings API**, which a ChatGPT subscription
  does not cover. In the test it failed with "You have no credits remaining" without affecting the
  turns. Give memory its own embedding provider or turn it off.
- **Pick a model ChatGPT accounts accept.** `openai/gpt-5.5` is refused on a ChatGPT account ("not
  supported when using Codex with a ChatGPT account"); use a model your `codex` CLI runs.
- **Opus 5.5 takes no `--thinking off`.** Use `low` or higher.
