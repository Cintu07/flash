//! One command regenerates every table and chart (section 7).
//!
//!     flash-bench                      run bench/tasks, write bench/report/
//!     flash-bench --tasks DIR --out DIR
//!     flash-bench --json               print measurements as json instead

use flash_bench::{BenchTask, flame_svg, report, run_all};
use std::path::PathBuf;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let mut tasks_dir = PathBuf::from("bench/tasks");
    let mut out_dir = PathBuf::from("bench/report");
    let mut json = false;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--tasks" => tasks_dir = args.next().map(PathBuf::from).unwrap_or(tasks_dir),
            "--out" => out_dir = args.next().map(PathBuf::from).unwrap_or(out_dir),
            "--json" => json = true,
            "-h" | "--help" => {
                println!("flash-bench [--tasks DIR] [--out DIR] [--json]");
                return;
            }
            _ => {}
        }
    }

    let tasks = match BenchTask::load_dir(&tasks_dir) {
        Ok(t) if !t.is_empty() => t,
        Ok(_) => {
            eprintln!("no tasks found in {}", tasks_dir.display());
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("could not read {}: {e}", tasks_dir.display());
            std::process::exit(1);
        }
    };

    eprintln!("running {} tasks x 4 ablations x 3 regimes", tasks.len());
    let measurements = match run_all(&tasks).await {
        Ok(m) => m,
        Err(e) => {
            eprintln!("benchmark failed: {e}");
            std::process::exit(1);
        }
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&measurements).unwrap());
        return;
    }

    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("could not create {}: {e}", out_dir.display());
        std::process::exit(1);
    }
    let md = report(&measurements);
    let svg = flame_svg(&measurements);
    let raw = serde_json::to_string_pretty(&measurements).unwrap();

    let _ = std::fs::write(out_dir.join("report.md"), &md);
    let _ = std::fs::write(out_dir.join("attribution.svg"), &svg);
    let _ = std::fs::write(out_dir.join("measurements.json"), &raw);

    println!("{md}");
    eprintln!(
        "wrote {}, {} and {}",
        out_dir.join("report.md").display(),
        out_dir.join("attribution.svg").display(),
        out_dir.join("measurements.json").display()
    );
}
