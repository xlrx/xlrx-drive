<script setup lang="ts">
// A public link (PLAN 9.2), for people without an account: the password first if there is one,
// then the file (preview, download) or the folder (browse, download, add files). A file request
// ("Nur hochladen") shows only where to drop files and what was sent in this visit.
definePageMeta({ layout: false });
useHead({ title: 'Geteilt über xlrx', meta: [{ name: 'robots', content: 'noindex, nofollow' }] });

const route = useRoute();
const router = useRouter();
const token = computed(() => String(route.params.token));
const base = computed(() => `/api/public/${token.value}`);
/** Folder or file shown, below the link's item (`?n=`). */
const at = computed(() => (route.query.n ? Number(route.query.n) : null));

const info = ref<PublicInfo | null>(null);
const node = ref<PublicNode | null>(null);
const children = ref<NodeInfo[]>([]);
const error = ref('');
const notice = ref('');
const password = ref('');
const busy = ref(false);
const picker = ref<HTMLInputElement | null>(null);
const dragging = ref(false);
/** Sent in this visit (file requests show nothing else). */
const sent = ref<string[]>([]);
const progress = ref<{ name: string; loaded: number; total: number } | null>(null);

async function load() {
	error.value = '';
	try {
		info.value = await apiGet<PublicInfo>(`/public/${token.value}`);
		if (info.value.locked || !info.value.node || !info.value.can.browse) {
			node.value = null;
			return;
		}
		const id = at.value ?? info.value.node.id;
		const n = await apiGet<PublicNode>(`/public/${token.value}/nodes/${id}`);
		children.value = n.kind === 'dir' ? await apiGet<NodeInfo[]>(`/public/${token.value}/nodes/${n.id}/children`) : [];
		node.value = n;
	} catch (e) {
		info.value = e instanceof ApiError && e.status === 401 ? info.value : null;
		node.value = null;
		error.value =
			e instanceof ApiError && e.status === 404
				? 'Diesen Link gibt es nicht (mehr). Vielleicht wurde er beendet – frag bei der Person nach, die ihn geschickt hat.'
				: errorMessage(e);
	}
}
watch([token, at], load, { immediate: true });

async function unlock() {
	busy.value = true;
	error.value = '';
	try {
		await apiPost(`/public/${token.value}/unlock`, { password: password.value });
		password.value = '';
		await load();
	} catch (e) {
		error.value = errorMessage(e);
	} finally {
		busy.value = false;
	}
}

const open = (id: number) => router.push({ query: id === info.value?.node?.id ? {} : { n: id } });
const collator = new Intl.Collator('de', { numeric: true, sensitivity: 'base' });
const sorted = computed(() =>
	[...children.value].sort((a, b) => (a.kind === b.kind ? collator.compare(a.name, b.name) : a.kind === 'dir' ? -1 : 1))
);
const meta = (c: NodeInfo) =>
	c.kind === 'file' ? [formatShortDate(c.mtime), formatSize(c.size)].filter(Boolean).join(' · ') : 'Ordner';
/** Where files go: the folder shown, or the link's folder for file requests. */
const target = computed(() => (node.value?.kind === 'dir' ? node.value.id : info.value?.can.browse ? null : info.value?.node?.id ?? null));
const canAdd = computed(() => !!info.value?.can.upload && target.value !== null);

async function send(files: File[]) {
	if (!canAdd.value || target.value === null || !info.value) return;
	notice.value = '';
	error.value = '';
	for (const file of files) {
		if (file.size > info.value.max_upload) {
			error.value = `„${file.name}“ ist zu groß (höchstens ${formatSize(info.value.max_upload)} über einen Link).`;
			continue;
		}
		progress.value = { name: file.name, loaded: 0, total: file.size };
		try {
			const q = new URLSearchParams({ name: file.name, size: String(file.size), mtime_ms: String(file.lastModified) });
			await sendFile('POST', `/public/${token.value}/nodes/${target.value}/files?${q}`, file, (loaded) => {
				if (progress.value) progress.value.loaded = loaded;
			});
			sent.value.push(file.name);
		} catch (e) {
			error.value = `„${file.name}“: ${errorMessage(e)}`;
		}
	}
	progress.value = null;
	if (sent.value.length) notice.value = sent.value.length === 1 ? '1 Datei gesendet.' : `${sent.value.length} Dateien gesendet.`;
	if (info.value.can.browse) await load();
}
function picked(e: Event) {
	const input = e.target as HTMLInputElement;
	send(Array.from(input.files ?? []));
	input.value = '';
}
function dropped(e: DragEvent) {
	dragging.value = false;
	send(Array.from(e.dataTransfer?.files ?? []));
}
const percent = computed(() => (progress.value?.total ? Math.round((progress.value.loaded / progress.value.total) * 100) : 0));
const until = computed(() =>
	info.value?.expires_at ? new Date(info.value.expires_at).toLocaleDateString('de-DE', { day: 'numeric', month: 'long', year: 'numeric' }) : ''
);
</script>

