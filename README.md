# VibeSys run: Stack, MPMC

A three-round VibeSys run in which coding agents optimized a concurrent stack for one
workload, starting from a deliberately naive Rust implementation (one `std::sync::Mutex` around a standard
collection). Part of the VibeSys data structure update on the [UW SyFI blog](https://syfi.cs.washington.edu/blog/).

- **Workload:** Bounded stack, capacity 1024, 8-byte copied values, try-style push and pop. Scenario `mpmc`: 4 producers, 4 consumers.
- **Agent:** Codex CLI, `gpt-5.6-luna` at `xhigh` for the orchestrator, implementer, and judge.
- **Rounds:** 3, with the round-1 candidate selected.
- **Correctness:** every candidate passes the trusted evaluator's ABI probes and Porcupine linearizability
  checker (100 histories of about 24 operations).

## Result

Re-measured back to back on the run machine (1-socket Intel Xeon Gold 6530), median of repeated 10 s runs,
million successful operations per second:

| Naive seed | VibeSys | Boost.Lockfree `stack` |
| ---: | ---: | ---: |
| 1.73 | **3.41** (2.0× seed) | 3.07 |

In the loop, the framework's official measurement went from 1.77 to 3.45 M ops/s
(1.94×).

## What the agent built

Two version-tagged Treiber stacks over a preallocated slot arena, one for free slots and one for published items, with no locks. A push claims a free slot, copies the payload, and publishes it; a pop claims the top item, copies it out, and recycles the slot. Each operation does two CAS steps.

## Layout

- `src/lib.rs`: the selected candidate. The tag `seed` marks the starting implementation, and
  `selected-round-1` the selected round.
- `_evaluator/stack/`: the trusted evaluator (Go checker and benchmark, Rust native runner, C ABI contract).
- `progress.md`, `roadmap.md`, `progress-artifacts/`: the per-round plans, judge verdicts, and evidence.
- `EXAMPLE-README.md`, `OBJECTIVE.md`, `vibesys.input.toml`: the task as the agents saw it.

## Reproduce

Requires Rust and Go.

```bash
make
go -C _evaluator/stack run . check --workspace ../.. --scenario mpmc --operations 24 --trials 100
go -C _evaluator/stack run . benchmark --workspace ../.. --scenario mpmc --repetitions 3
```
