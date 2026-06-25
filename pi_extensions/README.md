# Pi Self-Harness Extensions

Copy `self-harness.ts` to `~/.pi/agent/extensions/`, then run `/reload` or restart Pi.

```sh
cp pi_extensions/self-harness.ts ~/.pi/agent/extensions/
```

Use it inside Pi:

```text
/self-harness cargo test
/self-harness-show
/self-harness-clear
```

Accepted overlays are project-local at `.pi/self-harness/harness.md`; attempts are logged in `.pi/self-harness/log.jsonl`.
