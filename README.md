# Nano Agent

A minimal command-line agent for working in a shell with approval dialogs.

## Quick Start

```bash
export OPENAI_API_KEY=sk-...
cargo run -- "summarize this repo"
```

Nano shows each shell command before it runs. Approve one command, approve all,
or deny it.

## Install

```bash
cargo install --path .
nano-agent "find TODO comments"
```

## Use

```bash
nano-agent "list large files"   # One-shot prompt
nano-agent                      # Interactive mode
nano-agent -c                   # Continue last session
nano-agent -s                   # Pick a recent session
```

Interactive commands:

```text
:q       quit
:reset   clear the current session
```

## Configure

Config is loaded from `~/.config/nano/config.json`, then `./nano_config.json`
if the global file is missing.

No config is required for OpenAI. Set `OPENAI_API_KEY` and optionally
`OPENAI_MODEL`.

Use a config file for custom OpenAI-compatible providers:

```json
{
  "model": "gpt-5.5",
  "provider": "openrouter",
  "custom_providers": {
    "openrouter": {
      "provider_type": "openai",
      "base_url": "https://openrouter.ai/api/v1",
      "api_key": "sk-or-..."
    }
  }
}
```

Useful fields:

- `model` - model name
- `provider` - custom provider name
- `max_tokens` - response token limit
- `temperature` - sampling temperature
- `custom_providers` - OpenAI-compatible providers
- `quick_models` - named model presets
- `mcp_servers` - MCP servers exposed as tools
- `acp` - ACP server and delegation settings

Environment:

```bash
OPENAI_API_KEY=sk-...
OPENAI_BASE_URL=http://localhost:1234/v1
OPENAI_MODEL=gpt-5.5
NANO_MAX_STEPS=200
NANO_SANDBOX=0
```

## MCP

Configured MCP servers are loaded as agent tools.

```json
{
  "mcp_servers": {
    "docs": {
      "url": "https://mcp.example.com/mcp",
      "headers": {
        "AUTHORIZATION": "Bearer ..."
      }
    },
    "local": {
      "command": "uvx",
      "args": ["some-mcp-server"],
      "show_logs": true
    }
  }
}
```

Tools are discovered on startup and connected lazily when possible. Stdio MCP
server logs are hidden by default; set `show_logs` to `true` while debugging.

## ACP

Nano can expose itself as an ACP agent and delegate work to other ACP agents.

Enable the server:

```json
{
  "acp": {
    "enabled": true,
    "host": "127.0.0.1",
    "port": 8643,
    "agent_name": "nano",
    "description": "Nano local shell agent",
    "agents": {
      "remote-coder": {
        "endpoint": "http://localhost:8644",
        "agent_name": "coder",
        "timeout": 120
      }
    }
  }
}
```

Then start Nano:

```bash
nano-agent
```

Endpoints:

- `GET /ping`
- `GET /agents`
- `GET /agents/{name}`
- `POST /runs`
- `GET /runs/{run_id}`
- `GET /runs/{run_id}/events`
- `POST /runs/{run_id}/cancel`

Run a sync task:

```bash
curl -X POST http://127.0.0.1:8643/runs \
  -H "Content-Type: application/json" \
  -d '{"agent_name":"nano","mode":"sync","input":[{"role":"user","parts":[{"content_type":"text/plain","content":"find TODO comments in src"}]}]}'
```

Use `mode: "stream"` for `text/event-stream`. If `acp.api_key` is set,
include `Authorization: Bearer <token>`.
