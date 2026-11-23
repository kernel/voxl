//! Minimal MP4 writer for recordings: the H.264 voxl already sends, rewrapped without
//! re-encoding. Frames arrive only when the screen changes, so each sample lasts until
//! the next one (variable frame rate).

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
    let audio_edits = audio_edits(audio, first.ms, total);
    let audio = if audio_edits.is_empty() { &[] } else { audio };
    let audio_len: u64 = audio.iter().map(|p| p.data.len() as u64).sum();
    let mdat_len = video_len + audio_len;

    let ftyp = bx(b"ftyp", b"isom\0\0\x02\0isomiso2avc1mp41");
    if mdat_len + 8 > u32::MAX as u64 {
        return Err("recording too large".into());
    }
    // Patched in once moov's size is known (it doesn't depend on this value).
    let chunk_offset = 0;

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
    let stco = fbx(b"stco", 0, &u32s(&[1, chunk_offset]));
    let stbl = bx(b"stbl", &[stsd, stts, stss, stsz, stsc, stco].concat());

    let dinf = bx(
        b"dinf",
        &fbx(b"dref", 0, &[u32s(&[1]), fbx(b"url ", 1, &[])].concat()),
    );
    let vmhd = fbx(b"vmhd", 1, &[0u8; 8]);
    let minf = bx(b"minf", &[vmhd, dinf, stbl].concat());
    let mut hdlr = u32s(&[0]);
    hdlr.extend_from_slice(b"vide");
    hdlr.extend_from_slice(&[0u8; 12]);
    hdlr.extend_from_slice(b"voxl\0");
    let hdlr = fbx(b"hdlr", 0, &hdlr);
    let mut mdhd = u32s(&[0, 0, TIMESCALE, total as u32]);
    mdhd.extend_from_slice(&[0x55, 0xc4, 0, 0]); // language "und"
    let mdia = bx(b"mdia", &[fbx(b"mdhd", 0, &mdhd), hdlr, minf].concat());

    let mut tkhd = u32s(&[0, 0, 1, 0, total as u32, 0, 0]);
    tkhd.extend_from_slice(&[0u8; 4]); // layer, alternate group
    tkhd.extend_from_slice(&[0u8; 4]); // volume, reserved
    tkhd.extend_from_slice(&u32s(&MATRIX));
    tkhd.extend_from_slice(&u32s(&[width << 16, height << 16]));
    let trak = bx(b"trak", &[fbx(b"tkhd", 3, &tkhd), mdia].concat());

    let mut mvhd = u32s(&[0, 0, TIMESCALE, total as u32, 0x10000]);
    mvhd.extend_from_slice(&[1, 0, 0, 0]); // volume 1.0, reserved
    mvhd.extend_from_slice(&[0u8; 8]);
    mvhd.extend_from_slice(&u32s(&MATRIX));
    mvhd.extend_from_slice(&[0u8; 24]); // pre-defined
    let tracks = if audio.is_empty() { 1 } else { 2 };
    mvhd.extend_from_slice(&u32s(&[tracks + 1])); // next track id
    let audio_trak = if audio.is_empty() {
        Vec::new()
    } else {
        audio_track(audio, &audio_edits, total)
    };
    let mut moov = bx(
        b"moov",
        &[fbx(b"mvhd", 0, &mvhd), trak, audio_trak].concat(),
    );

    let offset = (ftyp.len() + moov.len() + 8) as u32;
    // The video, then the audio, each occupy one chunk in mdat. Each track's stco has one
    // chunk offset, its last 4 bytes.
    let stco: Vec<usize> = moov
        .windows(16)
        .enumerate()
        .filter_map(|(i, b)| (b == b"\0\0\0\x14stco\0\0\0\0\0\0\0\x01").then_some(i + 16))
        .collect();
    if stco.len() != tracks as usize {
        return Err("invalid chunk offsets".into());
    }
    let audio_offset = offset
        .checked_add(video_len as u32)
        .ok_or("recording too large")?;
    for (at, chunk) in stco.into_iter().zip([offset, audio_offset]) {
        moov[at..at + 4].copy_from_slice(&chunk.to_be_bytes());
    }
    let mut out = Vec::with_capacity(offset as usize + mdat_len as usize);
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

