# flash

Agent tasks as content-addressed build graphs. The second time you ask for something, it costs nothing.

```
cargo test --workspace          # 204 tests
cargo run -p flash-cli -- demo  # cold, warm and hot side by side
```

## What it actually does

An agent asked to update a quarterly report re-reads the CSVs, re-writes all twelve sections and re-renders thirty pages, every time, including the eleven sections whose inputs did not move. flash turns that task into a graph where every node is keyed on the content of its inputs, so the eleven sections are cache hits and the model is only asked about the one that changed.

Measured on three real 13 KB Rust files, decode simulated at 2 s per edit (the one part this machine cannot do for real):

| run | model calls | wall |
| --- | --- | --- |
| cold, nothing cached | 4 | 3.62 s |
| warm, one file's body edited | 1 | 2.28 s |
| hot, nothing changed | 0 | 6 ms |

Cold work costs what the model costs. A model has to type the ops and that floor is real. Everything after the first time is what this buys you.

So the question before adopting it is how much of what your agent does each week is a variation of something it already did. Exploring a codebase once: nothing, and flash is overhead. Regenerating a report every quarter, rebuilding a changelog every release, running the same class of edit across a repo: most of it.

## Two answers before you adopt anything

**Is this worth it for my repo?** Ask the repo.

```
flash churn 120
```

It parses every source file each commit touched, at that commit and its parent, and counts how many named entities actually moved. No model runs, no API key, a few seconds. On ripgrep's last 119 commits:

```
8095 entities in the files those commits touched
534 of them actually changed
a commit in this repo rewrites 6.6% of the entities in the files it touches
median per file revision: 6.5%
```

An agent that re-reads and re-writes whole files does work proportional to 8095. One that edits named entities and caches the rest does work proportional to 534. That ratio is the entire argument, measured on someone else's project rather than asserted.

**Four agents, four checkouts, and a full disk?**

```
flash worktree ../agent-2
```

The store already holds every file version exactly once, keyed by hash, so a working tree is a directory of hard links into it. Three trees of this repo: 2.01 MB logical, 670 KB on disk, and `fsutil hardlink list` shows all three copies of `README.md` are one inode. The tenth agent costs directory entries.

Store blobs are read only, so a tool that writes in place is refused rather than silently corrupting every other tree. Almost everything writes a temp file and renames, which is safe and is the same bet pnpm makes for `node_modules`. `detach` gives one file a private copy when a tool genuinely needs to mutate in place.

## Try it on something real

```
flash run "handle empty input" src/lib.rs \
  --planner-cmd "my-llm --model big" \
  --executor-cmd "my-llm --model small"
```

A model command is any process that reads one JSON request on stdin and prints one JSON response on stdout. That is the whole contract, so whatever you already use for inference works and there is no vendor SDK in the workspace.

Nothing is written to your working tree. A task produces content; `flash show <hash>` prints it and committing it is a separate step. That is what makes it safe to point at a document set you care about before you trust it.

As a daemon, which is how an editor or an MCP client talks to it:

```
claude mcp add flash -- flash serve --root . --planner-cmd ... --executor-cmd ...
```

Claude then has `flash_run` and `flash_status`. Tasks belong to the daemon, so closing the client does not stop one and you reattach by task id. This repo ships a `.mcp.json` so a team picks it up without being told. [docs/INTEGRATION.md](docs/INTEGRATION.md) has the rest.

## Four decisions that make it work

**Action keys come from input content, not from upstream node ids.** A node that reruns and emits byte-identical output leaves every downstream key unchanged, so the downstream stays hot. Hash node ids instead and one rerun anywhere invalidates everything below it, which is warm behaving exactly like cold. [scheduler.rs](crates/flash-engine/src/scheduler.rs)

**Only passing results are cached.** A model call is one sample from a distribution. Cache a failing sample and it is frozen forever, and under a shared team store it spreads to everyone. Failures still go into duration history, because they cost real time and an ETA that ignores the retry path lies. [memo.rs](crates/flash-store/src/memo.rs)

**A node can be replaced by the subgraph it computes.** A build system gets to demand the whole graph up front. An agent runtime cannot: producing the graph *is* the planner's job, and a failed check produces repair work nobody planned. One primitive covers both, and because the expansion is cached with the node, a task that once needed three repair attempts reruns hot. [exec.rs](crates/flash-engine/src/exec.rs)

**The planner depends on file outlines, not file contents.** Change a function body and every outline is byte-identical, so the plan is a hit and the subgraph it emitted replays for free. Add a function and the outline moves, so the planner looks again.

## Adapters