<template>
	<main class="page public" @dragover.prevent="dragging = canAdd" @dragleave.self="dragging = false" @drop.prevent="dropped">
		<p class="brand-line"><span class="brand-mark" aria-hidden="true"></span>xlrx</p>

		<!-- Password -->
		<div v-if="info?.locked" class="card narrow-card">
			<h1>Geschützter Link</h1>
			<p class="muted">Bitte das Passwort eingeben, das du mit dem Link bekommen hast.</p>
			<form @submit.prevent="unlock">
				<label for="link-pw">Passwort</label>
				<input id="link-pw" v-model="password" type="password" autocomplete="off" required />
				<button class="primary full" :disabled="busy">Öffnen</button>
			</form>
			<p v-if="error" class="error" role="alert">{{ error }}</p>
		</div>

		<!-- File request -->
		<template v-else-if="info && info.node && !info.can.browse">
			<h1>Dateien an {{ info.owner }} senden</h1>
			<p class="muted">Für den Ordner „{{ info.node.name }}“. Was du sendest, sieht nur {{ info.owner }} – du siehst nicht, was schon darin liegt.</p>
		</template>

		<!-- Folder or file -->
		<template v-else-if="info && node">
			<nav v-if="node.path.length > 1" class="crumbs" aria-label="Pfad">
				<template v-for="(c, i) in node.path.slice(0, -1)" :key="c.id">
					<Icon v-if="i" name="chevR" :size="14" :stroke="1.8" />
					<a href="#" @click.prevent="open(c.id)">{{ c.name }}</a>
				</template>
			</nav>
			<h1>{{ node.name }}</h1>
			<p class="meta">
				Geteilt von {{ info.owner }}<template v-if="until"> · bis {{ until }}</template>
				<template v-if="node.kind === 'file'"> · {{ formatSize(node.size) }}</template>
				<template v-if="info.downloads_left !== null"> · noch {{ info.downloads_left }} {{ info.downloads_left === 1 ? 'Download' : 'Downloads' }}</template>
			</p>
		</template>

		<p v-if="error && !info?.locked" class="error" role="alert">{{ error }}</p>
		<p v-if="notice" class="notice" role="status"><Icon name="check" :size="16" :stroke="2" />{{ notice }}</p>

		<template v-if="info && !info.locked">
			<!-- Adding files -->
			<section v-if="canAdd" class="drop-zone" :class="{ dragging }">
				<Icon name="upload" :size="26" :stroke="1.4" />
				<p>{{ dragging ? 'Loslassen zum Senden' : 'Dateien hierher ziehen oder' }}</p>
				<button class="primary" type="button" :disabled="!!progress" @click="picker?.click()">Dateien auswählen</button>
				<input ref="picker" type="file" multiple hidden data-testid="link-upload" @change="picked" />
				<p v-if="progress" class="muted small" role="status">„{{ progress.name }}“ – {{ percent }} %</p>
				<ul v-if="sent.length && !info.can.browse" class="sent">
					<li v-for="(s, i) in sent" :key="i"><Icon name="check" :size="15" :stroke="2" />{{ s }}</li>
				</ul>
			</section>

			<!-- Folder -->
			<ul v-if="node?.kind === 'dir' && sorted.length" class="rows">
				<li v-for="c in sorted" :key="c.id">
					<a href="#" class="open" :aria-describedby="`meta-${c.id}`" @click.prevent="open(c.id)">
						<FileMark :node="c" :base="base" />
						<span class="text-col">
							<span class="name">{{ c.name }}</span>
							<span :id="`meta-${c.id}`" class="sub" aria-hidden="true">{{ meta(c) }}</span>
						</span>
					</a>
					<a v-if="c.kind === 'file' && info.can.download" class="icon" :href="contentUrl(c.id, false, base)" download :aria-label="`${c.name} herunterladen`">
						<Icon name="download" :size="18" />
					</a>
				</li>
			</ul>
			<p v-else-if="node?.kind === 'dir'" class="muted empty">Dieser Ordner ist leer.</p>

			<!-- File -->
			<template v-else-if="node">
				<div v-if="info.can.download" class="row file-actions">
					<a class="button primary" :href="contentUrl(node.id, false, base)" download><Icon name="download" :size="18" />Herunterladen</a>
				</div>
				<FilePreview :node="node" :base="base" />
			</template>
		</template>
		<p v-else-if="!info && !error" class="muted">Lädt …</p>
	</main>
</template>

<style scoped>
.public { max-width: 56rem; margin: 0 auto; padding-top: 24px; }
.brand-line { margin: 0 0 24px; }
.narrow-card { max-width: 26rem; }
.crumbs { display: flex; flex-wrap: wrap; align-items: center; gap: 2px; font-size: 13px; color: var(--muted); margin: 0 0 6px -4px; }
.crumbs a { padding: 2px 4px; }
.meta { margin: 0 0 16px; font-size: 13px; color: var(--muted); }
.notice { display: flex; align-items: center; gap: 8px; margin: 12px 0; font-size: 14px; }
.drop-zone {
	display: flex; flex-direction: column; align-items: center; gap: 10px; padding: 28px 16px; margin: 8px 0 20px;
	border: 1.5px dashed var(--dash); border-radius: 14px; text-align: center; color: var(--ink-3);
}
.drop-zone p { margin: 0; }
.drop-zone.dragging { border-color: var(--accent); color: var(--accent); }
.sent { list-style: none; margin: 6px 0 0; padding: 0; font-size: 14px; color: var(--ink); }
.sent li { display: flex; align-items: center; gap: 6px; }
.rows li { gap: 0; }
.open { flex: 1; min-width: 0; min-height: 58px; display: flex; align-items: center; gap: 14px; padding-left: 4px; text-decoration: none; color: inherit; }
.text-col { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 3px; }
.name { font-size: 15px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.sub { font-size: 12px; color: var(--muted); }
.rows .icon { width: 40px; height: 40px; display: inline-flex; align-items: center; justify-content: center; color: var(--ink-3); }
.empty { padding: 1.5rem 0; }
.file-actions { margin: 0 0 18px; }
.small { font-size: 13px; }
</style>
