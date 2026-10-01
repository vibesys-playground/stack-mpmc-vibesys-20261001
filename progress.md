# Progress

## Round 1 — Orchestrator (pre-round)
- **need_profile**: False
- **profile_focus**: No profiling requested; defer throughput and bottleneck attribution to the design call.
- **reasoning**: Round 1 has no prior measurement, but static source evidence clearly identifies the baseline mechanism: every push and pop serializes on one Mutex<Vec<Vec<u8>>> and performs per-item allocation and copying. That is sufficient to rank a structural concurrency/allocation redesign first. Standalone profiling is disabled, so the missing quantitative attribution remains uncertainty for planning rather than a capture request.

## Round 1 — Orchestrator (plan)
- **hypothesis_id**: H1-preallocated-tagged-mpmc-lifo
- **reasoning**: Comparable trajectory is baseline-only: zero accepted rows, no measured end-to-end delta, and the remaining gap is the full throughput objective. The experiment changes only the candidate core from mutex-plus-heap-backed storage to preallocated atomic slot ownership; the ABI and evaluator boundary remain unchanged. A lock-free tagged-slot design is the highest-leverage structural parent indicated by the baseline. Sharded locks, batching, and instrumentation-only work are deferred because global LIFO publication still requires coordination, batching changes the try-style behavior, and profiling was not requested or needed to identify the current structural bottleneck.

### Hypothesis
Replacing the single Mutex<Vec<Vec<u8>>> with preallocated fixed-size slots, a tagged atomic free-slot reservation stack, and a separate tagged atomic published-top stack will improve end-to-end MPMC throughput by removing global lock convoying, per-operation heap allocation, and allocator contention while preserving reservation/publication semantics.

### Activation evidence
The baseline in /mnt/data/rairai77/stack-map-luna-runs/20261001-213228-09dbcf20-stack-map-stack-mpmc-luna/src/lib.rs serializes every push/pop on one mutex and allocates on successful push. The workload in /mnt/data/rairai77/stack-map-luna-runs/20261001-213228-09dbcf20-stack-map-stack-mpmc-luna/_evaluator/stack/native_runner/src/benchmark.rs uses concurrent pinned producers and consumers with copying and failed try operations included.

### Falsification criteria
A correctness-valid implementation whose paired trusted result remains below the 10% minimum, or shows no activation of the preallocated atomic path, falsifies this implementation of the mechanism. A flat or negative result is not itself a contract violation; frontier retention remains governed by feasible nondomination. Any ABI, ownership, LIFO, reservation/full, empty, or conservation failure falsifies the candidate immediately.

### Expected effect (forecast)
Forecast: approximately 20-80% higher MPMC total_ops_per_sec for the default small-value workload, with smaller or neutral gains for large copied values because memory bandwidth then dominates. This is a forecast, not a rejection threshold.

### Minimum acceptance criteria
Hard feasibility requires the required ./stack-candidate.so, unchanged copying C ABI, lifecycle validity, exact bounded LIFO behavior, MPMC reservation/publication correctness, no fabricated or duplicated items, and trusted correctness-gate success. For retention, require at least a 10% median total_ops_per_sec improvement over the retained parent with no more than 5% relative degradation in any measured accepted comparison. The 10% floor is above expected run noise and pays for the added atomic complexity; the allowed tradeoff is only create-time bounded storage of capacity times max_value_size plus metadata, with no operation-time allocation.

