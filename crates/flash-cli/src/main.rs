//! flash: a thin client over the runtime (d9 - no editor, no ide).
//!
//!     flash run "<instruction>" <file>...   run a task against real models
//!     flash serve                           json-rpc + mcp over stdio
//!     flash demo [--live]                   the section 6 scenario, with stub models
//!     flash stats                           what the store has learned
//!     flash show <hash>                     print content by hash
//!
//! `run` needs a model command: anything that reads a json request on stdin and prints a json
//! response on stdout. That is the whole integration contract, so the runtime never depends on a
//! vendor sdk:
//!
//!     flash run "handle empty input" src/lib.rs \
//!       --planner-cmd "my-llm --model big" --executor-cmd "my-llm --model small"

use flash_adapter::Adapter;
use flash_adapter_code::CodeAdapter;
use flash_adapter_doc::DocAdapter;
use flash_adapter_sheets::SheetsAdapter;
use flash_adapter_slides::SlidesAdapter;
use flash_core::{Digest, NodeKind};
use flash_engine::{
    Engine, EngineConfig, Event, Events, NodeSpec, NodeStatus, RunReport, TaskGraph,
};
use flash_orchestrator::model::{CommandModel, ModelClient};
use flash_orchestrator::{Runtime, RuntimeConfig, Task};
use flash_server::{Server, ServerConfig};
use flash_store::{Store, TreeSpec, Worktree};
use flash_stub::{Bucket, StubExecutor, StubOp, mutate_source, stub_node};
use std::path::PathBuf;
use std::sync::Arc;

// ---- the demo scenario -------------------------------------------------------------------

const SECTIONS: [&str; 12] = [
    "summary",
    "methodology",
    "definitions",
    "revenue",
    "costs",
    "margins",
    "headcount",
    "pipeline",
    "risks",
    "outlook",
    "appendix",
    "glossary",
];

/// The section 6 task: "make a 30 page quarterly report pdf from these three csvs and last
/// quarter's report."
///
/// Durations are the PRD's own numbers for this scenario, in milliseconds. The executor scales
/// them down so the demo runs in seconds, which changes nothing about the ratios or the shape.
fn quarterly_report(rows: [&str; 3]) -> TaskGraph {
    let mut g = TaskGraph::new();
    for (i, marker) in rows.iter().enumerate() {
        g = g.with(NodeSpec::source(
            format!("csv:{i}"),
            Digest::of(marker.as_bytes()),
        ));
        // Deterministic, no model. Two of the three summaries round to the same numbers even
        // when the raw rows move, which is what keeps most sections hot next quarter.
        let mut op = StubOp::new(format!("summarize csv {i}"))
            .cost(1_800)
            .bucket(Bucket::Apply);
        if i > 0 {
            op = op.constant();
        }
        g = g.with(stub_node(format!("data:{i}"), NodeKind::Data, op).dep(format!("csv:{i}")));
    }

    for (i, name) in SECTIONS.iter().enumerate() {
        g = g.with(
            stub_node(
                format!("section:{name}"),
                NodeKind::Op,
                StubOp::new(*name).cost(9_000).bucket(Bucket::Decode),
            )
            .dep(format!("data:{}", i % 3)),
        );
    }

    let sections: Vec<String> = SECTIONS.iter().map(|s| format!("section:{s}")).collect();
    g = g.with(
        stub_node(
            "render:pdf",
            NodeKind::Render,
            StubOp::new("render 30 pages")
                .cost(38_000)
                .bucket(Bucket::Render),
        )
        .deps(sections.clone()),
    );
    g.with(
        stub_node(
            "verify:pages",
            NodeKind::Verify,
            StubOp::new("visual check")
                .cost(6_000)
                .bucket(Bucket::Verify),
        )
        .dep("render:pdf"),
    )
}

// ---- arguments ---------------------------------------------------------------------------

struct Args {
    cmd: String,
    rest: Vec<String>,
    store: PathBuf,
    root: PathBuf,
    live: bool,
    json: bool,
    scale: f64,
    planner_cmd: Option<String>,
    executor_cmd: Option<String>,
}

