# Progress

## 2026-09-12: the runtime, end to end

The PRD's phases 0 through 4 are implemented and tested: build engine, four adapters, the
two-model orchestrator, the benchmark, and the daemon. 199 tests, no compiler warnings, clippy
clean.

### Phase 0: the engine, no models

Exit criteria enforced as tests:

- warm rerun recomputes **exactly** the dirty closure on 100 synthetic graphs, checked against an
  oracle in the test that simulates propagation independently, so a superset (slow but
  correct-looking) and a subset (stale output) both fail;
- eta error under 20% on graphs with history, against stubs that go 2 to 4 times long one run in fourteen;
- plus: hot runs make zero executor calls, jobs stream progress and cancel, a dropped run future
  resumes at the frontier from the journal alone with the memo deleted, a failing node skips its
  dependents and is never cached.

### Phase 1: code adapter, real models

- tree-sitter symbol graph for rust, python and typescript, with ids derived from names and scopes
  so moving a function within a file changes nothing;
- entity ops (replace body / signature, insert after, add import, add test, rename, delete) with an
  all-or-nothing materializer that refuses any batch which would break the parse;
- the unified-diff fallback, anchored on context rather than line numbers;
- ladder rungs 0 to 4, with external rungs reporting *unavailable* rather than passing;
- static test selection over the reference graph;
- deterministic, minimal context packs: signatures for neighbours, bodies only for the target.

### Phase 2: doc adapter and the pdf path

Block tree with section-scoped ids, block ops capped at 400 words, heading-order / empty-section /
broken-ref lint, deterministic pagination, and changed-page-only rendering. pdf is a render target,
never an edit target.

### Phase 3: benchmark

`flash-bench` runs frozen tasks through four ablations in three regimes and regenerates every table,
the attribution chart and the raw measurements with one command. It finds and prints its own losses.
Model calls are replayed from fixtures and every table says so.

### Phase 4: be the layer

Sheets (formula dependency graph, dependent-only recalc, planner assertions) and slides (layout
rules, contrast, per-slide thumbnails) adapters; json-rpc 2.0 + MCP over stdio with reattach by task
id; a thin cli.

## Five things found by building it

1. **Delegation had to be transitive.** A repair that is itself repaired resolved only one hop, so
   the original node's dependents waited forever and the whole tail reported as skipped while the
   work it needed sat finished in the store. Found by the diff-fallback test.
2. **Expansion names must be scoped under the node that emitted them.** Expansions are cached
   against *action* keys, and two logically different nodes with identical inputs share one action
   key by design, so a cached expansion can be replayed under a different parent, and absolute
   names collide on replay.
3. **A completed task must clear its journal.** Otherwise the next run replays itself as "resumed"
   and hit/computed counts become meaningless.
4. **A block edit that drops a trailing newline eats the next heading.** Block ranges include their
   terminator but not the blank line that separates blocks, so preserving separators is the
   difference between an edit and a corruption.
5. **A hot run stopped looking hot** once expansions counted as work. How a node's result was
   obtained (ran / cached) is a different fact from what it produced, and the report has to keep
   them apart.

## What dogfooding found (2026-09-12, same day)

Running the code adapter over this workspace's own source, and running one real task through an
external model process, found three defects that 199 fixture tests had not. All three are now
regression-tested in [dogfood.rs](../crates/flash-adapter-code/tests/dogfood.rs).

1. **Trait impls collided on entity id.** `impl Debug for Digest` and `impl Display for Digest`
   both define `fmt`, and scoping on the type alone gave them one id. Not just a lint problem: an
   op naming `fn:Digest::fmt` was ambiguous and impact analysis conflated them. Trait impls now
   scope as `<Digest as Debug>`.
2. **The unused-import lint was unsound and is gone.** It fired on `pub use` re-exports and on
   trait imports used through method syntax. Ten diagnostics on this repo, zero true positives.
   Each false positive costs two repair attempts and then fails the task. Rung 2 is a real
   compiler and reports this correctly; rung 1 had no business guessing.
3. **The verify node was handed an empty delta.** So `dangling-reference` could never fire and
   impacted-test selection always selected nothing. Rung 3 was passing trivially in every run.
   Verify nodes now take the pre-edit artifact as a second input and recompute the delta from
   content.

The third is the one worth remembering: a rung that silently passes is indistinguishable from a
rung that works, and only a real workload tells them apart.

## Storage: measured, and deliberately not changed yet

`flash stats` reports the shape of the store, not just its size, because the question "does this
need a real database" should be answered by a number:

```
part      files      bytes   mean   on disk  waste
memo         32      12987    405    131072  10.1x
history      20        908     45     81920  90.2x
```

Every part of the store is many small files. A 45 byte history record still costs a filesystem
block, so history wastes 90x and memo wastes 10x. One 20 node task writes about 32 memo entries,
so a team at 200 tasks a day writes 6,400 files a day and around 2.3 million a year: roughly 9 GB
of disk for under 1 GB of content, with directory enumeration and rsync both degrading.

**The trigger** is the store passing about a million files, which `flash stats` says out loud when
it happens. **The fix** is an embedded key value store, redb or lmdb, in the same directory. Not a
database server: the store being one rsyncable directory is the entire mechanism behind a shared
team cache, and a server takes that away.

Two shapes that look adjacent and are not:

- **A graph or vector database for the memo.** The access pattern is get and put by a 32 byte key,
  write once, no traversal and no ranking. A specialised store is strictly worse than an embedded
  key value store here.
- **Vector search in context packs.** Packs are deterministic assembly on purpose, so that the
  model call downstream of a pack is cacheable. Retrieval that ranks by similarity changes the
  pack whenever the index changes, which moves every downstream action key and takes the hit rate
  to zero without producing a single wrong answer. This one is not a trade off, it is a
  contradiction with the core design.

Where a graph store would genuinely earn its place is the cross file symbol graph, which is the
real gap in impact analysis today and is listed below.

## Open items

- **Eta ignores lane queueing.** Pure critical path; a wide graph on a narrow lane finishes later
  than promised. Fixing it means a scheduling simulation rather than a longest path.
- **No live-model cassette yet.** The benchmark's cold column measures runtime overhead, not agent
  latency, until someone records one. That recording is the single highest-value next step: it turns
  every table from "the runtime is cheap" into "here is where agent latency goes", which is the
  paper's claim 1.
- **Shared team cache (d10) is mechanically ready but unproven.** The store is one directory and
  nothing in it is machine-specific; what is missing is a fetch/push path and a test with two
  stores.
- **Coverage-based test impact** is still syntactic. Conservative, so it over-selects rather than
  under-selects, but it is the honest weak point in the impact story for code.
- **xlsx and docx containers.** The models behind both are done; only the file format layer is
  missing.

## Next, in order

1. Record a cassette against a real planner/executor pair and re-run the benchmark. Everything else
   is downstream of having those numbers.
2. Lane-aware eta, once real adapters make contention common.
3. Shared cache fetch/push, with a two-store test.
4. The paper draft (§10.2), which needs 1 and can be written against the existing ablation table.
