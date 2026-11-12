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
    /// Height of the last photo box this frame. Zero when the screen has none.
    photo_box_height: f32,
    /// Height of the bubble that holds that photo. Zero when the screen has none.
    photo_row_height: f32,
}

fn runtime() -> Runtime {
    Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

/// A spinner keeps requesting frames, so [`Harness::run`] never finishes.
/// A send in flight shows the busy spinner in the chat list header.
fn keeps_repainting(scenario: Scenario) -> bool {
    matches!(
        scenario,
        Scenario::LongChatLoadingOlder | Scenario::LoginError | Scenario::PendingSend
    )
}

fn scene(scenario: Scenario, theme: ThemeMode) -> (Runtime, Scene) {
    let runtime = runtime();
    let mut core = scenario.build(runtime.handle());
    core.dispatch(Intent::SetTheme(theme));
    let scene = Scene {
        core,
        hints: Hints::default(),
        photo_box_height: 0.0,
        photo_row_height: 0.0,
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
    let mut player = super::audio::Player::new();
    ui::draw(ui, &view, &mut state.hints, &mut player, &mut out);
    ui::draw_other_pane(ui, &mut state.core, &mut player, &mut out);
    state.photo_box_height = ui.data(|data| {
        data.get_temp(egui::Id::unique("thinwire-photo-box-height"))
            .unwrap_or(0.0)
    });
    state.photo_row_height = ui.data(|data| {
        data.get_temp(egui::Id::unique("thinwire-photo-row-height"))
            .unwrap_or(0.0)
    });
}

/// Same frame as [`draw`], and it applies a Link click and a draft edit.
/// The catalog shots stay on [`draw`].
#[cfg(all(feature = "ui-snapshots", feature = "signal-local"))]
fn draw_signal(ui: &mut egui::Ui, state: &mut Scene) {
    theme::install(ui.ctx());
    if !ui.style().text_styles.contains_key(&theme::row_title()) {
        return;
    }
    let mut out = Vec::new();
    let view = state.core.view();
    let mut player = super::audio::Player::new();
    ui::draw(ui, &view, &mut state.hints, &mut player, &mut out);
    for intent in out {
        if matches!(
            intent,
            Intent::Signal(thinwire_core::SignalIntent::Relink) | Intent::SetDraft { .. }
        ) {
            state.core.dispatch(intent);
        }
    }
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
    if waits_for_photo(scenario) {
        settle_photo(&mut harness);
    } else if keeps_repainting(scenario) {
        harness.run_steps(4);
    } else {
        harness.run();
        settle_images(&mut harness);
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
    settle_images(&mut harness);
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
    person_colors => PersonColors,
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
    collapsed_rail => CollapsedRail,
    split_view => SplitView,
    muted_inbox => MutedInbox,
    muted_here => MutedHere,
    muted_by_protocol => MutedByProtocol,
    forum_topics => ForumTopics,
    open_topic => OpenForumTopic,
    closed_topic => ClosedTopic,
    photo_tall => PhotoTall,
    photo_wide => PhotoWide,
    photo_spoiler => PhotoSpoiler,
    photo_failed => PhotoFailed,
    showcase => Showcase,
}

/// A loaded photo and a failed photo of the same size keep the same box
/// and the same bubble.
#[test]
fn a_photo_row_keeps_its_height_when_the_image_is_missing() {
    let loaded = photo_heights(Scenario::PhotoWide);
    let missing = photo_heights(Scenario::PhotoFailed);
    assert_eq!(loaded.0, missing.0, "photo box");
    assert_eq!(loaded.1, missing.1, "bubble");
    assert!(loaded.0 > 100.0, "the wide photo is on screen");
    assert!(loaded.1 > loaded.0, "the bubble is taller than the photo");
}

fn photo_heights(scenario: Scenario) -> (f32, f32) {
    let (runtime, built) = scene(scenario, ThemeMode::Light);
    let mut harness = Harness::builder()
        .with_size([1100.0, 720.0])
        .wgpu()
        .build_ui_state(draw, built);
    if waits_for_photo(scenario) {
        settle_photo(&mut harness);
    } else {
        harness.run();
    }
    let heights = (
        harness.state().photo_box_height,
        harness.state().photo_row_height,
    );
    drop(harness);
    drop(runtime);
    heights
}

fn waits_for_photo(scenario: Scenario) -> bool {
    matches!(
        scenario,
        Scenario::PhotoTall | Scenario::PhotoWide | Scenario::PhotoSpoiler | Scenario::PhotoFailed
    )
}

/// A demo avatar is bytes already in memory. Step until egui has decoded it.
fn settle_images(harness: &mut Harness<'_, Scene>) {
    for _ in 0..12 {
        if !harness.ctx.has_pending_images() {
            return;
        }
        harness.step();
    }
}

/// Blur and decode finish off the UI thread. Step until both are done.
fn settle_photo(harness: &mut Harness<'_, Scene>) {
    for _ in 0..80 {
        harness.step();
        if harness.state().photo_box_height > 0.0 && super::photos::visuals_ready(&harness.ctx) {
            harness.step();
            if super::photos::visuals_ready(&harness.ctx) {
                return;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    panic!("the photo did not settle");
}

/// Row menus for Mute, Unmute, and a disabled "Muted in Telegram" (#206).
/// Milo is the open chat, so the muted unread ring sits on the selected row.
mod mute_menu {
    use super::*;

    fn shoot_menu(theme: ThemeMode, size: [f32; 2]) {
        let name = format!(
            "mute_menu-{}-{}x{}",
            theme_slug(theme),
            size[0] as u32,
            size[1] as u32
        );
        let (runtime, mut built) = scene(Scenario::MutedHere, theme);
        built.hints.show_open_mute_menus();
        let mut harness = Harness::builder()
            .with_size(size)
            .wgpu()
            .build_ui_state(draw, built);
        harness.run();
        settle_images(&mut harness);
        harness.snapshot(&name);
        drop(harness);
        drop(runtime);
    }

    #[test]
    fn light_1100x720() {
        shoot_menu(ThemeMode::Light, [1100.0, 720.0]);
    }

    #[test]
    fn light_800x600() {
        shoot_menu(ThemeMode::Light, [800.0, 600.0]);
    }

    #[test]
    fn dark_1100x720() {
        shoot_menu(ThemeMode::Dark, [1100.0, 720.0]);
    }

    #[test]
    fn dark_800x600() {
        shoot_menu(ThemeMode::Dark, [800.0, 600.0]);
    }
}

/// The Settings button at the foot of the sidebar, clicked: the menu opens
/// above it with Refresh, the theme choice, and the notification switches.
mod settings_menu {
    use super::*;

    fn shoot_open(theme: ThemeMode, size: [f32; 2]) {
        let name = format!(
            "settings_menu-{}-{}x{}",
            theme_slug(theme),
            size[0] as u32,
            size[1] as u32
        );
        let (runtime, built) = scene(Scenario::LongChat, theme);
        let mut harness = Harness::builder()
            .with_size(size)
            .wgpu()
            .build_ui_state(draw, built);
        harness.run();
        harness
            .get_by_role_and_label(Role::Button, "Settings")
            .click();
        harness.run();
        settle_images(&mut harness);
        harness.get_by_role_and_label(Role::Button, "Refresh");
        harness.snapshot(&name);
        drop(harness);
        drop(runtime);
    }

    #[test]
    fn light_1100x720() {
        shoot_open(ThemeMode::Light, [1100.0, 720.0]);
    }

    #[test]
    fn dark_1100x720() {
        shoot_open(ThemeMode::Dark, [1100.0, 720.0]);
    }

    /// The menu is 232 px wide and the sidebar 188. It opens over the chat
    /// list, so a narrow window must still show all of it.
    #[test]
    fn light_800x600() {
        shoot_open(ThemeMode::Light, [800.0, 600.0]);
    }

    #[test]
    fn dark_800x600() {
        shoot_open(ThemeMode::Dark, [800.0, 600.0]);
    }
}

/// ADR 0014: a second Telegram account that is not linked shows its number,
/// Link, and Remove. Add account is open above it with the services of the
/// build.
mod add_account_menu {
    use super::*;

    fn shoot_open(theme: ThemeMode, size: [f32; 2]) {
        let name = format!(
            "add_account_menu-{}-{}x{}",
            theme_slug(theme),
            size[0] as u32,
            size[1] as u32
        );
        let (runtime, mut built) = scene(Scenario::LongChat, theme);
        built.core.dispatch(Intent::AddAccount(
            thinwire_protocol::ProtocolKind::Telegram,
        ));
        built
            .core
            .dispatch(Intent::Telegram(thinwire_core::TelegramIntent::Cancel));
        let mut harness = Harness::builder()
            .with_size(size)
            .wgpu()
            .build_ui_state(draw, built);
        harness.run();
        harness
            .get_by_role_and_label(Role::Button, "Add account")
            .click();
        harness.run();
        settle_images(&mut harness);
        harness.get_by_role_and_label(Role::Button, "Add a Telegram account");
        harness.get_by_role_and_label(Role::Button, "Remove Telegram 2");
        harness.snapshot(&name);
        drop(harness);
        drop(runtime);
    }

    #[test]
    fn light_1100x720() {
        shoot_open(ThemeMode::Light, [1100.0, 720.0]);
    }

    #[test]
    fn dark_1100x720() {
        shoot_open(ThemeMode::Dark, [1100.0, 720.0]);
    }

    #[test]
    fn light_800x600() {
        shoot_open(ThemeMode::Light, [800.0, 600.0]);
    }

    #[test]
    fn dark_800x600() {
        shoot_open(ThemeMode::Dark, [800.0, 600.0]);
    }
}

/// The login card is taller than a short window. The column scrolls, so
/// Change number and Cancel stay in reach. Before it scrolled, they were cut
/// off at 800x600 with no way to get to them.
mod login_scrolled {
    use super::*;

    fn shoot_scrolled(theme: ThemeMode) {
        let size = [800.0, 480.0];
        let height = size[1];
        let name = format!("login_code_scrolled-{}-800x480", theme_slug(theme));
        let (runtime, built) = scene(Scenario::LoginCode, theme);
        let mut harness = Harness::builder()
            .with_size(size)
            .wgpu()
            .build_ui_state(draw, built);
        harness.run();
        settle_images(&mut harness);
        // Without a scroll, the card runs past the bottom of the window. If it
        // fits, this test proves nothing, so it says so.
        let cancel = harness.get_by_role_and_label(Role::Button, "Cancel").rect();
        assert!(
            cancel.bottom() > height,
            "the card must overflow this window, or the test shows nothing: Cancel ends at {}",
            cancel.bottom()
        );
        harness.hover_at(egui::pos2(size[0] * 0.8, size[1] * 0.5));
        harness.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, -5_000.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::NONE,
            source: egui::MouseWheelSource::Wheel,
        });
        // egui spreads a wheel step over several frames.
        harness.run_steps(12);
        harness.remove_cursor();
        harness.run();
        settle_images(&mut harness);
        for label in ["Cancel", "Change number"] {
            let rect = harness.get_by_label(label).rect();
            assert!(
                rect.top() >= 0.0 && rect.bottom() <= height,
                "{label} is on screen after a scroll: {rect:?} in a window {height} high"
            );
        }
        harness.snapshot(&name);
        drop(harness);
        drop(runtime);
    }

    #[test]
    fn light_800x480() {
        shoot_scrolled(ThemeMode::Light);
    }

    #[test]
    fn dark_800x480() {
        shoot_scrolled(ThemeMode::Dark);
    }
}

/// A long chat scrolled up: the jump button sits over the bottom right of
/// the messages. 480 high, so the chat is taller than the view.
mod jump_to_latest {
    use super::*;

    fn shoot_scrolled(theme: ThemeMode) {
        let size = [800.0, 480.0];
        let name = format!("jump_to_latest-{}-800x480", theme_slug(theme));
        let (runtime, built) = scene(Scenario::LongChat, theme);
        let mut harness = Harness::builder()
            .with_size(size)
            .wgpu()
            .build_ui_state(draw, built);
        harness.run();
        harness.hover_at(egui::pos2(size[0] * 0.85, size[1] * 0.5));
        harness.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, 5_000.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::NONE,
            source: egui::MouseWheelSource::Wheel,
        });
        // egui spreads a wheel step over several frames.
        harness.run_steps(12);
        harness.remove_cursor();
        harness.run();
        settle_images(&mut harness);
        harness.get_by_role_and_label(Role::Button, "Jump to latest message");
        harness.snapshot(&name);
        drop(harness);
        drop(runtime);
    }

    #[test]
    fn light_800x480() {
        shoot_scrolled(ThemeMode::Light);
    }

    #[test]
    fn dark_800x480() {
        shoot_scrolled(ThemeMode::Dark);
    }
}

/// The account sidebar and the chat column are side by side.
/// In the chat column, the list header, the search field and the inbox avatar
/// share one x. The chat title sits one [`theme::space::M`] past the avatar.
/// The account unread badge sits inside the highlight by [`theme::ROW_INSET`].
#[test]
fn the_chat_list_sits_beside_the_account_rail() {
    for theme in [ThemeMode::Light, ThemeMode::Dark] {
        let (runtime, built) = scene(Scenario::LongChat, theme);
        let mut harness = Harness::builder()
            .with_size([1100.0, 720.0])
            .wgpu()
            .build_ui_state(draw, built);
        harness.run();
        let brand = harness
            .get_by_role_and_label(Role::Label, "thinwire")
            .rect();
        let list_title = harness
            .query_all(By::new().role(Role::Label).label("Telegram"))
            .map(|node| node.rect())
            .min_by(|a, b| a.min.y.total_cmp(&b.min.y))
            .expect("chat list header");
        let tile = harness
            .get_by_role_and_label(Role::Label, "account status")
            .rect();
        let title = harness
            .query_all(By::new().role(Role::Label).label("Ada Park"))
            .map(|node| node.rect())
            .min_by(|a, b| a.min.x.total_cmp(&b.min.x))
            .expect("inbox title");
        let avatar = harness
            .get_by_role_and_label(Role::Label, "avatar Ada Park")
            .rect();
        assert!(
            list_title.min.x > brand.max.x,
            "{theme:?} chats {} must sit to the right of the sidebar {}",
            list_title.min.x,
            brand.max.x
        );
        assert!(
            (list_title.min.y - brand.min.y).abs() < 48.0,
            "{theme:?} list header y {} sidebar mark y {}",
            list_title.min.y,
            brand.min.y
        );
        assert!(
            (list_title.min.x - avatar.min.x).abs() < 0.5,
            "{theme:?} list header {} avatar {}",
            list_title.min.x,
            avatar.min.x
        );
        assert!(
            (tile.width() - theme::TILE_SIDE).abs() < 0.5,
            "{theme:?} tile width {}",
            tile.width()
        );
        assert!(
            (avatar.width() - super::photos::AVATAR_SIDE).abs() < 0.5,
            "{theme:?} avatar width {}",
            avatar.width()
        );
        let gap = title.min.x - avatar.max.x;
        assert!(
            (gap - theme::space::M).abs() < 1.0,
            "{theme:?} avatar-to-title gap {gap}"
        );
        let book = harness
            .get_by_role_and_label(Role::Button, "Book club")
            .rect();
        let book_avatar = harness
            .get_by_role_and_label(Role::Label, "avatar Book club")
            .rect();
        let book_badge = harness
            .query_all(By::new().role(Role::Label).label("unread 2"))
            .map(|node| node.rect())
            .find(|rect| book.intersects(*rect))
            .expect("book club unread badge");
        assert!(
            !book_badge.intersects(book_avatar),
            "{theme:?} badge {:?} overlaps avatar {:?}",
            book_badge,
            book_avatar
        );
        let account = harness
            .query_all(By::new().role(Role::Button).label("Telegram"))
            .map(|node| node.rect())
            .find(|rect| rect.intersects(tile))
            .expect("account row");
        let badge = harness
            .query_all(By::new().role(Role::Label).label("unread 2"))
            .map(|node| node.rect())
            .find(|rect| account.intersects(*rect))
            .expect("account badge");
        let gap_x = account.right() - badge.right();
        let gap_top = badge.top() - account.top();
        let gap_bottom = account.bottom() - badge.bottom();
        assert!(
            (gap_x - theme::space::M).abs() < 1.0,
            "{theme:?} badge horizontal clearance {gap_x}"
        );
        assert!(
            gap_top + 0.5 >= theme::space::S,
            "{theme:?} badge top clearance {gap_top}"
        );
        assert!(
            gap_bottom + 0.5 >= theme::space::S,
            "{theme:?} badge bottom clearance {gap_bottom}"
        );

        let search = harness
            .query_all(By::new().role(Role::TextInput))
            .map(|node| node.rect())
            .min_by(|a, b| a.min.y.total_cmp(&b.min.y))
            .expect("search field");
        assert!(
            search.height() + 0.5 >= theme::SEARCH_HEIGHT,
            "{theme:?} search height {}",
            search.height()
        );
        drop(harness);
        drop(runtime);
    }
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

/// The phone removed this Signal device. The row and the open chat show the
/// line once each. A Link button is on the row. There is no error popup.
#[cfg(all(feature = "ui-snapshots", feature = "signal-local"))]
mod signal_removed {
    use super::*;
    use thinwire_core::SignalIntent;
    use thinwire_core::state::SIGNAL_SENDING_OFF;
    use thinwire_protocol::{
        AccountState, AdapterEvent, AdapterStatus, Arrival, ChatMessage, Conversation, Delivery,
        ProtocolId, SIGNAL_DEVICE_REMOVED,
    };

    fn removed_core(runtime: &Runtime) -> Core {
        let mut core = Scenario::EmptyInbox.build(runtime.handle());
        core.dispatch(Intent::Signal(SignalIntent::OpenNotice));
        core.dispatch(Intent::Signal(SignalIntent::AcknowledgeNotice));
        core.apply_event(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linked,
        });
        core.apply_event(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Ready,
            detail: "Signal is linked. Experimental client: expect breakage.".into(),
        });
        core.apply_event(AdapterEvent::ConversationUpsert {
            conversation: Conversation {
                protocol: ProtocolId::Signal,
                id: "signal:ada".into(),
                title: "Ada".into(),
                participant: "Ada".into(),
                preview: "kept".into(),
                unread: 0,
                order: 1,
                last_at: 1_772_445_600,
                is_group: false,
                writable: true,
                placeholder: false,
                muted: false,
                has_topics: false,
                avatar: Default::default(),
            },
        });
        core.apply_event(AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::Signal,
                conversation_id: "signal:ada".into(),
                id: "signal:ada:1".into(),
                sender: "Ada".into(),
                body: "kept".into(),
                outbound: false,
                delivery: Delivery::Sent,
                sent_at: 1_772_445_600,
                arrival: Arrival::History,
                topic: None,
                links: Vec::new(),
                photo: None,
                audio: None,
                video: None,
                file: None,
                reactions: Vec::new(),
                reply_to: None,
                edited: false,
            },
        });
        core.dispatch(Intent::SelectConversation {
            id: "signal:ada".into(),
        });
        core.apply_event(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Error,
            detail: SIGNAL_DEVICE_REMOVED.into(),
        });
        core.apply_event(AdapterEvent::AccountEnded {
            protocol: ProtocolId::Signal,
        });
        core.apply_event(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Unlinked,
        });
        core
    }

    fn shoot(theme: ThemeMode, size: [f32; 2]) {
        let name = format!(
            "signal_removed-{}-{}x{}",
            theme_slug(theme),
            size[0] as u32,
            size[1] as u32
        );
        let runtime = runtime();
        let mut core = removed_core(&runtime);
        core.dispatch(Intent::SetTheme(theme));
        let built = Scene {
            core,
            hints: Hints::default(),
            photo_box_height: 0.0,
            photo_row_height: 0.0,
        };
        let mut harness = Harness::builder()
            .with_size(size)
            .wgpu()
            .build_ui_state(draw_signal, built);
        harness.run();
        settle_images(&mut harness);
        let line = SIGNAL_DEVICE_REMOVED;
        let copies = harness
            .query_all(By::new().role(Role::Label).label(line).include_labels())
            .count();
        assert_eq!(copies, 2, "the line is on the row and in the open chat");
        let sending_off = harness
            .query_all(
                By::new()
                    .role(Role::Label)
                    .label(SIGNAL_SENDING_OFF)
                    .include_labels(),
            )
            .count();
        assert_eq!(sending_off, 1, "the open chat says sending is off");
        assert!(
            harness
                .query_all(
                    By::new()
                        .role(Role::Label)
                        .label("What happened")
                        .include_labels(),
                )
                .next()
                .is_none(),
            "no error popup"
        );
        assert!(
            harness
                .query_all(
                    By::new()
                        .role(Role::Label)
                        .label("Signal is linked.")
                        .include_labels(),
                )
                .next()
                .is_none(),
            "the status line does not say linked"
        );
        // The sidebar row and the chat list both offer Link. The row's is
        // the one that relinks the removed device.
        assert!(
            harness
                .query_all(By::new().role(Role::Button).label("Link"))
                .count()
                >= 1,
            "Link is on screen"
        );
        harness.snapshot(&name);
        let field = harness.get_by_role(Role::MultilineTextInput);
        field.click();
        field.type_text("nope");
        harness.run();
        assert!(
            harness.state().core.view().compose.is_empty(),
            "sending is off: the message input takes no text"
        );
        let link = harness
            .query_all(By::new().role(Role::Button).label("Link"))
            .min_by(|a, b| a.rect().min.x.total_cmp(&b.rect().min.x))
            .expect("Link on the account row");
        link.click();
        harness.run();
        harness.get_by_role_and_label(Role::Label, "Link a secondary device");
        drop(harness);
        drop(runtime);
    }

    #[test]
    fn light_1100x720() {
        shoot(ThemeMode::Light, [1100.0, 720.0]);
    }

    #[test]
    fn dark_1100x720() {
        shoot(ThemeMode::Dark, [1100.0, 720.0]);
    }
}

