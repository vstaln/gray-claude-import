<p align="center">
  <img src="assets/gray-logo.svg" alt="gray" width="96">
  <img src="assets/claude.svg" alt="claude" width="96">
</p>
<h1 align="center">gray-claude-import</h1>
<p align="center">Import Claude Code plugins — commands, agents, skills, MCP, hooks — into gray.</p>
<p align="center">
  <a href="https://github.com/vstaln/gray-claude-import/blob/main/LICENSE"><img alt="MIT License" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
  <img alt="gray plugin" src="https://img.shields.io/badge/gray-plugin-7aa2f7.svg">
  <img alt="rust" src="https://img.shields.io/badge/built%20with-rust-orange.svg">
</p>

Import Claude Code plugins (commands, agents, skills, MCP, hooks) into gray

A sidecar plugin for [gray](https://github.com/vstaln/gray), scaffolded by
[gray-account](https://github.com/vstaln/gray-account).

## What it does

`claude_import` (or `/cc-import <path>`) takes a Claude Code plugin directory —
a local path or a GitHub URL (`https://github.com/owner/repo[/tree/branch/subdir]`,
shallow-cloned into `~/.gray/claude-import/.cache/`) — and maps its artifacts
onto gray:

| Claude artifact | Becomes |
|---|---|
| `commands/*.md` | `~/.gray/claude-import/<plugin>/commands/` — run via `/cc <plugin> <cmd> [args]` (returns the file as a `{prompt}`; `$ARGUMENTS` is substituted). Slash commands can't be registered at runtime, so one dispatcher fronts them all. |
| `agents/*.md` | `~/.gray/subagents/agents/cc-<plugin>-<agent>.md` — direct gray-subagents profiles. |
| `skills/<s>/` | `~/.gray/skills/cc-<plugin>-<s>/` (skipped when no `SKILL.md`). |
| `.mcp.json` | printed as a merge snippet for `~/.gray/mcp.json`. |
| `hooks/hooks.json` | translated to `~/.gray/claude-import/<plugin>/hooks.json`, a bridge config this sidecar executes (see below). |

The import report lists every artifact as imported / bridged / skipped + why.

## Hook bridge

The sidecar claims `tool/before`, `tool/after` and `prompt/context` and replays
imported hook commands with claude-style stdin JSON
(`{hook_event_name, tool_name, tool_input, session_id, cwd}`, plus
`tool_response` for PostToolUse) and `CLAUDE_PLUGIN_ROOT` /
`CLAUDE_PROJECT_DIR` env vars:

- `PreToolUse` → `tool/before`: exit code 2, `{decision:"block"}`, or
  `permissionDecision:"deny"/"ask"` → `deny` with the claude reason.
- `PostToolUse` → `tool/after`: a block marks the tool result
  `{is_error:true, content:<reason>}` so the model sees it.
- `UserPromptSubmit` → `prompt/context`: stdout is injected as context.
- `SessionStart` → first `prompt/context` of each session: stdout injected,
  plus a one-time note listing bridged plugins and unmappable events.
- `Stop`, `SubagentStop`, `PreCompact`, `SessionEnd`, … → reported as skipped
  (no gray lifecycle equivalent) and named in the context note.

Matchers are claude regexes on the tool name; empty matches all. Hook commands
run via `timeout <s> sh -c` (per-hook `timeout`, default 30s, capped 120s).
Hooks that error or print no decision fail open — a broken import never blocks
your tools.

## Wire methods

- Tools: `claude_import {path_or_github_url}`
- Commands: `/cc <plugin> <cmd> [args]`, `/cc-import <path-or-github-url>`
- Hooks: `tool/before`, `tool/after`, `prompt/context` (protocol 2.0)
- Capabilities: none — works fully ungranted; state lives under
  `$GRAY_HOME/claude-import/` (fallback `$HOME/.gray`).

## Install

```sh
gray plugin install claude-import
```

## Develop

```sh
cargo test
gray account check      # entry point + manifest handshake
gray account publish    # check → build → release → publish to the gray registry
```

Bump `version` in `Cargo.toml` before each `publish`; the registry refuses to
republish a version.

---
Part of the [gray](https://github.com/vstaln/gray) plugin ecosystem —
the open-source AI agent harness. <https://gray.alignment.id>
