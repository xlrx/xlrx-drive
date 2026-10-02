// Every page except login and account setup requires a session.
const PUBLIC = ['/login', '/setup'];

export default defineNuxtRouteMiddleware(async (to) => {
	const { me, loaded, load } = useSession();
	if (!loaded.value) await load();
	const path = to.path.replace(/\/+$/, '') || '/';
	if (!me.value && !PUBLIC.includes(path)) return navigateTo('/login');
	if (me.value && path === '/login') return navigateTo('/');
});