fn parse_args() -> Args {
    let mut args = Args {
        cmd: String::new(),
        rest: Vec::new(),
        store: PathBuf::from(".flash"),
        root: PathBuf::from("."),
        live: false,
        json: false,
        scale: 0.02,
        planner_cmd: None,
        executor_cmd: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--live" => args.live = true,
            "--json" => args.json = true,
            "--store" => args.store = it.next().map(PathBuf::from).unwrap_or(args.store),
            "--root" => args.root = it.next().map(PathBuf::from).unwrap_or(args.root),
            "--scale" => {
                if let Some(v) = it.next()
                    && let Ok(f) = v.parse()
                {
                    args.scale = f;
                }
            }
            "--planner-cmd" => args.planner_cmd = it.next(),
            "--executor-cmd" => args.executor_cmd = it.next(),
            "-h" | "--help" => args.cmd = "help".into(),
            other => {
                if args.cmd.is_empty() && !other.starts_with('-') {
                    args.cmd = other.to_string();
                } else {
                    args.rest.push(other.to_string());
                }
            }
        }
    }
    if args.cmd.is_empty() {
        args.cmd = "help".into();
    }
    args
}

#[tokio::main]
async fn main() {
    let args = parse_args();
    match args.cmd.as_str() {
        "run" => run(args).await,
        "worktree" => worktree(args),
        "churn" => churn(args),
        "impact" => impact(args),
        "serve" => serve(args).await,
        "demo" => demo(args).await,
        "stats" => stats(args),
        "show" => show(args),
        _ => help(),
    }
}

fn help() {
    println!(
        "flash - incremental artifact runtime\n\n\
         usage:\n  \
         flash run \"<instruction>\" <file>...  --planner-cmd CMD --executor-cmd CMD [--root DIR]\n  \
         flash serve [--root DIR] [--planner-cmd CMD] [--executor-cmd CMD]\n  \
         flash worktree <dest> [--root DIR]   a working tree that shares bytes with the store\n  \
         flash churn [N] [--root DIR]         what a commit in this repo actually rewrites\n  \
         flash impact <entity-id> [--root DIR]  which tests a change to that entity reaches\n  \
         flash demo [--live] [--json] [--scale F]\n  \
         flash stats [--store DIR]\n  \
         flash show <hash> [--store DIR]\n\n\
         a model command reads one json request on stdin and prints one json response on stdout.\n"
    );
}

// ---- run ----------------------------------------------------------------------------------

fn split_cmd(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

async fn run(args: Args) {
    let Some((instruction, files)) = args.rest.split_first() else {
        eprintln!("usage: flash run \"<instruction>\" <file>...");
        std::process::exit(2);
    };
    if files.is_empty() {
        eprintln!("at least one file is required");
        std::process::exit(2);
    }

    let (Some(planner_cmd), Some(executor_cmd)) = (&args.planner_cmd, &args.executor_cmd) else {
        eprintln!(
            "flash run needs --planner-cmd and --executor-cmd.\n\
             each is a command that reads a json request on stdin and prints a json response.\n\
             try `flash demo` to see the runtime work without a model."
        );
        std::process::exit(2);
    };

    let to_client = |cmd: &str| -> Arc<dyn ModelClient> {
        let parts = split_cmd(cmd);
        let rest: Vec<&str> = parts[1..].iter().map(|s| s.as_str()).collect();
        Arc::new(CommandModel::new(parts[0].clone(), &rest))
    };

    let runtime = Runtime::new(
        vec![
            Arc::new(CodeAdapter::new()),
            Arc::new(DocAdapter::new()),
            Arc::new(SheetsAdapter::new()),
            Arc::new(SlidesAdapter::new()),
        ],
        to_client(planner_cmd),
        to_client(executor_cmd),
        RuntimeConfig {
            workspace: Some(args.root.clone()),
            ..Default::default()
        },
    );

    let (events, printer) = if args.live {
        let (e, rx) = Events::channel();
        (e, Some(spawn_printer(rx)))
    } else {
        (Events::none(), None)
    };

    let report = Task::new(instruction.clone())
        .root(&args.root)
        .files(files.to_vec())
        .events(events)
        .run(&args.store, runtime.clone())
        .await;

    if let Some(p) = printer {
        let _ = p.await;
    }

    match report {
        Err(e) => {
            eprintln!("task failed: {e}");
            std::process::exit(1);
        }
        Ok(report) => {
            let store = match Store::open(&args.store) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("cannot reopen store: {e}");
                    std::process::exit(1);
                }
            };
            if args.json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            } else {
                print_summary(&report);
                for f in files {
                    if let Some(d) = flash_orchestrator::result_for(&report, f) {
                        println!(
                            "  {f} -> {} ({} bytes)",
                            d.short(),
                            store.content.get(d).map(|b| b.len()).unwrap_or(0)
                        );
                    }
                }
                println!(
                    "\n  nothing was written to disk. `flash show <hash>` prints a result; \
                     committing it is a separate, explicit step."
                );
            }
            let m = runtime.metrics.snapshot();
            eprintln!(
                "op resolve {:.0}%, fallback {:.0}%, repairs {}",
                m.op_resolve_rate() * 100.0,
                m.fallback_rate() * 100.0,
                m.repairs
            );
            if !report.ok() {
                std::process::exit(1);
            }
        }
    }
}

