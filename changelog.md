# Changelog

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
