//! Minimal MP4 writer for recordings: the H.264 voxl already sends, rewrapped without
//! re-encoding. Frames arrive only when the screen changes, so each sample lasts until
//! the next one (variable frame rate).

use crate::opus;

/// One encoded frame: time since the recording started (ms), keyframe, Annex-B data.
pub struct Frame {
    pub ms: u64,
    pub key: bool,
    pub data: bytes::Bytes,
}

/// One encoded Opus packet and its timestamp relative to recording start (ms).
pub struct AudioPacket {
    pub ms: i64,
    pub data: bytes::Bytes,
}

const TIMESCALE: u32 = 1000;

/// NAL units of an Annex-B stream (start codes removed).
fn nals(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = None;
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = start {
                // A 4-byte start code leaves a zero at the end of the previous unit.
                let mut end = i;
                while end > s && data[end - 1] == 0 {
                    end -= 1;
                }
                out.push(&data[s..end]);
            }
            i += 3;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        out.push(&data[s..]);
    }
    out.into_iter().filter(|n| !n.is_empty()).collect()
}

fn bx(name: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(body.len() + 8);
    v.extend_from_slice(&(body.len() as u32 + 8).to_be_bytes());
    v.extend_from_slice(name);
    v.extend_from_slice(body);
    v
}

/// A full box: version 0 and `flags`, then the body.
fn fbx(name: &[u8; 4], flags: u32, body: &[u8]) -> Vec<u8> {
    let mut b = flags.to_be_bytes().to_vec();
    b.extend_from_slice(body);
    bx(name, &b)
}

fn u32s(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_be_bytes()).collect()
}

/// Identity transformation matrix of movie and track headers.
const MATRIX: [u32; 9] = [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000];

/// Data information: the file itself holds the media.
fn dinf() -> Vec<u8> {
    bx(
        b"dinf",
        &fbx(b"dref", 0, &[u32s(&[1]), fbx(b"url ", 1, &[])].concat()),
    )
}

