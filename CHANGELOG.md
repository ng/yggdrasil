# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1](https://github.com/ng/yggdrasil/compare/v0.1.0...v0.1.1) - 2026-07-06

### Other

- Stop the dashboard-spawned watcher from corrupting the TUI ([#120](https://github.com/ng/yggdrasil/pull/120))
- Track worktree teardown separately from window reaping
- Drain cleaned workers from list_cleanable with a one-shot reaped flag
- Make worker reaping self-healing and safe to run anywhere
- Keep the TUI refresh cascade off the input thread
- Default lock acquire/release agent to YGG_AGENT_NAME
- Anchor repo identity to origin URL, not local directory name ([#117](https://github.com/ng/yggdrasil/pull/117))
- Fallback when release bot credentials are unset
- Use release bot token for release-plz
- Cache tmux availability probe
- Address maintenance cleanup review feedback
- Harden maintenance cleanup tmux evidence
# Changelog

All notable changes to **Yggdrasil** will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

<!-- release-plz prepends new versions below this line. -->
