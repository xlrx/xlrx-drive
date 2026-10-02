// Upload queue of the file views: one file at a time, with progress and a question when a name
// is taken (replace keeps the old content as a version). Large files go in parts; one that failed
// can be continued where it stopped.

export interface UploadItem {
	key: number;
	name: string;
	size: number;
	loaded: number;
	state: 'waiting' | 'asking' | 'uploading' | 'done' | 'skipped' | 'error';
	error?: string;
	/** The server keeps what arrived: "Fortsetzen" sends only the rest. */
	resumable?: boolean;
}

interface Entry {
	file: File;
	parentId: number;
	item: UploadItem;
	/** Decided when the name was taken (kept for continuing). */
	target?: UploadTarget;
	parts: Resumable;
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
	const queue: Entry[] = [];
	const failed = new Map<number, Entry>();
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
			queue.push({ file, parentId, item, parts: {} });
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
			const entry = queue.shift()!;
			const { item } = entry;
			item.state = 'uploading';
			item.error = undefined;
			item.resumable = false;
			try {
				await uploadOne(entry);
			} catch (e) {
				item.state = 'error';
				item.error = errorMessage(e);
				item.resumable = !!entry.parts.uploadId;
				failed.set(item.key, entry);
			}
			onChange();
		}
		running = false;
		remembered = null;
	}

	async function uploadOne(entry: Entry) {
		const { file, parentId, item } = entry;
		const progress = (n: number) => (item.loaded = n);
		if (entry.target) {
			// Continuing: the decision about the name was made already.
			await uploadFile(file, entry.target, progress, entry.parts);
			item.loaded = file.size;
			item.state = 'done';
			return;
		}
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
		entry.target =
			choice === 'replace' && existing
				? { kind: 'replace', node_id: existing.id, base_rev: existing.rev }
				: { kind: 'new', parent_id: parentId, name: file.name, keep_both: choice === 'keep_both' };
		await uploadFile(file, entry.target, progress, entry.parts);
		item.loaded = file.size;
		item.state = 'done';
	}

	const active = computed(() => items.value.some((i) => ['waiting', 'asking', 'uploading'].includes(i.state)));

	function clear() {
		items.value = items.value.filter((i) => ['waiting', 'asking', 'uploading'].includes(i.state));
		for (const key of failed.keys()) if (!items.value.some((i) => i.key === key)) failed.delete(key);
	}

	/** Tries a failed upload again; with parts on the server, only the rest is sent. */
	function resume(key: number) {
		const entry = failed.get(key);
		if (!entry) return;
		failed.delete(key);
		entry.item.state = 'waiting';
		queue.push(entry);
		if (!running) void run();
	}

	return { items, question, active, add, clear, resume };
}
