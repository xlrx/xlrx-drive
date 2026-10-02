// Upload queue of the file views: one file at a time, with progress and a question when a name
// is taken (replace keeps the old content as a version).

export interface UploadItem {
	key: number;
	name: string;
	size: number;
	loaded: number;
	state: 'waiting' | 'asking' | 'uploading' | 'done' | 'skipped' | 'error';
	error?: string;
}

interface Question {
	name: string;
	canReplace: boolean;
	more: boolean;
	answer: (choice: ConflictChoice, forAll: boolean) => void;
}

export function useUploads(onChange: () => void) {
	const items = ref<UploadItem[]>([]);
	const question = shallowRef<Question | null>(null);
	const queue: { file: File; parentId: number; item: UploadItem }[] = [];
	let running = false;
	let remembered: ConflictChoice | null = null;
	let next = 0;

	function add(files: Iterable<File>, parentId: number) {
		for (const file of files) {
			const item = reactive<UploadItem>({
				key: next++,
				name: file.name,
				size: file.size,
				loaded: 0,
				state: 'waiting'
			});
			items.value.push(item);
			queue.push({ file, parentId, item });
		}
		if (!running) void run();
	}

	function ask(name: string, canReplace: boolean, more: boolean) {
		return new Promise<[ConflictChoice, boolean]>((resolve) => {
			question.value = {
				name,
				canReplace,
				more,
				answer: (choice, forAll) => {
					question.value = null;
					resolve([choice, forAll]);
				}
			};
		});
	}

	async function run() {
		running = true;
		while (queue.length) {
			const { file, parentId, item } = queue.shift()!;
			item.state = 'uploading';
			try {
				await uploadOne(file, parentId, item);
			} catch (e) {
				item.state = 'error';
				item.error = errorMessage(e);
			}
			onChange();
		}
		running = false;
		remembered = null;
	}

	async function uploadOne(file: File, parentId: number, item: UploadItem) {
		const progress = (n: number) => (item.loaded = n);
		// Check the name first, so a large file is not sent only to be refused.
		const children = await apiGet<NodeInfo[]>(`/nodes/${parentId}/children`);
		const existing = children.find((c) => sameName(c.name, file.name));
		let choice: ConflictChoice | null = null;
		if (existing) {
			const canReplace = existing.kind === 'file';
			choice = remembered === 'replace' && !canReplace ? null : remembered;
			if (!choice) {
				item.state = 'asking';
				const [c, forAll] = await ask(file.name, canReplace, queue.length > 0);
				item.state = 'uploading';
				choice = c;
				if (forAll) remembered = c;
			}
		}
		if (choice === 'skip') {
			item.state = 'skipped';
			return;
		}
		if (choice === 'replace' && existing) {
			const q = uploadQuery(file, { base_rev: String(existing.rev) });
			await sendFile('PUT', `/nodes/${existing.id}/content?${q}`, file, progress);
		} else {
			const q = uploadQuery(file, { name: file.name, ...(choice === 'keep_both' ? { keep_both: 'true' } : {}) });
			await sendFile('POST', `/nodes/${parentId}/files?${q}`, file, progress);
		}
		item.loaded = file.size;
		item.state = 'done';
	}

	const active = computed(() => items.value.some((i) => ['waiting', 'asking', 'uploading'].includes(i.state)));

	function clear() {
		items.value = items.value.filter((i) => ['waiting', 'asking', 'uploading'].includes(i.state));
	}

	return { items, question, active, add, clear };
}
