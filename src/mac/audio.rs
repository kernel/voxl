//! Desktop audio on macOS: system audio from ScreenCaptureKit (no loopback driver
//! needed) -> Opus (10 ms frames) -> clients, while any client wants audio.

use super::ffi::*;
use super::stream::{self, OutputType};
use crate::audio_pipeline::{pcm16, Pipeline};
use crate::hub::Hub;
use crate::opus;
use parking_lot::Mutex;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// Planar or interleaved float samples of one audio sample buffer as (left, right).
unsafe fn samples(sb: CMSampleBufferRef) -> Option<(Vec<f32>, Vec<f32>)> {
    let asbd =
        CMAudioFormatDescriptionGetStreamBasicDescription(CMSampleBufferGetFormatDescription(sb))
            .as_ref()?;
    if asbd.mFormatFlags & kAudioFormatFlagIsFloat == 0 || asbd.mBitsPerChannel != 32 {
        return None;
    }
    // Ask how big the buffer list must be, then fill one that size.
    let mut need = 0usize;
    CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
        sb,
        &mut need,
        std::ptr::null_mut(),
        0,
        std::ptr::null(),
        std::ptr::null(),
        kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
        std::ptr::null_mut(),
    );
    if need == 0 || need > std::mem::size_of::<AudioBufferList8>() {
        return None;
    }
    let mut list: AudioBufferList8 = std::mem::zeroed();
    let mut block: CFTypeRef = std::ptr::null();
    let st = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
        sb,
        std::ptr::null_mut(),
        &mut list,
        need,
        std::ptr::null(),
        std::ptr::null(),
        kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
        &mut block,
    );
    if st != 0 {
        return None;
    }
    let buf = |i: usize| {
        let b = list.mBuffers[i];
        std::slice::from_raw_parts(b.mData as *const f32, b.mDataByteSize as usize / 4)
    };
    let r = if list.mNumberBuffers >= 2 {
        Some((buf(0).to_vec(), buf(1).to_vec()))
    } else if list.mNumberBuffers == 1 {
        let ch = asbd.mChannelsPerFrame.max(1) as usize;
        let s = buf(0);
        let l: Vec<f32> = s.iter().step_by(ch).copied().collect();
        let r: Vec<f32> = if ch > 1 {
            s.iter().skip(1).step_by(ch).copied().collect()
        } else {
            l.clone()
        };
        Some((l, r))
    } else {
        None
    };
    if !block.is_null() {
        CFRelease(block);
    }
    r
}

pub fn spawn(_device: String, bitrate: i32, hub: Arc<Hub>) {
    std::thread::Builder::new()
        .name("audio".into())
        .spawn(move || {
            let pipeline = match Pipeline::new(bitrate) {
                Ok(pipeline) => Arc::new(Mutex::new(pipeline)),
                Err(e) => {
                    log::error!("opus init failed: {e}");
                    return;
                }
            };
            let want = || hub.wants_audio();
            loop {
                if !want() {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
                let (p, h) = (pipeline.clone(), hub.clone());
                let on_sample = move |sb: CMSampleBufferRef, ty: OutputType| {
                    if ty != OutputType::Audio {
                        return;
                    }
                    if let Some((l, r)) = unsafe { samples(sb) } {
                        let interleaved =
                            l.iter().zip(&r).flat_map(|(&a, &b)| [pcm16(a), pcm16(b)]);
                        p.lock().push(&h, interleaved);
                    }
                };
                // Audio rides on a display stream; keep its video as small and rare as possible.
                let s = stream::start(
                    super::display(),
                    "audio",
                    &[OutputType::Audio],
                    |cfg| unsafe {
                        cfg.setCapturesAudio(true);
                        cfg.setSampleRate(opus::RATE as isize);
                        cfg.setChannelCount(opus::CHANNELS as isize);
                        // voxl plays no audio itself, but a viewer on this Mac would loop.
                        cfg.setExcludesCurrentProcessAudio(true);
                        cfg.setWidth(2);
                        cfg.setHeight(2);
                        cfg.setMinimumFrameInterval(objc2_core_media::CMTime {
                            value: 1,
                            timescale: 1,
                            flags: objc2_core_media::CMTimeFlags::Valid,
                            epoch: 0,
                        });
                    },
                    on_sample,
                );
                let s = match s {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!("audio capture failed ({e}); retrying");
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                };
                log::info!("audio capture started");
                while want() && !s.stopped.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(200));
                }
                drop(s);
                pipeline.lock().restart();
                log::info!("audio capture stopped");
            }
        })
        .unwrap();
}
