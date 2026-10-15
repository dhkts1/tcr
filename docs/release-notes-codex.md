# Pending Codex release notes

Copy this section into the GitHub release body's `## What's new` section when the Codex
changes ship; that release body is the changelog shown by TcrBar.

## What's new

- Codex pooling uses marked connections from `tcr run --codex`. Other clients' `chatgpt.com`
  connections pass through with their TLS untouched.
- Installs whose generated certificate chain predates `tcr-ca.key` get a new CA on upgrade.
  Running `claude` sessions that loaded the old CA fail with `CERT_SIGNATURE_FAILURE` until
  they restart. Restart them with `tcr run` to load the new CA. Installs with a persisted CA
  key keep their existing CA when the leaf is renewed.
- Peer gateways continue to carry Anthropic's two hosts; Codex stays off the mesh.
- Imported Codex logins keep refreshed tokens in sync through guarded `auth.json` updates.
  tcr adopts newer tokens for the same ChatGPT account and leaves another login untouched.
