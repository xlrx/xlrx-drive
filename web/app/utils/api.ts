// Access to the xlrx API. Errors always arrive as `{ error: "…" }`.

export class ApiError extends Error {
	constructor(
		public status: number,
		message: string
	) {
		super(message);
	}

	/** The action needs a fresh second factor. */
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
		// Not JSON (e.g. the request body could not be parsed).
	}
	if (!res.ok) {
		const msg =
			(data as { error?: string } | null)?.error ?? (text.slice(0, 300) || `Fehler ${res.status}`);
		throw new ApiError(res.status, msg);
	}
	return data as T;
}

export const apiGet = <T>(path: string) => api<T>('GET', path);
export const apiPost = <T>(path: string, body: unknown = {}) => api<T>('POST', path, body);
export const apiDelete = <T>(path: string) => api<T>('DELETE', path);
export const apiPatch = <T>(path: string, body: unknown) => api<T>('PATCH', path, body);

export function errorMessage(e: unknown): string {
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

/** Passkey options as returned by the server (`{ publicKey: … }`). */
export interface PasskeyBegin {
	ceremony: string;
	options: { publicKey: Record<string, unknown> };
}

export const formatDate = (s: string | null) => (s ? new Date(s).toLocaleString('de-DE') : '–');
