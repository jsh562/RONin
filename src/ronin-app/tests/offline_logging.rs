//! Offline / private + local-logging tests (T048/T045/T046, US7, FR-014/FR-016/FR-015).
//!
//! Three concerns, all kept off the real OS config/log directories by using
//! temp paths and the test-only path-injecting helpers
//! ([`AppSettings::save_to`] / [`AppSettings::load_from`]):
//!
//! * **logging (FR-014)** — the daily-rolling, keep-7 file appender builds and
//!   writes a log line to a temp dir without panicking, and [`init_logging`]
//!   returns a [`WorkerGuard`].
//! * **settings (FR-016)** — [`AppSettings`] round-trips through `save_to`/
//!   `load_from`, and a corrupt/garbage file recovers to defaults without panic.
//! * **local-first audit (FR-015)** — a lightweight regression guard that scans
//!   the resolved dependency list for network/telemetry crate names and asserts
//!   none are present (see [`network_audit`]).
//!
//! Honest limitation: `init_logging` writes to the OS data-local `log/` dir,
//! which is not redirectable without an env hook, so the rolling-file behavior is
//! exercised by constructing the *same* appender (daily rotation + `max_log_files`)
//! against a temp dir directly. `init_logging` itself is only asserted to return a
//! guard and not panic — it shares its appender configuration with the tested
//! construction.

use std::collections::BTreeMap;

use ronin_app::settings::{AppSettings, WindowGeometry};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a unique temp directory for a test and return its path.
fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ronin_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

// ---------------------------------------------------------------------------
// Settings round-trip (FR-016)
// ---------------------------------------------------------------------------

