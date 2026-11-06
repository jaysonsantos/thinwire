mod engine;

use eframe::egui::{self, RichText};
use thinwire_core::Intent;
use thinwire_protocol::{AudioPath, ChatMessage, MessageAudio, MessageVideo, ProtocolId};

use super::motion;
use super::theme;

pub(super) trait Backend {
    fn send(&mut self, cmd: engine::Cmd);
    fn take_done(&mut self) -> Option<engine::Done>;
    fn shutdown(&mut self);
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct VoiceKey {
    protocol: ProtocolId,
    conversation_id: String,
    message_id: String,
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum Session {
    Idle,
    Waiting(VoiceKey),
    Playing(VoiceKey),
    Failed(VoiceKey),
}

impl Session {
    fn key(&self) -> Option<&VoiceKey> {
        match self {
            Self::Idle => None,
            Self::Waiting(key) | Self::Playing(key) | Self::Failed(key) => Some(key),
        }
    }
}

enum NoteState {
    Remote { file_id: i32 },
    Local { path: AudioPath },
    Broken { file_id: i32 },
}

enum Input {
    Click { key: VoiceKey, note: NoteState },
    Seen { key: VoiceKey, note: NoteState },
    Gone(VoiceKey),
    Ended,
    Rejected,
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum Effect {
    None,
    Download { file_id: i32 },
    Start(AudioPath),
    StopSink,
}

struct Transition {
    session: Session,
    effects: [Effect; 2],
}

fn step(session: &Session, input: Input) -> Transition {
    match input {
        Input::Ended | Input::Rejected if !matches!(session, Session::Playing(_)) => stay(session),
        Input::Click { key, note } => click(session, key, note),
        Input::Seen { key, note } => seen(session, key, note),
        Input::Gone(key) => gone(session, key),
        Input::Ended => idle(),
        Input::Rejected => match session {
            Session::Playing(key) => failed_stop(key.clone()),
            _ => stay(session),
        },
    }
}

fn click(session: &Session, key: VoiceKey, note: NoteState) -> Transition {
    let same = session.key().is_some_and(|current| current == &key);
    if same && matches!(session, Session::Waiting(_)) {
        return stay(session);
    }
    if same && matches!(session, Session::Playing(_)) {
        return idle_stop();
    }
    let leaving_play = matches!(session, Session::Playing(_));
    match note {
        NoteState::Local { path } => playing(key, path),
        // Every adapter that sets audio on a message can download it.
        NoteState::Remote { file_id } | NoteState::Broken { file_id } => {
            if leaving_play {
                waiting_stop(key, file_id)
            } else {
                waiting(key, file_id)
            }
        }
    }
}

fn seen(session: &Session, key: VoiceKey, note: NoteState) -> Transition {
    if session.key() != Some(&key) {
        return stay(session);
    }
    match session {
        Session::Waiting(_) => match note {
            NoteState::Local { path } => playing(key, path),
            NoteState::Broken { .. } => failed(key),
            NoteState::Remote { .. } => stay(session),
        },
        Session::Playing(_) | Session::Failed(_) | Session::Idle => stay(session),
    }
}

fn gone(session: &Session, key: VoiceKey) -> Transition {
    if session.key() != Some(&key) {
        return stay(session);
    }
    match session {
        Session::Playing(_) => idle_stop(),
        Session::Waiting(_) | Session::Failed(_) => idle(),
        Session::Idle => stay(session),
    }
}

fn stay(session: &Session) -> Transition {
    Transition {
        session: session.clone(),
        effects: [Effect::None, Effect::None],
    }
}

fn idle() -> Transition {
    Transition {
        session: Session::Idle,
        effects: [Effect::None, Effect::None],
    }
}

fn idle_stop() -> Transition {
    Transition {
        session: Session::Idle,
        effects: [Effect::StopSink, Effect::None],
    }
}

fn playing(key: VoiceKey, path: AudioPath) -> Transition {
    Transition {
        session: Session::Playing(key),
        effects: [Effect::Start(path), Effect::None],
    }
}

fn waiting(key: VoiceKey, file_id: i32) -> Transition {
    Transition {
        session: Session::Waiting(key),
        effects: [Effect::Download { file_id }, Effect::None],
    }
}

fn waiting_stop(key: VoiceKey, file_id: i32) -> Transition {
    Transition {
        session: Session::Waiting(key),
        effects: [Effect::StopSink, Effect::Download { file_id }],
    }
}

fn failed(key: VoiceKey) -> Transition {
    Transition {
        session: Session::Failed(key),
        effects: [Effect::None, Effect::None],
    }
}

fn failed_stop(key: VoiceKey) -> Transition {
    Transition {
        session: Session::Failed(key),
        effects: [Effect::StopSink, Effect::None],
    }
}

fn note_state(audio: &MessageAudio) -> NoteState {
    if let Some(path) = audio.file() {
        NoteState::Local { path: path.clone() }
    } else if audio.failed() {
        NoteState::Broken {
            file_id: audio.file_id(),
        }
    } else {
        NoteState::Remote {
            file_id: audio.file_id(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Face {
    Play,
    Loading,
    Pause,
    Retry,
}

fn face(session: &Session, protocol: ProtocolId, conversation_id: &str, message_id: &str) -> Face {
    let active = session.key().is_some_and(|key| {
        key.protocol == protocol
            && key.conversation_id == conversation_id
            && key.message_id == message_id
    });
    match session {
        Session::Waiting(_) if active => Face::Loading,
        Session::Playing(_) if active => Face::Pause,
        Session::Failed(_) if active => Face::Retry,
        _ => Face::Play,
    }
}

fn clock(total_secs: u32) -> String {
    format!("{}:{:02}", total_secs / 60, total_secs % 60)
}

pub(crate) struct Player<B: Backend = engine::Engine> {
    session: Session,
    /// Latest play sent to the device. Any other completion is ignored.
    generation: u64,
    engine: B,
    /// The video of the open chat (ADR 0015).
    #[cfg(feature = "video")]
    video: super::video::VideoSlot,
}

impl Player<engine::Engine> {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            session: Session::Idle,
            generation: 0,
            engine: engine::Engine::new(),
            #[cfg(feature = "video")]
            video: super::video::VideoSlot::default(),
        }
    }
}

impl<B: Backend> Drop for Player<B> {
    fn drop(&mut self) {
        self.engine.shutdown();
    }
}

impl<B: Backend> Player<B> {
    pub(crate) fn shutdown(&mut self) {
        self.engine.shutdown();
        self.session = Session::Idle;
        #[cfg(feature = "video")]
        self.video.shutdown();
    }

    /// Close the video view, as Esc does. `true` when one was open.
    pub(crate) fn take_video_escape(&mut self) -> bool {
        #[cfg(feature = "video")]
        {
            self.video.take_escape()
        }
        #[cfg(not(feature = "video"))]
        {
            false
        }
    }

    /// The video view over the whole window, when a clip plays.
    pub(crate) fn video_overlay(&mut self, ui: &mut egui::Ui, messages: &[ChatMessage]) {
        #[cfg(feature = "video")]
        self.video.overlay(ui, messages);
        #[cfg(not(feature = "video"))]
        let _ = (ui, messages);
    }

    /// The controls of a video in its row. `tapped` is a tap on its
    /// thumbnail. A build with no `video` feature draws nothing: the row
    /// keeps its thumbnail and its label.
    #[allow(
        clippy::too_many_arguments,
        reason = "one row: its ids, its clip, and the tap"
    )]
    pub(crate) fn show_video(
        &mut self,
        ui: &mut egui::Ui,
        protocol: ProtocolId,
        conversation_id: &str,
        message_id: &str,
        video: Option<&MessageVideo>,
        tapped: bool,
        out: &mut Vec<Intent>,
    ) {
        #[cfg(feature = "video")]
        if let Some(video) = video {
            self.video.show(
                ui,
                protocol,
                conversation_id,
                message_id,
                video,
                tapped,
                out,
            );
        }
        #[cfg(not(feature = "video"))]
        let _ = (
            ui,
            protocol,
            conversation_id,
            message_id,
            video,
            tapped,
            out,
        );
    }

    pub(crate) fn sync(&mut self, messages: &[ChatMessage]) {
        #[cfg(feature = "video")]
        self.video.sync(messages);
        // A completion can already be queued when a newer play replaces it.
        // The worker stamps the play that produced it. Applying an older one
        // would stop or clear the note that is current now.
        if let Some(done) = self.engine.take_done()
            && done.generation() == self.generation
        {
            let input = match done {
                engine::Done::Finished { .. } => Input::Ended,
                engine::Done::Rejected { .. } => Input::Rejected,
            };
            self.commit(step(&self.session, input));
        }
        let Some(key) = self.session.key().cloned() else {
            return;
        };
        let input = match messages.iter().find(|message| same_message(message, &key)) {
            Some(message) => match message.audio.as_ref() {
                Some(audio) => Input::Seen {
                    key,
                    note: note_state(audio),
                },
                None => Input::Gone(key),
            },
            None => Input::Gone(key),
        };
        self.commit(step(&self.session, input));
    }

    pub(crate) fn show(
        &mut self,
        ui: &mut egui::Ui,
        protocol: ProtocolId,
        conversation_id: &str,
        message_id: &str,
        audio: Option<&MessageAudio>,
        out: &mut Vec<Intent>,
    ) {
        let Some(audio) = audio else {
            return;
        };
        let clicked = ui
            .push_id((conversation_id, message_id), |ui| {
                paint(
                    ui,
                    face(&self.session, protocol, conversation_id, message_id),
                    audio,
                )
            })
            .inner;
        if clicked {
            self.press(protocol, conversation_id, message_id, audio, out);
        }
    }

    fn press(
        &mut self,
        protocol: ProtocolId,
        conversation_id: &str,
        message_id: &str,
        audio: &MessageAudio,
        out: &mut Vec<Intent>,
    ) {
        let key = VoiceKey {
            protocol,
            conversation_id: conversation_id.to_owned(),
            message_id: message_id.to_owned(),
        };
        let transition = step(
            &self.session,
            Input::Click {
                key,
                note: note_state(audio),
            },
        );
        if let Some(file_id) = self.commit(transition) {
            out.push(Intent::PlayAudio {
                protocol,
                conversation_id: conversation_id.to_owned(),
                message_id: message_id.to_owned(),
                file_id,
            });
        }
    }

    fn commit(&mut self, transition: Transition) -> Option<i32> {
        self.session = transition.session;
        let mut download = None;
        for effect in transition.effects {
            match effect {
                Effect::None => {}
                Effect::Download { file_id } => download = Some(file_id),
                Effect::Start(path) => {
                    self.generation += 1;
                    self.engine.send(engine::Cmd::Play {
                        path,
                        generation: self.generation,
                    });
                }
                Effect::StopSink => self.engine.send(engine::Cmd::Stop),
            }
        }
        download
    }
}

fn same_message(message: &ChatMessage, key: &VoiceKey) -> bool {
    message.protocol == key.protocol
        && message.conversation_id == key.conversation_id
        && message.id == key.message_id
}

fn paint(ui: &mut egui::Ui, face: Face, audio: &MessageAudio) -> bool {
    let palette = theme::palette(ui);
    match face {
        // Signal and some audio files do not say how long they are.
        Face::Play if audio.duration_secs() == 0 => voice_button(ui, "Play"),
        Face::Play => voice_button(ui, &format!("Play {}", clock(audio.duration_secs()))),
        Face::Pause => voice_button(ui, "Pause"),
        Face::Loading => {
            ui.horizontal(|ui| {
                motion::spinner(ui);
                ui.label(
                    RichText::new("Loading voice message…")
                        .small()
                        .color(palette.text2),
                );
            });
            false
        }
        Face::Retry => {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Couldn't play this voice message.")
                        .small()
                        .color(palette.error),
                );
                voice_button(ui, "Retry")
            })
            .inner
        }
    }
}