fn print_summary(report: &RunReport) {
    println!(
        "\n  {} nodes: {} computed, {} hit{}",
        report.nodes.len(),
        report.computed(),
        report.hits(),
        if report.saved_ms > 0 {
            format!(", {} saved", secs(report.saved_ms))
        } else {
            String::new()
        }
    );
    println!("  wall {}", secs(report.wall_ms));
    for n in report.nodes.iter().filter(|n| !n.diagnostics.is_empty()) {
        for d in &n.diagnostics {
            println!("  ! {}: {d}", n.key);
        }
    }
}

// ---- serve --------------------------------------------------------------------------------

async fn serve(args: Args) {
    let mut cfg = ServerConfig::new(&args.root);
    cfg.store_root = args.store.clone();
    cfg.planner_cmd = args.planner_cmd.as_deref().map(split_cmd);
    cfg.executor_cmd = args.executor_cmd.as_deref().map(split_cmd);

    let server = match Server::new(cfg) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot start: {e}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "flash daemon on stdio: json-rpc 2.0, mcp protocol {}. \
         tasks survive a client disconnect; reattach with flash/status.",
        flash_server::MCP_PROTOCOL_VERSION
    );
    if let Err(e) = server.serve_stdio().await {
        eprintln!("server stopped: {e}");
        std::process::exit(1);
    }
}

// ---- demo ---------------------------------------------------------------------------------

async fn demo(args: Args) {
    let store = match Store::open(&args.store) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot open store at {}: {e}", args.store.display());
            std::process::exit(1);
        }
    };
    let engine = Engine::new(store, EngineConfig::default());
    let exec = StubExecutor::scaled(args.scale);

    println!(
        "flash demo - section 6 scenario, stub models at {:.0}x speed\n\
         a 30 page quarterly report: 3 csvs, 3 data summaries, 12 sections, 1 render, 1 check\n",
        1.0 / args.scale
    );

    let cold_graph = quarterly_report(["q3-rows-a", "q3-rows-b", "q3-rows-c"]);
    let cold = run_one(
        &engine,
        "quarterly",
        &cold_graph,
        exec.clone(),
        &args,
        "cold",
    )
    .await;

    let mut warm_graph = quarterly_report(["q3-rows-a", "q3-rows-b", "q3-rows-c"]);
    for (i, marker) in ["q4-rows-a", "q4-rows-b", "q4-rows-c"].iter().enumerate() {
        mutate_source(&mut warm_graph, &format!("csv:{i}"), marker);
    }
    let warm = run_one(
        &engine,
        "quarterly",
        &warm_graph,
        exec.clone(),
        &args,
        "warm (three new csvs)",
    )
    .await;

    let hot = run_one(
        &engine,
        "quarterly",
        &warm_graph,
        exec.clone(),
        &args,
        "hot (nothing changed)",
    )
    .await;

    if args.json {
        let all = serde_json::json!({ "cold": cold, "warm": warm, "hot": hot });
        println!("{}", serde_json::to_string_pretty(&all).unwrap());
        return;
    }

    println!("\n  regime  wall      computed  hit  saved     first change  eta at start");
    for (label, r) in [("cold", &cold), ("warm", &warm), ("hot", &hot)] {
        println!(
            "  {:<7} {:<9} {:<9} {:<4} {:<9} {:<13} {}",
            label,
            secs(r.wall_ms),
            r.computed(),
            r.hits(),
            secs(r.saved_ms),
            r.first_change_ms.map(secs).unwrap_or("-".into()),
            r.eta_at_start.render(),
        );
    }

    println!("\n  where the cold run's time went (attribution per step kind)");
    let a = cold.attribution;
    for (name, ms) in [
        ("decode", a.decode_ms),
        ("apply", a.apply_ms),
        ("render", a.render_ms),
        ("verify", a.verify_ms),
    ] {
        let pct = if a.total_ms() > 0 {
            ms as f64 / a.total_ms() as f64 * 100.0
        } else {
            0.0
        };
        println!("  {name:<8} {:>8}  {pct:>5.1}%", secs(ms));
    }

    println!(
        "\n  warm recomputed {} of {} nodes; the rest were served from the memo store.",
        warm.computed(),
        warm.nodes.len()
    );
    let still_hot: Vec<&str> = warm
        .nodes
        .iter()
        .filter(|n| n.status == NodeStatus::Hit && n.key.as_str().starts_with("section:"))
        .map(|n| n.key.as_str())
        .collect();
    println!(
        "  sections that cost nothing this quarter: {}",
        still_hot.join(", ")
    );
}

