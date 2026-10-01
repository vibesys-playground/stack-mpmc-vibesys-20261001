# Progress

## Round 1 — Orchestrator (pre-round)
- **need_profile**: False
- **profile_focus**: No profiling requested; defer throughput and bottleneck attribution to the design call.
- **reasoning**: Round 1 has no prior measurement, but static source evidence clearly identifies the baseline mechanism: every push and pop serializes on one Mutex<Vec<Vec<u8>>> and performs per-item allocation and copying. That is sufficient to rank a structural concurrency/allocation redesign first. Standalone profiling is disabled, so the missing quantitative attribution remains uncertainty for planning rather than a capture request.

