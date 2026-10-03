// Look chosen in this browser: paper tone, ink, landscape on the start page.

export type Paper = 'warm' | 'sand' | 'hell';
export type Ink = 'schwarz' | 'blau' | 'rot' | 'gruen';
export interface Appearance {
	paper: Paper;
	ink: Ink;
	landscape: boolean;
}

export const PAPERS: { id: Paper; label: string; color: string }[] = [
	{ id: 'warm', label: 'Warm', color: '#F3EFE7' },
	{ id: 'sand', label: 'Sand', color: '#E6DFD4' },
	{ id: 'hell', label: 'Hell', color: '#F7F6F2' }
];
export const INKS: { id: Ink; label: string; color: string }[] = [
	{ id: 'schwarz', label: 'Schwarz', color: '#1B1A17' },
	{ id: 'blau', label: 'Blau', color: '#2C3E5C' },
	{ id: 'rot', label: 'Rot', color: '#6B2A1F' },
	{ id: 'gruen', label: 'Grün', color: '#3D4A2F' }
];

const KEY = 'xlrx-appearance';
const DEFAULT: Appearance = { paper: 'warm', ink: 'schwarz', landscape: true };

export function readAppearance(): Appearance {
	try {
		const saved = JSON.parse(localStorage.getItem(KEY) ?? '{}') as Partial<Appearance>;
		return {
			paper: PAPERS.some((p) => p.id === saved.paper) ? saved.paper! : DEFAULT.paper,
			ink: INKS.some((i) => i.id === saved.ink) ? saved.ink! : DEFAULT.ink,
			landscape: typeof saved.landscape === 'boolean' ? saved.landscape : DEFAULT.landscape
		};
	} catch {
		return { ...DEFAULT };
	}
}

export function applyAppearance(a: Appearance) {
	const root = document.documentElement;
	if (a.paper === 'warm') delete root.dataset.paper;
	else root.dataset.paper = a.paper;
	if (a.ink === 'schwarz') delete root.dataset.ink;
	else root.dataset.ink = a.ink;
}

export function saveAppearance(a: Appearance) {
	try {
		localStorage.setItem(KEY, JSON.stringify(a));
	} catch {
		// Storage blocked: the choice lasts until the page is reloaded.
	}
	applyAppearance(a);
}