fn audio_track(audio: &[AudioPacket], edit_list: &[(u32, i32)], movie_ms: u64) -> Vec<u8> {
    let n = audio.len() as u32;
    // Version 0, stereo, pre-skip 312 (the encoder's lookahead), 48 kHz, no gain, family 0.
    let dops = [
        &[0, 2][..],
        &312u16.to_be_bytes(),
        &48000u32.to_be_bytes(),
        &[0; 3],
    ]
    .concat();
    let mut opus = vec![0, 0, 0, 0, 0, 0, 0, 1]; // reserved, data reference index
    opus.extend_from_slice(&u32s(&[0, 0, 2 << 16 | 16, 0, 48000 << 16])); // stereo, 16-bit
    opus.extend_from_slice(&bx(b"dOps", &dops));
    let stsd = fbx(b"stsd", 0, &[u32s(&[1]), bx(b"Opus", &opus)].concat());
    let sizes: Vec<u32> = audio.iter().map(|p| p.data.len() as u32).collect();
    let stbl = [
        stsd,
        fbx(b"stts", 0, &u32s(&[1, n, 480])),
        fbx(b"stsz", 0, &[u32s(&[0, n]), u32s(&sizes)].concat()),
        fbx(b"stsc", 0, &u32s(&[1, 1, n, 1])),
        fbx(b"stco", 0, &u32s(&[1, 0])),
    ];
    let dinf = bx(
        b"dinf",
        &fbx(b"dref", 0, &[u32s(&[1]), fbx(b"url ", 1, &[])].concat()),
    );
    let minf = bx(
        b"minf",
        &[fbx(b"smhd", 0, &[0; 4]), dinf, bx(b"stbl", &stbl.concat())].concat(),
    );
    let hdlr = [&u32s(&[0])[..], b"soun", &[0; 12], b"voxl audio\0"].concat();
    let mdhd = [
        u32s(&[0, 0, 48000, n.saturating_mul(480)]),
        vec![0x55, 0xc4, 0, 0],
    ]
    .concat();
    let mdia = [fbx(b"mdhd", 0, &mdhd), fbx(b"hdlr", 0, &hdlr), minf].concat();
    let tkhd = [
        u32s(&[0, 0, 2, 0, movie_ms as u32, 0, 0, 0]),
        vec![1, 0, 0, 0], // volume 1.0
        u32s(&MATRIX),
        u32s(&[0, 0]),
    ];
    // Each edit: movie duration, media start (-1 for an empty edit), rate 1.0.
    let elst: Vec<u32> = edit_list
        .iter()
        .flat_map(|&(d, t)| [d, t as u32, 0x10000])
        .collect();
    let elst = fbx(
        b"elst",
        0,
        &[u32s(&[edit_list.len() as u32]), u32s(&elst)].concat(),
    );
    bx(
        b"trak",
        &[
            fbx(b"tkhd", 3, &tkhd.concat()),
            bx(b"edts", &elst),
            bx(b"mdia", &mdia),
        ]
        .concat(),
    )
}

/// Maps packet media time onto the movie timeline. Empty edits preserve capture gaps;
/// media-time offsets trim audio that began before the first video frame.
fn audio_edits(audio: &[AudioPacket], video_start_ms: u64, movie_ms: u64) -> Vec<(u32, i32)> {
    const PACKET_MS: i64 = 10;
    const TICKS_PER_MS: i64 = 48;
    const GAP_THRESHOLD_MS: i64 = 15;

    let origin = video_start_ms.min(i64::MAX as u64) as i64;
    let Some(first) = audio
        .iter()
        .position(|p| p.ms.saturating_add(PACKET_MS) > origin)
    else {
        return Vec::new();
    };
    let mut edits = Vec::new();
    let mut run_start = first;
    let mut movie_cursor = 0i64;

    while run_start < audio.len() {
        let mut run_end = run_start + 1;
        while run_end < audio.len()
            && audio[run_end].ms.saturating_sub(audio[run_end - 1].ms) <= GAP_THRESHOLD_MS
        {
            run_end += 1;
        }

        let relative_start = audio[run_start].ms.saturating_sub(origin);
        let trim_ms = if run_start == first {
            relative_start.saturating_neg().clamp(0, PACKET_MS)
        } else {
            0
        };
        let desired_start = relative_start.max(0);
        if desired_start > movie_cursor {
            let empty = (desired_start - movie_cursor).min(movie_ms as i64) as u32;
            if empty > 0 {
                edits.push((empty, -1));
            }
            movie_cursor += i64::from(empty);
        }

        let count = (run_end - run_start) as i64;
        let duration = count * PACKET_MS - trim_ms;
        let remaining = (movie_ms as i64).saturating_sub(movie_cursor).max(0);
        let duration = duration.min(remaining);
        if duration > 0 {
            let media_time =
                (run_start as i64 * 480 + trim_ms * TICKS_PER_MS).min(i32::MAX as i64) as i32;
            edits.push((duration as u32, media_time));
            movie_cursor += duration;
        }
        run_start = run_end;
    }
    edits
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

    #[test]
    fn audio_track_offsets_follow_video_and_preserve_packets() {
        let frame = Frame {
            ms: 0,
            key: true,
            data: bytes::Bytes::from_static(&[
                0, 0, 0, 1, 0x67, 100, 0, 40, 0xac, 0, 0, 0, 1, 0x68, 0xee, 0, 0, 0, 1, 0x65, 0xaa,
            ]),
        };
        let out = write(&[frame], 64, 32, 100, &packets(&[20, 30])).unwrap();
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
    fn audio_edits_trim_before_video_and_preserve_capture_gaps() {
        let before_video = packets(&(-1000..=0).step_by(10).collect::<Vec<_>>());
        assert_eq!(audio_edits(&before_video, 0, 100), vec![(10, 48_000)]);
        assert!(audio_edits(&before_video[..100], 0, 100).is_empty());
        assert_eq!(
            audio_edits(&packets(&[0, 10, 40, 50]), 0, 100),
            vec![(20, 0), (20, -1), (20, 960)]
        );
    }
}
