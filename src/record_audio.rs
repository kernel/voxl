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

    pub fn push(&mut self, hub: &Hub, pcm: &[i16]) {
        self.push_at(hub, pcm, Instant::now());
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

    fn push_at(&mut self, hub: &Hub, pcm: &[i16], now: Instant) {
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
        let captured = self.stamp(now);
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
    use tokio::sync::mpsc;

    #[test]
    fn silence_goes_to_recorder_not_live_viewer() {
        let hub = Hub::new();
        let (tx, mut recording) = mpsc::unbounded_channel();
        let recorder = hub.add_client(tx);
        recorder.recording_audio.store(true, Ordering::Relaxed);
        let (tx, mut live) = mpsc::unbounded_channel();
        let viewer = hub.add_client(tx);
        viewer.audio.store(true, Ordering::Relaxed);
        let mut encoder = Encoder::new(128_000);
        encoder.push(&hub, &[0; 960]);
        assert!(matches!(
            recording.try_recv(),
            Ok(Out::RecordingAudio(_, _))
        ));
        assert!(live.try_recv().is_err());
    }

    fn stamps(
        encoder: &mut Encoder,
        hub: &Hub,
        rx: &mut mpsc::UnboundedReceiver<Out>,
        at: &[Instant],
    ) -> Vec<Instant> {
        at.iter()
            .map(|&now| {
                encoder.push_at(hub, &[0; 960], now);
                match rx.try_recv() {
                    Ok(Out::RecordingAudio(_, captured)) => captured,
                    _ => panic!("no packet"),
                }
            })
            .collect()
    }

    fn recording() -> (std::sync::Arc<Hub>, mpsc::UnboundedReceiver<Out>, Encoder) {
        let hub = Hub::new();
        let (tx, rx) = mpsc::unbounded_channel();
        hub.add_client(tx)
            .recording_audio
            .store(true, Ordering::Relaxed);
        (hub, rx, Encoder::new(128_000))
    }

    #[test]
    fn bursty_and_late_audio_keeps_its_sample_timing() {
        let (hub, mut rx, mut encoder) = recording();
        let t = Instant::now() + Duration::from_secs(1);
        let ms = |n: u64| t + Duration::from_millis(n);
        // Two packets per 21 ms callback, then a 60 ms stall that loses nothing.
        let at = [
            ms(0),
            ms(0),
            ms(21),
            ms(21),
            ms(102),
            ms(102),
            ms(102),
            ms(102),
            ms(102),
            ms(102),
        ];
        let got = stamps(&mut encoder, &hub, &mut rx, &at);
        let start = got[0];
        for (i, s) in got.iter().enumerate() {
            assert_eq!(*s - start, PACKET * i as u32);
        }
    }

    #[test]
    fn dropped_samples_and_restarts_move_the_clock() {
        let (hub, mut rx, mut encoder) = recording();
        let t = Instant::now() + Duration::from_secs(1);
        let ms = |n: u64| t + Duration::from_millis(n);
        // 300 ms of audio never arrived: after LOST_PACKETS late packets the clock follows.
        let at: Vec<_> = (0..40)
            .map(|i| if i == 0 { ms(0) } else { ms(300 + 10 * i) })
            .collect();
        let got = stamps(&mut encoder, &hub, &mut rx, &at);
        let n = LOST_PACKETS as usize;
        assert_eq!(got[n - 1] - got[0], PACKET * (n as u32 - 1));
        assert_eq!(got[n], at[n] - PACKET);
        assert_eq!(got[n + 1] - got[n], PACKET);

        encoder.restart();
        let got = stamps(&mut encoder, &hub, &mut rx, &[ms(5000)]);
        assert_eq!(got[0], ms(5000) - PACKET);
    }
}