/// The pictures of a desktop notification: the thinwire mark with the tile
/// of the protocol in its corner. `thinwire-notify` has no egui, so it
/// ships these files (`crates/thinwire-notify/assets/`). This test makes
/// them and keeps them equal to the painters of the sidebar.
mod notify_badge {
    use egui::{Color32, Pos2, Rect, Stroke, Vec2};
    use egui_kittest::{Harness, SnapshotOptions};
    use thinwire_protocol::{ProtocolId, ProtocolKind};

    use super::super::{icons, motion, theme};

    /// Side of a picture, in pixels.
    const SIDE: f32 = 256.0;
    /// The body of `assets/icon.svg`: the gradient, and the square that
    /// holds it on the 1024 grid.
    const BODY_TOP: Color32 = Color32::from_rgb(0x6A, 0xA6, 0xFF);
    const BODY_BOTTOM: Color32 = Color32::from_rgb(0x1F, 0x5B, 0xDB);
    const SVG_ORIGIN: f32 = 100.0;
    const SVG_SIDE: f32 = 824.0;
    /// The "t" is smaller than in the app icon and sits up and left, so
    /// the protocol tile has the bottom right corner.
    const MARK_SCALE: f32 = 0.74;
    const MARK_SHIFT: f32 = -30.0;
    const TILE_SIDE: f32 = 112.0;
    const TILE_MARGIN: f32 = 12.0;
    const TILE_RING: f32 = 7.0;

