// Files: types of the browse API and display helpers.

export interface RootInfo {
	id: number;
	kind: 'home' | 'space';
	name: string;
	node_id: number;
	scanned_at: string | null;
}

export interface NodeInfo {
	id: number;
	parent_id: number | null;
	name: string;
	kind: 'file' | 'dir';
	size: number | null;
	/** Content revision (base for replacing the content). */
	rev: number;
	/** Last change of any kind (precondition for rename, move, delete). */
	seq: number;
	mtime: string | null;
	mime: string | null;
}

export interface NodeDetail extends NodeInfo {
	/** From the root directory down to the node itself. */
	path: { id: number; name: string }[];
	root_id: number;
}

export interface ScanReport {
	created: number;
	updated: number;
	moved: number;
	deleted: number;
	hashed_bytes: number;
	skipped: string[];
}

const sizeFormat = new Intl.NumberFormat('de-DE', { maximumFractionDigits: 1 });

export function formatSize(bytes: number | null): string {
	if (bytes === null) return '';
	if (bytes < 1000) return `${bytes} B`;
	const units = ['KB', 'MB', 'GB', 'TB'];
	let v = bytes / 1000;
	let i = 0;
	while (v >= 1000 && i < units.length - 1) {
		v /= 1000;
		i++;
	}
	return `${sizeFormat.format(v)} ${units[i]}`;
}

export const contentUrl = (id: number, inline = false) =>
	`/api/nodes/${id}/content${inline ? '?inline=true' : ''}`;

export type PreviewKind = 'image' | 'video' | 'audio' | 'pdf' | 'text' | 'office' | null;

// Text formats shown as plain text (never interpreted).
const TEXT_TYPES = new Set([
	'application/json',
	'application/xml',
	'application/javascript',
	'application/x-sh',
	'application/toml',
	'application/x-yaml',
	'application/yaml',
	'application/sql'
]);

/** What kind of preview a file gets in the browser. */
export function previewKind(mime: string | null): PreviewKind {
	if (!mime) return null;
	if (mime === 'image/svg+xml') return null; // could contain scripts: download only
	if (mime.startsWith('image/')) return 'image';
	if (mime.startsWith('video/')) return 'video';
	if (mime.startsWith('audio/')) return 'audio';
	if (mime === 'application/pdf') return 'pdf';
	if (mime.startsWith('text/') && mime !== 'text/html') return 'text';
	if (TEXT_TYPES.has(mime)) return 'text';
	if (
		mime.startsWith('application/vnd.openxmlformats-officedocument') ||
		mime.startsWith('application/vnd.ms-') ||
		mime === 'application/msword' ||
		mime.startsWith('application/vnd.oasis.opendocument')
	)
		return 'office';
	return null;
}

/** Types the server delivers for display in the browser (mirrors `inline_allowed` there). */
export function opensInBrowser(mime: string | null): boolean {
	if (!mime) return false;
	return (
		(mime.startsWith('image/') && mime !== 'image/svg+xml') ||
		mime.startsWith('video/') ||
		mime.startsWith('audio/') ||
		mime === 'application/pdf' ||
		mime === 'text/plain'
	);
}

/** Icon for a node (see FileIcon). */
export function iconKind(node: Pick<NodeInfo, 'kind' | 'mime'>): string {
	if (node.kind === 'dir') return 'folder';
	const p = previewKind(node.mime);
	return p === 'text' ? 'doc' : (p ?? 'file');
}

const timeFormat = new Intl.DateTimeFormat('de-DE', { hour: '2-digit', minute: '2-digit' });
const dayFormat = new Intl.DateTimeFormat('de-DE', { day: 'numeric', month: 'short', year: 'numeric' });

/** Today: only the time; otherwise the day. */
export function formatShortDate(s: string | null): string {
	if (!s) return '';
	const d = new Date(s);
	return d.toDateString() === new Date().toDateString() ? timeFormat.format(d) : dayFormat.format(d);
}

/** "Eingelesen: 2 neu, 1 geändert" – or that nothing changed. */
export function describeScan(r: ScanReport): string {
	const parts = [
		r.created && `${r.created} neu`,
		r.updated && `${r.updated} geändert`,
		r.moved && `${r.moved} verschoben`,
		r.deleted && `${r.deleted} gelöscht`
	].filter(Boolean);
	const skipped = r.skipped.length ? ` (${r.skipped.length} übersprungen)` : '';
	return parts.length ? `Eingelesen: ${parts.join(', ')}${skipped}.` : `Keine Änderungen${skipped}.`;
}

export interface VersionInfo {
	id: number;
	rev: number;
	size: number;
	mtime: string | null;
	created_at: string;
	created_by: string | null;
}

export interface TrashItem {
	id: number;
	name: string;
	kind: 'file' | 'dir';
	size: number | null;
	deleted_at: string;
	/** Folder it was deleted from, e.g. "Meine Ablage/Projekte". */
	from: string;
}

/**
 * Sends a file as the raw request body, with progress (fetch cannot report upload progress).
 * `path` is below /api, e.g. `/nodes/12/files?name=…`.
 */
export function sendFile(
	method: 'POST' | 'PUT',
	path: string,
	file: Blob,
	onProgress?: (loaded: number) => void
): Promise<NodeInfo> {
	return new Promise((resolve, reject) => {
		const xhr = new XMLHttpRequest();
		xhr.open(method, `/api${path}`);
		xhr.withCredentials = true;
		xhr.setRequestHeader('content-type', 'application/octet-stream');
		if (onProgress) xhr.upload.onprogress = (e) => onProgress(e.loaded);
		xhr.onload = () => {
			let data: unknown = null;
			try {
				data = JSON.parse(xhr.responseText);
			} catch {
				// not JSON
			}
			if (xhr.status >= 200 && xhr.status < 300) resolve(data as NodeInfo);
			else
				reject(
					new ApiError(
						xhr.status,
						(data as { error?: string } | null)?.error ?? `Fehler ${xhr.status}`
					)
				);
		};
		xhr.onerror = () => reject(new ApiError(0, 'Verbindung unterbrochen.'));
		xhr.send(file);
	});
}

/** Query string for an upload of `file` (name, size, modification time). */
export function uploadQuery(file: File, extra: Record<string, string> = {}): string {
	const q = new URLSearchParams({
		size: String(file.size),
		mtime_ms: String(file.lastModified),
		...extra
	});
	return q.toString();
}

/** Same name for Macs and SMB (case and Unicode normalization ignored). */
export const sameName = (a: string, b: string) =>
	a.normalize('NFC').toLowerCase() === b.normalize('NFC').toLowerCase();

/** Answer to "a file of that name exists already". */
export type ConflictChoice = 'replace' | 'keep_both' | 'skip';
