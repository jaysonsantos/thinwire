//! Pixel snapshots of the shell.
//!
//! Each scene is a [`thinwire_core::demo::Scenario`]. The scenario builds a
//! core with [`Clock::fixed_utc`], so day labels and clock times do not follow
//! the process zone. Reference images live in `tests/snapshots/`. Update them
//! with `scripts/update-snapshots.sh`.

use egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::{By, Queryable};
use thinwire_core::demo::Scenario;
use thinwire_core::{Core, Intent, ThemeMode, ViewNow};
use tokio::runtime::{Builder, Runtime};

use super::theme;
use super::ui::{self, Hints};

struct Scene {
    core: Core,
    hints: Hints,
}

fn runtime() -> Runtime {
    Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

/// A spinner keeps requesting frames, so [`Harness::run`] never finishes.
fn keeps_repainting(scenario: Scenario) -> bool {
    matches!(
        scenario,
        Scenario::LongChatLoadingOlder | Scenario::LoginError
    )
}

fn scene(scenario: Scenario, theme: ThemeMode) -> (Runtime, Scene) {
    let runtime = runtime();
    let mut core = scenario.build(runtime.handle());
    core.dispatch(Intent::SetTheme(theme));
    let scene = Scene {
        core,
        hints: Hints::default(),
    };
    (runtime, scene)
}

fn draw(ui: &mut egui::Ui, state: &mut Scene) {
    theme::install(ui.ctx());
    if !ui.style().text_styles.contains_key(&theme::row_title()) {
        return;
    }
    let mut out = Vec::new();
    let view = state.core.view();
    ui::draw(ui, &view, &mut state.hints, &mut out);
}

fn shoot(scenario: Scenario, theme: ThemeMode, size: [f32; 2]) {
    let name = format!(
        "{}-{}-{}x{}",
        scenario.name(),
        theme_slug(theme),
        size[0] as u32,
        size[1] as u32
    );
    // Declare the runtime first so the harness (and its core) drops first.
    let (runtime, built) = scene(scenario, theme);
    let mut harness = Harness::builder()
        .with_size(size)
        .wgpu()
        .build_ui_state(draw, built);
    if keeps_repainting(scenario) {
        harness.run_steps(4);
    } else {
        harness.run();
    }
    harness.snapshot(&name);
    drop(harness);
    drop(runtime);
}

fn pixels() -> Vec<u8> {
    let (runtime, built) = scene(Scenario::LongChat, ThemeMode::Light);
    let mut harness = Harness::builder()
        .with_size([800.0, 600.0])
        .wgpu()
        .build_ui_state(draw, built);
    harness.run();
    let raw = harness.render().expect("snapshot frame").into_raw();
    drop(harness);
    drop(runtime);
    raw
}

fn theme_slug(theme: ThemeMode) -> &'static str {
    match theme {
        ThemeMode::Light => "light",
        ThemeMode::Dark => "dark",
        ThemeMode::System => "system",
    }
}

macro_rules! shots {
    ($($name:ident => $scenario:ident),* $(,)?) => {
        $(
            mod $name {
                use super::*;

                #[test]
                fn light_1100x720() {
                    shoot(Scenario::$scenario, ThemeMode::Light, [1100.0, 720.0]);
                }

                #[test]
                fn light_800x600() {
                    shoot(Scenario::$scenario, ThemeMode::Light, [800.0, 600.0]);
                }

                #[test]
                fn dark_1100x720() {
                    shoot(Scenario::$scenario, ThemeMode::Dark, [1100.0, 720.0]);
                }

                #[test]
                fn dark_800x600() {
                    shoot(Scenario::$scenario, ThemeMode::Dark, [800.0, 600.0]);
                }
            }
        )*
    };
}

shots! {
    first_run => FirstRun,
    long_chat => LongChat,
    long_chat_loading_older => LongChatLoadingOlder,
    group_chat => GroupChat,
    failed_send => FailedSend,
    login_phone => LoginPhone,
    login_code => LoginCode,
    login_2fa => Login2fa,
    login_error => LoginError,
    empty_inbox => EmptyInbox,
    long_chat_start_of_chat => LongChatStartOfChat,
    pending_send => PendingSend,
    keychain_try_again => KeychainTryAgain,
    status_error => StatusError,
    status_notice => StatusNotice,
    several_protocols => SeveralProtocols,
}

#[test]
fn inbox_row_text_stays_inside_the_click_rect() {
    let (runtime, built) = scene(Scenario::LongChat, ThemeMode::Light);
    let mut harness = Harness::builder()
        .with_size([1100.0, 720.0])
        .wgpu()
        .build_ui_state(draw, built);
    harness.run();
    let view = harness.state().core.view();
    let ViewNow::Fixed(now) = view.now() else {
        panic!("demo clock");
    };
    let rows: Vec<(String, String, String, u32)> = view
        .visible_conversations()
        .iter()
        .map(|row| {
            (
                row.title.clone(),
                row.preview.clone(),
                super::thread_layout::list_time(row.last_at, &now),
                row.unread,
            )
        })
        .collect();
    for (title, preview, time, unread) in rows {
        let click = harness.get_by_role_and_label(Role::Button, &title).rect();
        assert_text_inside(&harness, click, &title);
        assert_text_inside(&harness, click, &preview);
        assert_text_inside(&harness, click, &time);
        if unread > 0 {
            assert_text_inside(&harness, click, &format!("unread {unread}"));
        }
    }
    drop(harness);
    drop(runtime);
}

fn assert_text_inside(harness: &Harness<'_, Scene>, click: egui::Rect, text: &str) {
    let node = harness
        .query_all(By::new().role(Role::Label).label(text).include_labels())
        .find(|node| click.intersects(node.rect()))
        .unwrap_or_else(|| panic!("missing inbox text {text}"));
    assert!(
        click.contains_rect(node.rect()),
        "{text} {:?} is outside the row {:?}",
        node.rect(),
        click
    );
}

#[test]
fn a_demo_screen_formats_times_from_the_fixed_utc_clock() {
    let runtime = runtime();
    let core = Scenario::LongChat.build(runtime.handle());
    let view = core.view();
    let ViewNow::Fixed(now) = view.now() else {
        panic!("demo clock");
    };
    assert_eq!(now.offset().local_minus_utc(), 0);
    let ada = view
        .visible_conversations()
        .into_iter()
        .find(|row| row.title == "Ada Park")
        .expect("ada");
    assert_eq!(super::thread_layout::list_time(ada.last_at, &now), "10:00");
    let yesterday = now.timestamp() - 24 * 60 * 60 - 3 * 60 * 60;
    assert_eq!(
        super::thread_layout::list_time(yesterday, &now),
        "Yesterday"
    );
}

#[test]
fn two_runs_draw_the_same_snapshot() {
    assert_eq!(pixels(), pixels());
}
