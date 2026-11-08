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
use thinwire_protocol::voice_files::VOICE_MAX_BYTES;

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
    match decode_webm_opus(file, MAX_DECODED_SAMPLES, &mut pace) {
        VoiceDecode::Cut => return Decoded::Cut,
        VoiceDecode::Opus(buffer) => return Decoded::Buffer(buffer),
        VoiceDecode::Rejected => {
            tracing::warn!("voice note is too large to play");
            return Decoded::Rejected;
        }
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

const EBML_ID: u64 = 0x1A45_DFA3;
const SEGMENT_ID: u64 = 0x1853_8067;
const TRACKS_ID: u64 = 0x1654_AE6B;
const TRACK_ENTRY_ID: u64 = 0xAE;
const TRACK_NUMBER_ID: u64 = 0xD7;
const CODEC_ID: u64 = 0x86;
const CODEC_PRIVATE_ID: u64 = 0x63A2;
const CLUSTER_ID: u64 = 0x1F43_B675;
const TIMECODE_ID: u64 = 0xE7;
const SIMPLE_BLOCK_ID: u64 = 0xA3;
const BLOCK_GROUP_ID: u64 = 0xA0;
const BLOCK_ID: u64 = 0xA1;

fn decode_webm_opus(
    mut reader: impl Read + Seek,
    max_samples: usize,
    mut pace: impl FnMut() -> Pace,
) -> VoiceDecode {
    let mut magic = [0u8; 4];
    if reader.read(&mut magic).ok() != Some(4) || magic != [0x1A, 0x45, 0xDF, 0xA3] {
        return VoiceDecode::TryVorbis;
    }
    if reader.seek(std::io::SeekFrom::Start(0)).is_err() {
        return VoiceDecode::Rejected;
    }
    let mut data = Vec::new();
    if reader
        .take((VOICE_MAX_BYTES as u64).saturating_add(1))
        .read_to_end(&mut data)
        .is_err()
        || data.len() > VOICE_MAX_BYTES
    {
        return VoiceDecode::Rejected;
    }
    if matches!(pace(), Pace::Halt) {
        return VoiceDecode::Cut;
    }
    let (head, packets) = match webm_opus(&data) {
        Ok(parsed) => parsed,
        Err(()) => return VoiceDecode::Rejected,
    };
    pcm_from_opus(&head, &packets, max_samples, &mut pace)
}

fn webm_opus(data: &[u8]) -> Result<(OpusHead, Vec<Vec<u8>>), ()> {
    let mut at = 0;
    let (id, after) = read_id(data, at)?;
    if id != EBML_ID {
        return Err(());
    }
    at = element_end(data, after, data.len())?;
    let mut parsed = None;
    while at < data.len() {
        let (id, after) = read_id(data, at)?;
        let (size, body_at) = read_size(data, after)?;
        if id == SEGMENT_ID {
            let end = match size {
                Some(size) => body_end(body_at, size, data.len())?,
                None => data.len(),
            };
            parsed = Some(walk_segment(data, body_at, end)?);
            break;
        }
        at = body_end(body_at, size.ok_or(())?, data.len())?;
    }
    let note = parsed.ok_or(())?;
    if note.entries != 1 || note.codec.as_deref() != Some("A_OPUS") || note.packets.is_empty() {
        return Err(());
    }
    let head = opus_head(&note.private)?;
    Ok((head, note.packets))
}

struct WebmNote {
    entries: u32,
    codec: Option<String>,
    track: u64,
    private: Vec<u8>,
    packets: Vec<Vec<u8>>,
}

fn walk_segment(data: &[u8], start: usize, end: usize) -> Result<WebmNote, ()> {
    let mut note = WebmNote {
        entries: 0,
        codec: None,
        track: 0,
        private: Vec::new(),
        packets: Vec::new(),
    };
    let mut at = start;
    while at < end {
        let (id, after) = read_id(data, at)?;
        let (size, body_at) = read_size(data, after)?;
        if id == CLUSTER_ID && size.is_none() {
            // Chrome writes an unknown cluster size. The next segment element
            // is the end, not another block.
            at = walk_cluster(data, body_at, end, true, &mut note)?;
            continue;
        }
        let body_end = body_end(body_at, size.ok_or(())?, end)?;
        match id {
            TRACKS_ID => walk_tracks(&data[body_at..body_end], &mut note)?,
            CLUSTER_ID => {
                walk_cluster(data, body_at, body_end, false, &mut note)?;
            }
            _ => {}
        }
        at = body_end;
    }
    Ok(note)
}

fn walk_tracks(body: &[u8], note: &mut WebmNote) -> Result<(), ()> {
    let mut at = 0;
    while at < body.len() {
        let (id, after) = read_id(body, at)?;
        let body_end = element_end(body, after, body.len())?;
        if id == TRACK_ENTRY_ID {
            if note.entries >= 1 {
                return Err(());
            }
            walk_track_entry(&body[after_size(body, after)?..body_end], note)?;
            note.entries += 1;
        }
        at = body_end;
    }
    Ok(())
}

fn walk_track_entry(body: &[u8], note: &mut WebmNote) -> Result<(), ()> {
    let mut at = 0;
    while at < body.len() {
        let (id, after) = read_id(body, at)?;
        let body_at = after_size(body, after)?;
        let body_end = element_end(body, after, body.len())?;
        let child = &body[body_at..body_end];
        match id {
            TRACK_NUMBER_ID => note.track = read_uint(child)?,
            CODEC_ID => {
                note.codec = Some(std::str::from_utf8(child).map_err(|_| ())?.to_string());
            }
            CODEC_PRIVATE_ID => note.private = child.to_vec(),
            _ => {}
        }
        at = body_end;
    }
    if note.codec.as_deref() != Some("A_OPUS") || note.track == 0 {
        return Err(());
    }
    Ok(())
}

fn walk_cluster(
    data: &[u8],
    start: usize,
    end: usize,
    stop_at_segment: bool,
    note: &mut WebmNote,
) -> Result<usize, ()> {
    let mut at = start;
    while at < end {
        let (id, after) = read_id(data, at)?;
        if stop_at_segment && segment_level(id) {
            return Ok(at);
        }
        let (size, body_at) = read_size(data, after)?;
        let body_end = body_end(body_at, size.ok_or(())?, end)?;
        match id {
            SIMPLE_BLOCK_ID => read_block(&data[body_at..body_end], note)?,
            BLOCK_GROUP_ID => read_block_group(&data[body_at..body_end], note)?,
            TIMECODE_ID => {}
            _ => {}
        }
        at = body_end;
    }
    Ok(end)
}

fn read_block_group(body: &[u8], note: &mut WebmNote) -> Result<(), ()> {
    let mut at = 0;
    while at < body.len() {
        let (id, after) = read_id(body, at)?;
        let body_at = after_size(body, after)?;
        let body_end = element_end(body, after, body.len())?;
        if id == BLOCK_ID {
            read_block(&body[body_at..body_end], note)?;
        }
        at = body_end;
    }
    Ok(())
}

fn read_block(body: &[u8], note: &mut WebmNote) -> Result<(), ()> {
    let (track, after) = read_size(body, 0)?;
    if track != Some(note.track) || after + 3 > body.len() {
        return Err(());
    }
    let flags = body[after + 2];
    let payload = &body[after + 3..];
    match (flags & 0x06) >> 1 {
        0 => push_packet(payload, &mut note.packets),
        1 => {
            for frame in xiph_frames(payload)? {
                push_packet(frame, &mut note.packets)?;
            }
            Ok(())
        }
        _ => Err(()),
    }
}

fn push_packet(frame: &[u8], packets: &mut Vec<Vec<u8>>) -> Result<(), ()> {
    if frame.is_empty() {
        return Err(());
    }
    packets.push(frame.to_vec());
    Ok(())
}

fn xiph_frames(payload: &[u8]) -> Result<Vec<&[u8]>, ()> {
    let count = *payload.first().ok_or(())? as usize + 1;
    let mut at = 1;
    let mut sizes = Vec::with_capacity(count.saturating_sub(1));
    for _ in 1..count {
        let mut size = 0usize;
        loop {
            let byte = *payload.get(at).ok_or(())? as usize;
            at += 1;
            size = size.checked_add(byte).ok_or(())?;
            if byte < 255 {
                break;
            }
        }
        sizes.push(size);
    }
    let mut frames = Vec::with_capacity(count);
    for size in sizes {
        let end = at.checked_add(size).ok_or(())?;
        if end > payload.len() {
            return Err(());
        }
        frames.push(&payload[at..end]);
        at = end;
    }
    frames.push(&payload[at..]);
    Ok(frames)
}

fn pcm_from_opus(
    head: &OpusHead,
    packets: &[Vec<u8>],
    max_samples: usize,
    mut pace: impl FnMut() -> Pace,
) -> VoiceDecode {
    let channel_count = match head.channels {
        1 => opus::Channels::Mono,
        2 => opus::Channels::Stereo,
        _ => return VoiceDecode::Rejected,
    };
    let mut decoder = match opus::Decoder::new(OPUS_RATE, channel_count) {
        Ok(decoder) => decoder,
        Err(_) => return VoiceDecode::Rejected,
    };
    let mut pcm = Vec::new();
    let mut frame = vec![0.0f32; OPUS_MAX_FRAME * head.channels as usize];
    let mut skip = head.pre_skip as usize;
    let mut produced = 0usize;
    for packet in packets {
        if matches!(pace(), Pace::Halt) {
            return VoiceDecode::Cut;
        }
        let decoded = match decoder.decode_float(packet, &mut frame, false) {
            Ok(decoded) => decoded,
            Err(_) => return VoiceDecode::Rejected,
        };
        let samples = decoded.saturating_mul(head.channels as usize);
        if samples > frame.len() {
            return VoiceDecode::Rejected;
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
        let drop_samples = drop_frames * head.channels as usize;
        skip -= drop_frames;
        pcm.extend_from_slice(&taken[drop_samples..]);
    }
    if pcm.is_empty() {
        return VoiceDecode::Rejected;
    }
    let Some(channels) = NonZero::new(u16::from(head.channels)) else {
        return VoiceDecode::Rejected;
    };
    let Some(rate) = NonZero::new(OPUS_RATE) else {
        return VoiceDecode::Rejected;
    };
    VoiceDecode::Opus(SamplesBuffer::new(channels, rate, pcm))
}

fn segment_level(id: u64) -> bool {
    matches!(
        id,
        0x114D_9B74
            | 0x1549_A966
            | TRACKS_ID
            | 0x1C53_BB6B
            | CLUSTER_ID
            | 0x1043_A770
            | 0x1254_C367
            | 0x1941_A469
    )
}

fn read_id(data: &[u8], at: usize) -> Result<(u64, usize), ()> {
    let (raw, width) = read_vint(data, at)?;
    Ok((raw, at + width))
}

fn read_size(data: &[u8], at: usize) -> Result<(Option<u64>, usize), ()> {
    let (raw, width) = read_vint(data, at)?;
    let bits = 7 * width;
    let mask = (1u64 << bits) - 1;
    let value = raw & mask;
    let size = (value != mask).then_some(value);
    Ok((size, at + width))
}

fn read_vint(data: &[u8], at: usize) -> Result<(u64, usize), ()> {
    let first = *data.get(at).ok_or(())?;
    if first == 0 {
        return Err(());
    }
    let width = first.leading_zeros() as usize + 1;
    if width > 8 || at + width > data.len() {
        return Err(());
    }
    let mut raw = 0u64;
    for byte in &data[at..at + width] {
        raw = (raw << 8) | u64::from(*byte);
    }
    Ok((raw, width))
}

fn element_end(data: &[u8], size_at: usize, limit: usize) -> Result<usize, ()> {
    let (size, body_at) = read_size(data, size_at)?;
    body_end(body_at, size.ok_or(())?, limit)
}

fn after_size(data: &[u8], size_at: usize) -> Result<usize, ()> {
    Ok(read_size(data, size_at)?.1)
}

fn body_end(body_at: usize, size: u64, limit: usize) -> Result<usize, ()> {
    let size = usize::try_from(size).map_err(|_| ())?;
    let end = body_at.checked_add(size).ok_or(())?;
    if end > limit { Err(()) } else { Ok(end) }
}

fn read_uint(bytes: &[u8]) -> Result<u64, ()> {
    if bytes.is_empty() || bytes.len() > 8 {
        return Err(());
    }
    let mut value = 0u64;
    for byte in bytes {
        value = (value << 8) | u64::from(*byte);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use rodio::source::Source;

    use thinwire_protocol::AudioPath;

    use super::super::Backend;
    use super::{
        Decoded, MAX_DECODED_SAMPLES, Pace, VoiceDecode, decode_opus, decode_webm_opus, read_voice,
        voice_buffer,
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
        let missing =
            AudioPath::new(dir.join("missing.ogg").to_string_lossy().into_owned()).expect("path");
        let mut late = Vec::new();

        shutdown_during_decode(&long, decode, &mut late, "shutdown", |_| {});
        shutdown_during_decode(&long, decode, &mut late, "stop", |engine| {
            engine.send(super::Cmd::Stop);
        });
        shutdown_during_decode(&long, decode, &mut late, "play", |engine| {
            engine.send(super::Cmd::Play {
                path: missing,
                generation: 2,
            });
        });

        let _ = std::fs::remove_dir_all(&dir);
        assert!(late.is_empty(), "{}", late.join("\n"));
    }

    fn shutdown_during_decode(
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

    #[test]
    fn an_unlaced_webm_opus_note_decodes_to_samples() {
        let packet = silence_frame();
        let bytes = webm_opus(&packet, Block::Plain);
        let VoiceDecode::Opus(buffer) =
            decode_webm_opus(Cursor::new(bytes.clone()), usize::MAX, || Pace::Go)
        else {
            panic!("webm opus");
        };
        assert_eq!(buffer.channels().get(), 1);
        assert_eq!(buffer.sample_rate().get(), 48_000);
        let count = buffer.into_iter().count();
        assert!(count > 0, "unlaced note has samples");

        let xiph = webm_opus(&packet, Block::Xiph);
        let VoiceDecode::Opus(buffer) =
            decode_webm_opus(Cursor::new(xiph), usize::MAX, || Pace::Go)
        else {
            panic!("xiph opus");
        };
        assert!(buffer.into_iter().count() > count);

        let unknown = webm_opus(&packet, Block::UnknownSegment);
        assert!(matches!(
            decode_webm_opus(Cursor::new(unknown), usize::MAX, || Pace::Go),
            VoiceDecode::Opus(_)
        ));

        let dir = std::env::temp_dir()
            .join("thinwire-webm-tests")
            .join(std::process::id().to_string());
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("slack.webm");
        std::fs::write(&path, &bytes).expect("write");
        let audio = AudioPath::new(path.to_string_lossy().into_owned()).expect("path");
        let Decoded::Buffer(played) = read_voice(&audio, || Pace::Go) else {
            panic!("player reads the webm");
        };
        assert!(played.into_iter().count() > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn webm_that_is_not_opus_is_refused_and_other_bytes_are_not() {
        let packet = silence_frame();
        assert!(matches!(
            decode_webm_opus(
                Cursor::new(webm_opus(&packet, Block::Vp8)),
                usize::MAX,
                || { Pace::Go }
            ),
            VoiceDecode::Rejected
        ));
        assert!(matches!(
            decode_webm_opus(
                Cursor::new(webm_opus(&packet, Block::SecondTrack)),
                usize::MAX,
                || Pace::Go
            ),
            VoiceDecode::Rejected
        ));
        assert!(matches!(
            decode_webm_opus(
                Cursor::new(webm_opus(&packet, Block::FixedLace)),
                usize::MAX,
                || Pace::Go
            ),
            VoiceDecode::Rejected
        ));
        assert!(matches!(
            decode_webm_opus(Cursor::new(b"not ebml".to_vec()), usize::MAX, || Pace::Go),
            VoiceDecode::TryVorbis
        ));
        assert!(matches!(
            decode_webm_opus(Cursor::new(ogg_opus(1)), usize::MAX, || Pace::Go),
            VoiceDecode::TryVorbis
        ));
    }

    fn silence_frame() -> Vec<u8> {
        let mut encoder =
            opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Audio)
                .expect("encoder");
        encoder.encode_vec(&[0i16; 960], 4_000).expect("frame")
    }

    enum Block {
        Plain,
        Xiph,
        UnknownSegment,
        Vp8,
        SecondTrack,
        FixedLace,
    }

    fn webm_opus(packet: &[u8], kind: Block) -> Vec<u8> {
        let codec = if matches!(kind, Block::Vp8) {
            "V_VP8"
        } else {
            "A_OPUS"
        };
        let mut entry = Vec::new();
        entry.extend(ebml_uint(&[0xD7], 1));
        entry.extend(ebml_bytes(&[0x86], codec.as_bytes()));
        if codec == "A_OPUS" {
            entry.extend(ebml_bytes(&[0x63, 0xA2], &opus_head_bytes()));
        }
        let mut tracks_body = ebml_bytes(&[0xAE], &entry);
        if matches!(kind, Block::SecondTrack) {
            tracks_body.extend(ebml_bytes(&[0xAE], &entry));
        }
        let tracks = ebml_bytes(&[0x16, 0x54, 0xAE, 0x6B], &tracks_body);
        let block = match kind {
            Block::Xiph => {
                let again = silence_frame();
                let mut body = vec![0x81, 0x00, 0x00, 0x82, 1];
                let mut size = packet.len();
                while size >= 255 {
                    body.push(255);
                    size -= 255;
                }
                body.push(size as u8);
                body.extend_from_slice(packet);
                body.extend_from_slice(&again);
                ebml_bytes(&[0xA3], &body)
            }
            Block::FixedLace => {
                let mut body = vec![0x81, 0x00, 0x00, 0x84, 0x00];
                body.extend_from_slice(packet);
                ebml_bytes(&[0xA3], &body)
            }
            Block::Vp8 => Vec::new(),
            _ => {
                let mut body = vec![0x81, 0x00, 0x00, 0x80];
                body.extend_from_slice(packet);
                ebml_bytes(&[0xA3], &body)
            }
        };
        let mut segment = tracks;
        if !block.is_empty() {
            segment.extend(ebml_bytes(&[0x1F, 0x43, 0xB6, 0x75], &block));
        }
        let mut file = ebml_bytes(&[0x1A, 0x45, 0xDF, 0xA3], &[]);
        if matches!(kind, Block::UnknownSegment) {
            file.extend_from_slice(&[0x18, 0x53, 0x80, 0x67, 0xFF]);
            file.extend(segment);
        } else {
            file.extend(ebml_bytes(&[0x18, 0x53, 0x80, 0x67], &segment));
        }
        file
    }

    fn ebml_uint(id: &[u8], value: u64) -> Vec<u8> {
        ebml_bytes(id, &[value as u8])
    }

    fn ebml_bytes(id: &[u8], body: &[u8]) -> Vec<u8> {
        let mut out = id.to_vec();
        out.extend(ebml_size(body.len()));
        out.extend_from_slice(body);
        out
    }

    fn ebml_size(len: usize) -> Vec<u8> {
        if len < 0x7F {
            vec![0x80 | len as u8]
        } else {
            let marked = (len as u16) | 0x4000;
            marked.to_be_bytes().to_vec()
        }
    }

    fn opus_tags() -> Vec<u8> {
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&1u32.to_le_bytes());
        tags.push(b't');
        tags.extend_from_slice(&0u32.to_le_bytes());
        tags
    }
}
