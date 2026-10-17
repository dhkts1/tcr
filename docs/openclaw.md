# Using tcr with OpenClaw

[OpenClaw](https://github.com/openclaw/openclaw) can run on a tcr pool with no change to OpenClaw
or to tcr. You start OpenClaw with the proxy settings tcr would give its own launches, and OpenClaw
passes them on: to the real `claude` binary for Claude turns, and to its own HTTP client for
ChatGPT turns. Tested on 2026-10-17 with OpenClaw 2026.10.4 and tcr 1.2.10.

This is for one person pooling their own subscriptions. Anthropic's terms allow the unmodified
Claude Code binary on your own subscription and do not allow routing other people's requests
through plan credentials
([Claude Code legal and compliance](https://code.claude.com/docs/en/legal-and-compliance)).

Both examples use port 3456, tcr's default, and the CA tcr keeps at `~/.config/tcr-ca.pem`. The
addresses are the ones in [the request key on a launch](cli.md#the-request-key-on-a-launch).

## Claude turns

OpenClaw's `claude-cli` runtime runs `claude` itself. Select it for Anthropic models in
`openclaw.json`:

```json
{
  "agents": {
    "defaults": {
      "model": "anthropic/claude-opus-5-5",
      "models": { "anthropic/*": { "agentRuntime": { "id": "claude-cli" } } }
    }
  }
}
```

Then start OpenClaw with the pair `tcr run` gives `claude`. OpenClaw passes `HTTPS_PROXY` and
`NODE_EXTRA_CA_CERTS` through to every `claude` it starts:

```sh
export HTTPS_PROXY="http://tcr:$(tcr request-key)@127.0.0.1:3456"
export https_proxy="$HTTPS_PROXY"
export NODE_EXTRA_CA_CERTS="$HOME/.config/tcr-ca.pem"
openclaw agent --local --message "hello"
```

OpenClaw keeps one `claude` process per conversation and gives it a stable session id, so tcr pins
the conversation to one account and its prompt cache stays warm. In the test, three turns of one
conversation showed up in tcr as one session with three requests, with cache reads on the second
and third turns.

## ChatGPT turns on Codex accounts

OpenClaw's built-in runtime calls ChatGPT's Codex endpoint from Node. Node 24 and later send
`fetch` through `HTTPS_PROXY` when `NODE_USE_ENV_PROXY=1` is set. Point it at tcr with the
`tcr-codex` marker, the address `tcr run --codex` gives `codex`:

```sh
export NODE_USE_ENV_PROXY=1
export HTTPS_PROXY="http://tcr-codex:$(tcr request-key)@127.0.0.1:3456"
export https_proxy="$HTTPS_PROXY"
export NODE_EXTRA_CA_CERTS="$HOME/.config/tcr-ca.pem"
openclaw agent --local --model openai/gpt-6.1-sol --message "hello"
```

Allow the OpenAI models next to the Anthropic ones, as `"openai/*": {}` under
`agents.defaults.models`. OpenClaw still wants one ChatGPT sign-in of its own before it starts a
turn (`openclaw models auth login --provider openai`); tcr serves the request from a Codex account
in the pool ([Codex requests](architecture.md#codex-requests)). In the test, three turns raised the
pool account's request count from 0 to 3.

## Things to know

- **The settings cover the whole OpenClaw process.** With `NODE_USE_ENV_PROXY=1`, all of OpenClaw's
  own HTTPS goes through tcr, not only its model calls.
- **One marker per process.** `HTTPS_PROXY` carries either `tcr` (Claude) or `tcr-codex` (Codex).
  The two examples above ran as separate processes; one process serving both was not tested.
- **Pick a model ChatGPT accounts accept.** `openai/gpt-5.5` is refused on a ChatGPT account ("not
  supported when using Codex with a ChatGPT account"); use a model your `codex` CLI runs.
- **Opus 5.5 takes no `--thinking off`.** Use `low` or higher.
