// Sending files: small ones in one request, larger ones in parts of 8 MiB. The server confirms a
// part only once it is on disk; after a network error the upload continues where it stopped
// (automatically a few times, then with "Fortsetzen").

export const PART_SIZE = 8 * 1024 * 1024;

export type UploadTarget =
	| { kind: 'new'; parent_id: number; name: string; keep_both?: boolean }
	| { kind: 'replace'; node_id: number; base_rev: number };

interface UploadInfo {
	id: string;
	size: number;
	received: [number, number][];
	state: 'open' | 'committing' | 'committed';
}

/** Remembers the server's upload, so a later attempt continues it instead of starting over. */
export interface Resumable {
	uploadId?: string;
}

/** Ranges of `[0, size)` not yet covered by `received` (sorted, non-overlapping). */
export function gaps(received: [number, number][], size: number): [number, number][] {
	const out: [number, number][] = [];
	let at = 0;
	for (const [start, end] of received) {
		if (start > at) out.push([at, start]);
		at = Math.max(at, end);
	}
	if (at < size) out.push([at, size]);
	return out;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Network trouble and server overload are worth another try; refusals are not. */
const transient = (e: unknown) => e instanceof ApiError && (e.status === 0 || e.status === 429 || e.status >= 500);

async function retrying<T>(fn: () => Promise<T>): Promise<T> {
	for (let attempt = 0; ; attempt++) {
		try {
			return await fn();
		} catch (e) {
			if (!transient(e) || attempt >= 5) throw e;
			await sleep(1000 * 2 ** attempt);
		}
	}
}

function hex(buf: ArrayBuffer): string {
	return Array.from(new Uint8Array(buf), (b) => b.toString(16).padStart(2, '0')).join('');
}

function sendPart(id: string, offset: number, part: Blob, onProgress: (loaded: number) => void): Promise<void> {
	return part.arrayBuffer().then(async (buf) => {
		// SHA-256 needs a secure context (https or localhost); without it TLS alone protects the part.
		const sum = globalThis.crypto?.subtle ? hex(await crypto.subtle.digest('SHA-256', buf)) : null;
		await new Promise<void>((resolve, reject) => {
			const xhr = new XMLHttpRequest();
			xhr.open('PUT', `/api/uploads/${id}/parts?offset=${offset}`);
			xhr.withCredentials = true;
			xhr.setRequestHeader('content-type', 'application/octet-stream');
			if (sum) xhr.setRequestHeader('x-content-sha256', sum);
			xhr.upload.onprogress = (e) => onProgress(e.loaded);
			xhr.onload = () => {
				if (xhr.status >= 200 && xhr.status < 300) return resolve();
				let msg = `Fehler ${xhr.status}`;
				try {
					msg = JSON.parse(xhr.responseText).error ?? msg;
				} catch {
					// not JSON
				}
				reject(new ApiError(xhr.status, msg));
			};
			xhr.onerror = () => reject(new ApiError(0, 'Verbindung unterbrochen.'));
			xhr.send(buf);
		});
	});
}

async function inParts(file: File, target: UploadTarget, onProgress: (loaded: number) => void, state: Resumable): Promise<NodeInfo> {
	let info: UploadInfo | null = null;
	if (state.uploadId) {
		try {
			info = await retrying(() => apiGet<UploadInfo>(`/uploads/${state.uploadId}`));
		} catch (e) {
			// Expired or given up meanwhile: start over.
			if (!(e instanceof ApiError && e.status === 404)) throw e;
		}
	}
	if (!info) {
		info = await apiPost<UploadInfo>('/uploads', { size: file.size, target, mtime_ms: file.lastModified });
		state.uploadId = info.id;
	}
	const id = info.id;
	if (info.state === 'open') {
		const missing = gaps(info.received, file.size);
		let done = file.size - missing.reduce((n, [s, e]) => n + e - s, 0);
		onProgress(done);
		for (const [start, end] of missing) {
			for (let offset = start; offset < end; offset += PART_SIZE) {
				const part = file.slice(offset, Math.min(offset + PART_SIZE, end));
				await retrying(() => sendPart(id, offset, part, (n) => onProgress(done + n)));
				done += part.size;
				onProgress(done);
			}
		}
	}
	const node = await retrying(() => apiPost<NodeInfo>(`/uploads/${id}/commit`, {}));
	state.uploadId = undefined;
	return node;
}

/** Sends `file` to `target`; larger files in parts, resumable through `state`. */
export function uploadFile(
	file: File,
	target: UploadTarget,
	onProgress: (loaded: number) => void = () => {},
	state: Resumable = {}
): Promise<NodeInfo> {
	if (file.size > PART_SIZE || state.uploadId) return inParts(file, target, onProgress, state);
	if (target.kind === 'replace') {
		return sendFile('PUT', `/nodes/${target.node_id}/content?${uploadQuery(file, { base_rev: String(target.base_rev) })}`, file, onProgress);
	}
	const extra: Record<string, string> = { name: target.name, ...(target.keep_both ? { keep_both: 'true' } : {}) };
	return sendFile('POST', `/nodes/${target.parent_id}/files?${uploadQuery(file, extra)}`, file, onProgress);
}
