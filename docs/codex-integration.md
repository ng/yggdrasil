# Codex CLI Integration

Yggdrasil's Codex integration should stay hook-first. The Rust binary remains
the integration point; Codex-specific work belongs at the CLI boundary where
hook payloads, install locations, and display surfaces differ from Claude Code.

## MVP Surfaces

1. Keep `ygg status`, `ygg prime`, and hook-injected context as the primary
   Yggdrasil status surfaces.
2. Use Codex's built-in footer status line for Codex-native session details.
3. Use `ygg status --format codex` when a compact Yggdrasil line is useful in
   Codex docs, prompts, hooks, or terminal panes.

## Codex Footer Sample

Codex currently exposes footer items through `[tui].status_line` in
`$CODEX_HOME/config.toml` or project `.codex/config.toml`. It is an ordered list
of built-in item IDs, not an external status command hook.

```toml
[tui]
status_line = ["model-with-reasoning", "context-remaining", "git-branch", "current-dir"]
```

That keeps model, context, branch, and directory visible inside Codex while
leaving Yggdrasil's shared coordination state to `ygg`.

## Compact Yggdrasil Status

For a Codex-friendly one-line summary:

```bash
ygg status --format codex
```

Example:

```text
ygg agents 12/18 active | locks 3 | tasks 4 ready, 9 open, 2 in_progress, 1 blocked | workers 2/3 running
```

For one agent:

```bash
ygg status --agent yggdrasil --format codex
```

Example:

```text
ygg yggdrasil waiting_tool | locks 1 | pressure 48231 tok | workers 2/3 running | updated 22:14:07
```

The compact format is intentionally read-only and lossy. Use plain
`ygg status`, `ygg dashboard`, or `ygg logs --follow` when investigating
details.

## Hook Plan

Codex supports command hooks for `SessionStart`, `UserPromptSubmit`,
`PreToolUse`, `PreCompact`, and `Stop`, which map to Yggdrasil's current Claude
hook responsibilities. The next implementation step is a provider adapter in
`ygg hook`:

- parse Claude and Codex hook payloads into a normalized internal shape;
- keep current `ygg hook session-start` aliases for Claude compatibility;
- add `ygg hook --provider codex <event>` commands for Codex installs;
- install Codex hooks through `~/.codex/hooks.json` or project
  `.codex/hooks.json`;
- verify Codex hook stdout/blocking payload behavior with a local probe before
  enabling lock and stop-check enforcement by default.
