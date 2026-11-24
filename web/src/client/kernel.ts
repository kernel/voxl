// Kernel's embed contract (https://kernel.sh/docs/browsers/live-view), the same one
// Kernel's neko live view keeps on Linux. The page tells the frame that embeds it how
// the connect is going, and takes a read-only lock from the URL, from the live view
// proxy (window.KERNEL_LIVE_VIEW_READ_ONLY) and from that frame.
//
// A connect walks transport (socket open) -> signaling (hello) -> media (first
// frame), each stage on its own bound. When embedded, a first connect that fails or
// runs out a bound is reported once and given up on, so the embedder decides what
// to do next. After the first frame, reconnects are voxl's own, as on a top-level
// page, which reports nothing and never gives up.

type Stage = 'transport' | 'signaling' | 'media';

declare global {
	interface Window {
		/** Injected by the live view proxy for read-only links. */
		KERNEL_LIVE_VIEW_READ_ONLY?: unknown;
		/** Injected by the live view proxy: per-stage bounds in ms. */
		KERNEL_LIVE_VIEW_CONNECT_TIMEOUTS_MS?: unknown;
	}
}

const DEFAULT_BOUNDS: Record<Stage, number> = {
	transport: 15000,
	signaling: 3000,
	media: 30000,
};

/** The bounds, with any stage the override sets to a positive number replaced. */
function stageBounds(overrides: unknown): Record<Stage, number> {
	const bounds = { ...DEFAULT_BOUNDS };
	if (typeof overrides !== 'object' || overrides === null) return bounds;
	for (const stage of Object.keys(bounds) as Stage[]) {
		const v = (overrides as Record<string, unknown>)[stage];
		if (typeof v === 'number' && Number.isFinite(v) && v > 0) bounds[stage] = v;
	}
	return bounds;
}

/** `?readOnly=`, `?readonly=` or `?ro=` set to 1, true or yes. */
function queryReadOnly(search: string): boolean {
	const q = new URLSearchParams(search);
	const v = q.get('readOnly') || q.get('readonly') || q.get('ro') || '';
	return ['1', 'true', 'yes'].includes(v.toLowerCase());
}

/** A KERNEL_SET_READ_ONLY from the embedding frame, at the origin it was loaded from. */
function isSetReadOnly(e: MessageEvent, parent: Window, origin: string) {
	const d = e.data;
	return (
		e.source === parent &&
		origin !== '*' &&
		origin !== 'null' &&
		e.origin === origin &&
		typeof d === 'object' &&
		d !== null &&
		d.type === 'KERNEL_SET_READ_ONLY' &&
		typeof d.readOnly === 'boolean' &&
		(d.requestId === undefined || typeof d.requestId === 'string')
	);
}

function parentOrigin(): string {
	try {
		if (document.referrer) return new URL(document.referrer).origin;
	} catch {}
	return '*';
}

export function createKernel() {
	const embedded = window.parent !== window;
	const origin = parentOrigin();
	const forced = window.KERNEL_LIVE_VIEW_READ_ONLY === true;
	const bounds = stageBounds(window.KERNEL_LIVE_VIEW_CONNECT_TIMEOUTS_MS);
	let stage: Stage = 'transport';
	let timer: ReturnType<typeof setTimeout> | undefined;
	let socketOpen = false;
	let connected = false;
	let playing = false;
	let everPlayed = false;
	let ended = false;
	// Ends the socket for good, once a connect has started.
	let stop: (() => void) | undefined;

	const post = (m: object) => {
		if (embedded) window.parent.postMessage(m, origin);
	};
	// Still on the first connect, with an embedder to report to.
	const waiting = () => embedded && !everPlayed && !ended;
	function arm(s: Stage) {
		stage = s;
		clearTimeout(timer);
		if (waiting())
			timer = setTimeout(() => fail('KERNEL_CONNECTION_TIMEOUT', s), bounds[s]);
	}
	function fail(type: string, reason: string) {
		if (!waiting()) return;
		ended = true;
		clearTimeout(timer);
		post({ type, reason, socketOpen });
		stop?.();
	}

	return {
		/** The lock the page starts with. */
		locked: forced || queryReadOnly(location.search),
		/** Gave up on the first connect. */
		ended: () => ended,
		/**
		 * Takes lock changes from the embedder. `setLocked` applies one and returns
		 * whether the viewer is now read-only.
		 */
		listen(setLocked: (locked: boolean) => boolean) {
			window.addEventListener('message', (e) => {
				if (!embedded || !isSetReadOnly(e, window.parent, origin)) return;
				const readOnly = setLocked(forced || e.data.readOnly);
				post({
					type: 'KERNEL_READ_ONLY_CHANGED',
					readOnly,
					requestId: e.data.requestId,
				});
			});
		},
		start(stopSocket: () => void) {
			stop = stopSocket;
			arm('transport');
		},
		unsupported: () => fail('KERNEL_CONNECTION_FAILED', 'unsupported'),
		open() {
			socketOpen = true;
			connected = false;
			arm('signaling');
		},
		hello() {
			if (connected) return;
			connected = true;
			post({
				type: 'KERNEL_CONNECTED',
				connected: true,
				capabilities: ['setReadOnly'],
			});
			arm('media');
		},
		playing() {
			clearTimeout(timer);
			playing = everPlayed = true;
			post({ type: 'KERNEL_PLAYING', playing: true });
		},
		closed() {
			socketOpen = false;
			if (playing) {
				playing = false;
				post({ type: 'KERNEL_PAUSED', playing: false });
			}
			fail('KERNEL_CONNECTION_FAILED', 'transport');
		},
		/** Frames only flow while the page shows video, so media waits for it. */
		video(on: boolean) {
			if (stage !== 'media' || !waiting()) return;
			if (on) arm('media');
			else clearTimeout(timer);
		},
	};
}
