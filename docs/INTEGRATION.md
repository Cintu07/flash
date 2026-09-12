# Using flash from an agent you already use

flash is not a replacement for Claude Code, Codex or Cursor. It is a cache and a scheduler that
sits underneath one, so the work your agent has already done stops being done again.

The integration is MCP. `flash serve` speaks json-rpc 2.0 on stdio in two dialects at once: MCP
(`initialize`, `tools/list`, `tools/call`) for any MCP client, and a native dialect
(`flash/run`, `flash/status`, `flash/jobs`, `flash/cancel`, `flash/content`) for a thin client or
an ACP host.

## Claude Code

Add flash as an MCP server, either with the cli:

```
claude mcp add flash -- flash serve --root . \
  --planner-cmd "<your model command>" \
  --executor-cmd "<your model command>"
```

or by committing `.mcp.json` at the repo root so the whole team gets it (this repo ships one):

```json
{
  "mcpServers": {
    "flash": {
      "command": "flash",
      "args": ["serve", "--root", "."],
      "env": {}
    }
  }
}
```

Claude then has two tools:

| tool | what it does |
| --- | --- |
| `flash_run` | run an instruction over named files; repeat work is served from the cache |
| `flash_status` | fetch a task's result by id, including diagnostics and content hashes |

A model command is anything that reads one json request on stdin and prints one json response on
stdout. That is the entire contract, and flash has no vendor sdk in it, so whatever you already use
for inference works.

## What actually gets faster

Be precise about this, because the honest answer is narrower than "agents get faster".

flash does not make a **cold** task faster. A model still has to type the ops, and that floor is
8 to 20 s per real step on the critical path. What it removes is everything you are doing for the
second time:

- **Hot**, same request and unchanged inputs: zero model calls, single-digit milliseconds.
- **Warm**, same request with some inputs changed: only the dirty closure runs. A body edit does not
  even re-plan, because the planner depends on file *outlines*, not file contents.

Measured on three real 13 KB source files with decode simulated at 2 s per edit:

| run | model calls | wall |
| --- | --- | --- |
| cold | 4 | 3.62 s |
| warm (one file's body touched) | 1 | 2.28 s |
| hot | 0 | 6 ms |

So the question to ask before adopting it is not "is my agent slow" but **"how much of what my
agent does each week is a variation of something it already did?"** For a codebase being explored
once, the answer is nothing and flash is overhead. For a report regenerated every quarter, a
changelog rebuilt every release, a doc set regenerated from changing data, or a repo where the
same class of edit recurs, most of the work is repeat work.

## If you have a lot of documents already

This is the case the doc adapter exists for, and it is the one where the cache pays immediately,
because documents are regenerated far more often than code is rewritten.

1. Point a task at the documents that change together:

   ```
   flash run "update the revenue section for the new figures" docs/report.md \
     --planner-cmd ... --executor-cmd ...
   ```

2. The first run costs what it costs. Every later run pays only for the sections whose inputs
   moved. `methodology` and `definitions` are hot hits, and only pages containing changed blocks
   are re-rendered.

3. Nothing is written to your working tree. A task produces content; `flash show <hash>` prints
   it, and committing it is a separate, explicit step. That is what makes it safe to run against a
   document set you care about before you trust it.

## Sharing the cache with a team

The store is one directory (`.flash` by default) and nothing in it is machine-specific: blobs are
blake3-addressed, memo entries are keyed on content, and history is keyed on logical node names.
Two people on the same repo, the same adapter versions and the same model versions can share it,
and the second person's first run is warm.

The fetch/push path for that is not written yet, so today you would rsync or mount the directory.
The mechanism is ready; the ergonomics are not.

## What to check before trusting a result

- `flash run` prints op resolve rate and fallback rate on stderr. A high fallback rate means the
  op schema does not fit your language, not that the model is bad.
- A rung that cannot run reports **unavailable**, never a pass. If `cargo` is not on the path, the
  typecheck rung says so and the task fails rather than caching unverified work.
- A failed node is never memoized, so a failure costs you time but never poisons the cache.
