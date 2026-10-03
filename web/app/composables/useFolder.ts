// The folder shown right now (target of "Neu"); null outside a folder.

export function useFolder() {
	return useState<{ id: number; name: string } | null>('current-folder', () => null);
}
