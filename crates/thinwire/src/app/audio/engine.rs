//! rodio 0.23's symphonia features decode Vorbis, not Opus. Telegram voice
//! notes are Opus inside Ogg, so this module reads those packets itself.

use std::fs::File;
use std::io::{Read, Seek};
use std::num::NonZero;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::Duration;

use rodio::buffer::SamplesBuffer;
use rodio::source::Source;
use rodio::{Decoder, DeviceSinkBuilder, Player as Sink};
use thinwire_protocol::AudioPath;

use super::Backend;

/// At most one Opus frame, in samples per channel.
const OPUS_MAX_FRAME: usize = 5760;
const OPUS_RATE: u32 = 48_000;
const END_POLL: Duration = Duration::from_millis(50);
/// PCM kept for one note. 48 kHz `f32` mono is 11.52 MB a minute, so half an
/// hour is about 346 MB. A decode past this is refused.
const MAX_DECODED_BYTES: usize = 256 * 1024 * 1024;
const MAX_DECODED_SAMPLES: usize = MAX_DECODED_BYTES / std::mem::size_of::<f32>();
const DECODE_POLL_SAMPLES: usize = 960;

pub(crate) enum Cmd {
    Play { path: AudioPath, generation: u64 },
    Stop,
}

/// End of one play. `generation` is the [`Cmd::Play`] that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Done {
    Finished { generation: u64 },
    Rejected { generation: u64 },
}

impl Done {
    pub(super) fn generation(self) -> u64 {
        match self {
            Self::Finished { generation } | Self::Rejected { generation } => generation,
        }
    }
}

enum Wire {
    Play { path: AudioPath, generation: u64 },
    Stop,
    Shutdown,
}

struct WireRx {
    rx: Receiver<Wire>,
    held: Option<Wire>,
}

#[must_use = "Halt has taken a command into WireRx; return Cut or skip append"]
enum Pace {
    Go,
    Halt,
}

enum Recv {
    Cmd(Wire),
    Timeout,
    Dead,
}

impl WireRx {
    fn new(rx: Receiver<Wire>) -> Self {
        Self { rx, held: None }
    }

    fn poll(&mut self) -> Pace {
        if self.held.is_some() {
            return Pace::Halt;
        }
        match self.rx.try_recv() {
            Ok(cmd) => {
                self.held = Some(cmd);
                Pace::Halt
            }
            Err(TryRecvError::Empty) => Pace::Go,
            Err(TryRecvError::Disconnected) => {
                self.held = Some(Wire::Shutdown);
                Pace::Halt
            }
        }
    }

    fn recv(&mut self) -> Result<Wire, ()> {
        if let Some(cmd) = self.held.take() {
            return Ok(cmd);
        }
        self.rx.recv().map_err(|_| ())
    }

    fn recv_timeout(&mut self, wait: Duration) -> Recv {
        if let Some(cmd) = self.held.take() {
            return Recv::Cmd(cmd);
        }
        match self.rx.recv_timeout(wait) {
            Ok(cmd) => Recv::Cmd(cmd),
            Err(RecvTimeoutError::Timeout) => Recv::Timeout,
            Err(RecvTimeoutError::Disconnected) => Recv::Dead,
        }
    }
}

pub(crate) struct Engine {
    cmd_tx: Option<Sender<Wire>>,
    done_rx: Receiver<Done>,
    done_tx: Option<Sender<Done>>,
    join: Option<JoinHandle<()>>,
}

impl Engine {
    pub(super) fn new() -> Self {
        let (done_tx, done_rx) = mpsc::channel();
        Self {
            cmd_tx: None,
            done_rx,
            done_tx: Some(done_tx),
            join: None,
        }
    }
}

impl Backend for Engine {
    fn send(&mut self, cmd: Cmd) {
        if self.cmd_tx.is_none() && matches!(cmd, Cmd::Stop) {
            return;
        }
        self.start();
        let Some(tx) = &self.cmd_tx else {
            return;
        };
        let wire = match cmd {
            Cmd::Play { path, generation } => Wire::Play { path, generation },
            Cmd::Stop => Wire::Stop,
        };
        let _ = tx.send(wire);
    }

    fn take_done(&mut self) -> Option<Done> {
        self.done_rx.try_recv().ok()
    }

