// Live updates: one server-sent event stream per tab (`/api/sync/notify`). Views watch `change`
// and reload when their root changed. The browser reconnects by itself after interruptions.

export interface LiveChange {
	root: number;
	seq: number;
}

let source: EventSource | null = null;

export function useLive() {
	const change = useState<LiveChange[] | null>('live-change', () => null);

	function connect() {
		if (source || !import.meta.client) return;
		source = new EventSource('/api/sync/notify');
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

	return { change, connect, disconnect, onRootChange };
}
