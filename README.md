# nano-agent

Small Rust shell agent with OpenAI-compatible model calls, MCP tools, and optional ACP support.

## Build

```sh
cargo build
cargo build --features acp
```

## Run

```sh
OPENAI_API_KEY=... cargo run -- "inspect this repo"
cargo run -- -c
cargo run -- -s
```

`-c` continues the last session in the current directory. `-s` lets you pick a recent session.

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