fn spawn_printer(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Event>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let t0 = std::time::Instant::now();
        while let Some(ev) = rx.recv().await {
            let at = t0.elapsed().as_millis();
            match ev {
                Event::NodeHit { key, saved_ms, .. } => {
                    println!("    {at:>6}ms  hit      {key}  (saved {})", secs(saved_ms))
                }
                Event::NodeStarted { key, job, .. } => {
                    let tag = if job.is_some() { "job" } else { "run" };
                    println!("    {at:>6}ms  {tag}      {key}")
                }
                Event::JobProgress { key, progress, .. } => println!(
                    "    {at:>6}ms  ..       {key}  {}%  {}",
                    progress.fraction.map(|f| (f * 100.0) as u32).unwrap_or(0),
                    progress.phase
                ),
                Event::GraphExpanded { key, added, .. } => {
                    println!("    {at:>6}ms  plan     {key} emitted {added} nodes")
                }
                Event::NodeFinished {
                    key,
                    status,
                    exec_ms,
                } => println!(
                    "    {at:>6}ms  done     {key}  {status:?} {}",
                    secs(exec_ms)
                ),
                Event::EtaUpdated { eta } => println!("    {at:>6}ms  eta      {}", eta.render()),
                Event::TaskFinished { wall_ms, .. } => {
                    println!("    {at:>6}ms  finished in {}", secs(wall_ms))
                }
                _ => {}
            }
        }
    })
}

async fn run_one(
    engine: &Engine,
    task: &str,
    graph: &TaskGraph,
    exec: Arc<StubExecutor>,
    args: &Args,
    label: &str,
) -> RunReport {
    if !args.json {
        println!("  run: {label}");
    }
    let (events, printer) = if args.live {
        let (e, rx) = Events::channel();
        (e, Some(spawn_printer(rx)))
    } else {
        (Events::none(), None)
    };

    let report = engine
        .run(task, graph, exec, events)
        .await
        .expect("the demo graph is valid");
    if let Some(p) = printer {
        let _ = p.await;
    }
    report
}

// ---- store commands -----------------------------------------------------------------------

