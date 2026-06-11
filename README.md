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
```

Every command the agent wants to run is shown first:

```
$ cargo test
Approve? [y] Approve  [a] Approve All  [n] Deny:
```

In the REPL: `:q` quits, `:reset` starts over, end a line with `\` for multiline.

## Other models

Point it anywhere with an OpenAI-compatible API:

```sh
export OPENAI_BASE_URL=http://localhost:11434/v1   # e.g. Ollama
export OPENAI_MODEL=gemma4
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

Useful environment variables: `OPENAI_API_KEY`, `OPENAI_BASE_URL`, `OPENAI_MODEL`, `NANO_MAX_STEPS`, `NANO_SANDBOX=0` (disable bwrap sandboxing).

## Development

```sh
cargo test
cargo test --features acp
```