/// Writes `frames` (the first must be a keyframe) as an MP4 of `width`x`height`
/// lasting until `end_ms`, with the `audio` that overlaps the video.
pub fn write(
    frames: &[Frame],
    width: u32,
    height: u32,
    end_ms: u64,
    audio: &[AudioPacket],
) -> Result<Vec<u8>, String> {
    let first = frames.first().ok_or("no frames were recorded")?;
    if !first.key {
        return Err("recording doesn't start with a keyframe".into());
    }
    let (mut sps, mut pps) = (None, None);
    for n in nals(&first.data) {
        match n[0] & 0x1f {
            7 if sps.is_none() => sps = Some(n.to_vec()),
            8 if pps.is_none() => pps = Some(n.to_vec()),
            _ => {}
        }
    }
    let (sps, pps) = (
        sps.ok_or("keyframe without SPS")?,
        pps.ok_or("keyframe without PPS")?,
    );
    if sps.len() < 4 {
        return Err("bad SPS".into());
    }

    // Samples: length-prefixed NAL units, parameter sets and delimiters left out. They
    // are only sized here and copied once, straight into the output, at the end.
    let mut samples: Vec<Vec<&[u8]>> = Vec::with_capacity(frames.len());
    let (mut sizes, mut durations, mut sync) = (Vec::new(), Vec::new(), Vec::new());
    for (i, f) in frames.iter().enumerate() {
        let units: Vec<&[u8]> = nals(&f.data)
            .into_iter()
            .filter(|n| !matches!(n[0] & 0x1f, 7..=9))
            .collect();
        sizes.push(units.iter().map(|n| n.len() as u32 + 4).sum::<u32>());
        samples.push(units);
        let next = frames
            .get(i + 1)
            .map(|n| n.ms)
            .unwrap_or(end_ms.max(f.ms + 17));
        durations.push(next.saturating_sub(f.ms).max(1) as u32);
        if f.key {
            sync.push(i as u32 + 1);
        }
    }
    let total: u64 = durations.iter().map(|&d| d as u64).sum();
    let n = frames.len() as u32;
    let video_len: u64 = sizes.iter().map(|&s| s as u64).sum();
    let audio = audio_window(audio, first.ms, total);
    let audio_edits = audio_edits(audio, first.ms, total)?;
    // Packets the edit list doesn't play are left out of the file.
    let audio = if audio_edits.is_empty() { &[] } else { audio };
    let audio_len: u64 = audio.iter().map(|p| p.data.len() as u64).sum();
    let mdat_len = video_len + audio_len;

    let ftyp = bx(b"ftyp", b"isom\0\0\x02\0isomiso2avc1mp41");

    let mut avcc = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
    avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    avcc.extend_from_slice(&sps);
    avcc.push(1);
    avcc.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    avcc.extend_from_slice(&pps);
    if matches!(sps[1], 100 | 110 | 122 | 144) {
        // 4:2:0, 8-bit, no SPS extensions.
        avcc.extend_from_slice(&[0xfd, 0xf8, 0xf8, 0]);
    }
    let mut avc1 = vec![0u8; 6];
    avc1.extend_from_slice(&1u16.to_be_bytes()); // data reference index
    avc1.extend_from_slice(&[0u8; 16]);
    avc1.extend_from_slice(&(width as u16).to_be_bytes());
    avc1.extend_from_slice(&(height as u16).to_be_bytes());
    avc1.extend_from_slice(&u32s(&[0x48_0000, 0x48_0000, 0]));
    avc1.extend_from_slice(&1u16.to_be_bytes()); // frame count
    avc1.extend_from_slice(&[0u8; 32]); // compressor name
    avc1.extend_from_slice(&[0, 0x18, 0xff, 0xff]); // depth, pre-defined
    avc1.extend_from_slice(&bx(b"avcC", &avcc));
    let stsd = fbx(b"stsd", 0, &[u32s(&[1]), bx(b"avc1", &avc1)].concat());

    // Runs of equal durations.
    let mut stts: Vec<(u32, u32)> = Vec::new();
    for &d in &durations {
        match stts.last_mut() {
            Some((c, last)) if *last == d => *c += 1,
            _ => stts.push((1, d)),
        }
    }
    let stts = fbx(
        b"stts",
        0,
        &[
            u32s(&[stts.len() as u32]),
            stts.iter().flat_map(|&(c, d)| u32s(&[c, d])).collect(),
        ]
        .concat(),
    );
    let stss = fbx(
        b"stss",
        0,
        &[u32s(&[sync.len() as u32]), u32s(&sync)].concat(),
    );
    let stsz = fbx(b"stsz", 0, &[u32s(&[0, n]), u32s(&sizes)].concat());
    let stsc = fbx(b"stsc", 0, &u32s(&[1, 1, n, 1]));
    // Everything in the sample table but the chunk offset, which depends on moov's size.
    let stbl_head = [stsd, stts, stss, stsz, stsc].concat();

    let vmhd = fbx(b"vmhd", 1, &[0u8; 8]);
    let mut hdlr = u32s(&[0]);
    hdlr.extend_from_slice(b"vide");
    hdlr.extend_from_slice(&[0u8; 12]);
    hdlr.extend_from_slice(b"voxl\0");
    let hdlr = fbx(b"hdlr", 0, &hdlr);
    let mut mdhd = u32s(&[0, 0, TIMESCALE, total as u32]);
    mdhd.extend_from_slice(&[0x55, 0xc4, 0, 0]); // language "und"
    let mdhd = fbx(b"mdhd", 0, &mdhd);
    let mut tkhd = u32s(&[0, 0, 1, 0, total as u32, 0, 0]);
    tkhd.extend_from_slice(&[0u8; 4]); // layer, alternate group
    tkhd.extend_from_slice(&[0u8; 4]); // volume, reserved
    tkhd.extend_from_slice(&u32s(&MATRIX));
    tkhd.extend_from_slice(&u32s(&[width << 16, height << 16]));
    let tkhd = fbx(b"tkhd", 3, &tkhd);

    let mut mvhd = u32s(&[0, 0, TIMESCALE, total as u32, 0x10000]);
    mvhd.extend_from_slice(&[1, 0, 0, 0]); // volume 1.0, reserved
    mvhd.extend_from_slice(&[0u8; 8]);
    mvhd.extend_from_slice(&u32s(&MATRIX));
    mvhd.extend_from_slice(&[0u8; 24]); // pre-defined
    let tracks: u32 = if audio.is_empty() { 1 } else { 2 };
    mvhd.extend_from_slice(&u32s(&[tracks + 1])); // next track id
    let mvhd = fbx(b"mvhd", 0, &mvhd);

    // moov's size doesn't depend on the chunk offsets, so it is built once with placeholders
    // to measure it, then again with the real offsets.
    let moov = |video_chunk: u32, audio_chunk: u32| {
        let stco = fbx(b"stco", 0, &u32s(&[1, video_chunk]));
        let stbl = bx(b"stbl", &[stbl_head.as_slice(), stco.as_slice()].concat());
        let minf = bx(
            b"minf",
            &[vmhd.as_slice(), dinf().as_slice(), stbl.as_slice()].concat(),
        );
        let mdia = bx(
            b"mdia",
            &[mdhd.as_slice(), hdlr.as_slice(), minf.as_slice()].concat(),
        );
        let video_trak = bx(b"trak", &[tkhd.as_slice(), mdia.as_slice()].concat());
        let audio_trak = if audio.is_empty() {
            Vec::new()
        } else {
            audio_track(audio, &audio_edits, total, audio_chunk)
        };
        bx(
            b"moov",
            &[
                mvhd.as_slice(),
                video_trak.as_slice(),
                audio_trak.as_slice(),
            ]
            .concat(),
        )
    };
    let probe = moov(0, 0);
    let file_len = ftyp.len() as u64 + probe.len() as u64 + 8 + mdat_len;
    // Chunk offsets are 32-bit, so the whole file has to be addressable.
    if file_len > u32::MAX as u64 {
        return Err("recording too large".into());
    }
    // mdat holds the video chunk, then the audio chunk, after its 8-byte header.
    let video_chunk = (ftyp.len() + probe.len() + 8) as u32;
    let audio_chunk = video_chunk + video_len as u32;
    let moov = moov(video_chunk, audio_chunk);

    let mut out = Vec::with_capacity(file_len as usize);
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&moov);
    out.extend_from_slice(&(mdat_len as u32 + 8).to_be_bytes());
    out.extend_from_slice(b"mdat");
    for n in samples.iter().flatten() {
        out.extend_from_slice(&(n.len() as u32).to_be_bytes());
        out.extend_from_slice(n);
    }
    for packet in audio {
        out.extend_from_slice(&packet.data);
    }
    Ok(out)
}

