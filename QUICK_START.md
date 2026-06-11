## Quick Start for Nano Agent

**1. Install:**
```sh
cargo install --path .
```

**2. Configure (create `nano_config.json`):**
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

**3. Run:**
```sh
cargo run -- "your task here"
```

Or enter REPL mode:
```sh
cargo run
```
Then `:q` to quit, `:reset` to clear session.

**4. With ACP support:**
```sh
cargo run --features acp -- --acp
```

**5. Mito Mode (for local model testing):**
Add to your `nano_config.json`:
```json
{
  "mito-mode": {
    "enabled": true,
    "provider": "local",
    "model": "gemma4"
  }
}
```

The `/mito` command is available in the REPL to switch to mito mode, which uses the configured mito-mode settings.

**6. Common just shortcuts:**
```sh
just build      # Build release
just test       # Run tests
just run "task" # Run with task
```

That's it! Set API key, configure if needed, and start using.
