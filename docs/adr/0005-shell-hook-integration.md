# 0005 — Shell hooks over MCP server or Agent SDK plugin

- **Status**: Accepted
- **Date**: 2026-04-15

## Context

Yggdrasil needs to be *in the loop* of every Claude Code session — capturing prompts, injecting context, digesting transcripts, watching tool calls. There are three supported ways to do that:

1. **Claude Code shell hooks** — `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `Stop`, `PreCompact`. The harness shells out to a configured command at each lifecycle event; the command's stdout is injected into the conversation context.
2. **An MCP server** — a long-running process the harness speaks to over stdio/HTTP, exposing tools and resources the agent can call.
3. **The Claude Agent SDK** — build a bespoke agent binary on top of the SDK that embeds Yggdrasil's logic directly.

## Decision

Yggdrasil integrates via **shell hooks**, calling `ygg` subcommands at each Claude Code lifecycle event. Hooks live in `~/.claude/ygg-hooks/` (installed by `ygg init`) and are wired through `~/.claude/settings.json`.

### Alternatives considered

- **MCP server**. Would work — we could expose `ygg.inject`, `ygg.lock`, etc. as MCP tools. But MCP tools are *agent-invoked*: the model chooses when to call them. We need unconditional behavior (every prompt gets memory-injected; every tool call gets observed; every session ends with a digest). That's what hooks are for; MCP is the wrong fit.
- **Agent SDK plugin / custom harness**. Most control, least compatibility. Users already run Claude Code; asking them to switch to a bespoke CLI to get Yggdrasil is a non-starter. Hooks let us compose with Claude Code instead of replacing it.
- **Wrap Claude Code with a parent process that intercepts stdio**. Fragile — breaks on every harness change — and reproduces what hooks already give us for free.

## Consequences

**Positive**

- Composability: runs alongside whatever else the user has configured. No exclusive harness.
- Lifecycle coverage: the five hook types cover every moment Yggdrasil cares about (session boundaries, every prompt, every tool call, compaction, session close).
- Deterministic invocation: hooks fire on every event, not at the model's discretion.
- Low-friction install: `ygg init` writes hook scripts and edits `settings.json`.
- Language-agnostic: the hook contract is just stdin JSON + stdout markdown, so future Yggdrasil rewrites stay compatible.

**Negative**

- Hooks are user-global (`~/.claude/settings.json`), so they fire in every directory — we short-circuit when no Postgres is reachable, which adds a startup cost per session. Currently ~100ms, acceptable.
- Stdout-as-context has a size ceiling; large `ygg prime` outputs eat the user's context budget. We compensate by making `prime` concise and `inject` selective.
- Hooks don't have a bidirectional tool-call story — they can only emit markdown to the conversation. For genuinely *agent-invoked* features (e.g. "check Yggdrasil's state before deciding"), we'd need MCP. None of our current features want that shape.

**Future triggers to revisit**

- If we want Yggdrasil to be queryable mid-turn by the agent itself (not just pre-injected), add a small MCP server *alongside* the hooks — they don't conflict.
- If Claude Code's hook contract changes, we regenerate the scripts in `ygg init`; the core CLI logic is independent.

## References

- [Claude Code hooks documentation](https://docs.anthropic.com/en/docs/claude-code/hooks) — the integration surface Yggdrasil uses.
- [Model Context Protocol (MCP)](https://modelcontextprotocol.io/) — the agent-invoked tool protocol we considered and rejected for this use case.
- [Claude Agent SDK](https://docs.anthropic.com/en/docs/claude-code/sdk) — the bespoke-harness alternative, rejected for composability reasons.
- [beads](https://github.com/steveyegge/beads) — the prior-art example of this hook-based integration pattern.
