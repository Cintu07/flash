# flash

An incremental artifact runtime. Agent tasks become content-addressed build graphs, so repeated
work costs nothing and new work is the only work.

Implements `incremental-artifact-runtime v3`: the build engine, four adapters, the two-model
orchestrator, the benchmark, and the daemon. 199 tests, no warnings.

```
cargo test --workspace          # 199 tests
cargo run -p flash-cli -- demo  # the section 6 scenario, stub models
cargo run -p flash-bench        # regenerates every table and chart
```

## The idea in one table

| regime | what it is | what it costs here |
| --- | --- | --- |
| cold | new work | the floor: a model has to type the ops |
| warm | same task, some inputs changed | only the dirty closure, which is usually smaller than you think |
| hot | identical inputs | zero model calls, single-digit milliseconds |

Every agent treats everything as cold. This one is built so warm and hot are the default.

## What is here

| crate | what it holds |
| --- | --- |
| [flash-core](crates/flash-core/src/lib.rs) | identities, framed blake3 hashing, action keys, attribution |
| [flash-store](crates/flash-store/src/lib.rs) | blob CAS, verdict-gated memo, duration history, crash journal |
| [flash-engine](crates/flash-engine/src/scheduler.rs) | task graph, scheduler, lanes, jobs, eta, dynamic expansion |
| [flash-adapter](crates/flash-adapter/src/lib.rs) | the one trait every artifact type implements (§4) |
| [flash-adapter-code](crates/flash-adapter-code/src/lib.rs) | tree-sitter (rust, python, ts), entity ops, ladder, test selection |
| [flash-adapter-doc](crates/flash-adapter-doc/src/lib.rs) | markdown block tree, block ops, changed-page render path |
| [flash-adapter-sheets](crates/flash-adapter-sheets/src/lib.rs) | formula dependency graph, dependent-only recalc, planner assertions |
| [flash-adapter-slides](crates/flash-adapter-slides/src/lib.rs) | deck model, layout rules, per-slide thumbnails |
| [flash-orchestrator](crates/flash-orchestrator/src/lib.rs) | plan → pack → ops → verify → repair, and the model clients |
| [flash-server](crates/flash-server/src/lib.rs) | json-rpc 2.0 + mcp over stdio, reattach by task id |
| [flash-bench](crates/flash-bench/src/lib.rs) | agent-latency-anatomy: cold/warm/hot, ablations, losses |
| [flash-cli](crates/flash-cli/src/main.rs) | `run`, `serve`, `demo`, `stats`, `show` |

## Claims, and where each is checked

| claim | test |
| --- | --- |
| a warm rerun recomputes *exactly* the dirty closure, on 100 synthetic graphs | [incremental.rs](crates/flash-engine/tests/incremental.rs) |
| identical inputs cost zero model calls | same file |
| a step whose output did not move keeps its children hot (early cutoff) | same file |
| eta error under 20% once nodes have history; unknown is never guessed | [eta_calibration.rs](crates/flash-engine/tests/eta_calibration.rs) |
| a render is a job; the scheduler never blocks on it | [jobs_and_recovery.rs](crates/flash-engine/tests/jobs_and_recovery.rs) |
| a crash resumes at the graph frontier, not from zero | same file |
| a failure skips its dependents and is never cached | same file |
| a planner emits work that did not exist, and a rerun replays it without re-planning | [expansion.rs](crates/flash-engine/tests/expansion.rs) |
| a failed rung repairs itself, and the repair is cached | same file |
| a runaway repair loop is stopped rather than left to burn tokens | same file |
| ops → ops+diagnostics → unified diff → give up, with the fallback rate counted | [escalation.rs](crates/flash-orchestrator/tests/escalation.rs) |
| a body edit does not re-plan; a new entity does | [orchestrator lib.rs](crates/flash-orchestrator/src/lib.rs) |
| an edit to one entity leaves every other entity byte-identical | [ops.rs](crates/flash-adapter-code/src/ops.rs) |
| impact reaches tests through a call chain and spares unrelated ones | [impact.rs](crates/flash-adapter-code/src/impact.rs) |
| an incremental recalc reaches the same answer as a full one | [formula.rs](crates/flash-adapter-sheets/src/formula.rs) |
| a rung that cannot run reports unavailable, never a pass | [ladder.rs](crates/flash-adapter-code/src/ladder.rs) |

## Design, and where it departs from the PRD

**Two identities.** `NodeKey` is the logical step (`section:methodology`) and is what duration
history is keyed on. `ActionKey` is `blake3(kind, op, env, input *content* hashes)` and is the memo
key.

