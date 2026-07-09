# nano-agent

A tiny shell agent. Talks to any OpenAI-compatible API, runs commands **you approve**, stays out of the way.

```sh
cargo install --path .
export OPENAI_API_KEY=sk-...
nano-agent "what's in this repo?"
```

No args → REPL. That's the product.

## Shape

```
you → model → execute_shell? → you approve → bwrap → output → model → answer
```

No plugin system. No second UI. One tool that matters, plus optional MCP/ACP if you ask for them.

## Usage

```sh
nano-agent "fix the failing test"   # one-shot
nano-agent                          # REPL
nano-agent -c                       # continue last session here
nano-agent -s                       # pick a recent session
nano-agent --show-config
nano-agent --help
```

Approval for each command:

```
# list rust sources
$ rg --files -t rust  [safe]
  [y]  [a]all  [s]safe  [n]  [esc]
```

| Key | Meaning |
|-----|---------|
| **Enter** | accept the **suggestion** from the risk tag: `[safe]` → `s`, `[write]` → `y`; `[danger]` will **not** run (type `y` explicitly) |
| `y` | run this once |
| `a` | run **all** remaining this turn |
| `s` | run this + auto-ok **safe** patterns (`ls`, `git status`, `cargo test`, `rg`, …) |
| `n` / Esc | deny / cancel turn |

REPL: `›` prompt · `:q` · `:reset` · `:config` · `/mito` · `/self-harness <validator>` · line ending `\` continues.

**Shell shortcuts** (no approval — you typed it):

```
! cat text.md     # run; output printed + noted for the model's next turn
!! cat secret.md  # run; printed only, model never sees it
```

**Cancel:** press **Esc** (or Ctrl+C) while the spinner shows `thinking · esc cancel`, or while a long shell command runs. Approval prompt still uses Esc to cancel the turn.

## Other models

```sh
export OPENAI_BASE_URL=http://localhost:11434/v1   # Ollama
export OPENAI_MODEL=gemma4
# localhost needs no API key
```

Or `~/.config/nano/config.json` / `./nano_config.json` (local overlays global). See [example_config.json](example_config.json).

## Env

| Variable | Meaning |
|----------|---------|
| `OPENAI_API_KEY` | required unless provider has a key or endpoint is localhost |
| `OPENAI_BASE_URL` | OpenAI-compatible base → chat-completions |
| `OPENAI_MODEL` | model id |
| `NANO_MAX_STEPS` | tool-loop cap (default 200) |
| `NANO_SANDBOX` | `off` · `fs` (default, no net) · `fs+net` |

## Optional extras

- **MCP** — `mcp_servers` in config
- **mito** — `/mito` local planner (needs `mito-mode` + chat-completions provider)
- **self-harness** — `/self-harness cargo test` proposes `.nano/harness.md` if validator passes
- **ACP** — `--features acp` → `nano-agent --acp` and child agents under `acp_agents`

## Design (why this, not AutoGPT)

1. **Trust boundary is the human.** Every shell line is visible; risk-tagged (`safe` / `write` / `danger`).
2. **One primary tool.** Multi-tool dragons hide failure modes.
3. **Short system prompt.** Procedures the model can follow, not a novel.
4. **Session resume that fails loud.** Format mismatch → start fresh, don't half-context.
5. **Sandbox with a name.** `fs` vs `fs+net` vs `off` — isolation is a policy, not a mystery.

## Arch

```sh
yay -S nano-agent
```
