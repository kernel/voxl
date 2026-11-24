//! The part of libopus voxl uses: one encoder for live and recorded desktop audio, and
//! the stream constants the MP4 writer needs.
#![cfg_attr(
    not(any(
        all(target_os = "linux", feature = "pulseaudio"),
        target_os = "macos",
        windows
    )),
    allow(dead_code)
)]

use std::ffi::{c_int, c_void};
use std::ptr::NonNull;

/// Sample rate of desktop audio.
pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// Samples per channel in one packet.
pub const FRAME: usize = 480;
/// Length of one packet.
pub const FRAME_MS: i64 = 10;
/// Interleaved samples in one packet.
pub const FRAME_SAMPLES: usize = FRAME * CHANNELS;
/// Lookahead of a restricted low-delay encoder at `RATE`: the decoder skips this many
/// samples from the start of the stream (`dOps` pre-skip).
pub const PRE_SKIP: u16 = 120;

const _: () = assert!(FRAME as u32 * 1000 == FRAME_MS as u32 * RATE);

const APPLICATION_RESTRICTED_LOWDELAY: c_int = 2051;
const SET_BITRATE: c_int = 4002;
#[cfg(test)]
const GET_LOOKAHEAD: c_int = 4027;
/// Largest packet libopus writes for these settings.
const MAX_PACKET: usize = 4000;

#[cfg_attr(not(windows), link(name = "opus"))]
extern "C" {
    fn opus_encoder_create(
        fs: i32,
        channels: c_int,
        application: c_int,
        error: *mut c_int,
    ) -> *mut c_void;
    fn opus_encoder_destroy(st: *mut c_void);
    fn opus_encoder_ctl(st: *mut c_void, request: c_int, ...) -> c_int;
    fn opus_encode(
        st: *mut c_void,
        pcm: *const i16,
        frame_size: c_int,
        data: *mut u8,
        max_data_bytes: i32,
    ) -> i32;
}

/// A libopus error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error(c_int);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "opus error {}", self.0)
    }
}

impl std::error::Error for Error {}

/// An Opus encoder for `RATE` stereo audio, tuned for low delay.
pub struct Encoder {
    state: NonNull<c_void>,
    packet: Box<[u8]>,
}

// The state is plain memory that libopus doesn't tie to a thread. It isn't Sync, since
// encoding takes `&mut self`.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(bitrate: i32) -> Result<Self, Error> {
        let mut error = 0;
        let state = unsafe {
            opus_encoder_create(
                RATE as i32,
                CHANNELS as c_int,
                APPLICATION_RESTRICTED_LOWDELAY,
                &mut error,
            )
        };
        let state = NonNull::new(state).ok_or(Error(error))?;
        let encoder = Self {
            state,
            packet: vec![0; MAX_PACKET].into_boxed_slice(),
        };
        ok(unsafe { opus_encoder_ctl(encoder.state.as_ptr(), SET_BITRATE, bitrate) })?;
        Ok(encoder)
    }

    /// Encodes one packet of interleaved stereo PCM (`FRAME_SAMPLES` samples). The result
    /// borrows the encoder until the next call. It is empty when libopus emits nothing.
    pub fn encode(&mut self, pcm: &[i16]) -> Result<&[u8], Error> {
        assert_eq!(pcm.len(), FRAME_SAMPLES);
        let n = unsafe {
            opus_encode(
                self.state.as_ptr(),
                pcm.as_ptr(),
                FRAME as c_int,
                self.packet.as_mut_ptr(),
                self.packet.len() as i32,
            )
        };
        let n = usize::try_from(n).map_err(|_| Error(n))?;
        Ok(&self.packet[..n])
    }

    #[cfg(test)]
    fn lookahead(&self) -> Result<u16, Error> {
        let mut lookahead: i32 = 0;
        ok(unsafe {
            opus_encoder_ctl(
                self.state.as_ptr(),
                GET_LOOKAHEAD,
                &mut lookahead as *mut i32,
            )
        })?;
        Ok(lookahead as u16)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { opus_encoder_destroy(self.state.as_ptr()) };
    }
}

fn ok(code: c_int) -> Result<(), Error> {
    if code == 0 {
        Ok(())
    } else {
        Err(Error(code))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_skip_is_the_encoder_lookahead() {
        let encoder = Encoder::new(128_000).unwrap();
        assert_eq!(encoder.lookahead().unwrap(), PRE_SKIP);
    }
}