    fn shutdown(&mut self) {
        if let Some(tx) = self.cmd_tx.take() {
            let _ = tx.send(Wire::Shutdown);
        }
        self.done_tx.take();
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Engine {
    fn start(&mut self) {
        if self.cmd_tx.is_some() {
            return;
        }
        let Some(done_tx) = self.done_tx.take() else {
            return;
        };
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let join = std::thread::Builder::new()
            .name("thinwire-voice".into())
            .spawn(move || worker(cmd_rx, done_tx))
            .expect("voice playback thread");
        self.cmd_tx = Some(cmd_tx);
        self.join = Some(join);
    }
}

struct Output {
    player: Option<Sink>,
    sink: Option<rodio::MixerDeviceSink>,
    owes_end: Option<u64>,
}

fn worker(rx: Receiver<Wire>, done: Sender<Done>) {
    let mut cmds = WireRx::new(rx);
    let mut output = Output {
        player: None,
        sink: None,
        owes_end: None,
    };
    loop {
        let incoming = if output.owes_end.is_some() {
            cmds.recv_timeout(END_POLL)
        } else {
            match cmds.recv() {
                Ok(cmd) => Recv::Cmd(cmd),
                Err(()) => Recv::Dead,
            }
        };
        match incoming {
            Recv::Cmd(Wire::Play { path, generation }) => {
                play(&mut output, &path, generation, &done, &mut cmds);
            }
            Recv::Cmd(Wire::Stop) => stop(&mut output),
            Recv::Cmd(Wire::Shutdown) | Recv::Dead => break,
            Recv::Timeout => {}
        }
        if let Some(generation) = output.owes_end
            && output.player.as_ref().is_some_and(Sink::empty)
        {
            output.owes_end = None;
            output.player = None;
            let _ = done.send(Done::Finished { generation });
        }
    }
}

fn play(
    output: &mut Output,
    path: &AudioPath,
    generation: u64,
    done: &Sender<Done>,
    cmds: &mut WireRx,
) {
    stop(output);
    if matches!(cmds.poll(), Pace::Halt) {
        return;
    }
    match read_voice(path, || cmds.poll()) {
        Decoded::Cut => {}
        Decoded::Rejected => reject(cmds, done, generation),
        Decoded::Buffer(buffer) => {
            if matches!(cmds.poll(), Pace::Halt) {
                return;
            }
            if ensure_sink(output).is_err() {
                tracing::warn!("voice output is unavailable");
                reject(cmds, done, generation);
                return;
            }
            if matches!(cmds.poll(), Pace::Halt) {
                return;
            }
            let player = {
                let sink = output.sink.as_ref().expect("sink");
                Sink::connect_new(sink.mixer())
            };
            player.append(buffer);
            output.player = Some(player);
            output.owes_end = Some(generation);
        }
    }
}

fn reject(cmds: &mut WireRx, done: &Sender<Done>, generation: u64) {
    if matches!(cmds.poll(), Pace::Halt) {
        return;
    }
    let _ = done.send(Done::Rejected { generation });
}

fn stop(output: &mut Output) {
    output.owes_end = None;
    output.player = None;
}

fn ensure_sink(output: &mut Output) -> Result<(), ()> {
    if output.sink.is_some() {
        return Ok(());
    }
    match DeviceSinkBuilder::open_default_sink() {
        Ok(sink) => {
            output.sink = Some(sink);
            Ok(())
        }
        Err(_) => Err(()),
    }
}

fn read_voice(path: &AudioPath, mut pace: impl FnMut() -> Pace) -> Decoded {
    if matches!(pace(), Pace::Halt) {
        return Decoded::Cut;
    }
    let file = match File::open(path.as_path()) {
        Ok(file) => file,
        Err(_) => {
            tracing::warn!("voice file could not be read");
            return Decoded::Rejected;
        }
    };
    match voice_buffer(file, &mut pace) {
        VoiceDecode::Cut => return Decoded::Cut,
        VoiceDecode::Rejected => {
            tracing::warn!("voice note is too large to play");
            return Decoded::Rejected;
        }
        VoiceDecode::Opus(buffer) => return Decoded::Buffer(buffer),
        VoiceDecode::TryVorbis => {}
    }
    if matches!(pace(), Pace::Halt) {
        return Decoded::Cut;
    }
    let file = match File::open(path.as_path()) {
        Ok(file) => file,
        Err(_) => {
            tracing::warn!("voice file could not be read");
            return Decoded::Rejected;
        }
    };
    let decoder = match Decoder::try_from(file) {
        Ok(decoder) => decoder,
        Err(_) => {
            tracing::warn!("voice file could not be decoded");
            return Decoded::Rejected;
        }
    };
    match samples_from_decoder(decoder, &mut pace) {
        Decoded::Rejected => {
            tracing::warn!("voice file could not be decoded");
            Decoded::Rejected
        }
        decoded => decoded,
    }
}

fn samples_from_decoder<R>(decoder: Decoder<R>, mut pace: impl FnMut() -> Pace) -> Decoded
where
    R: Read + Seek + Send + 'static,
{
    let channels = decoder.channels();
    let rate = decoder.sample_rate();
    let mut data = Vec::new();
    let mut since = 0usize;
    for sample in decoder {
        if data.len() >= MAX_DECODED_SAMPLES {
            return Decoded::Rejected;
        }
        data.push(sample);
        since += 1;
        if since == DECODE_POLL_SAMPLES {
            since = 0;
            if matches!(pace(), Pace::Halt) {
                return Decoded::Cut;
            }
        }
    }
    if data.is_empty() || !data.len().is_multiple_of(channels.get() as usize) {
        return Decoded::Rejected;
    }
    Decoded::Buffer(SamplesBuffer::new(channels, rate, data))
}

enum Decoded {
    Buffer(SamplesBuffer),
    Rejected,
    Cut,
}

enum VoiceDecode {
    Opus(SamplesBuffer),
    TryVorbis,
    Rejected,
    Cut,
}

struct OpusHead {
    channels: u8,
    pre_skip: u16,
}

fn voice_buffer(reader: impl Read + Seek, pace: &mut impl FnMut() -> Pace) -> VoiceDecode {
    decode_opus(reader, MAX_DECODED_SAMPLES, pace)
}

fn decode_opus(
    reader: impl Read + Seek,
    max_samples: usize,
    mut pace: impl FnMut() -> Pace,
) -> VoiceDecode {
    let mut reader = ogg::PacketReader::new(reader);
    let head = match reader.read_packet() {
        Ok(Some(packet)) => packet,
        Ok(None) | Err(_) => return VoiceDecode::TryVorbis,
    };
    let OpusHead { channels, pre_skip } = match opus_head(&head.data) {
        Ok(head) => head,
        Err(()) => return VoiceDecode::TryVorbis,
    };
    let channel_count = match channels {
        1 => opus::Channels::Mono,
        2 => opus::Channels::Stereo,
        _ => return VoiceDecode::TryVorbis,
    };
    let mut decoder = match opus::Decoder::new(OPUS_RATE, channel_count) {
        Ok(decoder) => decoder,
        Err(_) => return VoiceDecode::TryVorbis,
    };
    let mut pcm = Vec::new();
    let mut frame = vec![0.0f32; OPUS_MAX_FRAME * channels as usize];
    let mut skip = pre_skip as usize;
    let mut produced = 0usize;
    let mut checked_tags = false;
    loop {
        if matches!(pace(), Pace::Halt) {
            return VoiceDecode::Cut;
        }
        let packet = match reader.read_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(_) => return VoiceDecode::TryVorbis,
        };
        if !checked_tags {
            checked_tags = true;
            if packet.data.starts_with(b"OpusTags") {
                continue;
            }
        }
        let decoded = match decoder.decode_float(&packet.data, &mut frame, false) {
            Ok(decoded) => decoded,
            Err(_) => return VoiceDecode::TryVorbis,
        };
        let samples = decoded.saturating_mul(channels as usize);
        if samples > frame.len() {
            return VoiceDecode::TryVorbis;
        }
        produced = produced.saturating_add(samples);
        if produced > max_samples {
            return VoiceDecode::Rejected;
        }
        let taken = &frame[..samples];
        if skip == 0 {
            pcm.extend_from_slice(taken);
            continue;
        }
        let drop_frames = skip.min(decoded);
        let drop_samples = drop_frames * channels as usize;
        skip -= drop_frames;
        pcm.extend_from_slice(&taken[drop_samples..]);
    }
    if pcm.is_empty() {
        return VoiceDecode::TryVorbis;
    }
    let Some(channels) = NonZero::new(u16::from(channels)) else {
        return VoiceDecode::TryVorbis;
    };
    let Some(rate) = NonZero::new(OPUS_RATE) else {
        return VoiceDecode::TryVorbis;
    };
    VoiceDecode::Opus(SamplesBuffer::new(channels, rate, pcm))
}

fn opus_head(data: &[u8]) -> Result<OpusHead, ()> {
    if data.len() < 19 || &data[..8] != b"OpusHead" || data[8] > 15 {
        return Err(());
    }
    let channels = data[9];
    if channels == 0 || channels > 2 {
        return Err(());
    }
    let pre_skip = u16::from_le_bytes([data[10], data[11]]);
    Ok(OpusHead { channels, pre_skip })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use rodio::source::Source;

    use thinwire_protocol::AudioPath;

    use super::super::Backend;
    use super::{
        Decoded, MAX_DECODED_SAMPLES, Pace, VoiceDecode, decode_opus, read_voice, voice_buffer,
    };

    #[test]
    fn bytes_that_are_not_ogg_opus_are_refused() {
        let junk = Cursor::new(b"not a voice note".to_vec());
        assert!(matches!(
            voice_buffer(junk, &mut || Pace::Go),
            VoiceDecode::TryVorbis
        ));
    }

    #[test]
    fn an_ogg_opus_page_decodes_without_opening_a_device() {
        let VoiceDecode::Opus(buffer) = voice_buffer(Cursor::new(ogg_opus(1)), &mut || Pace::Go)
        else {
            panic!("opus");
        };
        assert_eq!(buffer.channels().get(), 1);
        assert_eq!(buffer.sample_rate().get(), 48_000);
        assert!(buffer.into_iter().next().is_some());
    }

    #[test]
    fn a_note_past_the_decode_cap_is_refused() {
        let bytes = ogg_opus(4);
        let VoiceDecode::Opus(buffer) =
            decode_opus(Cursor::new(bytes.clone()), usize::MAX, || Pace::Go)
        else {
            panic!("opus");
        };
        let count = buffer.into_iter().count();
        assert!(count > 1);
        assert!(matches!(
            decode_opus(Cursor::new(bytes), count - 1, || Pace::Go),
            VoiceDecode::Rejected
        ));
    }

    /// Signal voice notes are AAC, in M4A or in ADTS. Audio files can be
    /// MP3. Each one decodes through the path that the player uses. The
    /// samples are 0.4 s of a 440 Hz tone, made with ffmpeg.
    #[test]
    fn aac_and_mp3_files_decode_for_the_player() {
        let dir = std::env::temp_dir()
            .join("thinwire-engine-tests")
            .join(std::process::id().to_string());
        std::fs::create_dir_all(&dir).expect("dir");
        for (name, bytes) in [
            ("voice.m4a", &include_bytes!("fixtures/voice.m4a")[..]),
            ("voice.aac", &include_bytes!("fixtures/voice.aac")[..]),
            ("audio.mp3", &include_bytes!("fixtures/audio.mp3")[..]),
        ] {
            let path = dir.join(name);
            std::fs::write(&path, bytes).expect("write");
            let audio = AudioPath::new(path.to_string_lossy().into_owned()).expect("path");
            let Decoded::Buffer(buffer) = read_voice(&audio, || Pace::Go) else {
                panic!("{name} decodes");
            };
            assert_eq!(buffer.channels().get(), 1, "{name}");
            assert!(buffer.into_iter().count() > 1_000, "{name} has samples");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_play_and_shutdown_do_not_wait_for_the_rest_of_a_long_decode() {
        let (bytes, decode) = long_enough_opus();
        let dir = std::env::temp_dir()
            .join("thinwire-engine-tests")
            .join(format!("abort-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let long_path = dir.join("long.ogg");
        std::fs::write(&long_path, &bytes).expect("write");
        let long = AudioPath::new(long_path.to_string_lossy().into_owned()).expect("path");
        let gone =
            AudioPath::new(dir.join("gone.ogg").to_string_lossy().into_owned()).expect("path");
        let mut late = Vec::new();

        // The clock starts after the worker is in the decode. It stops when
        // shutdown joins that worker. The next play names a missing file, so
        // the timed section does not open an output device.
        command_during_decode(&long, decode, &mut late, "shutdown", |_| {});
        command_during_decode(&long, decode, &mut late, "stop", |engine| {
            engine.send(super::Cmd::Stop);
        });
        command_during_decode(&long, decode, &mut late, "play", |engine| {
            engine.send(super::Cmd::Play {
                path: gone,
                generation: 2,
            });
        });

        let _ = std::fs::remove_dir_all(&dir);
        assert!(late.is_empty(), "{}", late.join("\n"));
    }

    fn command_during_decode(
        long: &AudioPath,
        decode: std::time::Duration,
        late: &mut Vec<String>,
        what: &str,
        then: impl FnOnce(&mut super::Engine),
    ) {
        let mut engine = super::Engine::new();
        engine.send(super::Cmd::Play {
            path: long.clone(),
            generation: 1,
        });
        std::thread::sleep(decode / 5);
        then(&mut engine);
        let started = std::time::Instant::now();
        engine.shutdown();
        note_if_late(late, started.elapsed(), decode, what);
        while let Some(done) = engine.take_done() {
            if done.generation() == 1 {
                late.push(format!("{what} reported generation 1"));
            }
        }
    }

    fn note_if_late(
        late: &mut Vec<String>,
        waited: std::time::Duration,
        decode: std::time::Duration,
        what: &str,
    ) {
        let limit = decode / 4;
        if waited >= limit {
            late.push(format!(
                "{what} waited {waited:?} for a note whose decode takes {decode:?}"
            ));
        }
    }

    fn long_enough_opus() -> (Vec<u8>, std::time::Duration) {
        let mut frames = 16_000usize;
        loop {
            let bytes = ogg_opus(frames);
            let started = std::time::Instant::now();
            let decoded = decode_opus(Cursor::new(bytes.clone()), usize::MAX, || Pace::Go);
            let decode = started.elapsed();
            assert!(
                matches!(decoded, VoiceDecode::Opus(_)),
                "the baseline note must decode"
            );
            if decode >= std::time::Duration::from_millis(500) {
                return (bytes, decode);
            }
            frames = frames.saturating_mul(2);
            assert!(
                frames <= 1_024_000,
                "could not build a decode long enough to measure"
            );
        }
    }

    #[test]
    fn thirty_minutes_of_mono_pcm_is_over_the_decode_cap() {
        let samples = 48_000usize * 30 * 60;
        assert!(samples > MAX_DECODED_SAMPLES);
    }

    fn ogg_opus(frames: usize) -> Vec<u8> {
        let mut encoder =
            opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Audio)
                .expect("encoder");
        let mut bytes = Vec::new();
        {
            let mut writer = ogg::PacketWriter::new(&mut bytes);
            writer
                .write_packet(opus_head_bytes(), 1, ogg::PacketWriteEndInfo::EndPage, 0)
                .expect("head");
            writer
                .write_packet(opus_tags(), 1, ogg::PacketWriteEndInfo::EndPage, 0)
                .expect("tags");
            for index in 0..frames {
                let packet = encoder.encode_vec(&[0i16; 960], 4_000).expect("frame");
                let end = if index + 1 == frames {
                    ogg::PacketWriteEndInfo::EndStream
                } else {
                    ogg::PacketWriteEndInfo::EndPage
                };
                writer
                    .write_packet(packet, 1, end, (index as u64 + 1) * 960)
                    .expect("audio");
            }
        }
        bytes
    }

    fn opus_head_bytes() -> Vec<u8> {
        let mut head = b"OpusHead".to_vec();
        head.push(1);
        head.push(1);
        head.extend_from_slice(&0u16.to_le_bytes());
        head.extend_from_slice(&48_000u32.to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes());
        head.push(0);
        head
    }

    fn opus_tags() -> Vec<u8> {
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&1u32.to_le_bytes());
        tags.push(b't');
        tags.extend_from_slice(&0u32.to_le_bytes());
        tags
    }
}