fn stats(args: Args) {
    let store = match Store::open(&args.store) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot open store at {}: {e}", args.store.display());
            std::process::exit(1);
        }
    };
    println!("store: {}", store.root().display());

    // Report the shape, not just the size. Every part of this store is many small files, which is
    // the right call while a store is small and the wrong one at a few million entries: a 45 byte
    // history record still costs a filesystem block. The number that decides whether this needs an
    // embedded key value store instead is the ratio below, so print it rather than argue about it.
    const BLOCK: u64 = 4096;
    let mut grand_files = 0u64;
    let mut grand_bytes = 0u64;
    let mut grand_allocated = 0u64;

    println!("\n  part      files      bytes   mean   on disk  waste");
    for part in ["blobs", "memo", "history", "journal"] {
        let dir = store.root().join(part);
        let (mut files, mut bytes) = (0u64, 0u64);
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if let Ok(m) = e.metadata() {
                    files += 1;
                    bytes += m.len();
                }
            }
        }
        let allocated = files * BLOCK.max(1);
        grand_files += files;
        grand_bytes += bytes;
        grand_allocated += allocated;
        println!(
            "  {part:<9} {files:>5} {bytes:>10} {:>6} {:>9} {:>5.1}x",
            bytes.checked_div(files).unwrap_or(0),
            allocated,
            if bytes > 0 {
                allocated as f64 / bytes as f64
            } else {
                0.0
            }
        );
    }

    let (hits, misses) = store.memo.counters();
    println!(
        "\n  {grand_files} files, {grand_bytes} bytes of content occupying about {grand_allocated} \
         bytes of disk"
    );
    println!("  memo lookups this process: {hits} hit, {misses} miss");
    if grand_files > 500_000 {
        println!(
            "\n  Past roughly a million small files this layout stops paying for itself: the waste\n  \
             column is the cost, and enumerating or rsyncing the store gets slow. The fix is an\n  \
             embedded key value store in the same directory, not a database server, because the\n  \
             store being one rsyncable directory is what makes a shared team cache work at all."
        );
    }
}

fn show(args: Args) {
    let Some(hash) = args.rest.first() else {
        eprintln!("usage: flash show <hash>");
        std::process::exit(2);
    };
    let store = match Store::open(&args.store) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot open store: {e}");
            std::process::exit(1);
        }
    };
    let Some(digest) = Digest::from_hex(hash) else {
        eprintln!("{hash} is not a content hash");
        std::process::exit(2);
    };
    match store.content.get(&digest) {
        Ok(bytes) => print!("{}", String::from_utf8_lossy(&bytes)),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

// ---- worktree ------------------------------------------------------------------------------

/// Give an agent its own working tree without giving it its own copy of the bytes.
///
/// Four agents on one repository means four checkouts and four times the disk. The store already
/// holds every file version exactly once, so a tree is a directory of hard links into it, and the
/// fourth agent costs directory entries rather than gigabytes.
fn worktree(args: Args) {
    let Some(dest) = args.rest.first().cloned() else {
        eprintln!(
            "usage: flash worktree <dest> [--root DIR] [--store DIR]\n\
             ingests --root once, then materializes it at <dest> by hard linking from the store."
        );
        std::process::exit(2);
    };

    let store = match Store::open(&args.store) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot open store: {e}");
            std::process::exit(1);
        }
    };

    // Skip what no agent needs a private copy of, and what would dwarf the source anyway.
    let skip = [".git", "target", "node_modules", ".flash"];
    let source = Worktree::new(&store.content, &args.root);
    let spec: TreeSpec = match source.ingest(&skip) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read {}: {e}", args.root.display());
            std::process::exit(1);
        }
    };

    let tree = Worktree::new(&store.content, &dest);
    match tree.materialize(&spec) {
        Err(e) => {
            eprintln!("cannot materialize {dest}: {e}");
            std::process::exit(1);
        }
        Ok(stats) => {
            let mb = |b: u64| format!("{:.1} MB", b as f64 / 1_048_576.0);
            println!("  {dest}");
            println!(
                "  {} files, {} logical, {} written",
                stats.files,
                mb(stats.logical_bytes),
                mb(stats.bytes_written())
            );
            println!(
                "  {} linked to content already stored, {} copied, {} unchanged",
                stats.linked, stats.copied, stats.unchanged
            );
            if stats.copied > 0 {
                println!(
                    "\n  {} files could not be linked and were copied. That happens across volumes\n  \
                     or on a filesystem without hard links; the tree is correct either way.",
                    stats.copied
                );
            }
            println!(
                "\n  store blobs are read only. Tools that write a temp file and rename it over the\n  \
                 target are safe; a tool that writes in place will be refused, and `detach` gives\n  \
                 that file a private copy."
            );
        }
    }
}

// ---- impact --------------------------------------------------------------------------------

