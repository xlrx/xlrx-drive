// Angemeldete Person (global, reaktiv).
import { ApiError, get, type Me } from './api';

export const session = $state<{ me: Me | null; loaded: boolean }>({ me: null, loaded: false });

export async function loadMe(): Promise<Me | null> {
	try {
		session.me = await get<Me>('/me');
	} catch (e) {
		if (!(e instanceof ApiError) || e.status !== 401) throw e;
		session.me = null;
	}
	session.loaded = true;
	return session.me;
}
