// Files: types of the browse API and display helpers.

export type Role = 'viewer' | 'editor' | 'manager' | 'owner';

/** "Ansehen", "Bearbeiten", … as in the design. */
export const ROLE_LABEL: Record<Role, string> = {
	viewer: 'Ansehen',
	editor: 'Bearbeiten',
	manager: 'Verwalten',
	owner: 'Besitzer'
};

/** May change things here (upload, rename, move, delete inside). */
export const canEdit = (r: Role | undefined) => r === 'editor' || r === 'manager' || r === 'owner';

export interface RootInfo {
	id: number;
	kind: 'home' | 'space';
	name: string;
	node_id: number;
	/** My role over the whole root. */
	role: Role;
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

/** An entry of a folder listing. */
export interface ChildInfo extends NodeInfo {
	/** A data class set on this folder itself. */
	data_class?: DataClass;
}

/** "Nur lokal" or "Cloud erlaubt" (PLAN 7.4). */
export type DataClass = 'local' | 'cloud';

export interface DataClassInfo {
	class: DataClass;
	/** Set on this folder itself. */
	explicit: boolean;
	/** Inherited from this folder (if I can see it). */
	from: { id: number; name: string } | null;
	/** Nothing set anywhere above: the server's default. */
	default: boolean;
	can_change: boolean;
}

export const CLASS_LABEL: Record<DataClass, string> = { local: 'Nur lokal', cloud: 'Cloud erlaubt' };

/** Where the data class of a node comes from, in words. */
export function classSource(d: DataClassInfo): string {
	if (d.explicit) return 'für diesen Ordner festgelegt';
	if (d.from) return `vererbt von „${d.from.name}“`;
	if (d.default) return 'Voreinstellung des Servers';
	return 'vererbt von einem Ordner darüber';
}

export interface NodeDetail extends NodeInfo {
	/** From the highest folder I can see (root directory, or the shared folder) down to the node. */
	path: { id: number; name: string }[];
	root_id: number;
	/** My role here. */
	role: Role;
	/** Seen through a share (not as owner or member of the whole root). */
	shared: boolean;
	data_class: DataClassInfo;
}

/** A person or group something is shared with. */
export interface Principal {
	type: 'user' | 'group';
	id: number;
	name: string;
}

export interface ShareInfo {
	id: number;
	to: Principal;
	role: Exclude<Role, 'owner'>;
	expires_at: string | null;
	expired: boolean;
	node_id: number;
	node_name: string;
	/** On a folder above (inherited). */
	inherited: boolean;
	created_by: string | null;
}

export interface AccessInfo {
	role: Role;
	can_share: boolean;
	owner: string | null;
	space: string | null;
	members: { to: Principal; role: Exclude<Role, 'owner'> }[];
	shares: ShareInfo[];
}

export interface People {
	users: { id: number; name: string; username: string }[];
	groups: { id: number; name: string; members: number }[];
}

export interface SharedItem extends NodeInfo {
	role: Role;
	/** Whose it is: the owner, or the shared root. */
	owner: string;
	shared_by: string | null;
	shared_at: string;
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

/** `base`: `/api` when signed in, `/api/public/{token}` through a public link. */
export const contentUrl = (id: number, inline = false, base = '/api') =>
	`${base}/nodes/${id}/content${inline ? '?inline=true' : ''}`;

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

/** Types the server makes thumbnails of (same list as `thumbs::supported`). */
const THUMB_TYPES = new Set(['image/jpeg', 'image/png', 'image/gif', 'image/webp', 'image/bmp', 'image/tiff']);

/** Thumbnail address; with the revision, so the browser may keep it until the content changes. */
export function thumbUrl(
	node: Pick<NodeInfo, 'id' | 'kind' | 'mime' | 'rev'>,
	size: 64 | 256 | 1024,
	base = '/api'
): string | null {
	return node.kind === 'file' && node.mime && THUMB_TYPES.has(node.mime)
		? `${base}/nodes/${node.id}/thumbnail?s=${size}&v=${node.rev}`
		: null;
}

const timeFormat = new Intl.DateTimeFormat('de-DE', { hour: '2-digit', minute: '2-digit' });
const dayFormat = new Intl.DateTimeFormat('de-DE', { day: 'numeric', month: 'short', year: 'numeric' });

/** "gerade eben", "vor 12 Min.", "vor 2 Std.", "gestern", otherwise the day. */
export function formatAgo(s: string | null, now = Date.now()): string {
	if (!s) return '';
	const d = new Date(s);
	const min = Math.round((now - d.getTime()) / 60_000);
	if (min < 1) return 'gerade eben';
	if (min < 60) return `vor ${min} Min.`;
	if (min < 12 * 60) return `vor ${Math.round(min / 60)} Std.`;
	const yesterday = new Date(now - 86_400_000);
	if (d.toDateString() === new Date(now).toDateString()) return `heute, ${timeFormat.format(d)}`;
	if (d.toDateString() === yesterday.toDateString()) return 'gestern';
	return dayFormat.format(d);
}

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

/** Public links (PLAN 9.2). */
export type LinkKind = 'view' | 'download' | 'upload' | 'edit';

export const LINK_KIND_LABEL: Record<LinkKind, string> = {
	view: 'Ansehen',
	download: 'Herunterladen',
	upload: 'Nur hochladen',
	edit: 'Bearbeiten'
};

export const LINK_KIND_HINT: Record<LinkKind, string> = {
	view: 'Wer den Link hat, kann ansehen – ohne Konto.',
	download: 'Wer den Link hat, kann ansehen und herunterladen – ohne Konto.',
	upload: 'Wer den Link hat, kann Dateien in diesen Ordner legen, sieht aber nicht, was darin ist.',
	edit: 'Wer den Link hat, kann herunterladen, Dateien hinzufügen und neue Fassungen hochladen. Löschen und Umbenennen geht nicht; alte Fassungen bleiben als Versionen.'
};

export interface LinkInfo {
	id: number;
	url: string | null;
	kind: LinkKind;
	password: boolean;
	expires_at: string | null;
	expired: boolean;
	max_downloads: number | null;
	downloads: number;
	created_by: string | null;
	created_at: string;
}

/** What a public link shows to someone without an account. */
export interface PublicInfo {
	kind: LinkKind;
	/** A password is needed first. */
	locked: boolean;
	node: NodeInfo | null;
	owner: string | null;
	expires_at: string | null;
	can: { browse: boolean; download: boolean; upload: boolean; replace: boolean };
	max_upload: number;
	downloads_left: number | null;
}

export interface PublicNode extends NodeInfo {
	/** From the link's item down to this one. */
	path: { id: number; name: string }[];
}

/** Activity (PLAN 8.3). */
export type ActivityKind =
	| 'created'
	| 'uploaded'
	| 'edited'
	| 'renamed'
	| 'moved'
	| 'deleted'
	| 'restored'
	| 'shared'
	| 'link_created'
	| 'link_download'
	| 'link_upload'
	| 'link_edit';

export interface ActivityItem extends NodeInfo {
	deleted: boolean;
	prev_name: string | null;
}

export interface ActivityGroup {
	kind: ActivityKind;
	actor: { id: number; name: string } | null;
	/** No person: found on the NAS, or someone through a public link. */
	via: 'nas' | 'link' | null;
	mine: boolean;
	at: string;
	since: string;
	count: number;
	items: ActivityItem[];
	folder: string | null;
	folder_id: number | null;
	details: { to?: string; role?: Role; kind?: LinkKind } | null;
}

export interface ActivityPage {
	groups: ActivityGroup[];
	next: string | null;
}

const ROLE_WORD: Record<string, string> = { viewer: 'zum Ansehen', editor: 'zum Bearbeiten', manager: 'zum Verwalten' };

/** The sentence of an activity entry, split around the item (which may become a link). */
export function activitySentence(g: ActivityGroup): { pre: string; obj: string; post: string } {
	const one = g.count === 1;
	const first = g.items[0];
	const allImages = g.items.length === g.count && g.items.every((i) => i.mime?.startsWith('image/'));
	const allDirs = g.items.length === g.count && g.items.every((i) => i.kind === 'dir');
	const allFiles = g.items.length === g.count && g.items.every((i) => i.kind === 'file');
	const noun = allImages ? 'Fotos' : allDirs ? 'Ordner' : allFiles ? 'Dateien' : 'Elemente';
	const obj = one && first ? `„${first.name}“` : `${g.count} ${noun}`;
	const where = g.folder ? g.folder.split('/').at(-1) : '';
	const verbs: Record<ActivityKind, string> = {
		created: 'angelegt',
		uploaded: 'hinzugefügt',
		edited: 'bearbeitet',
		renamed: 'umbenannt',
		moved: where ? `nach „${where}“ verschoben` : 'verschoben',
		deleted: g.via === 'nas' ? 'gelöscht' : 'in den Papierkorb gelegt',
		restored: 'wiederhergestellt',
		shared: g.details?.to ? `mit ${g.details.to} ${ROLE_WORD[g.details.role ?? ''] ?? ''} geteilt`.replace(/ +/g, ' ').trim() : 'geteilt',
		link_created: 'per Link freigegeben',
		link_download: 'heruntergeladen',
		link_upload: 'hinzugefügt',
		link_edit: 'geändert'
	};
	const verb = verbs[g.kind];
	if (g.kind === 'renamed' && one && first?.prev_name) {
		return {
			pre: g.via ? `„${first.prev_name}“ wurde auf dem NAS in ` : `${g.mine ? 'Du hast' : `${g.actor?.name ?? 'Jemand'} hat`} „${first.prev_name}“ in `,
			obj: `„${first.name}“`,
			post: ' umbenannt'
		};
	}
	const objOut = g.kind === 'created' && one ? `den Ordner ${obj}` : obj;
	if (g.via) {
		const how = g.via === 'nas' ? 'auf dem NAS' : 'über einen Link';
		return { pre: '', obj: objOut, post: ` ${one ? 'wurde' : 'wurden'} ${how} ${verb}` };
	}
	const who = g.mine ? 'Du hast' : `${g.actor?.name ?? 'Jemand'} hat`;
	return { pre: `${who} `, obj: objOut, post: ` ${verb}` };
}
