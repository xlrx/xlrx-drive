// Live updates: one server-sent event stream per tab (`/api/sync/notify`). Views watch `change`
// and reload when their root changed. The browser reconnects by itself after interruptions.

export interface LiveChange {
	root: number;
	seq: number;
}

let source: EventSource | null = null;

export function useLive() {
	const change = useState<LiveChange[] | null>('live-change', () => null);
	/** Unread notifications (the bell), pushed by the server whenever they change. */
	const unread = useState<number>('live-unread', () => 0);

	function connect() {
		if (source || !import.meta.client) return;
		source = new EventSource('/api/sync/notify');
		source.addEventListener('notification', (e) => {
			try {
				unread.value = (JSON.parse((e as MessageEvent<string>).data) as { unread: number }).unread;
			} catch {
				// ignore malformed events
			}
		});
		source.addEventListener('change', (e) => {
			try {
				change.value = JSON.parse((e as MessageEvent<string>).data) as LiveChange[];
			} catch {
				// ignore malformed events
			}
		});
	}

	function disconnect() {
		source?.close();
		source = null;
	}

	/** Calls `fn` (debounced) whenever something changed in the given root. */
	function onRootChange(root: () => number | undefined, fn: () => void) {
		let timer: ReturnType<typeof setTimeout> | undefined;
		watch(change, (c) => {
			const r = root();
			if (r === undefined || !c?.some((x) => x.root === r)) return;
			clearTimeout(timer);
			timer = setTimeout(fn, 250);
		});
		onBeforeUnmount(() => clearTimeout(timer));
	}

	/** Any change in a root the person can see (start page). */
	function onAnyChange(fn: () => void) {
		let timer: ReturnType<typeof setTimeout> | undefined;
		watch(change, (c) => {
			if (!c?.length) return;
			clearTimeout(timer);
			timer = setTimeout(fn, 250);
		});
		onBeforeUnmount(() => clearTimeout(timer));
	}

	return { change, unread, connect, disconnect, onRootChange, onAnyChange };
}
