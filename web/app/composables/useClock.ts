// The current time, updated every minute.

export function useClock() {
	const now = ref(new Date());
	let timer: ReturnType<typeof setInterval> | undefined;
	onMounted(() => {
		timer = setInterval(() => (now.value = new Date()), 60_000);
	});
	onBeforeUnmount(() => clearInterval(timer));
	return now;
}
