# Nano Agent

A minimal Rust shell agent with OpenAI-compatible model calls, approval-gated command execution, MCP tools, and optional ACP support.

## Install

```sh
cargo install --path .
```

## Setup

Set an API key:

```sh
export OPENAI_API_KEY=sk-...
```

Or create `~/.config/nano/config.json` or `./nano_config.json`:

```json
{
  "provider": "openai-compatible",
  "custom_providers": {
    "openai-compatible": {
      "provider_type": "openai",
      "base_url": "https://api.openai.com/v1",
      "api_key": "sk-...",
      "model": "gpt-4-turbo"
    },
    "local": {
      "provider_type": "openai",
      "base_url": "http://localhost:11434/v1",
      "api_key": "",
      "model": "gemma4"
    }
  }
}
```

## Build

```sh
cargo build
cargo build --features acp
```

## Run

```sh
OPENAI_API_KEY=... cargo run -- "inspect this repo"
cargo run
cargo run -- -c
cargo run -- -s
```

`-c` continues the last session in the current directory. `-s` lets you pick a recent session. In the REPL, `:q` quits and `:reset` clears the session.

## Configuration

Config file priority is `~/.config/nano/config.json`, then `./nano_config.json`.

Supported fields:

- `model`
- `provider`
- `max_tokens`
- `temperature`
- `custom_providers`
- `mcp_servers`
- `acp_agents`

Environment:

- `OPENAI_API_KEY`
- `OPENAI_BASE_URL`
- `OPENAI_MODEL`
- `NANO_MAX_STEPS`

## ACP

Run nano as an ACP stdio agent:

```sh
cargo run --features acp -- --acp
```

Configure child ACP agents in `nano_config.json` or `~/.config/nano/config.json`:

```json
{
  "acp_agents": {
    "worker": {
      "command": "cargo",
      "args": ["run", "--features", "acp", "--", "--acp"],
      "working_directory": "/path/to/project",
      "timeout_secs": 600
    }
  }
}
```

When configured, nano exposes `delegate_task` and `delegate_tasks` to spawn child ACP agents.
`working_directory` is the tool boundary for that child. If it is omitted or null, spawned tools are disabled.

## Test

```sh
cargo test
cargo test --features acp
```

$$info
