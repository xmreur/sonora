//! Headless UI smoke test: boot the real `SonoraApp` inside an
//! `egui_kittest` harness and step it through its first frames.
//!
//! The sync tests in `player.rs` drive the engine without ever drawing a
//! pixel; this one runs the actual eframe app, so a panic in the layout
//! (a font that never loaded, a `None` the chrome assumes away, an input
//! widget with no accessible name, …) fails `cargo test` instead of the
//! first launch.

use crate::player::tests::test_app;
use egui_kittest::kittest::{By, Queryable};
use egui_kittest::Harness;

#[test]
fn boots_the_real_app_and_renders() {
    // Isolated config dir + the same persistence lock the sync tests use.
    let test = test_app();
    let app = test.app; // partial move; `test._guard` lives to the end
    let mut harness = Harness::builder()
        .with_size(egui::vec2(1100.0, 720.0))
        .build_eframe(move |cc| {
            // Same context setup as `main` — fonts and theme install in
            // the CreationContext, before the first pass.
            crate::theme::apply(&cc.egui_ctx);
            app
        });

    // Bounded steps: the app keeps requesting repaints while its
    // animations run, so `Harness::run()` (run-until-settled) would never
    // finish. The accessibility check runs on every step: it panics if an
    // input widget has no accessible name.
    harness.run_steps(3);

    // Frames actually painted something (not just an empty pass).
    assert!(
        !harness.output().shapes.is_empty(),
        "the first frames painted nothing"
    );

    // Boot banner from `SonoraApp::new` made it into the activity log.
    let state = harness.state();
    assert!(
        state.activity.iter().any(|r| r.text.contains("egui shell")),
        "boot banner missing from the activity log"
    );

    // The chrome is queryable through the accessibility tree: the sidebar
    // nav and the top-bar search of the boot page. "Queue" exists twice
    // (sidebar + player bar), so count instead of fetching one node.
    harness.get_by_label("Browse");
    harness.get_by_label("Settings");
    harness.get_by_label("Search");
    let queues = harness.query_all(By::new().label("Queue")).count();
    assert!(
        queues >= 2,
        "expected sidebar + player bar Queue, got {queues}"
    );
}
