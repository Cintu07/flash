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
use flash_store::Store;
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
    let (hits, misses) = store.memo.counters();
    println!("store: {}", store.root().display());
    println!("memo lookups this process: {hits} hit, {misses} miss");
    println!("(node history is on disk under history/, keyed by node key)");
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

fn secs(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.2}s", ms as f64 / 1000.0)
    }
}
