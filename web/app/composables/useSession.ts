// The signed-in person (global, reactive).

export function useSession() {
	const me = useState<Me | null>('me', () => null);
	const loaded = useState('me-loaded', () => false);

	async function load(): Promise<Me | null> {
		try {
			me.value = await apiGet<Me>('/me');
		} catch (e) {
			if (!(e instanceof ApiError) || e.status !== 401) throw e;
			me.value = null;
		}
		loaded.value = true;
		return me.value;
	}

	function set(value: Me | null) {
		me.value = value;
		loaded.value = true;
	}

	return { me, loaded, load, set };
}
