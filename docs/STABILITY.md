### Stability engineering, since it's your #1

Beyond the process split, the things that actually kill multi-hour runs:

- **No panics in the engine's hot path.** Ban `unwrap`/`expect`; wrap each step in `catch_unwind` or run workers as supervised tasks that restart rather than abort the process.
- **Checkpoint to disk after every step/word.** A worst-case restart should resume mid-workflow, not lose hours. This makes a rare crash a hiccup instead of a catastrophe.
- **CDP auto-reconnect with backoff.** The WebSocket _will_ drop over a multi-hour run (navigations, target churn, idle GC). Treat the browser as flaky: every CDP call can time out, so retry idempotent ops and re-attach to the target on disconnect. This is the single most common long-run failure mode.
- **Run under a supervisor** — a Windows service or systemd unit that restarts the engine on death; it reloads from checkpoint. (Caveat below.)
