//! Desktop audio for recordings: a continuous Opus track of its own. Live viewers hear a
//! separate encoder that skips silence; a recording encodes every frame, so its track has
//! no holes where the desktop is quiet.
//!
//! Packets are timed by the samples captured, not by when they reach the encoder: capture
//! hands over audio in bursts and late, and wall-clock stamps would read that as gaps.
//! The clock restarts with each capture, and when audio stays behind it for long enough
//! that samples must have been dropped, it follows the wall clock again.

use crate::hub::Hub;
use crate::opus;
use bytes::Bytes;
use std::time::{Duration, Instant};

/// Length of one packet.
const PACKET: Duration = Duration::from_millis(opus::FRAME_MS as u64);
/// Audio that stays this far behind its clock for LOST_PACKETS packets in a row means
/// capture dropped samples.
const LOST: Duration = Duration::from_millis(100);
const LOST_PACKETS: u32 = 25;

pub struct Encoder {
    bitrate: i32,
    /// Created when a recording first wants audio, dropped when none does.
    encoder: Option<opus::Encoder>,
    clock: Clock,
}

impl Encoder {
    pub fn new(bitrate: i32) -> Self {
        Self {
            bitrate,
            encoder: None,
            clock: Clock::default(),
        }
    }

    /// Capture stopped: the next packet starts a new clock.
    pub fn restart(&mut self) {
        self.clock = Clock::default();
    }

    /// Encodes one packet of `pcm` for the recordings that want audio.
    pub fn push(&mut self, hub: &Hub, pcm: &[i16]) {
        if !hub.recording_audio_wanted() {
            self.encoder = None;
            self.restart();
            return;
        }
        // Stamped before encoding, so a packet that fails to encode leaves a gap.
        let captured = self.clock.stamp(Instant::now());
        let encoder = match self.encoder() {
            Ok(encoder) => encoder,
            Err(e) => {
                log::error!("recording Opus encoder: {e}");
                return;
            }
        };
        match encoder.encode(pcm) {
            Ok([]) => {}
            Ok(packet) => hub.send_recorded_audio(Bytes::copy_from_slice(packet), captured),
            Err(e) => log::warn!("recording Opus encode: {e}"),
        }
    }

    fn encoder(&mut self) -> Result<&mut opus::Encoder, opus::Error> {
        let bitrate = self.bitrate;
        match &mut self.encoder {
            Some(encoder) => Ok(encoder),
            slot => Ok(slot.insert(opus::Encoder::new(bitrate)?)),
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