    /// A point of `icon.svg` on the picture.
    fn at(x: f32, y: f32) -> Pos2 {
        let full = |v: f32| (v - SVG_ORIGIN) / SVG_SIDE * SIDE;
        let small = |v: f32| (full(v) - SIDE / 2.0) * MARK_SCALE + SIDE / 2.0 + MARK_SHIFT;
        Pos2::new(small(x), small(y))
    }

    /// A stroke with round ends, as `stroke-linecap="round"` of the SVG.
    fn round_line(painter: &egui::Painter, points: Vec<Pos2>, svg_width: f32) {
        let width = svg_width / SVG_SIDE * SIDE * MARK_SCALE;
        for end in [points[0], points[points.len() - 1]] {
            painter.circle_filled(end, width / 2.0, Color32::WHITE);
        }
        painter.add(egui::Shape::line(
            points,
            Stroke::new(width, Color32::WHITE),
        ));
    }

    fn draw(ui: &mut egui::Ui, protocol: &mut ProtocolId) {
        // The whole picture: the margin of the root panel clips the
        // painter of `ui`, and a layer painter has no such clip.
        let painter = &ui.ctx().layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::unique("notify_picture"),
        ));
        let full = Rect::from_min_size(Pos2::ZERO, Vec2::splat(SIDE));
        // One pixel over each edge: the picture has no soft border.
        icons::gradient_tile(painter, full.expand(1.0), 0.0, BODY_TOP, BODY_BOTTOM);

        // The stem, its curve (a quadratic with the corner as control
        // point), and the foot. Then the bar.
        let mut stem = vec![at(482.0, 280.0), at(482.0, 610.0)];
        for step in 1..=12 {
            let t = step as f32 / 12.0;
            let x = (1.0 - t).powi(2) * 482.0 + 2.0 * (1.0 - t) * t * 482.0 + t * t * 602.0;
            let y = (1.0 - t).powi(2) * 610.0 + 2.0 * (1.0 - t) * t * 730.0 + t * t * 730.0;
            stem.push(at(x, y));
        }
        stem.push(at(660.0, 730.0));
        round_line(painter, stem, 120.0);
        round_line(painter, vec![at(362.0, 430.0), at(640.0, 430.0)], 110.0);

        let corner = SIDE - TILE_MARGIN;
        let tile = Rect::from_min_max(
            Pos2::new(corner - TILE_SIDE, corner - TILE_SIDE),
            Pos2::new(corner, corner),
        );
        let ink = theme::protocol_tile(*protocol);
        let radius = TILE_SIDE * 0.28;
        painter.rect_filled(
            tile.expand(TILE_RING),
            egui::CornerRadius::same((radius + TILE_RING) as u8),
            // The ring has the color of the body at that height, so the
            // tile looks cut out of the "t".
            motion::mix(BODY_TOP, BODY_BOTTOM, tile.center().y / SIDE),
        );
        icons::gradient_tile(painter, tile, radius, ink.top, ink.bottom);
        icons::protocol(
            painter,
            tile.center(),
            TILE_SIDE * 0.6,
            *protocol,
            ink.mark,
            motion::mix(ink.top, ink.bottom, 0.5),
        );
    }

    /// Side of the picture of a tile alone, in pixels.
    const ALONE_SIDE: f32 = 96.0;

    /// The tile of a protocol over the whole picture, with square corners.
    /// `thinwire-notify` cuts the round corners and puts the tile on the
    /// picture of a chat.
    fn draw_tile(ui: &mut egui::Ui, protocol: &mut ProtocolId) {
        // The whole picture: the margin of the root panel clips the
        // painter of `ui`, and a layer painter has no such clip.
        let painter = &ui.ctx().layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::unique("notify_picture"),
        ));
        let full = Rect::from_min_size(Pos2::ZERO, Vec2::splat(ALONE_SIDE));
        let ink = theme::protocol_tile(*protocol);
        icons::gradient_tile(painter, full.expand(1.0), 0.0, ink.top, ink.bottom);
        icons::protocol(
            painter,
            full.center(),
            ALONE_SIDE * 0.6,
            *protocol,
            ink.mark,
            motion::mix(ink.top, ink.bottom, 0.5),
        );
    }

    fn shoot_one(
        kind: ProtocolKind,
        name: &str,
        side: f32,
        draw: fn(&mut egui::Ui, &mut ProtocolId),
    ) {
        let mut harness = Harness::builder()
            .with_size([side, side])
            .wgpu()
            .build_ui_state(
                move |ui, protocol| {
                    egui::CentralPanel::default()
                        .frame(egui::Frame::NONE)
                        .show(ui, |ui| draw(ui, protocol));
                },
                ProtocolId::primary(kind),
            );
        harness.run();
        harness.snapshot_options(
            name,
            &SnapshotOptions::new().output_path("../thinwire-notify/assets"),
        );
    }

    /// The mark with the tile, and the tile alone, of each protocol.
    macro_rules! pictures {
        ($($kind:ident => $name:ident),+ $(,)?) => {$(
            mod $name {
                use super::*;

                #[test]
                fn badge() {
                    let name = concat!("badge-", stringify!($name));
                    shoot_one(ProtocolKind::$kind, name, SIDE, draw);
                }

                #[test]
                fn tile() {
                    let name = concat!("tile-", stringify!($name));
                    shoot_one(ProtocolKind::$kind, name, ALONE_SIDE, draw_tile);
                }
            }
        )+};
    }

    pictures! {
        Telegram => telegram,
        WhatsApp => whatsapp,
        Signal => signal,
        Discord => discord,
        Slack => slack,
    }
}
