# Code Review TODO

The review findings from the latest pass have been implemented.

Verification:

- `cargo test` passes: 22 tests.
- `cargo clippy --all-targets -- -D warnings` passes.

Implemented fixes:

- Kept semaphore permits alive for the full spawned worker lifetime.
- Wired reconciliation cancellation through running entries, workers, and the agent runner so cancelled child processes are killed and cancelled outcomes are not reported as fresh work.
- Preserved child stdin across real-agent continuation turns.
- Fixed retry attempt accounting for retry worker failures.
- Reinserted due retries when an issue is already running without lock upgrade deadlocks.
- Made dynamic reload use the configured workflow path.
- Updated retry worker token totals.
- Simplified command launching and validated empty commands.
- Preserved suffixes for `$VAR` and `${VAR}` path expansion.
- Made hook timeouts kill and wait for the hook process.
- Parsed YAML front matter delimiters as full delimiter lines.
- Wired `/api/v1/refresh` to the orchestrator poll loop.
- Cleaned up strict clippy findings.
