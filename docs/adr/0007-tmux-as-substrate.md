# 0007 — tmux as the multi-agent display substrate

- **Status**: Accepted
- **Date**: 2026-04-17

## Context

Yggdrasil orchestrates multiple concurrent coding agents. A human supervising them needs to be able to:

- see each agent's live output,
- jump between them without losing session state,
- attach from a fresh terminal after a machine sleep or SSH drop,
- have Yggdrasil *launch* new agents into visible, scrollable windows (not into headless background processes nobody can watch).

The choice of display substrate is load-bearing — `ygg spawn` isn't useful if a spawned agent ends up in a process nobody can observe.

## Decision

**tmux** is the display substrate. `ygg up` opens (or attaches to) a `ygg` tmux session with a dashboard pane; `ygg spawn` opens a new window in that session running a Claude Code agent with the appropriate `YGG_AGENT_NAME` environment variable set. `src/tmux.rs` wraps the subset of `tmux` commands Yggdrasil uses (`new-session`, `new-window`, `send-keys`, `list-windows`).

### Alternatives considered

- **Zellij**. Modern, pleasant config format, strong layouts. But smaller ecosystem, less universal, and the scripting surface (`zellij action`) is less stable than `tmux` for programmatic window management. Users who have Zellij usually also have tmux; the reverse is less true.
- **GNU Screen**. Universal, but the command surface for programmatic control is awkward and the UX is dated. Most of our target users don't run it.
- **A bespoke terminal multiplexer / TUI window manager**. Rewriting tmux is not our job. Would be a massive time sink to reach feature parity on attach/detach, scrollback, and mouse handling.
- **Separate OS terminal windows per agent**. Nonportable (macOS Terminal / iTerm2 / Alacritty / Kitty / Wezterm all have different scripting stories), can't survive an SSH session, and the window-management burden falls on the user.
- **Headless background processes with `ygg logs --follow`**. Works for *telemetry* but loses the direct terminal-over-agent interaction that Claude Code depends on. An agent stuck on a prompt with no attached TTY is a dead agent.

We chose tmux for a specific, pragmatic reason: **Claude Code runs in tmux for both the author and for Claude Code itself when it spawns sub-agents in integrated IDE workflows.** The substrate was already there. Building on tmux composes with how people already work; building on anything else fights it.

## Consequences

**Positive**

- `ygg spawn` produces a *visible* window the human can switch to, scroll through, and type into. Claude Code runs correctly under tmux (it already does so for the author's normal workflow).
- SSH-friendly. Detach, reconnect, everything is still there.
- Programmatic control is well-documented and stable: `tmux new-window`, `tmux send-keys`, `tmux list-windows -F`.
- `src/tmux.rs` is a thin wrapper — easy to extend, easy to mock in tests.
- Status line integration via the existing `tmux status-right` gives us a cheap place to surface agent state.

**Negative**

- Users who don't have tmux must install it. We document this in `ygg init`.
- Windows-native users are poorly served; WSL is the escape hatch today.
- tmux's config surface is famously baroque — first-time users attaching to a `ygg` session can get lost. We ship a minimal `ygg.conf` include to make the dashboard navigable.
- Programmatic `send-keys` has edge cases with special characters; we sanitize agent task strings before injection.

**Future triggers to revisit**

- If a significant user base runs Zellij, add a Zellij backend behind the same `src/tmux.rs` interface.
- If Claude Code gains a first-class sub-agent display API that's richer than tmux-over-stdout, adopt it.

## References

- [tmux](https://github.com/tmux/tmux) — the multiplexer.
- [Zellij](https://zellij.dev/) — the primary alternative we evaluated.
- [ratatui](https://github.com/ratatui-org/ratatui) — the TUI framework behind our dashboard, which runs inside the tmux pane.
