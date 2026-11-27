//! Headless UI smoke test: boot the real `SonoraApp` inside an
//! `egui_kittest` harness and step it through its first frames.
//!
//! The sync tests in `player.rs` drive the engine without ever drawing a
//! pixel; this one runs the actual eframe app, so a panic in the layout
//! (a font that never loaded, a `None` the chrome assumes away, an input
//! widget with no accessible name, …) fails `cargo test` instead of the
//! first launch.

use crate::app::{ImBg, View};
use crate::player::tests::test_app;
use egui_kittest::kittest::{By, NodeT, Queryable};
use egui_kittest::Harness;
use shell::jam::{JamRole, Participant, PathKind};

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
    // nav and the top-bar search of the boot page. The queue pane starts
    // closed, so the only "Queue" label is the player-bar toggle.
    harness.get_by_label("Browse");
    harness.get_by_label("Settings");
    harness.get_by_label("Search");
    // Homepage storefront picker (Browse is the boot view).
    harness.get_by_label("Storefront");
    let queues: Vec<_> = harness
        .query_all(By::new().label("Queue"))
        .map(|n| format!("{:?}@{:?}", n.accesskit_node().role(), n.rect()))
        .collect();
    assert_eq!(
        queues.len(),
        1,
        "expected the player bar queue toggle, got {queues:?}"
    );
}

#[test]
fn dynamic_blended_fullscreen_falls_back_without_motion_art() {
    let test = test_app();
    let mut app = test.app;
    app.fullscreen = true;
    app.settings.immersive = true;
    app.settings.im_bg = ImBg::DynamicBlended;
    let mut harness = Harness::builder()
        .with_size(egui::vec2(1280.0, 720.0))
        .build_eframe(move |cc| {
            crate::theme::apply(&cc.egui_ctx);
            app
        });
    // No manifest is cached in the isolated config dir, so the Motion slot
    // stays empty and the mode must fall back to the still-cover layout.
    harness.run_steps(3);
    assert!(!harness.output().shapes.is_empty());
}

#[test]
fn jam_panel_renders_idle_connecting_and_live_states() {
    let test = test_app();
    let mut app = test.app;
    // Stay off Browse so live storefront holiday emojis cannot flake glyph checks.
    app.view = View::Files;
    app.settings.jam_open = true;
    app.jam.show_jam = true;
    let mut harness = Harness::builder()
        .with_size(egui::vec2(1180.0, 720.0))
        .build_eframe(move |cc| {
            crate::theme::apply(&cc.egui_ctx);
            app
        });

    // 1. Idle state
    harness.run_steps(3);
    harness.get_by_label("Start a Jam");
    harness.get_by_label("Join Jam");

    // 2. Connecting state
    harness.state_mut().jam.connecting = Some(JamRole::Host);
    harness.run_steps(3);
    harness.get_by_label("Cancel");

    // 3. Live host state with participants and notice
    {
        let st = harness.state_mut();
        st.jam.connecting = None;
        st.jam.role = Some(JamRole::Host);
        st.jam.peer_id = "host-1".into();
        st.jam.ticket = "sonora-jam:1:eyJhZGRyIjoiYWJjZGVmZ2hpamtsbW5vcHFyc3R1dnd4eXoifQ".into();
        st.jam.path = PathKind::Direct;
        st.jam.rtt_ms = Some(14);
        st.jam.participants = vec![
            Participant {
                peer_id: "host-1".into(),
                name: "Alice".into(),
                host: true,
            },
            Participant {
                peer_id: "guest-2".into(),
                name: "Bob".into(),
                host: false,
            },
        ];
    }
    harness.run_steps(3);
    harness.get_by_label("Copy invite code");
    harness.get_by_label("End Jam");
    harness.get_by_label("Remove Bob");

    // 4. Live guest state (with notice banner and Relayed path)
    {
        let st = harness.state_mut();
        st.jam.role = Some(JamRole::Guest);
        st.jam.peer_id = "guest-2".into();
        st.jam.path = PathKind::Relayed;
        st.jam.rtt_ms = Some(48);
        st.jam.guests_can_control = false;
        st.jam.notice = Some("Reconnecting…".into());
    }
    harness.run_steps(3);
    harness.get_by_label("Leave Jam");

    // 5. Queue tab during live jam shows the live banner cleanly
    {
        let st = harness.state_mut();
        st.jam.show_jam = false;
        st.settings.queue_open = true;
        st.settings.jam_open = false;
    }
    harness.run_steps(3);
    assert!(!harness.output().shapes.is_empty());
}
