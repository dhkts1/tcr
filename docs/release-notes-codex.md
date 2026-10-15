# Pending Codex release notes

Copy this section into the GitHub release body's `## What's new` section when the Codex
changes ship; that release body is the changelog shown by TcrBar.

## What's new

- Codex pooling uses marked connections from `tcr run --codex`. Other clients' `chatgpt.com`
  connections pass through with their TLS untouched.
- Installs whose generated certificate chain predates `tcr-ca.key` get a new CA at the next
  proxy server startup after upgrading. Running `claude` sessions that loaded the old CA fail
  with `CERT_SIGNATURE_FAILURE` until they restart. Restart them with `tcr run` to load the new
  CA. Installs with a persisted CA key keep their existing CA when the leaf is renewed.
  `tcr run` only reads existing certificates and never rotates a running proxy's CA.
- Account locks bind only their own provider, and each provider has an independent fleet
  throttle. A Claude lock or burst does not disable or pace Codex, or the other way round.
- Peer gateways continue to carry Anthropic's two hosts; Codex stays off the mesh.
- Imported Codex logins share refreshed tokens through guarded `auth.json` updates. tcr
  serializes its writers and checks the complete file before replacement, but Codex does not
  take that lock: a simultaneous Codex write can still race the replacement. A separate
  `tcr login --codex` login avoids sharing. After a rejected refresh, tcr can recover a changed,
  unexpired pair for the same ChatGPT account even when token timestamps are ambiguous;
  known older tokens and different accounts remain rejected.
