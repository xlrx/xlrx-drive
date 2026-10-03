// Search: types of the search API, labels of the kinds, recent searches (only in this browser).

/** A piece of a snippet or name; `hit` marks what matched. Always shown as text, never as HTML. */
export interface Part {
	text: string;
	hit: boolean;
}

export interface SearchHit extends NodeInfo {
	/** The folder it is in, e.g. "Meine Ablage/Belege". */
	folder: string;
	snippet: Part[] | null;
}

export interface SearchResult {
	total: number;
	hits: SearchHit[];
	facets: { typ: string; n: number }[];
	/** Nothing matched exactly; these are similar spellings. */
	fuzzy: boolean;
	/** Changes the index has not taken in yet. */
	pending: number;
}

export interface Suggestion extends NodeInfo {
	folder: string;
	parts: Part[];
}

/** Kinds in the order of the chips, with their labels (keys as in `typ:`). */
export const KINDS: { typ: string; label: string }[] = [
	{ typ: 'ordner', label: 'Ordner' },
	{ typ: 'pdf', label: 'PDF' },
	{ typ: 'dokument', label: 'Dokumente' },
	{ typ: 'tabelle', label: 'Tabellen' },
	{ typ: 'praesentation', label: 'Präsentationen' },
	{ typ: 'bild', label: 'Bilder' },
	{ typ: 'video', label: 'Videos' },
	{ typ: 'audio', label: 'Audio' },
	{ typ: 'mail', label: 'Mails' },
	{ typ: 'text', label: 'Texte' },
	{ typ: 'archiv', label: 'Archive' },
	{ typ: 'sonstiges', label: 'Sonstiges' }
];

export const kindLabel = (typ: string) => KINDS.find((k) => k.typ === typ)?.label ?? typ;

/** Building blocks of the search syntax, offered as chips while typing. */
export const OPERATORS: { key: string; example: string; hint: string }[] = [
	{ key: 'typ:', example: 'pdf', hint: 'Dateityp, z. B. pdf, bild, tabelle' },
	{ key: 'in:', example: 'Ordner', hint: 'Nur in Ordnern dieses Namens' },
	{ key: 'nach:', example: '2025-01-01', hint: 'Geändert ab diesem Tag (oder Monat, Jahr)' },
	{ key: 'vor:', example: '2026', hint: 'Geändert vor diesem Tag (oder Monat, Jahr)' },
	{ key: '"', example: 'genau so"', hint: 'Genau diese Wortfolge' },
	{ key: '-', example: 'ohne', hint: 'Treffer mit diesem Wort weglassen' }
];

/** "Meine Ablage/Haus/Rechnungen" → "Meine Ablage › Haus › Rechnungen". */
export const folderTrail = (folder: string) => folder.split('/').join(' › ');

const RECENT_KEY = 'xlrx.search.recent';
const RECENT_MAX = 6;

/** Searches made in this browser, newest first (never sent anywhere). */
export function recentSearches(): string[] {
	try {
		const v = JSON.parse(localStorage.getItem(RECENT_KEY) ?? '[]');
		return Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string').slice(0, RECENT_MAX) : [];
	} catch {
		return [];
	}
}

export function rememberSearch(q: string): string[] {
	const t = q.trim();
	const list = [t, ...recentSearches().filter((x) => x !== t)].filter(Boolean).slice(0, RECENT_MAX);
	try {
		localStorage.setItem(RECENT_KEY, JSON.stringify(list));
	} catch {
		// Storage blocked (private window): just not remembered.
	}
	return list;
}

export function forgetSearches(): void {
	try {
		localStorage.removeItem(RECENT_KEY);
	} catch {
		// nothing to forget
	}
}

/** "Aug. 2025" for the tiles of pictures. */
export function monthYear(s: string | null): string {
	if (!s) return '';
	return new Intl.DateTimeFormat('de-DE', { month: 'short', year: 'numeric' }).format(new Date(s));
}
