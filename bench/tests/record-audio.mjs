// Linux regression test: a known tone, silence and tone played into a PulseAudio sink come
// back in the recording's Opus track at the right times. Run it against a voxl with a
// display and a sink that only this test plays into:
// VOXL_URL=http://127.0.0.1:6901 VOXL_TOKEN=... PULSE_SINK=... node bench/tests/record-audio.mjs
// Needs ffmpeg, ffprobe and paplay.
import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const { VOXL_URL, VOXL_TOKEN, PULSE_SINK } = process.env;
if (!VOXL_URL || !VOXL_TOKEN || !PULSE_SINK) throw new Error('Set VOXL_URL, VOXL_TOKEN and PULSE_SINK');
const dir = mkdtempSync(join(tmpdir(), 'voxl-record-audio-'));
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function run(program, args) {
  const result = spawnSync(program, args, { encoding: 'utf8' });
  if (result.status !== 0) throw new Error(`${program}: ${result.stderr || result.error}`);
  return result.stdout;
}
const streams = (file) => JSON.parse(run('ffprobe', ['-v', 'error', '-show_entries',
  'stream=codec_name,codec_type', '-of', 'json', file])).streams;
async function get(path, method = 'GET') {
  const headers = { Authorization: `Bearer ${VOXL_TOKEN}` };
  const response = await fetch(new URL(path, VOXL_URL), { method, headers });
  if (!response.ok) throw new Error(`${path}: HTTP ${response.status}: ${await response.text()}`);
  return response;
}
const save = async (response, file) => writeFileSync(file, Buffer.from(await response.arrayBuffer()));

try {
  const wav = join(dir, 'tone.wav');
  // 440 Hz for a second, digital silence for a second, then 880 Hz for a second.
  run('ffmpeg', ['-v', 'error', '-f', 'lavfi', '-i', 'sine=frequency=440:sample_rate=48000:duration=1',
    '-f', 'lavfi', '-i', 'anullsrc=r=48000:cl=stereo:d=1',
    '-f', 'lavfi', '-i', 'sine=frequency=880:sample_rate=48000:duration=1',
    '-filter_complex', '[0:a]aformat=channel_layouts=stereo[a];[2:a]aformat=channel_layouts=stereo[b];[a][1:a][b]concat=n=3:v=0:a=1[out]',
    '-map', '[out]', '-y', wav]);

  const started = await (await get('/record/start', 'POST')).json();
  if (started.audio !== true) throw new Error(`recording did not report audio: ${JSON.stringify(started)}`);
  // Capture starts a moment after the request, so play after a pause and pad the end.
  await sleep(800);
  await new Promise((resolve, reject) => {
    const player = spawn('paplay', ['--device', PULSE_SINK, wav], { stdio: 'inherit' });
    player.on('error', reject);
    player.on('exit', (code) => (code === 0 ? resolve() : reject(new Error(`paplay exited ${code}`))));
  });
  await sleep(400);

  const mp4 = join(dir, 'recording.mp4');
  await save(await get(`/record/stop?id=${started.id}`, 'POST'), mp4);
  const kinds = streams(mp4).map((s) => `${s.codec_type}:${s.codec_name}`);
  if (!kinds.includes('video:h264') || !kinds.includes('audio:opus')) {
    throw new Error(`recording has ${kinds}, want H.264 video and Opus audio`);
  }
  // audio=0 leaves the audio out.
  const plain = join(dir, 'plain.mp4');
  await save(await get('/record?seconds=1&audio=0'), plain);
  if (streams(plain).some((s) => s.codec_type === 'audio')) throw new Error('audio=0 recording has audio');

  const decoded = spawnSync('ffmpeg', ['-v', 'error', '-i', mp4, '-map', '0:a:0',
    '-ac', '1', '-ar', '48000', '-f', 'f32le', '-'], { maxBuffer: 16 * 1024 * 1024 });
  if (decoded.status !== 0) throw new Error(`audio decode failed: ${decoded.stderr}`);
  // Root mean square of the mono track over [from, to) seconds.
  const level = (from, to) => {
    const bytes = decoded.stdout.subarray(Math.floor(from * 48000) * 4, Math.floor(to * 48000) * 4);
    if (bytes.length < (to - from) * 48000 * 4) throw new Error('audio ends early');
    let sum = 0;
    for (let i = 0; i < bytes.length; i += 4) sum += bytes.readFloatLE(i) ** 2;
    return Math.sqrt(sum / (bytes.length / 4));
  };
  // The monitor may start producing audio only once playback does, so find the first tone
  // rather than assume a startup delay.
  const onset = [...Array(20).keys()].map((i) => i * 0.1)
    .find((t) => (t + 0.2) * 48000 * 4 <= decoded.stdout.length && level(t, t + 0.2) > 0.02);
  if (onset === undefined) throw new Error('no audible tone in recorded audio');
  const first = level(onset + 0.2, onset + 0.7);
  const silence = level(onset + 1.1, onset + 1.5);
  const second = level(onset + 2.1, onset + 2.6);
  if (first < 0.02 || second < 0.02 || silence > 0.005) {
    throw new Error(`unexpected tone/silence timing: rms=${first},${silence},${second}`);
  }
  console.log('PASS: H.264 and Opus tracks; tone, silence and tone retain their timing');
} finally {
  rmSync(dir, { recursive: true, force: true });
}
