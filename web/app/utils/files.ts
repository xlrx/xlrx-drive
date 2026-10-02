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
	rev: number;
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
