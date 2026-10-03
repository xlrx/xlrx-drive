// Every page except login and account setup requires a session. After signing in, the person
// returns to the page they wanted (e.g. allowing a device).
const PUBLIC = ['/login', '/setup'];

export default defineNuxtRouteMiddleware(async (to) => {
	// Public links are for people without an account: no session at all.
	if (to.path.startsWith('/s/')) return;
	const { me, loaded, load } = useSession();
	if (!loaded.value) await load();
	const path = to.path.replace(/\/+$/, '') || '/';
	if (!me.value && !PUBLIC.includes(path)) {
		return navigateTo(path === '/' ? '/login' : { path: '/login', query: { next: to.fullPath } });
	}
	if (me.value && path === '/login') return navigateTo(safeNext(to.query.next));
});