/// The Opus track: one sample per packet, mapped onto the movie by `edits`.
fn audio_track(audio: &[AudioPacket], edits: &[(u32, i32)], movie_ms: u64, chunk: u32) -> Vec<u8> {
    let n = audio.len() as u32;

    // dOps: version 0, channels, pre-skip, input sample rate, no output gain, mapping
    // family 0.
    let dops = [
        &[0, opus::CHANNELS as u8][..],
        &opus::PRE_SKIP.to_be_bytes(),
        &opus::RATE.to_be_bytes(),
        &[0; 3],
    ]
    .concat();
    // Sample entry: data reference index 1, stereo, 16-bit, 48 kHz as 16.16.
    let mut entry = vec![0; 6];
    entry.extend_from_slice(&1u16.to_be_bytes());
    entry.extend_from_slice(&[0; 8]); // reserved
    entry.extend_from_slice(&(opus::CHANNELS as u16).to_be_bytes());
    entry.extend_from_slice(&16u16.to_be_bytes()); // sample size
    entry.extend_from_slice(&[0; 4]); // pre-defined, reserved
    entry.extend_from_slice(&(opus::RATE << 16).to_be_bytes());
    entry.extend_from_slice(&bx(b"dOps", &dops));
    let stsd = fbx(b"stsd", 0, &[u32s(&[1]), bx(b"Opus", &entry)].concat());

    let sizes: Vec<u32> = audio.iter().map(|p| p.data.len() as u32).collect();
    let stbl = bx(
        b"stbl",
        &[
            stsd,
            fbx(b"stts", 0, &u32s(&[1, n, opus::FRAME as u32])),
            fbx(b"stsz", 0, &[u32s(&[0, n]), u32s(&sizes)].concat()),
            fbx(b"stsc", 0, &u32s(&[1, 1, n, 1])),
            fbx(b"stco", 0, &u32s(&[1, chunk])),
        ]
        .concat(),
    );
    let smhd = fbx(b"smhd", 0, &[0; 4]);
    let minf = bx(b"minf", &[smhd, dinf(), stbl].concat());
    let hdlr = fbx(
        b"hdlr",
        0,
        &[&u32s(&[0])[..], b"soun", &[0; 12], b"voxl audio\0"].concat(),
    );
    let mut mdhd = u32s(&[0, 0, opus::RATE, n.saturating_mul(opus::FRAME as u32)]);
    mdhd.extend_from_slice(&[0x55, 0xc4, 0, 0]); // language "und"
    let mdia = bx(b"mdia", &[fbx(b"mdhd", 0, &mdhd), hdlr, minf].concat());

    // Track 2, enabled; identity matrix, volume 1.0, no picture size.
    let mut tkhd = u32s(&[0, 0, 2, 0, movie_ms as u32, 0, 0]);
    tkhd.extend_from_slice(&[0; 4]); // layer, alternate group
    tkhd.extend_from_slice(&[1, 0, 0, 0]); // volume 1.0, reserved
    tkhd.extend_from_slice(&u32s(&MATRIX));
    tkhd.extend_from_slice(&u32s(&[0, 0]));
    let tkhd = fbx(b"tkhd", 3, &tkhd);

    // Each edit: movie duration, media time in samples (-1 for an empty edit), rate 1.
    // The -1 is stored as its two's complement.
    let entries: Vec<u32> = edits
        .iter()
        .flat_map(|&(duration, media)| [duration, media as u32, 0x10000])
        .collect();
    let elst = fbx(
        b"elst",
        0,
        &[u32s(&[edits.len() as u32]), u32s(&entries)].concat(),
    );

    bx(b"trak", &[tkhd, bx(b"edts", &elst), mdia].concat())
}

