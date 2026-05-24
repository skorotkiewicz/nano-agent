# Nano Agent

A minimal shell agent for running commands with approval dialogs.

## Install

```bash
cargo install --path .
```

## Setup

Set your OpenAI API key:

```bash
export OPENAI_API_KEY=sk-...
```

Or create `~/.config/nano/config.json`:

```json
{
  "model": "gpt-4-turbo",
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

## Usage

```bash
nano-agent ls -la    # Run a command
nano-agent           # Interactive mode
nano-agent -c        # Continue last session
```

## Configuration

Config file: `~/.config/nano/config.json` (or `./nano_config.json`)

- `model` - Default model name
- `provider` - Custom provider name
- `max_tokens` - Max tokens per request
- `temperature` - Sampling temperature
- `custom_providers` - Map of provider configs (name -> base_url, api_key)
- `quick_models` - Named model presets

**Priority:** CLI > env vars > config file > defaults

**Environment:**

- `OPENAI_API_KEY`, `OPENAI_BASE_URL`, `OPENAI_MODEL` - API settings
- `NANO_MAX_STEPS` - Max tool calls (default: 200)

**Interactive commands:** `:q` quit, `:reset` clear session.
