//! Desktop audio after capture, the same on every platform: samples in, Opus out to live
//! viewers and to recordings. A capture backend converts its samples and calls `push`.
//!
//! A recording encodes every frame, silence included, so its track has no holes where the
//! desktop is quiet. Packets are timed by the samples captured, not by when they reach the
//! encoder: capture hands over audio in bursts and late, and wall-clock stamps would read
//! that as gaps. The clock restarts with each capture, and when audio stays behind it long
//! enough that samples must have been dropped, it follows the wall clock again.

use crate::hub::{Hub, Out};
use crate::opus::{self, FRAME_SAMPLES};
use bytes::{BufMut, Bytes, BytesMut};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// First byte of a live audio message (video frames start with 0x01).
const LIVE_AUDIO: u8 = 0x02;
const PACKET: Duration = Duration::from_millis(opus::FRAME_MS as u64);
/// Audio this far behind its clock, for LOST_PACKETS packets in a row, lost samples.
const LOST: Duration = Duration::from_millis(100);
const LOST_PACKETS: u32 = 25;

pub struct Pipeline {
    live: opus::Encoder,
    bitrate: i32,
    /// Created when a recording first wants audio, dropped when none does.
    recording: Option<opus::Encoder>,
    clock: Clock,
    /// Samples of the next frame, until it is whole.
    pending: Vec<i16>,
}

impl Pipeline {
    pub fn new(bitrate: i32) -> Result<Self, opus::Error> {
        Ok(Self {
            live: opus::Encoder::new(bitrate)?,
            bitrate,
            recording: None,
            clock: Clock::default(),
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
        self.clock = Clock::default();
    }

    fn frame(&mut self, hub: &Hub, pcm: &[i16]) {
        self.record(hub, pcm);
        if pcm.iter().all(|&s| s == 0) || !hub.live_audio_listening() {
            return;
        }
        let Ok(packet) = self.live.encode(pcm) else {
            return;
        };
        if !packet.is_empty() {
            let mut msg = BytesMut::with_capacity(packet.len() + 1);
            msg.put_u8(LIVE_AUDIO);
            msg.put_slice(packet);
            hub.send_live_audio(msg.freeze());
        }
    }

    fn record(&mut self, hub: &Hub, pcm: &[i16]) {
        if !hub.recording_audio_wanted() {
            self.recording = None;
            self.clock = Clock::default();
            return;
        }
        // Stamped before encoding, so a packet that fails to encode leaves a gap.
        let captured = self.clock.stamp(Instant::now());
        let encoder = match &mut self.recording {
            Some(encoder) => encoder,
            slot => match opus::Encoder::new(self.bitrate) {
                Ok(encoder) => slot.insert(encoder),
                Err(e) => {
                    log::error!("recording Opus encoder: {e}");
                    return;
                }
            },
        };
        match encoder.encode(pcm) {
            Ok([]) => {}
            Ok(packet) => hub.send_recorded_audio(Bytes::copy_from_slice(packet), captured),
            Err(e) => log::warn!("recording Opus encode: {e}"),
        }
    }
}

/// Stamps each packet with the time its samples were captured.
#[derive(Default)]
struct Clock {
    /// Capture time of the first packet since the restart, and how many packets followed.
    origin: Option<(Instant, u32)>,
    /// Packets in a row that arrived more than LOST behind the clock.
    behind: u32,
}

impl Clock {
    /// When the packet that reached the encoder at `now` was captured.
    fn stamp(&mut self, now: Instant) -> Instant {
        // The packet's samples end now.
        let arrived = now.checked_sub(PACKET).unwrap_or(now);
        let (start, n) = *self.origin.get_or_insert((arrived, 0));
        let expected = start + PACKET * n;
        if arrived.saturating_duration_since(expected) > LOST {
            self.behind += 1;
        } else {
            self.behind = 0;
        }
        if self.behind >= LOST_PACKETS {
            // Samples were dropped: follow the wall clock from here.
            self.origin = Some((arrived, 1));
            self.behind = 0;
            return arrived;
        }
        self.origin = Some((start, n + 1));
        expected
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
    fn recording_audio_wanted(&self) -> bool {
        let mut recorders = self.audio_recorders.lock();
        recorders.retain(|tx| !tx.is_closed());
        !recorders.is_empty()
    }

    fn send_recorded_audio(&self, packet: Bytes, captured: Instant) {
        self.audio_recorders
            .lock()
            .retain(|tx| tx.send((captured, packet.clone())).is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Capture times, in ms, of packets that reach the clock at these times.
    fn stamps(clock: &mut Clock, at: &[i64]) -> Vec<i64> {
        let t = Instant::now();
        let at_ms = |n: i64| t + Duration::from_millis((1000 + n) as u64);
        let ms = |i: Instant| (i - t).as_millis() as i64 - 1000;
        at.iter().map(|&n| ms(clock.stamp(at_ms(n)))).collect()
    }

    #[test]
    fn bursty_and_late_audio_keeps_its_sample_timing() {
        // Two packets per 21 ms callback, then a 60 ms stall that loses nothing.
        let at = [0, 0, 21, 21, 102, 102, 102, 102, 102, 102];
        let want: Vec<i64> = (0..10).map(|i| i * 10 - 10).collect();
        assert_eq!(stamps(&mut Clock::default(), &at), want);
    }

    #[test]
    fn dropped_samples_and_restarts_move_the_clock() {
        let mut clock = Clock::default();
        // 300 ms of audio never arrived: after LOST_PACKETS late packets the clock follows.
        let at: Vec<i64> = (0..40)
            .map(|i| if i == 0 { 0 } else { 300 + 10 * i })
            .collect();
        let got = stamps(&mut clock, &at);
        let n = LOST_PACKETS as usize;
        assert_eq!(got[n - 1], got[0] + 10 * (n as i64 - 1));
        let late: Vec<i64> = at[n..].iter().map(|t| t - 10).collect();
        assert_eq!(got[n..], late);

        clock = Clock::default();
        assert_eq!(stamps(&mut clock, &[5000]), [4990]);
    }
}
