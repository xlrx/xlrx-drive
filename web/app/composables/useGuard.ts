// Runs an action; if the server asks for a fresh second factor, the action is retried
// automatically after confirmation (component `StepUpDialog`).

export function useGuard() {
	const pending = shallowRef<null | (() => Promise<void>)>(null);
	const error = ref('');
	const busy = ref(false);

	async function run(fn: () => Promise<void>) {
		error.value = '';
		busy.value = true;
		try {
			await fn();
		} catch (e) {
			if (e instanceof ApiError && e.stepUp) pending.value = fn;
			else error.value = errorMessage(e);
		} finally {
			busy.value = false;
		}
	}

	async function confirmed() {
		const fn = pending.value;
		pending.value = null;
		if (fn) await run(fn);
	}

	function cancel() {
		pending.value = null;
	}

	return { pending, error, busy, run, confirmed, cancel };
}