/// The packets that play within the movie, which starts at `origin` (ms) and lasts
/// `movie_ms`: from the first packet that reaches past `origin` to the last one that
/// starts before the movie ends.
fn audio_window(audio: &[AudioPacket], origin: u64, movie_ms: u64) -> &[AudioPacket] {
    let origin = i64::try_from(origin).unwrap_or(i64::MAX);
    let end = origin.saturating_add(i64::try_from(movie_ms).unwrap_or(i64::MAX));
    let from = audio
        .iter()
        .position(|p| p.ms.saturating_add(opus::FRAME_MS) > origin)
        .unwrap_or(audio.len());
    let rest = &audio[from..];
    let to = rest.iter().position(|p| p.ms >= end).unwrap_or(rest.len());
    &rest[..to]
}

/// Maps `audio` (already limited by `audio_window`) onto the movie as edit list entries:
/// (movie duration, media time in samples, or -1 for an empty edit). Gaps in capture
/// become empty edits. A first packet that starts before the video is trimmed to start
/// with it.
fn audio_edits(
    audio: &[AudioPacket],
    origin: u64,
    movie_ms: u64,
) -> Result<Vec<(u32, i32)>, String> {
    /// Packets closer together than this play back to back; wider gaps are kept.
    const GAP_MS: i64 = 15;
    const TICKS_PER_MS: i64 = opus::RATE as i64 / 1000;

    let origin = i64::try_from(origin).unwrap_or(i64::MAX);
    let movie = i64::try_from(movie_ms).unwrap_or(i64::MAX);
    let mut edits = Vec::new();
    // Movie time covered by the edits so far.
    let mut cursor = 0;
    // Index of the first packet of the current run.
    let mut first = 0;
    for run in audio.chunk_by(|a, b| b.ms.saturating_sub(a.ms) <= GAP_MS) {
        let relative = run[0].ms.saturating_sub(origin);
        let trim = if first == 0 {
            (-relative).clamp(0, opus::FRAME_MS)
        } else {
            0
        };
        let start = relative.clamp(0, movie);
        if start > cursor {
            edits.push(((start - cursor) as u32, -1));
            cursor = start;
        }
        let duration = (run.len() as i64 * opus::FRAME_MS - trim).min(movie - cursor);
        if duration > 0 {
            let samples = first as i64 * opus::FRAME as i64 + trim * TICKS_PER_MS;
            let media = i32::try_from(samples).map_err(|_| "recording too long for audio")?;
            edits.push((duration as u32, media));
            cursor += duration;
        }
        first += run.len();
    }
    Ok(edits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_annex_b() {
        let d = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5,
        ];
        let n = nals(&d);
        assert_eq!(
            n,
            vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4, 5][..]]
        );
    }

    #[test]
    fn moov_first_and_chunk_offset_points_at_the_samples() {
        let key = bytes::Bytes::from_static(&[
            0, 0, 0, 1, 0x67, 100, 0, 40, 0xac, 0, 0, 0, 1, 0x68, 0xee, 0, 0, 0, 1, 0x65, 0xaa,
        ]);
        let delta = bytes::Bytes::from_static(&[0, 0, 0, 1, 0x41, 0xbb, 0xcc]);
        let frames = [
            Frame {
                ms: 0,
                key: true,
                data: key,
            },
            Frame {
                ms: 40,
                key: false,
                data: delta,
            },
        ];
        let out = write(&frames, 64, 32, 100, &[]).unwrap();
        let pos = |t: &[u8]| out.windows(4).position(|w| w == t).unwrap();
        assert!(pos(b"moov") < pos(b"mdat"));
        let stco = pos(b"stco");
        let off = u32::from_be_bytes(out[stco + 12..stco + 16].try_into().unwrap()) as usize;
        assert_eq!(off, pos(b"mdat") + 4);
        // Both samples, length-prefixed, parameter sets left out.
        assert_eq!(
            &out[off..],
            &[0, 0, 0, 2, 0x65, 0xaa, 0, 0, 0, 3, 0x41, 0xbb, 0xcc]
        );
    }

    /// Packets at these times, each holding its time twice as its bytes.
    fn packets(ms: &[i64]) -> Vec<AudioPacket> {
        let data = |ms: i64| bytes::Bytes::from(vec![ms as u8; 2]);
        ms.iter()
            .map(|&ms| AudioPacket { ms, data: data(ms) })
            .collect()
    }

    fn key_frame() -> Frame {
        Frame {
            ms: 0,
            key: true,
            data: bytes::Bytes::from_static(&[
                0, 0, 0, 1, 0x67, 100, 0, 40, 0xac, 0, 0, 0, 1, 0x68, 0xee, 0, 0, 0, 1, 0x65, 0xaa,
            ]),
        }
    }

    #[test]
    fn audio_track_offsets_follow_video_and_preserve_packets() {
        let out = write(&[key_frame()], 64, 32, 100, &packets(&[20, 30])).unwrap();
        let offset = |stco: usize| {
            u32::from_be_bytes(out[stco + 12..stco + 16].try_into().unwrap()) as usize
        };
        let chunks: Vec<_> = (0..out.len() - 4)
            .filter(|&i| &out[i..i + 4] == b"stco")
            .map(offset)
            .collect();
        assert_eq!(&out[chunks[0]..chunks[1]], &[0, 0, 0, 2, 0x65, 0xaa]);
        assert_eq!(&out[chunks[1]..], &[20, 20, 30, 30]);
    }

    #[test]
    fn opus_track_declares_the_encoder_pre_skip() {
        let out = write(&[key_frame()], 64, 32, 100, &packets(&[0])).unwrap();
        let dops = out.windows(4).position(|w| w == b"dOps").unwrap();
        // The pre-skip follows the version and channel count.
        assert_eq!(&out[dops + 6..dops + 8], &opus::PRE_SKIP.to_be_bytes());
    }

    #[test]
    fn audio_edits_trim_before_video_and_preserve_capture_gaps() {
        let before_video = packets(&(-1000..=0).step_by(10).collect::<Vec<_>>());
        let window = audio_window(&before_video, 0, 100);
        assert_eq!(window.len(), 1);
        assert_eq!(audio_edits(window, 0, 100).unwrap(), vec![(10, 0)]);

        // Starts 5 ms before the video: 5 ms trimmed, so media starts at 5 ms (240 samples).
        let straddling = packets(&[-5, 5]);
        let window = audio_window(&straddling, 0, 100);
        assert_eq!(audio_edits(window, 0, 100).unwrap(), vec![(15, 240)]);

        let gapped = packets(&[0, 10, 40, 50]);
        assert_eq!(
            audio_edits(audio_window(&gapped, 0, 100), 0, 100).unwrap(),
            vec![(20, 0), (20, -1), (20, 960)]
        );
    }

    #[test]
    fn audio_after_the_movie_is_left_out() {
        let late = packets(&[100, 110]);
        assert!(audio_window(&late, 0, 100).is_empty());
        assert!(audio_edits(audio_window(&late, 0, 100), 0, 100)
            .unwrap()
            .is_empty());
    }
}
