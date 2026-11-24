//! Desktop audio: PulseAudio monitor capture -> Opus (10 ms frames) -> clients.

use crate::audio_pipeline::Pipeline;
use crate::hub::Hub;
use crate::opus;
use std::ffi::{c_char, c_int, c_void, CString};
use std::sync::Arc;
use std::time::Duration;

#[repr(C)]
struct PaSampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}
#[repr(C)]
struct PaBufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}
#[link(name = "pulse-simple")]
extern "C" {
    fn pa_simple_new(
        server: *const c_char,
        name: *const c_char,
        dir: c_int,
        dev: *const c_char,
        stream: *const c_char,
        ss: *const PaSampleSpec,
        map: *const c_void,
        attr: *const PaBufferAttr,
        error: *mut c_int,
    ) -> *mut c_void;
    fn pa_simple_read(s: *mut c_void, data: *mut c_void, bytes: usize, error: *mut c_int) -> c_int;
    fn pa_simple_free(s: *mut c_void);
}

pub fn spawn(device: String, bitrate: i32, hub: Arc<Hub>) {
    std::thread::Builder::new()
        .name("audio".into())
        .spawn(move || {
            let mut pipeline = match Pipeline::new(bitrate) {
                Ok(pipeline) => pipeline,
                Err(e) => {
                    log::error!("opus init failed: {e}");
                    return;
                }
            };
            let name = c"voxl";
            let dev = CString::new(device).expect("device name has no NUL bytes");
            let ss = PaSampleSpec {
                format: 3,
                rate: opus::RATE,
                channels: opus::CHANNELS as u8,
            };
            let frag = (opus::FRAME_SAMPLES * 2) as u32;
            let attr = PaBufferAttr {
                maxlength: frag * 8,
                tlength: u32::MAX,
                prebuf: u32::MAX,
                minreq: u32::MAX,
                fragsize: frag,
            };
            let mut pcm = vec![0i16; opus::FRAME_SAMPLES];
            let mut err = 0;
            loop {
                if !hub.wants_audio() {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
                let s = unsafe {
                    pa_simple_new(
                        std::ptr::null(),
                        name.as_ptr(),
                        2,
                        dev.as_ptr(),
                        name.as_ptr(),
                        &ss,
                        std::ptr::null(),
                        &attr,
                        &mut err,
                    )
                };
                if s.is_null() {
                    log::warn!("pulse connect failed ({err}); retrying");
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
                log::info!("audio capture started");
                let mut checked = 0u32;
                loop {
                    let read = unsafe {
                        pa_simple_read(s, pcm.as_mut_ptr().cast(), pcm.len() * 2, &mut err)
                    };
                    if read < 0 {
                        log::warn!("pulse read failed ({err})");
                        break;
                    }
                    checked += 1;
                    if checked % 50 == 0 && !hub.wants_audio() {
                        break;
                    }
                    pipeline.push(&hub, pcm.iter().copied());
                }
                unsafe { pa_simple_free(s) };
                pipeline.restart();
                log::info!("audio capture stopped");
            }
        })
        .unwrap();
}
