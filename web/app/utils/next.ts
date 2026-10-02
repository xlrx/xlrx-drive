// Where to go after signing in: only paths within this app (no other hosts, no `//host`).

export function safeNext(next: unknown): string {
	const s = Array.isArray(next) ? next[0] : next;
	return typeof s === 'string' && s.startsWith('/') && !s.startsWith('//') && !s.startsWith('/\\') ? s : '/';
}