Four artifact types behind one trait. The interesting part is what "impact" means in each.

| adapter | model | ops | what a change re-checks |
| --- | --- | --- | --- |
| [code](crates/flash-adapter-code/src/lib.rs) | tree-sitter, rust / python / typescript | replace body, replace signature, insert after, add import, add test, rename, delete | the tests that reach the change through the call graph |
| [doc](crates/flash-adapter-doc/src/lib.rs) | markdown block tree, stable per-section ids | insert, replace, delete, move, set style, set metadata | only the pages containing changed blocks |
| [sheets](crates/flash-adapter-sheets/src/lib.rs) | formula dependency graph | set values, set formula, insert rows, add sheet, define name, assert | the dependent cell subgraph, exactly |
| [slides](crates/flash-adapter-slides/src/lib.rs) | deck, typed elements, theme | add slide, replace element, reorder, apply layout, bind chart data | one thumbnail per changed slide |

Every adapter runs the same ladder shape: cheapest check first, stop at the first failure, expensive check once at the end. A rung that cannot run reports **unavailable**, never a pass. If `cargo` is missing, the typecheck rung says so and the task fails rather than writing unverified work into a cache other people read.

When entity ops fail, the executor gets the diagnostics and tries again; when that fails, it falls back to one unified diff; then it stops. Each step is a node, so a repair done before is a cache hit, and `flash run` prints the op-resolve and fallback rates on stderr. A high fallback rate means the op schema does not fit that language, which is worth knowing early.

## What is proven and what is not

204 tests, clippy clean. The claims that have teeth:

- a warm rerun recomputes **exactly** the dirty closure, over 100 generated graphs, checked against an oracle the test computes independently. A superset is the bug every incremental system ships first, because answers stay correct and it is merely slow ([incremental.rs](crates/flash-engine/tests/incremental.rs))
- a crash resumes at the graph frontier: the test drops the run future partway through, deletes the memo store, and the journal alone has to carry it ([jobs_and_recovery.rs](crates/flash-engine/tests/jobs_and_recovery.rs))
- a failed rung repairs itself and the repair is cached, so the second run of a task that needed one is hot ([expansion.rs](crates/flash-engine/tests/expansion.rs))
- ETA error under 20 percent once nodes have history, against stubs that run 2 to 4 times long one run in fourteen; a node never run reports unknown instead of a guess ([eta_calibration.rs](crates/flash-engine/tests/eta_calibration.rs))

What is not proven: none of the numbers here include real model decode. `cargo run -p flash-bench` replays model calls from fixtures, which makes the cold column an underestimate and the warm and hot columns honest, since what is claimed there is that work does not happen. Recording a cassette against a live planner/executor pair is the next real milestone and every table says so until then.

Running the code adapter over this workspace's own source found three defects that the fixture tests had not, which is in [dogfood.rs](crates/flash-adapter-code/tests/dogfood.rs) now. The one worth repeating: verify nodes were handed an empty delta, so the dangling-reference check could never fire and impacted-test selection selected nothing. A check that silently passes is indistinguishable from a check that works, and only real input tells them apart.

## Limits

- The ETA is a critical path and does not model queueing behind a lane cap, so a graph 40 nodes wide on a 4-slot lane finishes later than promised.
- Doc block ids are section-scoped ordinals. Inserting a block renumbers its later siblings in that section. The blast radius is one section; perfect stability needs anchors written into the file, which is a worse trade for markdown a human also edits.
- Code reference edges are syntactic. Conservative, so it over-selects tests rather than under-selecting, but coverage data is the real answer.
- The sheets adapter reads a JSON workbook. The xlsx container is mechanical work that teaches nothing about incremental recompute.
- The doc and slides vision rungs report unavailable until a vision model is configured.
- Team cache sharing works mechanically, since the store is one directory, blake3-addressed, with nothing machine-specific in it, but there is no fetch/push path yet, so today you rsync it.

## Layout

```
crates/
  flash-core        identities, framed hashing, action keys, attribution
  flash-store       blobs, memo, duration history, crash journal
  flash-engine      graph, scheduler, lanes, jobs, eta, dynamic expansion
  flash-adapter     the trait, plus the shared entity and diagnostic types
  flash-adapter-*   code, doc, sheets, slides
  flash-orchestrator plan, pack, ops, verify, repair, and the model clients
  flash-server      json-rpc 2.0 and mcp over stdio
  flash-bench       cold/warm/hot across four ablations, one command
  flash-cli         run, serve, demo, stats, show
```

Progress and open items: [docs/PROGRESS.md](docs/PROGRESS.md).
