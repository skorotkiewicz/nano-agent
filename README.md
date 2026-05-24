# Nano Agent

A minimal shell agent for running commands with approval dialogs.

## Install

```bash
cargo install --path .
```

Requires Rust 2024 edition.

## Setup

Set your OpenAI API key:

```bash
export OPENAI_API_KEY=sk-...
```

For OpenAI-compatible APIs (OpenRouter, llama.cpp, etc.):

```bash
export OPENAI_API_KEY=sk-...
export OPENAI_BASE_URL=https://openrouter.ai/api/v1
export OPENAI_MODEL=deepseek/deepseek-chat
```

## Usage

```bash
# Run a command
nano-agent ls -la

# Interactive mode
nano-agent

# Continue last session
nano-agent -c
```

## Environment

- `OPENAI_API_KEY` - API key (one of key or base URL required)
- `OPENAI_BASE_URL` - Custom API endpoint
- `OPENAI_MODEL` - Model name
- `NANO_MAX_STEPS` - Max tool calls (default: 200)

## Commands

In interactive mode:
- `:q` - Quit
- `:reset` - Clear session

## Development

```bash
just check  # Format + clippy
just test   # Test suite
```
