//! Local structured logging via `tracing` (FR-014).
//!
//! Logs are written to a **daily-rolling file** in the OS log/data-local
//! directory through a non-blocking writer, so logging never blocks the UI
//! thread. The default level is `INFO`; `verbose` raises it to `DEBUG`. Retention
//! is capped at the 7 most recent files.
//!
//! [`init_logging`] returns the [`WorkerGuard`] for the non-blocking writer; the
//! caller (`main`) MUST keep it alive for the program's lifetime, otherwise
//! buffered log lines may be dropped on exit.
//!
//! Local-first (project-instructions §VI): logs go to disk only — no network,
//! no telemetry.
//!
//! # Local-first audit (FR-015, US7)
//!
//! Verified via `cargo tree -p ronin-app` for E003 Wave 5. The resolved
//! dependency graph contains **no** network-capable or telemetry/analytics crate:
//! no `reqwest` / `hyper` / `ureq` / `isahc` / `curl` / `surf`, no
//! `tungstenite` / `websocket`, no `tokio` net / `async-std` net, no
//! `tonic` / `quinn` (HTTP/2/QUIC), no `rustls` / `native-tls` / `openssl`
//! (TLS), and no `sentry` / `opentelemetry` / analytics SDK. The only I/O-
//! capable dependencies are local: `directories` (resolving the OS config/log
//! dirs), `rfd` (the native file-open/save dialog), `arboard` (system clipboard,
//! pulled transitively by egui), the `tracing*` stack (this local-file logger),
//! and `std::fs`. User-provided Rust source is parsed statically by `ronin-core`
//! and never executed. The `deny.toml` security gate and the
//! `network_audit_no_networking_or_telemetry_crates` regression test in
//! `tests/offline_logging.rs` enforce this policy continuously.

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// `ProjectDirs` triple identifying RONin's OS directories (matches `settings`).
const QUALIFIER: &str = "dev";
const ORGANIZATION: &str = "ronin";
const APPLICATION: &str = "RONin";

/// How many rolling log files to retain.
const MAX_LOG_FILES: usize = 7;

/// Initialise the global tracing subscriber over a non-blocking, daily-rolling
/// file writer (FR-014).
///
/// * Log directory: the OS data-local dir's `log/` subfolder
///   (`ProjectDirs::data_local_dir().join("log")`), created if absent. If no
///   project directory can be resolved, falls back to a `ronin-logs` folder in
///   the system temp dir so logging still works.
/// * Rotation: daily; retention capped at the 7 most recent files via the
///   appender builder's `max_log_files`.
/// * Level: `INFO` by default, `DEBUG` when `verbose` is set. An existing
///   `RUST_LOG` environment variable, if present and valid, overrides this.
///
/// Returns the [`WorkerGuard`]; **keep it alive** for the process lifetime.
#[must_use = "hold the WorkerGuard for the program's lifetime; dropping it may lose buffered logs"]
pub fn init_logging(verbose: bool) -> WorkerGuard {
    let log_dir = log_directory();
    // Best-effort create; if it fails the appender will surface errors lazily,
    // but we must not panic the app over a logging-setup hiccup.
    let _ = std::fs::create_dir_all(&log_dir);

    // Daily-rolling file appender with bounded retention.
    let file_appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("ronin")
        .filename_suffix("log")
        .max_log_files(MAX_LOG_FILES)
        .build(&log_dir)
        // If the appender can't be built (e.g. dir truly unwritable), fall back
        // to a plain daily appender, which never fails to construct.
        .unwrap_or_else(|_| tracing_appender::rolling::daily(&log_dir, "ronin.log"));

    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let default_level = if verbose { "debug" } else { "info" };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(true);

    // `try_init` rather than `init` so a double-call (e.g. in tests) is a no-op
    // instead of a panic.
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .try_init();

    guard
}

/// Resolve the directory log files are written to.
fn log_directory() -> std::path::PathBuf {
    directories::ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
        .map(|dirs| dirs.data_local_dir().join("log"))
        .unwrap_or_else(|| std::env::temp_dir().join("ronin-logs"))
}
