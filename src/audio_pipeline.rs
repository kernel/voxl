//! Desktop audio after capture, the same on every platform: samples in, Opus out to live
//! viewers and to recordings. A capture backend converts its samples and calls `push`.

use crate::hub::{Hub, Out};
use crate::opus::{self, FRAME_SAMPLES};
use crate::record_audio;
use bytes::{BufMut, Bytes, BytesMut};
use std::sync::atomic::Ordering;
use std::time::Instant;

/// First byte of a live audio message (video frames start with 0x01).
const LIVE_AUDIO: u8 = 0x02;

pub struct Pipeline {
    live: opus::Encoder,
    recording: record_audio::Encoder,
    /// Samples of the next frame, until it is whole.
    pending: Vec<i16>,
}

impl Pipeline {
    pub fn new(bitrate: i32) -> Result<Self, opus::Error> {
        Ok(Self {
            live: opus::Encoder::new(bitrate)?,
            recording: record_audio::Encoder::new(bitrate),
            pending: Vec::with_capacity(FRAME_SAMPLES),
        })
    }

    /// Interleaved stereo samples at `opus::RATE`, in capture order, in any number.
    pub fn push(&mut self, hub: &Hub, samples: impl IntoIterator<Item = i16>) {
        let mut pending = std::mem::take(&mut self.pending);
        pending.extend(samples);
        let whole = pending.len() - pending.len() % FRAME_SAMPLES;
        for frame in pending[..whole].chunks_exact(FRAME_SAMPLES) {
            self.frame(hub, frame);
        }
        pending.drain(..whole);
        self.pending = pending;
    }

    /// Capture stopped: the partial frame is dropped and the recording clock restarts.
    pub fn restart(&mut self) {
        self.pending.clear();
        self.recording.restart();
    }

    fn frame(&mut self, hub: &Hub, pcm: &[i16]) {
        // Recordings get every frame, silence included.
        self.recording.push(hub, pcm);
        if pcm.iter().all(|&s| s == 0) || !hub.live_audio_listening() {
            return;
        }
        let Ok(packet) = self.live.encode(pcm) else {
            return;
        };
        if packet.is_empty() {
            return;
        }
        let mut msg = BytesMut::with_capacity(packet.len() + 1);
        msg.put_u8(LIVE_AUDIO);
        msg.put_slice(packet);
        hub.send_live_audio(msg.freeze());
    }
}

/// A sample in [-1, 1] as 16-bit PCM.
#[cfg(any(target_os = "macos", windows))]
pub fn pcm16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32767.0) as i16
}

/// Desktop audio is captured while a live viewer or a recording wants it.
impl Hub {
    pub fn wants_audio(&self) -> bool {
        self.live_audio_listening() || self.recording_audio_wanted()
    }

    fn live_audio_listening(&self) -> bool {
        self.clients
            .lock()
            .iter()
            .any(|c| c.audio.load(Ordering::Relaxed))
    }

    fn send_live_audio(&self, msg: Bytes) {
        for c in self
            .clients
            .lock()
            .iter()
            .filter(|c| c.audio.load(Ordering::Relaxed))
        {
            c.send(Out::Bin(msg.clone()));
        }
    }

    /// Whether a recording is subscribed. Subscriptions of finished recordings go here.
    pub(crate) fn recording_audio_wanted(&self) -> bool {
        let mut recorders = self.audio_recorders.lock();
        recorders.retain(|tx| !tx.is_closed());
        !recorders.is_empty()
    }

    /// Hands one recorded packet, with the time its samples were captured, to every
    /// recording subscribed to audio.
    pub(crate) fn send_recorded_audio(&self, packet: Bytes, captured: Instant) {
        self.audio_recorders
            .lock()
            .retain(|tx| tx.send((captured, packet.clone())).is_ok());
    }
}
