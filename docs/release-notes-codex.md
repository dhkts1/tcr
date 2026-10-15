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

- A Codex quota or rate-limit error inside a successful SSE stream holds its serving account
  so the next pinned turn can rotate. Disabled and otherwise unusable rows cannot hide a spent
  pool's real reset. Other stream errors remain observational.
- Claude launches preserve Codex CA settings. Codex launches preserve the complete selected
  CA bundle, including corporate roots from `SSL_CERT_FILE`. A bundle missing tcr's CA causes
  a direct launch on Codex's own login with an explanation. Shell tools get an unmarked proxy,
  without changing Codex's config file. Doctor recognizes combined bundles and reports
  keyring-backed sign-in as unchecked when no auth file exists.
- Imported logins can recover a newer refresh token even when its access token has expired.
  Failed write-backs retry with backoff while the original source token still matches.
  Pending publication survives restart and appears as `token_sync=pending` in text status,
  `tokenSyncPending` in JSON, and help on the account's TcrBar state pill. New imports refuse symlinked auth files; a symlink introduced
  after import is preserved. A later `codex login`
  in the shared folder revokes that login and may require tcr to sign in again.
- The v1 quota endpoint remains Claude-only. Provider-aware status includes Codex, and both
  status and doctor recognize a configured Codex fleet behind an older proxy.
- TcrBar correctly shows body-only refusals, partially priced totals and unreadable sessions;
  sign-in messages identify the selected tcr executable when available.

- Token refresh and import workspace checks send tcr's own User-Agent. An expired import
  asks the user to run codex once to refresh its login before trying again, rather than
  immediately replacing that login with another browser sign-in.
