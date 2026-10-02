// Zugriff auf die xlrx-API. Fehler kommen immer als `{ error: "…" }`.

export class ApiError extends Error {
	constructor(
		public status: number,
		message: string
	) {
		super(message);
	}

	/** Die Aktion braucht einen frischen zweiten Faktor. */
	get stepUp(): boolean {
		return this.status === 403 && this.message === 'step_up_required';
	}
}

export async function api<T = unknown>(method: string, path: string, body?: unknown): Promise<T> {
	const res = await fetch(`/api${path}`, {
		method,
		credentials: 'same-origin',
		headers: body === undefined ? {} : { 'content-type': 'application/json' },
		body: body === undefined ? undefined : JSON.stringify(body)
	});
	if (res.status === 204) return undefined as T;
	const text = await res.text();
	let data: unknown = null;
	try {
		data = JSON.parse(text);
	} catch {
		// keine JSON-Antwort (z.B. Fehler beim Lesen der Anfrage)
	}
	if (!res.ok) {
		const msg =
			(data as { error?: string } | null)?.error ?? (text.slice(0, 300) || `Fehler ${res.status}`);
		throw new ApiError(res.status, msg);
	}
	return data as T;
}

export const get = <T>(path: string) => api<T>('GET', path);
export const post = <T>(path: string, body: unknown = {}) => api<T>('POST', path, body);
export const del = <T>(path: string) => api<T>('DELETE', path);
export const patch = <T>(path: string, body: unknown) => api<T>('PATCH', path, body);

export function message(e: unknown): string {
	if (e instanceof ApiError) return e.message;
	if (e instanceof DOMException && e.name === 'NotAllowedError')
		return 'Vorgang abgebrochen oder nicht erlaubt.';
	if (e instanceof Error) return e.message;
	return 'Unbekannter Fehler';
}

export interface Me {
	id: number;
	username: string;
	display_name: string;
	email: string | null;
	is_admin: boolean;
	totp: boolean;
	passkeys: { id: number; name: string; created_at: string; last_used_at: string | null }[];
	recovery_codes_left: number;
	step_up_valid?: boolean;
}
