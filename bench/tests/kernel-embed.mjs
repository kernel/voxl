// Kernel's embed contract (web/src/client/kernel.ts): what the page posts to the frame
// that embeds it, and the read-only lock it takes. Starts its own voxl (VOXL_BIN) on
// BENCH_DISPLAY, embeds it from another origin and checks each case:
//   VOXL_BIN=target/release/voxl BENCH_DISPLAY=:2 node bench/tests/kernel-embed.mjs
// Needs xdotool, and something changing on the display so frames keep coming.
import { createRequire } from 'module';
import { execSync, spawn } from 'node:child_process';
import http from 'node:http';
import assert from 'node:assert/strict';
const require = createRequire(process.env.PLAYWRIGHT_NODE_MODULES || process.cwd() + '/node_modules/');
const { chromium } = require('playwright');
const DISPLAY = process.env.BENCH_DISPLAY || ':2';
const PORT = process.env.VOXL_PORT || '6904';
const VOXL = `http://127.0.0.1:${PORT}`;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const pointer = () => execSync('xdotool getmouselocation', { env: { ...process.env, DISPLAY } }).toString().split(' ').slice(0, 2).join(' ');
const park = () => execSync('xdotool mousemove 5 5', { env: { ...process.env, DISPLAY } });

let voxl;
const startVoxl = () => {
  voxl = spawn(process.env.VOXL_BIN || 'voxl', [`--port=${PORT}`, '--addr=127.0.0.1', '--enable-basic-auth=false', '--audio-enabled=false', '--file-transfers=none'],
    { env: { ...process.env, DISPLAY }, stdio: 'ignore' });
};
// The embedding page, on another origin. ?src= is the frame's URL.
const parent = http.createServer((req, res) => {
  const src = new URL(req.url, 'http://x').searchParams.get('src') || VOXL + '/';
  res.setHeader('content-type', 'text/html');
  res.end(`<script>window.got=[];addEventListener('message',e=>{if(String(e.data?.type).startsWith('KERNEL_'))got.push(e.data)})</script>
<iframe id=f src="${src}" width=800 height=500></iframe>`);
}).listen(0);
// The page through a proxy that drops every socket upgrade.
const dropper = http.createServer((req, res) => {
  req.pipe(http.request({ host: '127.0.0.1', port: PORT, path: req.url, method: req.method, headers: req.headers }, (r) => {
    res.writeHead(r.statusCode, r.headers);
    r.pipe(res);
  }));
}).listen(0);
dropper.on('upgrade', (req, socket) => socket.destroy());

startVoxl();
await sleep(1500);
const browser = await chromium.launch({ executablePath: process.env.CHROMIUM || 'chromium', args: ['--no-sandbox'] });
async function open(src = VOXL + '/', init) {
  const page = await browser.newPage();
  if (init) await page.addInitScript(init);
  await page.goto(`http://localhost:${parent.address().port}/?src=${encodeURIComponent(src)}`);
  return page;
}
const got = (page) => page.evaluate(() => got);
const types = async (page) => (await got(page)).map((m) => m.type + (m.reason ? `:${m.reason}` : ''));
async function until(page, type) {
  for (let i = 0; i < 100 && !(await types(page)).includes(type); i++) await sleep(100);
  return types(page);
}
let moves = 0;
async function moveIn(page) {
  const box = await page.locator('#f').boundingBox();
  moves += 10;
  await page.mouse.move(box.x + 300 + moves, box.y + 200);
  await sleep(400);
  return pointer() !== 'x:5 y:5';
}
async function setReadOnly(page, readOnly, requestId) {
  await page.evaluate(([readOnly, requestId, origin]) =>
    document.getElementById('f').contentWindow.postMessage({ type: 'KERNEL_SET_READ_ONLY', readOnly, requestId }, origin), [readOnly, requestId, VOXL]);
  await sleep(300);
  return (await got(page)).at(-1);
}
const status = (page) => page.frameLocator('#f').locator('#status').textContent();

// Connects and plays; the embedder locks and unlocks it.
let page = await open();
assert.deepEqual(await until(page, 'KERNEL_PLAYING'), ['KERNEL_CONNECTED', 'KERNEL_PLAYING']);
assert.deepEqual((await got(page))[0].capabilities, ['setReadOnly']);
park();
assert.deepEqual(await setReadOnly(page, true, 'a'), { type: 'KERNEL_READ_ONLY_CHANGED', readOnly: true, requestId: 'a' });
assert.equal(await moveIn(page), false, 'a locked viewer moved the pointer');
assert.deepEqual(await setReadOnly(page, false, 'b'), { type: 'KERNEL_READ_ONLY_CHANGED', readOnly: false, requestId: 'b' });
assert.equal(await moveIn(page), true, 'an unlocked viewer could not move the pointer');

// After it played, a lost server pauses it, and it reconnects on its own.
voxl.kill();
await sleep(1500);
assert.equal((await types(page)).at(-1), 'KERNEL_PAUSED');
assert.equal(await status(page), 'Reconnecting…');
startVoxl();
await sleep(4000);
assert.deepEqual((await types(page)).slice(-2), ['KERNEL_CONNECTED', 'KERNEL_PLAYING']);
await page.close();

// ?readOnly=true starts locked.
park();
page = await open(VOXL + '/?readOnly=true');
await until(page, 'KERNEL_PLAYING');
assert.equal(await moveIn(page), false, '?readOnly=true moved the pointer');
await page.close();

// A read-only link (the proxy sets the global) stays locked.
park();
page = await open(undefined, () => { window.KERNEL_LIVE_VIEW_READ_ONLY = true; });
await until(page, 'KERNEL_PLAYING');
assert.equal((await setReadOnly(page, false, 'c')).readOnly, true);
assert.equal(await moveIn(page), false, 'a read-only link moved the pointer');
await page.close();

// A stage bound from the proxy runs out: reported once, and it stops trying.
page = await open(undefined, () => { window.KERNEL_LIVE_VIEW_CONNECT_TIMEOUTS_MS = { transport: 1 }; });
await sleep(3000);
assert.deepEqual(await types(page), ['KERNEL_CONNECTION_TIMEOUT:transport']);
assert.equal(await status(page), 'Disconnected');
await page.close();

// The socket closes before the first frame.
page = await open(`http://127.0.0.1:${dropper.address().port}/`);
await sleep(3000);
assert.deepEqual(await types(page), ['KERNEL_CONNECTION_FAILED:transport']);
await page.close();

// No WebCodecs.
page = await open(undefined, () => { delete window.VideoDecoder; });
await sleep(1500);
assert.deepEqual(await types(page), ['KERNEL_CONNECTION_FAILED:unsupported']);

console.log('PASS: Kernel embed messages and read-only lock');
await browser.close();
voxl.kill();
parent.close();
dropper.close();