### Invariants
Hard constraints and ABI: /mnt/data/rairai77/stack-map-luna-runs/20261001-213228-09dbcf20-stack-map-stack-mpmc-luna/.vibesys/state/runs/20261001-213228-09dbcf20-stack-map-stack-mpmc-luna/runtime/effective-objective.md, /mnt/data/rairai77/stack-map-luna-runs/20261001-213228-09dbcf20-stack-map-stack-mpmc-luna/_evaluator/stack/CANDIDATE_CONTRACT.md, and /mnt/data/rairai77/stack-map-luna-runs/20261001-213228-09dbcf20-stack-map-stack-mpmc-luna/src/ffi.rs. Evaluator correctness and metric: /mnt/data/rairai77/stack-map-luna-runs/20261001-213228-09dbcf20-stack-map-stack-mpmc-luna/_evaluator/stack/DESIGN.md and /mnt/data/rairai77/stack-map-luna-runs/20261001-213228-09dbcf20-stack-map-stack-mpmc-luna/_evaluator/stack/benchmark.go. Diagnostics must establish reservation-head exhaustion returns FULL, publication-head exhaustion returns EMPTY, tagged heads prevent ABA, publication uses release/acquire ordering, output is copied before slot recycle, and each slot has one owner at every transition.

### Task
Implement H1 behind the unchanged C ABI. At stack creation, allocate bounded fixed-size storage for capacity slots and initialize a tagged free-slot head. Maintain a separate tagged published-top head for LIFO visibility, with per-slot length and next-index metadata. Push must claim a free slot before copying, so reserved capacity is visible to FULL; copy the borrowed bytes, then publish the slot with release ordering. Pop must acquire and claim only the published top, validate output capacity before mutation, copy the value out, and recycle the slot only after copying completes. Use a portable tagged index representation sized for the allowed capacity to avoid ABA on both heads, and preserve all existing invalid-input, status, handle-lifecycle, and teardown behavior. Verification strategy: first confirm the shared library builds and exports the existing ABI; then run the cheapest changed-path correctness checks covering the concurrent MPMC cases above and ownership/conservation; finally collect one short comparable A/B throughput point against the retained parent. Do not duplicate the framework's canonical benchmark command. If a correctness failure occurs, isolate the changed dataflow locally before any further target call; stop after fair falsification.

### Pass criteria
Activation: the candidate builds and the changed path demonstrably uses preallocated slots, tagged atomic heads, and no operation-time allocation. Correctness: exercise small-capacity concurrent MPMC behavior including duplicate payloads, LIFO publication, FULL during reservation, EMPTY while unpublished, insufficient-output preservation, and exact conservation. Cleanup: destroy handles and stack without leaks or dangling slot ownership, and retain the baseline as the explicit default if this arm is rejected. Evidence: record one comparable target result against the retained parent and the activation/correctness observations. Use at most one target allocation for the initial candidate and one additional target allocation only after a correctness-only repair; no tuning expansion this round. A wrapper rejection counts as unspent only with raw proof that target allocation never began.

## Round 1 — Implementer (attempt 1)
- **expected_behavior**: Push reserves capacity before copying and publication; pop validates output capacity, copies before recycling, preserves LIFO order, and avoids operation-time allocation.
- **hypothesis_outcome**: nominated
- **next_step**: (none)

- **candidate_disposition**: unassessed
- **candidate_metrics**: {}
- **candidate_evaluation_artifact**: (missing)
- **candidate_operating_point**: (none)
- **candidate_retention_reason**: No target-bound comparable measurement was collected; framework evaluation remains pending.

### Summary
Implemented H1 with preallocated fixed-size slot storage, tagged atomic free/published heads, release/acquire ownership transitions, and unchanged C ABI.

### Evidence
cargo test --locked passed 3 tests, including concurrent 4P/4C conservation and insufficient-output preservation. Release build succeeded; all 9 required vsst_* symbols exported. No framework-owned correctness or benchmark command was duplicated, so no throughput row is claimed.

## Round 1 — Judge (attempt 1)
- **verdict**: pass

### Analysis
PASS. The nominated outcome is ready for framework gates. Candidate identity is clean at commit a73a7a4; the tracked stack-candidate.so matches the committed artifact, all nine required ABI exports are present, and evaluator/ABI files are unchanged. Static review verifies preallocated bounded storage, tagged free/published heads, reservation-aware FULL, publication acquire/release ordering, copy-before-recycle, LIFO linkage, lifecycle ownership, and no operation-time allocation. The validation recipes are bounded and compliant; no target or official gate was duplicated. No performance claim is made, consistent with nomination pending framework evaluation.

### Feedback

