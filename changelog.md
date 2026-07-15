# Changelog

## Unreleased

- Repo-local `nano_config.json` now asks for trust once per canonical project path and remembers the answer under `~/.nano/trusted-projects/`; non-interactive runs still fail closed.
- `--no-ctx` omits Nano's system prompt and skips project doc, skill, and harness discovery.
- Individual `mcp_servers` and `acp_agents` entries can now be paused with `"enabled": false`; omitted flags remain enabled for compatibility.
- Invalid config and unknown configured providers now fail loud instead of falling through to another endpoint.
- Esc during model-requested shell execution now cancels the turn instead of returning a normal tool result and continuing.
- Shell output is bounded while streaming, preventing noisy commands from exhausting Nano's memory before truncation.
- Nano-owned state under `~/.nano` is made private on Unix, and MCP cache fingerprints no longer contain raw config secrets.
- Tool-loop limits now inspect the final post-tool response instead of fetching and discarding it.
- Mito turns reset `[a]all`/`[s]safe` approval state before using tools.
- Long session labels remain stable, trimmed Chat history drops orphan tool results, and cancelled Responses turns preserve pending `!` context.
- Executable readers (`env`, `awk`, `sed`, `find`) and mutating `git branch`/`git remote` forms are no longer tagged `[safe]`.
- Approval prompts show the documented `[safe]`, `[write]`, or `[danger]` risk tag again.
- Approval prompts now show non-default `cwd`/`timeout`/`env`, so write/delete commands are easier to judge.
- Secret-looking env values in approval prompts are redacted.
- Source-editing commands like `cargo fmt`, `rustfmt file`, `sed -i`, and `awk -i` are no longer tagged `[safe]`.
- Deletion commands like `rm`, `unlink`, `rmdir`, `git rm`, `find -delete`, `find -exec rm`, and `xargs rm` are now tagged `[danger]`, including common `sudo` wrappers.
- Data-destroying commands like `dd of=...`, `rsync --delete`, and `shred` are now tagged `[danger]`.
- Git discard/delete commands like `git restore`, `git checkout --`, `git clean`, `git stash drop`, and `git branch -D` are now tagged `[danger]`, including common `sudo` / `git -C` forms.
- `[danger]` commands now always require explicit `y`; `a`/`s` shortcuts no longer approve them.
- ACP shell calls refuse `[danger]` commands by default unless `NANO_ACP_ALLOW_DANGER=1` is set.
- Default `fs` sandbox failures that look network-related now suggest `NANO_SANDBOX=fs+net`.
- The system prompt now explicitly tells Nano to inspect git status before edits/deletes and preserve user changes.
- The REPL MCP banner now distinguishes cached tools from live connected servers.
- `--show-config` now lists configured MCP server and ACP agent names, not just counts.

## 0.4.0

- **Bang shell shortcuts** in REPL / one-shot:
  - `! cmd` runs and notes the result for the model's next turn
  - `!! cmd` runs but stays hidden from the model
  - both use the tool sandbox, no approval prompt; leading `$` colored on TTY
- **Home layout `~/.nano/`**: `config.json`, `mcp_cache.json`, and per-directory `sessions/<cwd-hash>.jsonl`
- **Session history per cwd** (was one shared `~/.nano_sessions.json`); silent migration from legacy XDG + big-file layouts
- **Enter approves by risk tag**: `[safe]` → `s`, `[write]` → `y`, `[danger]` refuses

## 0.3.0 — agent shape

Product redesign: same binary, clearer agent.

- **Home layout** `~/.nano/` — `config.json`, `mcp_cache.json`, `sessions/<cwd-hash>.jsonl` (migrates legacy XDG + `~/.nano_sessions.json`)
- **Esc / Ctrl+C cancel** in-flight API think (spinner) and long shell runs; approval Esc still cancels the turn

- **System prompt** rewritten as short working rules (tool-first, no fluff/songs, refuse unprompted destruction)
- Doc/skill scan: max depth 5, fewer entries — monorepos no longer stall startup
- **REPL** banner one line (`nano  model  sandbox  mcp`); prompt `›` / `…`
- REPL `:config` / `:help`
- **Approval** risk tag on every command: `[safe]` · `[write]` · `[danger]`
- Compact approval keys: `[y] [a]all [s]safe [n] [esc]`
- Localhost endpoints skip `OPENAI_API_KEY` (from 0.2.4 line)
- Session/content caps, kill-on-timeout, API error surface (carried from 0.2.x)

## 0.2.4

Reliability stack: API error bodies, timeout reaps, sandbox modes (`off|fs|fs+net`),
`[s] Safe` approval, `--show-config` / `--help`, session atomic write + caps,
HTTP timeouts, mito handoff schema, drop `chrono`.

## 0.2.3 and earlier

Shredder rewrite; see git history and `SHREDDER_REPORT.md`.