fn voice_button(ui: &mut egui::Ui, label: &str) -> bool {
    ui.add(egui::Button::new(RichText::new(label).small()).small())
        .clicked()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use thinwire_core::Intent;
    use thinwire_protocol::{Arrival, AudioPath, ChatMessage, Delivery, MessageAudio, ProtocolId};

    use super::engine::{Cmd, Done};
    use super::{
        Backend, Effect, Input, NoteState, Player, Session, VoiceKey, clock, note_state, step,
    };

    #[derive(Default)]
    struct Fake {
        cmds: Vec<Cmd>,
        done: VecDeque<Done>,
    }

    impl Backend for Fake {
        fn send(&mut self, cmd: Cmd) {
            self.cmds.push(cmd);
        }

        fn take_done(&mut self) -> Option<Done> {
            self.done.pop_front()
        }

        fn shutdown(&mut self) {}
    }

    fn test_player() -> Player<Fake> {
        Player {
            session: Session::Idle,
            generation: 0,
            engine: Fake::default(),
            #[cfg(feature = "video")]
            video: super::super::video::VideoSlot::default(),
        }
    }

    fn play_generation(cmd: &Cmd) -> u64 {
        let Cmd::Play { generation, .. } = cmd else {
            panic!("expected play");
        };
        *generation
    }

    fn key(message_id: &str) -> VoiceKey {
        VoiceKey {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            message_id: message_id.into(),
        }
    }

    fn path() -> AudioPath {
        AudioPath::new("voice.oga").expect("path")
    }

    fn effects(transition: &super::Transition) -> Vec<Effect> {
        transition
            .effects
            .iter()
            .filter(|effect| !matches!(effect, Effect::None))
            .cloned()
            .collect()
    }

    fn assert_step(from: &Session, input: Input, next: Session, want: &[Effect]) {
        let transition = step(from, input);
        assert_eq!(transition.session, next);
        assert_eq!(effects(&transition), want);
    }

    #[test]
    fn step_follows_the_playback_table() {
        let k = key("m");
        let other = key("n");
        let local = NoteState::Local { path: path() };
        let remote = NoteState::Remote { file_id: 42 };
        let broken = NoteState::Broken { file_id: 42 };
        let start = Effect::Start(path());
        let download = Effect::Download { file_id: 42 };
        let other_download = Effect::Download { file_id: 9 };
        let other_remote = NoteState::Remote { file_id: 9 };

        assert_step(
            &Session::Idle,
            Input::Click {
                key: k.clone(),
                note: remote,
            },
            Session::Waiting(k.clone()),
            std::slice::from_ref(&download),
        );
        // A helper protocol downloads its audio too.
        let mut signal = k.clone();
        signal.protocol = ProtocolId::Signal;
        assert_step(
            &Session::Idle,
            Input::Click {
                key: signal.clone(),
                note: NoteState::Remote { file_id: 42 },
            },
            Session::Waiting(signal),
            std::slice::from_ref(&download),
        );
        assert_step(
            &Session::Idle,
            Input::Click {
                key: k.clone(),
                note: NoteState::Local { path: path() },
            },
            Session::Playing(k.clone()),
            std::slice::from_ref(&start),
        );
        assert_step(
            &Session::Idle,
            Input::Click {
                key: k.clone(),
                note: broken,
            },
            Session::Waiting(k.clone()),
            std::slice::from_ref(&download),
        );

        assert_step(
            &Session::Waiting(k.clone()),
            Input::Click {
                key: k.clone(),
                note: NoteState::Remote { file_id: 42 },
            },
            Session::Waiting(k.clone()),
            &[],
        );
        assert_step(
            &Session::Waiting(k.clone()),
            Input::Click {
                key: other.clone(),
                note: other_remote,
            },
            Session::Waiting(other.clone()),
            std::slice::from_ref(&other_download),
        );
        assert_step(
            &Session::Waiting(k.clone()),
            Input::Click {
                key: other.clone(),
                note: NoteState::Local { path: path() },
            },
            Session::Playing(other.clone()),
            std::slice::from_ref(&start),
        );
        assert_step(
            &Session::Waiting(k.clone()),
            Input::Seen {
                key: k.clone(),
                note: NoteState::Local { path: path() },
            },
            Session::Playing(k.clone()),
            std::slice::from_ref(&start),
        );
        assert_step(
            &Session::Waiting(k.clone()),
            Input::Seen {
                key: k.clone(),
                note: NoteState::Broken { file_id: 42 },
            },
            Session::Failed(k.clone()),
            &[],
        );
        assert_step(
            &Session::Waiting(k.clone()),
            Input::Seen {
                key: k.clone(),
                note: NoteState::Remote { file_id: 42 },
            },
            Session::Waiting(k.clone()),
            &[],
        );
        assert_step(
            &Session::Waiting(k.clone()),
            Input::Gone(k.clone()),
            Session::Idle,
            &[],
        );

        assert_step(
            &Session::Playing(k.clone()),
            Input::Click {
                key: k.clone(),
                note: local,
            },
            Session::Idle,
            &[Effect::StopSink],
        );
        assert_step(
            &Session::Playing(k.clone()),
            Input::Click {
                key: other.clone(),
                note: NoteState::Remote { file_id: 9 },
            },
            Session::Waiting(other.clone()),
            &[Effect::StopSink, other_download],
        );
        assert_step(
            &Session::Playing(k.clone()),
            Input::Click {
                key: other.clone(),
                note: NoteState::Local { path: path() },
            },
            Session::Playing(other.clone()),
            std::slice::from_ref(&start),
        );
        for note in [
            NoteState::Remote { file_id: 1 },
            NoteState::Local { path: path() },
            NoteState::Broken { file_id: 1 },
        ] {
            assert_step(
                &Session::Playing(k.clone()),
                Input::Seen {
                    key: k.clone(),
                    note,
                },
                Session::Playing(k.clone()),
                &[],
            );
        }
        assert_step(
            &Session::Playing(k.clone()),
            Input::Gone(k.clone()),
            Session::Idle,
            &[Effect::StopSink],
        );
        assert_step(
            &Session::Playing(k.clone()),
            Input::Ended,
            Session::Idle,
            &[],
        );
        assert_step(
            &Session::Playing(k.clone()),
            Input::Rejected,
            Session::Failed(k.clone()),
            &[Effect::StopSink],
        );

        assert_step(
            &Session::Failed(k.clone()),
            Input::Click {
                key: k.clone(),
                note: NoteState::Remote { file_id: 42 },
            },
            Session::Waiting(k.clone()),
            std::slice::from_ref(&download),
        );
        assert_step(
            &Session::Failed(k.clone()),
            Input::Click {
                key: k.clone(),
                note: NoteState::Broken { file_id: 42 },
            },
            Session::Waiting(k.clone()),
            &[download],
        );
        assert_step(
            &Session::Failed(k.clone()),
            Input::Click {
                key: k.clone(),
                note: NoteState::Local { path: path() },
            },
            Session::Playing(k.clone()),
            &[start],
        );
        assert_step(
            &Session::Failed(k.clone()),
            Input::Seen {
                key: k.clone(),
                note: NoteState::Local { path: path() },
            },
            Session::Failed(k.clone()),
            &[],
        );
        assert_step(
            &Session::Failed(k.clone()),
            Input::Gone(k.clone()),
            Session::Idle,
            &[],
        );

        for session in [
            Session::Idle,
            Session::Waiting(k.clone()),
            Session::Failed(k.clone()),
        ] {
            assert_step(&session, Input::Ended, session.clone(), &[]);
            assert_step(&session, Input::Rejected, session.clone(), &[]);
        }

        assert_step(
            &Session::Idle,
            Input::Seen {
                key: k,
                note: NoteState::Local { path: path() },
            },
            Session::Idle,
            &[],
        );
    }

    #[test]
    fn duration_is_minutes_and_seconds() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(5), "0:05");
        assert_eq!(clock(65), "1:05");
        assert_eq!(clock(3661), "61:01");
    }

    fn voice(id: &str, audio: Option<MessageAudio>) -> ChatMessage {
        ChatMessage {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            id: id.into(),
            sender: "Ada".into(),
            body: "Voice message".into(),
            outbound: false,
            delivery: Delivery::Sent,
            sent_at: 0,
            arrival: Arrival::History,
            topic: None,
            links: Vec::new(),
            photo: None,
            audio,
            video: None,
            file: None,
            reactions: Vec::new(),
            reply_to: None,
            edited: false,
        }
    }

    #[test]
    fn two_clicks_while_waiting_ask_for_the_file_once() {
        let audio = MessageAudio::telegram(12, 42, None).expect("note");
        let mut player = test_player();
        let mut out = Vec::new();
        player.press(ProtocolId::Telegram, "telegram:1", "m", &audio, &mut out);
        player.press(ProtocolId::Telegram, "telegram:1", "m", &audio, &mut out);
        assert_eq!(
            out,
            vec![Intent::PlayAudio {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:1".into(),
                message_id: "m".into(),
                file_id: 42,
            }]
        );
        assert!(player.engine.cmds.is_empty());
        assert!(matches!(player.session, Session::Waiting(_)));
    }

    #[test]
    fn a_local_file_plays_without_an_intent_and_pause_stops_it() {
        let audio = MessageAudio::telegram(3, 7, Some(path())).expect("note");
        let mut player = test_player();
        let mut out = Vec::new();
        player.press(ProtocolId::Telegram, "telegram:1", "m", &audio, &mut out);
        assert!(out.is_empty());
        assert!(matches!(player.engine.cmds.as_slice(), [Cmd::Play { .. }]));
        player.press(ProtocolId::Telegram, "telegram:1", "m", &audio, &mut out);
        assert!(out.is_empty());
        assert!(matches!(
            player.engine.cmds.as_slice(),
            [Cmd::Play { .. }, Cmd::Stop]
        ));
        assert!(matches!(player.session, Session::Idle));
    }

    #[test]
    fn a_finished_download_starts_playback_and_leaving_the_thread_stops_it() {
        let waiting = MessageAudio::telegram(4, 9, None).expect("note");
        let mut player = test_player();
        let mut out = Vec::new();
        player.press(ProtocolId::Telegram, "telegram:1", "m", &waiting, &mut out);
        assert_eq!(out.len(), 1);
        let mut ready = waiting;
        ready.set_file(path());
        player.sync(std::slice::from_ref(&voice("m", Some(ready))));
        assert!(matches!(player.session, Session::Playing(_)));
        assert!(matches!(player.engine.cmds.as_slice(), [Cmd::Play { .. }]));
        player.sync(&[]);
        assert!(matches!(player.session, Session::Idle));
        assert!(matches!(
            player.engine.cmds.as_slice(),
            [Cmd::Play { .. }, Cmd::Stop]
        ));
    }

    #[test]
    fn pause_then_a_finished_download_does_not_resume() {
        let audio = MessageAudio::telegram(2, 5, Some(path())).expect("note");
        let mut player = test_player();
        let mut out = Vec::new();
        player.press(ProtocolId::Telegram, "telegram:1", "m", &audio, &mut out);
        player.press(ProtocolId::Telegram, "telegram:1", "m", &audio, &mut out);
        player.sync(std::slice::from_ref(&voice("m", Some(audio))));
        assert!(matches!(player.session, Session::Idle));
        assert!(matches!(
            player.engine.cmds.as_slice(),
            [Cmd::Play { .. }, Cmd::Stop]
        ));
    }

    #[test]
    fn a_done_from_a_superseded_play_does_not_stop_or_clear_the_current_note() {
        let first = MessageAudio::telegram(3, 7, Some(path())).expect("note");
        let second = MessageAudio::telegram(4, 8, Some(path())).expect("note");
        let mut player = test_player();
        let mut out = Vec::new();
        player.press(ProtocolId::Telegram, "telegram:1", "a", &first, &mut out);
        let older = play_generation(&player.engine.cmds[0]);
        // The end of note A is already queued when the user starts note B.
        player
            .engine
            .done
            .push_back(Done::Finished { generation: older });
        player.press(ProtocolId::Telegram, "telegram:1", "b", &second, &mut out);
        let current = play_generation(&player.engine.cmds[1]);
        assert_ne!(older, current);

        let open = [voice("a", Some(first)), voice("b", Some(second))];
        player.sync(&open);
        assert!(matches!(
            &player.session,
            Session::Playing(key) if key.message_id == "b"
        ));
        assert!(matches!(
            player.engine.cmds.as_slice(),
            [Cmd::Play { .. }, Cmd::Play { .. }]
        ));

        // Note A was still decoding and rejects after B is the current play.
        player
            .engine
            .done
            .push_back(Done::Rejected { generation: older });
        player.sync(&open);
        assert!(matches!(
            &player.session,
            Session::Playing(key) if key.message_id == "b"
        ));
        assert!(matches!(
            player.engine.cmds.as_slice(),
            [Cmd::Play { .. }, Cmd::Play { .. }]
        ));

        player.sync(&[]);
        assert!(matches!(player.session, Session::Idle));
        assert!(matches!(
            player.engine.cmds.as_slice(),
            [Cmd::Play { .. }, Cmd::Play { .. }, Cmd::Stop]
        ));
    }

    #[test]
    fn a_finished_event_of_the_current_play_clears_the_session() {
        let note = MessageAudio::telegram(3, 7, Some(path())).expect("note");
        let mut player = test_player();
        let mut out = Vec::new();
        player.press(ProtocolId::Telegram, "telegram:1", "a", &note, &mut out);
        let generation = play_generation(&player.engine.cmds[0]);
        player.engine.done.push_back(Done::Finished { generation });
        player.sync(std::slice::from_ref(&voice("a", Some(note))));
        assert!(matches!(player.session, Session::Idle));
        assert!(matches!(player.engine.cmds.as_slice(), [Cmd::Play { .. }]));
    }

    #[test]
    fn a_rejected_event_of_the_current_play_stops_it() {
        let note = MessageAudio::telegram(3, 7, Some(path())).expect("note");
        let mut player = test_player();
        let mut out = Vec::new();
        player.press(ProtocolId::Telegram, "telegram:1", "a", &note, &mut out);
        let generation = play_generation(&player.engine.cmds[0]);
        player.engine.done.push_back(Done::Rejected { generation });
        player.sync(std::slice::from_ref(&voice("a", Some(note))));
        assert!(matches!(
            &player.session,
            Session::Failed(key) if key.message_id == "a"
        ));
        assert!(matches!(
            player.engine.cmds.as_slice(),
            [Cmd::Play { .. }, Cmd::Stop]
        ));
    }

    #[test]
    fn a_path_wins_over_the_failed_flag() {
        let mut audio = MessageAudio::telegram(1, 3, Some(path())).expect("note");
        audio.mark_failed();
        assert!(matches!(note_state(&audio), NoteState::Local { .. }));
    }
}
