//! A separate continuous Opus encoder for recordings. Live audio keeps its own encoder
//! and can continue skipping silence without desynchronizing its decoder.
//!
//! Packets are timed by the samples captured, not by when they reach the encoder:
//! capture hands over audio in bursts and late, and wall-clock stamps would read that
//! as gaps. The clock restarts with each capture, and when audio stays behind it for
//! long enough that samples must have been dropped.
use crate::hub::{Hub, Out};
use bytes::Bytes;
use std::ffi::{c_int, c_void};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// One Opus packet: 480 samples at 48 kHz.
const PACKET: Duration = Duration::from_millis(10);
/// Audio that stays this far behind its sample clock for LOST_PACKETS packets in a row
/// means capture dropped samples.
const LOST: Duration = Duration::from_millis(100);
const LOST_PACKETS: u32 = 25;

#[cfg_attr(not(windows), link(name = "opus"))]
extern "C" {
    fn opus_encoder_create(
        rate: i32,
        channels: c_int,
        application: c_int,
        error: *mut c_int,
    ) -> *mut c_void;
    fn opus_encoder_destroy(encoder: *mut c_void);
    fn opus_encoder_ctl(encoder: *mut c_void, request: c_int, ...) -> c_int;
    fn opus_encode(
        encoder: *mut c_void,
        pcm: *const i16,
        samples: c_int,
        data: *mut u8,
        max: i32,
    ) -> c_int;
}

pub struct Encoder {
    encoder: *mut c_void,
    bitrate: i32,
    buffer: [u8; 4000],
    /// When the first packet since the clock started was captured, and packets since.
    clock: Option<(Instant, u32)>,
    /// Packets in a row that arrived more than LOST behind the clock.
    behind: u32,
}
// The encoder is used by only one capture thread or under the macOS callback mutex.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(bitrate: i32) -> Self {
        Self {
            encoder: std::ptr::null_mut(),
            bitrate,
            buffer: [0; 4000],
            clock: None,
            behind: 0,
        }
    }

    /// Capture stopped: the next packet starts a new clock.
    pub fn restart(&mut self) {
        self.clock = None;
        self.behind = 0;
    }

    /// When the packet that reached the encoder at `now` was captured.
    fn stamp(&mut self, now: Instant) -> Instant {
        let arrived = now.checked_sub(PACKET).unwrap_or(now);
        let (start, n) = *self.clock.get_or_insert((arrived, 0));
        let expected = start + PACKET * n;
        self.behind = if arrived.saturating_duration_since(expected) > LOST {
            self.behind + 1
        } else {
            0
        };
        if self.behind >= LOST_PACKETS {
            self.clock = Some((arrived, 1));
            self.behind = 0;
            return arrived;
        }
        self.clock = Some((start, n + 1));
        expected
    }

    pub fn push(&mut self, hub: &Hub, pcm: &[i16]) {
        let clients: Vec<_> = hub
            .clients
            .lock()
            .iter()
            .filter(|c| c.recording_audio.load(Ordering::Relaxed))
            .cloned()
            .collect();
        if clients.is_empty() {
            self.reset();
            return;
        }
        if self.encoder.is_null() {
            let mut error = 0;
            self.encoder = unsafe { opus_encoder_create(48000, 2, 2051, &mut error) };
            if self.encoder.is_null() {
                log::error!("recording Opus encoder init failed: {error}");
                return;
            }
            unsafe { opus_encoder_ctl(self.encoder, 4002, self.bitrate as c_int) };
        }
        // Timed before encoding, so a packet that fails to encode leaves a gap.
        let captured = self.stamp(Instant::now());
        let n = unsafe {
            opus_encode(
                self.encoder,
                pcm.as_ptr(),
                480,
                self.buffer.as_mut_ptr(),
                self.buffer.len() as i32,
            )
        };
        if n <= 0 {
            return;
        }
        let data = Bytes::copy_from_slice(&self.buffer[..n as usize]);
        for client in clients {
            client.send(Out::RecordingAudio(data.clone(), captured));
        }
    }

    fn reset(&mut self) {
        self.restart();
        if !self.encoder.is_null() {
            unsafe { opus_encoder_destroy(self.encoder) };
            self.encoder = std::ptr::null_mut();
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        self.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Capture times of packets that reach the encoder at these times, in ms.
    fn stamps(encoder: &mut Encoder, at: &[i64]) -> Vec<i64> {
        let t = Instant::now();
        let at_ms = |n: i64| t + Duration::from_millis((1000 + n) as u64);
        let ms = |i: Instant| (i - t).as_millis() as i64 - 1000;
        at.iter().map(|&n| ms(encoder.stamp(at_ms(n)))).collect()
    }

    #[test]
    fn bursty_and_late_audio_keeps_its_sample_timing() {
        // Two packets per 21 ms callback, then a 60 ms stall that loses nothing.
        let at = [0, 0, 21, 21, 102, 102, 102, 102, 102, 102];
        let want: Vec<i64> = (0..10).map(|i| i * 10 - 10).collect();
        assert_eq!(stamps(&mut Encoder::new(128_000), &at), want);
    }

    #[test]
    fn dropped_samples_and_restarts_move_the_clock() {
        let mut encoder = Encoder::new(128_000);
        // 300 ms of audio never arrived: after LOST_PACKETS late packets the clock follows.
        let at: Vec<i64> = (0..40)
            .map(|i| if i == 0 { 0 } else { 300 + 10 * i })
            .collect();
        let got = stamps(&mut encoder, &at);
        let n = LOST_PACKETS as usize;
        assert_eq!(got[n - 1], got[0] + 10 * (n as i64 - 1));
        let late: Vec<i64> = at[n..].iter().map(|t| t - 10).collect();
        assert_eq!(got[n..], late);

        encoder.restart();
        assert_eq!(stamps(&mut encoder, &[5000]), [4990]);
    }
}