#[test]
fn settings_round_trip_through_save_to_load_from() {
    // FR-016: settings persist and restore. Use a temp path via the test-only
    // path-injecting helpers so the real OS config file is never touched.
    let dir = unique_temp_dir("settings_rt");
    let path = dir.join("settings.json");

    let mut prefs = BTreeMap::new();
    prefs.insert("theme".to_string(), "dark".to_string());
    prefs.insert("font".to_string(), "mono".to_string());
    let original = AppSettings {
        window_geometry: Some(WindowGeometry {
            pos: Some((100.0, 200.0)),
            size: (1024.0, 768.0),
        }),
        preferences: prefs,
        large_file_threshold: 1_234_567,
        ..AppSettings::default()
    };

    original.save_to(&path).expect("save settings to temp path");
    let loaded = AppSettings::load_from(&path);

    assert_eq!(loaded, original, "settings must round-trip byte-for-byte");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn settings_save_to_creates_missing_parent_dirs() {
    // FR-016: `save_to` must create the config directory if it does not yet
    // exist, so a first-run write never fails because the dir is missing.
    let dir = unique_temp_dir("settings_mkdir");
    let nested = dir.join("a").join("b").join("settings.json");

    AppSettings::default()
        .save_to(&nested)
        .expect("save must create parent dirs");
    assert!(
        nested.exists(),
        "settings file must be written under new dirs"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn corrupt_settings_file_yields_defaults_without_panic() {
    // FR-016 / project-instructions §I: a corrupt settings file must never panic
    // and must recover to defaults so a bad file can't lock the user out.
    let dir = unique_temp_dir("settings_corrupt");
    let path = dir.join("settings.json");
    std::fs::write(&path, b"{ this is not valid json at all >>>").expect("write garbage");

    let loaded = AppSettings::load_from(&path);
    assert_eq!(
        loaded,
        AppSettings::default(),
        "corrupt settings must fall back to defaults"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_settings_file_yields_defaults() {
    // FR-016: an absent settings file is the first-run case — defaults, no error.
    let dir = unique_temp_dir("settings_missing");
    let path = dir.join("does_not_exist.json");

    let loaded = AppSettings::load_from(&path);
    assert_eq!(loaded, AppSettings::default());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn settings_persist_no_session_state() {
    // FR-016: settings must NOT carry any session/document state. The serialized
    // form must contain only the three known fields and no path/tab/document keys.
    let dir = unique_temp_dir("settings_no_session");
    let path = dir.join("settings.json");
    AppSettings::default()
        .save_to(&path)
        .expect("save defaults");
    let json = std::fs::read_to_string(&path).expect("read back settings json");

    for forbidden in [
        "open_documents",
        "documents",
        "tabs",
        "tab_order",
        "paths",
        "session",
        "recent",
    ] {
        assert!(
            !json.contains(forbidden),
            "settings JSON must not persist session state (found `{forbidden}`)"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Logging (FR-014)
// ---------------------------------------------------------------------------

#[test]
fn rolling_appender_builds_and_writes_to_temp_dir() {
    // FR-014: the daily-rolling, keep-7 appender — the exact configuration
    // `init_logging` uses — builds against a temp dir and a written line lands in
    // a file, without panicking. We construct the appender directly because the
    // OS log dir `init_logging` targets is not redirectable in a test (honest
    // limitation; see module docs).
    use std::io::Write;

    let dir = unique_temp_dir("logging");
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("ronin")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)
        .expect("rolling appender (daily, keep-7) must build");

    let (non_blocking, guard) = tracing_appender::non_blocking(appender);
    {
        let mut writer = non_blocking;
        writeln!(writer, "ronin test log line").expect("write log line");
    }
    // Drop the guard to flush the non-blocking writer to disk before we assert.
    drop(guard);

    // A daily-rolling file should now exist in the temp dir (name carries a date
    // suffix, so match by prefix rather than an exact name).
    let mut found = false;
    for entry in std::fs::read_dir(&dir).expect("read temp log dir") {
        let entry = entry.expect("dir entry");
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("ronin") && name.ends_with("log") {
            found = true;
            break;
        }
    }
    assert!(found, "a ronin*.log file must be created by the appender");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_logging_returns_guard_without_panicking() {
    // FR-014: `init_logging` must construct the global subscriber + non-blocking
    // writer and return a WorkerGuard without panicking. The guard's only role is
    // to keep the writer alive; we simply assert it constructs. (Writing into the
    // OS log dir is the honest, non-redirectable part — covered by the temp-dir
    // appender test above.)
    let guard = ronin_app::logging::init_logging(false);
    // Emit an event; with the global subscriber installed this must not panic.
    tracing::info!("init_logging smoke event");
    drop(guard);
}

// ---------------------------------------------------------------------------
// Local-first / no-network audit (FR-015)
// ---------------------------------------------------------------------------

/// Crate-name fragments for network or telemetry capabilities that MUST NOT
/// appear anywhere in `ronin-app`'s dependency graph (FR-015, project-
/// instructions §VI "Local-First & Private").
///
/// Matched as substrings against the `name = "..."` lines of `Cargo.lock`. This
/// is a lightweight regression guard; the authoritative gate is `deny.toml` plus
/// the `cargo tree` audit recorded in `logging.rs`.
const NETWORK_DENYLIST: &[&str] = &[
    "reqwest",
    "hyper",
    "ureq",
    "isahc",
    "curl",
    "surf",
    "tungstenite",
    "tokio-tungstenite",
    "websocket",
    "async-std",
    "tonic",
    "quinn",
    "rustls",
    "native-tls",
    "openssl",
    "sentry",
    "opentelemetry",
    "analytics",
    "telemetry",
    "datadog",
    "posthog",
    "mixpanel",
    "segment",
];

#[test]
fn network_audit_no_networking_or_telemetry_crates() {
    // FR-015: confirm no network-capable or telemetry crate is in the resolved
    // dependency graph. Scan the workspace `Cargo.lock` (the resolved tree) for
    // any denylisted crate name. Honest scope note: this scans the WORKSPACE lock
    // (shared with ronin-core/ronin-types, which are local-first by construction), so
    // a positive would still be a real local-first violation to investigate.
    let lock = locate_cargo_lock();
    let contents = std::fs::read_to_string(&lock)
        .unwrap_or_else(|e| panic!("read Cargo.lock at {}: {e}", lock.display()));

    let mut offenders = Vec::new();
    for line in contents.lines() {
        let trimmed = line.trim();
        // Package names appear as `name = "<crate>"` in Cargo.lock.
        let Some(rest) = trimmed.strip_prefix("name = \"") else {
            continue;
        };
        let Some(name) = rest.strip_suffix('"') else {
            continue;
        };
        for needle in NETWORK_DENYLIST {
            // Exact or hyphen-boundary match to avoid false hits on unrelated
            // crates that merely contain a fragment (e.g. "future" vs nothing
            // here, but keep the match tight).
            if name == *needle || name.starts_with(&format!("{needle}-")) {
                offenders.push(name.to_string());
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "FR-015 local-first violation: network/telemetry crates in the dependency \
         graph: {offenders:?}"
    );
}

/// Resolve the workspace `Cargo.lock` path by walking up from this test's source
/// directory until a `Cargo.lock` is found.
fn locate_cargo_lock() -> std::path::PathBuf {
    // `CARGO_MANIFEST_DIR` is `.../src/ronin-app`; the lock lives at the workspace
    // root, a couple of levels up. Walk ancestors to be robust to layout shifts.
    let start = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for dir in start.ancestors() {
        let candidate = dir.join("Cargo.lock");
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!(
        "could not locate Cargo.lock above {}",
        env!("CARGO_MANIFEST_DIR")
    );
}
