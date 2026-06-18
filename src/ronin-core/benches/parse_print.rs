//! Informational parse/print benchmark harness (TR-016).
//!
//! This is **informational only**: it measures parse and print throughput over
//! the corpus to inform later interactive-performance epics (E003+). It imposes
//! **no pass/fail gate** and is **never** a QC or release criterion for E001
//! (plan §Performance Goals / TR-016). There is no latency/throughput/memory
//! threshold and no assertion on timing.
//!
//! It uses **no external benchmark dependency** (no criterion): `harness = false`
//! in `Cargo.toml` lets this be a plain `fn main()` that times with
//! `std::time::Instant`. `std::fs` is used to load corpus fixtures — fine here
//! because a bench is not the WASM-clean core (TR-007 applies to the library, not
//! its dev harnesses).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ronin_core::{parse, print};

/// Recursively collect every `.ron` fixture under `dir`.
fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("ron") {
            out.push(path);
        }
    }
}

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("corpus")
}

/// Median of a sorted slice of nanosecond durations.
fn median(sorted: &[u128]) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[sorted.len() / 2]
}

fn main() {
    let mut fixtures = Vec::new();
    collect(&corpus_dir(), &mut fixtures);
    fixtures.sort();

    if fixtures.is_empty() {
        eprintln!("no corpus fixtures found under {}", corpus_dir().display());
        return;
    }

    // Load all fixtures up front (I/O is excluded from the measured region).
    let mut inputs: Vec<(String, String)> = Vec::new();
    let mut total_bytes = 0usize;
    for path in &fixtures {
        if let Ok(text) = fs::read_to_string(path) {
            total_bytes += text.len();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?")
                .to_string();
            inputs.push((name, text));
        }
    }

    const WARMUP: usize = 2;
    const RUNS: usize = 20;

    println!("# ronin-core parse/print benchmark (informational; no pass/fail gate)");
    println!(
        "# {} fixtures, {} total bytes, {RUNS} timed runs each (after {WARMUP} warmups)\n",
        inputs.len(),
        total_bytes
    );
    println!(
        "{:<32} {:>10} {:>14} {:>14}",
        "fixture", "bytes", "parse (µs)", "print (µs)"
    );
    println!("{}", "-".repeat(74));

    let mut grand_parse = 0u128;
    let mut grand_print = 0u128;

    for (name, src) in &inputs {
        // Warmup (excluded from timing).
        for _ in 0..WARMUP {
            let doc = parse(src);
            std::hint::black_box(print(&doc));
        }

        let mut parse_ns = Vec::with_capacity(RUNS);
        let mut print_ns = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            let t0 = Instant::now();
            let doc = std::hint::black_box(parse(src));
            parse_ns.push(t0.elapsed().as_nanos());

            let t1 = Instant::now();
            std::hint::black_box(print(&doc));
            print_ns.push(t1.elapsed().as_nanos());
        }
        parse_ns.sort_unstable();
        print_ns.sort_unstable();
        let p = median(&parse_ns);
        let q = median(&print_ns);
        grand_parse += p;
        grand_print += q;

        println!(
            "{:<32} {:>10} {:>14.2} {:>14.2}",
            truncate(name, 32),
            src.len(),
            p as f64 / 1000.0,
            q as f64 / 1000.0,
        );
    }

    println!("{}", "-".repeat(74));
    println!(
        "{:<32} {:>10} {:>14.2} {:>14.2}",
        "TOTAL (median sum)",
        total_bytes,
        grand_parse as f64 / 1000.0,
        grand_print as f64 / 1000.0,
    );
    println!("\n# informational only — no thresholds asserted (TR-016).");
}

/// Truncate `s` to at most `max` chars for table alignment.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}