/// What breaks if this changes?
///
/// Indexes the repository, then walks reference edges across every file to find the tests and
/// files a change to one entity can reach. Useful on its own, and it is the same index the verify
/// ladder uses to pick which tests to run after an edit.
fn impact(args: Args) {
    let Some(target) = args.rest.first().cloned() else {
        eprintln!(
            "usage: flash impact <entity-id> [--root DIR]\n\
             run without an entity id to list what is indexed, e.g. fn:Parser::parse"
        );
        std::process::exit(2);
    };

    let adapter = CodeAdapter::new();
    let mut files: Vec<(flash_adapter::Artifact, flash_adapter::Outline)> = Vec::new();
    let mut stack = vec![args.root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                if matches!(name.as_ref(), "target" | ".git" | ".flash" | "node_modules") {
                    continue;
                }
                stack.push(path);
                continue;
            }
            let rel = path
                .strip_prefix(&args.root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if !adapter.handles(&rel) {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let artifact = flash_adapter::Artifact::new(rel, bytes);
            if let Ok(outline) = adapter.outline(&artifact) {
                files.push((artifact, outline));
            }
        }
    }

    if files.is_empty() {
        eprintln!(
            "no source files this runtime handles under {}",
            args.root.display()
        );
        std::process::exit(1);
    }

    let started = std::time::Instant::now();
    let ix = flash_adapter_code::index::RepoIndex::build(&files);
    let build_ms = started.elapsed().as_millis();

    println!(
        "  indexed {} entities across {} files in {build_ms} ms",
        ix.len(),
        ix.files()
    );

    if ix.get(&target).is_none() {
        eprintln!("\n  {target} is not in the index. Nearby ids:");
        let needle = target.rsplit(':').next().unwrap_or(&target).to_lowercase();
        let mut shown = 0;
        for (id, _) in files
            .iter()
            .flat_map(|(_, o)| o.entities.iter().map(|e| (e.id.clone(), e.kind.clone())))
        {
            if id.to_lowercase().contains(&needle) && shown < 10 {
                eprintln!("    {id}");
                shown += 1;
            }
        }
        std::process::exit(1);
    }

    let seeds = vec![target.clone()];
    let tests = ix.reachable_tests(&seeds);
    let touched_files = ix.reachable_files(&seeds);
    let reached = ix.reachable(&seeds);

    println!("\n  changing {target} reaches:");
    println!("    {} entities", reached.len());
    println!("    {} files", touched_files.len());
    println!("    {} tests", tests.len());

    if !tests.is_empty() {
        println!("\n  tests worth running:");
        for t in tests.iter().take(20) {
            println!("    {t}");
        }
        if tests.len() > 20 {
            println!("    and {} more", tests.len() - 20);
        }
    }

    println!(
        "\n  Resolution is by simple name, so this over-links where a name is reused rather than\n  \
         missing an edge. Extra tests cost seconds; a missed test costs a false green."
    );
}

// ---- churn ---------------------------------------------------------------------------------

