# nano-agent roadmap

**Version 0.4.0** — bang shell, `~/.nano/` home, per-cwd sessions, Enter-approves-by-risk.

Philosophy: *trust on a prod laptop* — fewer tools, clearer gates, short prompt.

---

## Done

Everything that used to be open is checked. See `changelog.md`.

Optional only if real pain shows up:

- Tunable `is_safe_command` allowlist from usage complaints
- Suggest `fs+net` when models keep hitting network denials
- Per-server MCP log UI

## Frozen public surface

`nano-agent [prompt]` · REPL · `-c`/`-s` · `--show-config` · `--help` · `--acp` · `/mito` · `/self-harness` · `:q`/`:reset`/`:config`/`:help` · `OPENAI_*` / `NANO_*` env
