# nano-agent

A tiny shell agent in Rust. Talks to any OpenAI-compatible API, runs commands with your approval, and stays out of the way.

## Quick start

```sh
cargo install --path .
export OPENAI_API_KEY=sk-...

nano-agent "what's in this repo?"
```

That's it. Run `nano-agent` with no arguments for an interactive REPL.

## Arch Linux

Install from the AUR with your preferred helper:

```sh
yay -S nano-agent
# or
paru -S nano-agent
```

## Usage

```sh
nano-agent "fix the failing test"   # one-shot prompt
nano-agent                          # REPL
nano-agent -c                       # continue last session here
nano-agent -s                       # pick a recent session
nano-agent --show-config            # resolved provider/model/sandbox
nano-agent --help
```

Every command the agent wants to run is shown first:

```
$ cargo test
Approve? [y] Approve  [a] Approve All  [s] Safe  [n] Deny  [Esc] Cancel:
```

`[s] Safe` auto-approves later read-only-looking commands this turn (`ls`, `git status`, `cargo test`, `rg`, …). Risky / compound commands still ask.

In the REPL: `:q` quits, `:reset` starts over, end a line with `\` for multiline.

Self-harness mode proposes one local prompt overlay from recent session evidence, temporarily installs it, runs your validator, and keeps it only if the validator passes:

```sh
nano-agent "/self-harness cargo test"
```

Accepted overlays live at `.nano/harness.md`; rejected and accepted attempts are logged under `.nano/self-harness/`.

## Other models

Point it anywhere with an OpenAI-compatible API:

```sh
export OPENAI_BASE_URL=http://localhost:11434/v1   # e.g. Ollama
export OPENAI_MODEL=gemma4
# no API key required for localhost endpoints
```

Or keep providers in `~/.config/nano/config.json` (or `./nano_config.json`):

```json
{
  "provider": "local",
  "custom_providers": {
    "local": {
      "provider_type": "openai",
      "base_url": "http://localhost:11434/v1",
      "api_key": "",
      "model": "gemma4"
    }
  }
}
```

See [example_config.json](example_config.json) for the full format.

## Going further

- **MCP tools** — add servers under `mcp_servers` in the config; their tools are exposed to the model automatically.
- **Planning mode** — prefix a message with `/mito` to talk to a separate local planning agent that prepares a detailed handoff before the main model acts (enable `mito-mode` in the config).
- **ACP** — build with `--features acp` to run nano as an ACP stdio agent (`nano-agent --acp`) or to delegate subtasks to child agents configured under `acp_agents`. A child's `working_directory` is its sandbox boundary; without one, its tools are disabled.

Useful environment variables:

| Variable | Meaning |
|----------|---------|
| `OPENAI_API_KEY` | API key (or set on a custom provider) |
| `OPENAI_BASE_URL` | OpenAI-compatible base URL (uses chat-completions) |
| `OPENAI_MODEL` | Model id |
| `NANO_MAX_STEPS` | Tool-loop cap (default 200) |
| `NANO_SANDBOX` | `off` / `0` — no bwrap; `fs` (default) — isolate FS, no net; `fs+net` — isolate FS, share network |

Also see [ROADMAP.md](ROADMAP.md).

## Development

```sh
cargo test
cargo test --features acp
```