/// Answer "is this worth it for my repo" with the repo's own history.
fn churn(args: Args) {
    let repo = args.root.clone();
    let commits: usize = args
        .rest
        .first()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);

    let git = |a: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(a)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).to_string())
    };

    if git(&["rev-parse", "--git-dir"]).is_none() {
        eprintln!("{} is not a git repository", repo.display());
        std::process::exit(1);
    }

    let adapter = CodeAdapter::new();
    let revs: Vec<String> = git(&["log", "--format=%H", "-n", &commits.to_string()])
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    if revs.len() < 2 {
        eprintln!("need at least two commits");
        std::process::exit(1);
    }

    // One long-lived `git cat-file --batch` instead of a `git show` per file.
    //
    // The per-file version spawned two processes for every changed file, which on a hundred
    // commits is a thousand processes, and on Windows that storm wedges: the tool sat at zero cpu
    // with a child git that never returned. Reading blobs over one pipe is both the fix and about
    // ten times faster, and it is how git itself expects to be scripted.
    let mut cat = match std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["cat-file", "--batch"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot start git cat-file: {e}");
            std::process::exit(1);
        }
    };
    let mut cat_in = cat.stdin.take().expect("stdin was piped");
    let mut cat_out = std::io::BufReader::new(cat.stdout.take().expect("stdout was piped"));

    let mut at = |rev: &str, path: &str| -> Option<Vec<u8>> {
        use std::io::{BufRead, Read, Write};
        writeln!(cat_in, "{rev}:{path}").ok()?;
        cat_in.flush().ok()?;

        // Header is either "<sha> <type> <size>" or "<what> missing".
        let mut header = String::new();
        cat_out.read_line(&mut header).ok()?;
        let fields: Vec<&str> = header.split_whitespace().collect();
        if fields.len() < 3 {
            return None; // missing object, and nothing follows it on the pipe
        }
        let size: usize = fields[2].parse().ok()?;

        // Drain the body even when it is not a blob. git writes the object after the header
        // whatever its type, so returning early here leaves those bytes in the pipe, the next
        // header read consumes object data instead, and the read after that blocks forever on a
        // length parsed out of garbage. One unread tree object wedges the whole run.
        let mut body = vec![0u8; size + 1]; // git appends a newline after the object
        cat_out.read_exact(&mut body).ok()?;
        body.truncate(size);

        (fields[1] == "blob").then_some(body)
    };

    // Guard rails, because this runs against repositories nobody has looked at first. A single
    // generated file or a thousand file reformat commit should cost a skip line, never a hang.
    const MAX_FILE_BYTES: usize = 512 * 1024;
    const MAX_FILES_PER_COMMIT: usize = 200;

    let (mut files, mut total, mut changed) = (0usize, 0usize, 0usize);
    let (mut skipped_big, mut skipped_wide) = (0usize, 0usize);
    let mut ratios: Vec<f64> = Vec::new();

    for (i, pair) in revs.windows(2).enumerate() {
        if i % 10 == 0 {
            eprint!("\r  {i}/{} commits", revs.len() - 1);
        }
        let (newer, older) = (&pair[0], &pair[1]);
        let Some(list) = git(&["diff", "--name-only", older, newer]) else {
            continue;
        };
        let touched: Vec<&str> = list
            .lines()
            .filter(|p| !p.is_empty() && adapter.handles(p))
            .collect();
        // A commit that rewrites the world is a reformat or a vendor drop, and averaging it in
        // says more about that one commit than about how the project is worked on.
        if touched.len() > MAX_FILES_PER_COMMIT {
            skipped_wide += 1;
            continue;
        }
        for path in touched {
            let Some(new_bytes) = at(newer, path) else {
                continue;
            };
            if new_bytes.len() > MAX_FILE_BYTES {
                skipped_big += 1;
                continue;
            }
            let new_art = flash_adapter::Artifact::new(path.to_string(), new_bytes);
            let Ok(new_outline) = adapter.outline(&new_art) else {
                continue;
            };
            if new_outline.entities.is_empty() {
                continue;
            }
            let (old_art, old_outline) = match at(older, path) {
                Some(b) => {
                    let a = flash_adapter::Artifact::new(path.to_string(), b);
                    match adapter.outline(&a) {
                        Ok(o) => (a, o),
                        Err(_) => continue,
                    }
                }
                None => (
                    flash_adapter::Artifact::new(path.to_string(), Vec::new()),
                    flash_adapter::Outline {
                        path: path.to_string(),
                        entities: Vec::new(),
                    },
                ),
            };
            let delta =
                flash_adapter::delta_between(&old_outline, &new_outline, &old_art, &new_art);
            let c = delta.changed.len() + delta.added.len() + delta.removed.len();
            files += 1;
            total += new_outline.entities.len();
            changed += c;
            ratios.push(c as f64 / new_outline.entities.len().max(1) as f64);
        }
    }

    drop(cat_in);
    let _ = cat.wait();
    eprint!("\r                              \r");
    if files == 0 {
        eprintln!("no source files this runtime handles were touched in those commits");
        std::process::exit(1);
    }
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap());

    if skipped_big > 0 || skipped_wide > 0 {
        println!(
            "  skipped {skipped_big} files over 512 KB and {skipped_wide} commits touching over \
             200 files"
        );
    }
    println!("  {} commits, {} file revisions", revs.len() - 1, files);
    println!("  {total} entities in the files those commits touched");
    println!("  {changed} of them actually changed");
    println!(
        "\n  a commit in this repo rewrites {:.1}% of the entities in the files it touches",
        changed as f64 / total as f64 * 100.0
    );
    println!(
        "  median per file revision: {:.1}%",
        ratios[ratios.len() / 2] * 100.0
    );
    println!(
        "\n  An agent that re-reads and re-writes whole files does work proportional to {total}.\n  \
         One that edits named entities and caches the rest does work proportional to {changed}.\n  \
         No model ran to produce this: it is your history, parsed."
    );
}

fn secs(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.2}s", ms as f64 / 1000.0)
    }
}