1. **d4 amended: the action key comes from input content, not upstream node ids.** Hashing node ids
   gives no early cutoff: a node that reruns and emits byte-identical output would still invalidate
   everything downstream. Computing the key from what inputs actually produced is what makes "only
   sections whose data summary changed rerun" true.

2. **d4 amended: memoization is verdict-gated.** Only a node whose ladder passed is written to the
   memo. An executor call is one sample from a distribution; caching a failing sample freezes it
   forever, and under d10's shared cache it spreads to the team. Failures still go into duration
   history, because they cost real time.

3. **The eta does a memo lookahead.** A pure history walk quotes 30 s for work the cache serves in
   4 ms. Before a task starts, the estimator walks the topo order predicting content forward and
   probing the memo, so warm and hot runs get honest numbers from t=0.

4. **The graph is not fixed** — the gap the PRD left open. A node may return an *expansion*: a
   subgraph plus the node in it that stands in for it. That one primitive covers both the planner
   (whose job is to produce the graph) and repair (work nobody planned). Expansion names are scoped
   under the emitting node, and expansions are cached with the node, so replaying a plan or a repair
   costs nothing.

5. **The planner depends on outlines, not file contents.** A body edit leaves every outline
   byte-identical, so the plan hits and its whole subgraph is replayed for free. A new entity moves
   the outline and the planner looks again. This is §6's "the outline node hits unless the planner
   sees new headers", made mechanical.

## Running a real task

`flash run` needs a model command: anything that reads one json request on stdin and prints one
json response on stdout. That is the whole integration contract — no vendor sdk anywhere in the
workspace.

```
flash run "handle empty input" src/lib.rs \
  --planner-cmd "my-llm --model big" \
  --executor-cmd "my-llm --model small"
```

Nothing is written to your working tree. A task produces content; `flash show <hash>` prints it and
committing it is a separate, explicit step.

As a daemon (d8, d9 — no editor, no ide):

```
flash serve --root . --planner-cmd ... --executor-cmd ...
```

It speaks json-rpc 2.0 on stdio, in two dialects: native (`flash/run`, `flash/status`,
`flash/jobs`, `flash/cancel`, `flash/content`) and MCP (`initialize`, `tools/list`, `tools/call`).
Tasks belong to the daemon, so a client that disconnects loses nothing and reattaches by task id.

## Benchmark

`cargo run -p flash-bench` runs every frozen task in [bench/tasks](bench/tasks/) through four
ablations (ops only → ops+ladder → +impact → +incremental) in three regimes, and writes
`bench/report/`: the tables, an attribution chart, and the raw measurements.

Model calls are replayed from fixtures, so **decode time is not in those numbers**. That is
deliberate and stated on every table: it makes the cold column an underestimate by exactly the
decode time, and makes the warm and hot columns honest, because what is claimed there is that work
does not happen at all. Point the harness at a real model client and the same tables come out with
decode included.

The report finds its own losses — any lever that made a regime slower is printed with the number,
per §7's "losses published with reasons. no cherry picking."

## Known limitations, stated rather than hidden

- **The eta is a pure critical path.** It does not model queueing behind a lane cap, so a graph 40
  nodes wide on a 4-slot lane finishes later than promised.
- **Doc block ids are section-scoped ordinals.** Inserting a block renumbers its later siblings *in
  that section*. The blast radius is one section; perfect stability needs persistent anchors
  written into the file, which is a worse trade for markdown a human also edits.
- **Code reference edges are syntactic.** Good enough for test selection and conservative by
  construction; coverage-based selection is the upgrade.
- **The sheets adapter reads a json workbook, not xlsx.** The container is mechanical work that
  teaches nothing about incremental recompute; the dependency graph is the part that matters.
- **Doc and slides rung 3 (vision) reports unavailable** until a vision model is configured. It is
  never reported as a pass.

## Local build note

This machine is ARM64 Windows and its Visual Studio build tools ship only x64/x86 linkers, so the
msvc target cannot link here. Use the self-contained toolchain:

```
cargo +stable-aarch64-pc-windows-gnullvm test --workspace
```

Git Bash also puts a GNU `link` on PATH that shadows MSVC's `link.exe`; run cargo from PowerShell.
Neither issue exists on the Linux daemon box the design targets.

## Next

See [docs/PROGRESS.md](docs/PROGRESS.md). The short version: the runtime is complete end to end
against scripted and replayed models, and the next real milestone is a recorded cassette against a
live model pair, which turns the benchmark's cold column from "runtime overhead" into "agent
latency".
