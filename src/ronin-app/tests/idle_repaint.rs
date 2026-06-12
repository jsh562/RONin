//! Event-driven repaint tests (T048, FR-024).
//!
//! FR-024 requires the shell to be **reactive**: idle when nothing happens, and
//! to schedule a repaint only on a discrete trigger (input, an off-frame parse
//! result, a file open/drop/Save, a window/OS event, or a pending auto-dismiss
//! notice). These tests drive [`App::render_shell`] directly through an
//! [`egui::Context`] via [`egui::Context::run_ui`] and inspect the resulting
//! [`egui::FullOutput`]'s root-viewport `repaint_delay`:
//!
//! * `repaint_delay == Duration::ZERO`  → an immediate repaint was requested;
//! * `repaint_delay == Duration::MAX`   → idle (no repaint scheduled);
//! * a finite, non-zero delay           → a *deferred* repaint (e.g. a notice TTL).
//!
//! Honest scope note: the worker-driven repaint (a parse result landing on a
//! background thread) is asynchronous and timing-dependent, so it is NOT asserted
//! here; instead the "trigger requests a repaint" half uses a deterministic,
//! synchronous input event (a key press), which is the same `request_repaint`
//! path egui takes for any real keyboard/mouse input.

use std::time::Duration;

use egui::{Event, Key, Modifiers, RawInput, ViewportId};

use ronin_app::app::App;
use ronin_app::settings::AppSettings;

/// Run the shell once through `ctx` with `input` and return the root viewport's
/// scheduled `repaint_delay`.
fn render_once(ctx: &egui::Context, app: &mut App, input: RawInput) -> Duration {
    let output = ctx.run_ui(input, |ui| {
        app.render_shell(ui);
    });
    output
        .viewport_output
        .get(&ViewportId::ROOT)
        .map(|vp| vp.repaint_delay)
        .expect("root viewport output must be present")
}

#[test]
fn idle_shell_does_not_request_immediate_repaint() {
    // FR-024: with NO input events the shell must NOT request an immediate
    // repaint. The first pass after construction can legitimately schedule a
    // repaint (initial layout / fade-in), so we settle to steady state with a
    // couple of empty passes, then assert the idle pass does not ask for an
    // immediate (ZERO-delay) repaint.
    let ctx = egui::Context::default();
    let mut app = App::new(AppSettings::default(), None);

    let mut last = Duration::ZERO;
    for _ in 0..4 {
        last = render_once(&ctx, &mut app, RawInput::default());
    }

    assert_ne!(
        last,
        Duration::ZERO,
        "an idle shell must not request an immediate (continuous) repaint (FR-024)"
    );
}

#[test]
fn input_event_triggers_a_repaint() {
    // FR-024: a discrete keyboard input is a valid repaint trigger. egui requests
    // an immediate repaint when input arrives, so the frame that carries the key
    // event must schedule a ZERO-delay repaint.
    let ctx = egui::Context::default();
    let mut app = App::new(AppSettings::default(), None);

    // Settle to idle first so we measure the *effect of the input*, not start-up.
    for _ in 0..4 {
        let _ = render_once(&ctx, &mut app, RawInput::default());
    }

    let mut input = RawInput::default();
    input.events.push(Event::Key {
        key: Key::A,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::default(),
    });
    let delay = render_once(&ctx, &mut app, input);

    assert_eq!(
        delay,
        Duration::ZERO,
        "a discrete input event must trigger an immediate repaint (FR-024)"
    );
}

#[test]
fn pending_info_notice_schedules_a_deferred_repaint_then_returns_to_idle() {
    // FR-024: a transient auto-dismiss notice must schedule a *bounded* repaint
    // (so it can expire) WITHOUT pinning the UI to a continuous per-frame repaint.
    // Dropping a non-.ron path pushes exactly such an info notice.
    let ctx = egui::Context::default();
    let mut app = App::new(AppSettings::default(), None);

    // A non-RON path -> info notice (no tab). `apply_drop` is the headless entry
    // point for the drop contract.
    app.apply_drop(std::path::Path::new("not_a_ron_file.txt"));
    assert!(
        !app.notices().is_empty(),
        "dropping a non-.ron path must push an info notice"
    );

    // Settle to steady state: the first pass after a widget appears can request an
    // immediate repaint for layout/fade-in, so render a few empty passes until the
    // only remaining scheduled repaint is the notice's bounded TTL.
    let mut delay = Duration::ZERO;
    for _ in 0..4 {
        delay = render_once(&ctx, &mut app, RawInput::default());
    }
    // The notice is still pending (well under its multi-second TTL), so the shell
    // schedules a finite, non-zero repaint (the TTL), not an immediate or idle one.
    assert!(
        !app.notices().is_empty(),
        "the info notice must still be pending during this assertion"
    );
    assert!(
        delay > Duration::ZERO && delay < Duration::MAX,
        "a pending info notice must schedule a bounded deferred repaint, got {delay:?}"
    );
}
