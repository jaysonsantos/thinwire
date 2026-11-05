//! The output device. The UI thread never calls into rodio.
//!
//! rodio 0.23's symphonia features decode Vorbis, not Opus. Telegram voice
//! notes are Opus inside Ogg, so this module reads those packets itself.

use std::fs::File;
use std::io::{Read, Seek};
use std::num::NonZero;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
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
/// How often the worker looks for the end of the buffer while a note plays.
const END_POLL: Duration = Duration::from_millis(50);

pub(crate) enum Cmd {
    Play(AudioPath),
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Done {
    Finished,
    Rejected,
}

enum Wire {
    Play(AudioPath),
    Stop,
    Shutdown,
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
            Cmd::Play(path) => Wire::Play(path),
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

/// The sink stays open across notes. `playing` means this buffer still owes
/// the UI an end event. Stop and a new play clear it before the old player
/// is dropped, so the end of a replaced note is not reported.
struct Output {
    player: Option<Sink>,
    sink: Option<rodio::MixerDeviceSink>,
    playing: bool,
}

fn worker(rx: Receiver<Wire>, done: Sender<Done>) {
    let mut output = Output {
        player: None,
        sink: None,
        playing: false,
    };
    loop {
        let incoming = if output.playing {
            match rx.recv_timeout(END_POLL) {
                Ok(cmd) => Some(cmd),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(cmd) => Some(cmd),
                Err(_) => break,
            }
        };
        if let Some(cmd) = incoming {
            match cmd {
                Wire::Play(path) => play(&mut output, &path, &done),
                Wire::Stop => stop(&mut output),
                Wire::Shutdown => break,
            }
        }
        if output.playing && output.player.as_ref().is_some_and(Sink::empty) {
            output.playing = false;
            output.player = None;
            let _ = done.send(Done::Finished);
        }
    }
}

fn play(output: &mut Output, path: &AudioPath, done: &Sender<Done>) {
    stop(output);
    let buffer = match read_voice(path) {
        Ok(buffer) => buffer,
        Err(()) => {
            let _ = done.send(Done::Rejected);
            return;
        }
    };
    if ensure_sink(output).is_err() {
        tracing::warn!("voice output is unavailable");
        let _ = done.send(Done::Rejected);
        return;
    }
    let player = {
        let sink = output.sink.as_ref().expect("sink");
        Sink::connect_new(sink.mixer())
    };
    player.append(buffer);
    output.player = Some(player);
    output.playing = true;
}

fn stop(output: &mut Output) {
    output.playing = false;
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

fn read_voice(path: &AudioPath) -> Result<SamplesBuffer, ()> {
    let file = match File::open(path.as_path()) {
        Ok(file) => file,
        Err(_) => {
            tracing::warn!("voice file could not be read");
            return Err(());
        }
    };
    if let Ok(buffer) = voice_buffer(file) {
        return Ok(buffer);
    }
    let file = match File::open(path.as_path()) {
        Ok(file) => file,
        Err(_) => {
            tracing::warn!("voice file could not be read");
            return Err(());
        }
    };
    match Decoder::try_from(file)
        .ok()
        .and_then(|decoder| samples_from_decoder(decoder).ok())
    {
        Some(buffer) => Ok(buffer),
        None => {
            tracing::warn!("voice file could not be decoded");
            Err(())
        }
    }
}

fn samples_from_decoder<R>(decoder: Decoder<R>) -> Result<SamplesBuffer, ()>
where
    R: Read + Seek + Send + 'static,
{
    let channels = decoder.channels();
    let rate = decoder.sample_rate();
    let data: Vec<rodio::Sample> = decoder.collect();
    if data.is_empty() || !data.len().is_multiple_of(channels.get() as usize) {
        return Err(());
    }
    Ok(SamplesBuffer::new(channels, rate, data))
}

/// Opus packets in an Ogg page. `Err` when the file is not that, so the
/// caller can try rodio's Vorbis decoder on the same path.
fn voice_buffer(reader: impl Read + Seek) -> Result<SamplesBuffer, ()> {
    let mut reader = ogg::PacketReader::new(reader);
    let head = reader.read_packet().map_err(|_| ())?.ok_or(())?;
    let (channels, pre_skip) = opus_head(&head.data)?;
    let mut packets = Vec::new();
    if let Some(second) = reader.read_packet().map_err(|_| ())?
        && !second.data.starts_with(b"OpusTags")
    {
        packets.push(second.data);
    }
    while let Some(packet) = reader.read_packet().map_err(|_| ())? {
        packets.push(packet.data);
    }
    let channel_count = match channels {
        1 => opus::Channels::Mono,
        2 => opus::Channels::Stereo,
        _ => return Err(()),
    };
    let mut decoder = opus::Decoder::new(OPUS_RATE, channel_count).map_err(|_| ())?;
    let mut pcm = Vec::new();
    let mut frame = vec![0.0f32; OPUS_MAX_FRAME * channels as usize];
    let mut skip = pre_skip as usize;
    for packet in &packets {
        let decoded = decoder
            .decode_float(packet, &mut frame, false)
            .map_err(|_| ())?;
        let samples = decoded.saturating_mul(channels as usize);
        if samples > frame.len() {
            return Err(());
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
        return Err(());
    }
    let channels = NonZero::new(u16::from(channels)).ok_or(())?;
    let rate = NonZero::new(OPUS_RATE).ok_or(())?;
    Ok(SamplesBuffer::new(channels, rate, pcm))
}

/// `Ok((channels, pre_skip))` for an OpusHead packet.
fn opus_head(data: &[u8]) -> Result<(u8, u16), ()> {
    if data.len() < 19 || &data[..8] != b"OpusHead" || data[8] > 15 {
        return Err(());
    }
    let channels = data[9];
    if channels == 0 || channels > 2 {
        return Err(());
    }
    let pre_skip = u16::from_le_bytes([data[10], data[11]]);
    Ok((channels, pre_skip))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use rodio::source::Source;

    use super::voice_buffer;

    #[test]
    fn bytes_that_are_not_ogg_opus_are_refused() {
        let junk = Cursor::new(b"not a voice note".to_vec());
        assert!(voice_buffer(junk).is_err());
    }

    #[test]
    fn an_ogg_opus_page_decodes_without_opening_a_device() {
        let mut encoder =
            opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Audio)
                .expect("encoder");
        let packet = encoder.encode_vec(&[0i16; 960], 4_000).expect("frame");
        let mut bytes = Vec::new();
        {
            let mut writer = ogg::PacketWriter::new(&mut bytes);
            writer
                .write_packet(opus_head_bytes(), 1, ogg::PacketWriteEndInfo::EndPage, 0)
                .expect("head");
            writer
                .write_packet(opus_tags(), 1, ogg::PacketWriteEndInfo::EndPage, 0)
                .expect("tags");
            writer
                .write_packet(packet, 1, ogg::PacketWriteEndInfo::EndStream, 960)
                .expect("audio");
        }
        let buffer = voice_buffer(Cursor::new(bytes)).expect("opus");
        assert_eq!(buffer.channels().get(), 1);
        assert_eq!(buffer.sample_rate().get(), 48_000);
        assert!(buffer.into_iter().next().is_some());
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
