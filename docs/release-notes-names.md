# Pending release notes: the `tcr` file names

Copy this section into the GitHub release body's `## What's new` section for the first
release that ships `tcr migrate`; that release body is the changelog shown by TcrBar.

## What's new

- Updating now moves an older install's files to the `tcr` names: `~/.config/teamclaude.json`
  becomes `~/.config/tcr.json`, and `~/.cache/teamclaude/` becomes `~/.cache/tcr/`, logs
  included. TcrBar does it on its first launch of the new version, before it starts the proxy;
  `tcr update` does it after a successful install. If a proxy is running the files stay where
  they are, and `tcr migrate` moves them once it is stopped. Anything that reads
  `~/.config/teamclaude.json` by name should read `~/.config/tcr.json` instead.
