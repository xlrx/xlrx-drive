<script setup lang="ts">
// Search (as in the design): while typing, building blocks of the syntax, matching file names and
// recent searches; after Enter the results with counts per kind, pictures as tiles and text
// snippets with the matched words marked. The query lives in the address (?q=…&typ=…&folder=…),
// so Back and reloading keep it.
const route = useRoute();
const router = useRouter();

const text = ref(String(route.query.q ?? ''));
const input = ref<HTMLInputElement | null>(null);
const submitted = computed(() => String(route.query.q ?? ''));
const typ = computed(() => (route.query.typ ? String(route.query.typ) : ''));
const folderId = computed(() => (route.query.folder ? Number(route.query.folder) : null));
const editing = ref(!submitted.value);

// ---- While typing ----
const suggestions = ref<Suggestion[]>([]);
const active = ref(-1);
const recent = ref<string[]>([]);
let asked = 0;
let timer: ReturnType<typeof setTimeout> | undefined;

watch(text, (t) => {
	clearTimeout(timer);
	active.value = -1;
	if (t.trim().length < 2) {
		suggestions.value = [];
		return;
	}
	timer = setTimeout(async () => {
		const mine = ++asked;
		try {
			const s = await apiGet<Suggestion[]>(`/search/suggest?q=${encodeURIComponent(t)}&limit=6`);
			if (mine === asked) suggestions.value = s;
		} catch {
			if (mine === asked) suggestions.value = [];
		}
	}, 150);
});

const showSuggestions = computed(() => editing.value && suggestions.value.length > 0);

function edit() {
	editing.value = true;
}

function submit() {
	const q = text.value.trim();
	if (!q) return;
	recent.value = rememberSearch(q);
	editing.value = false;
	input.value?.blur();
	// The same search again (e.g. after new files arrived) asks the server again.
	if (q === submitted.value) load();
	else router.push({ path: '/search', query: { q, ...(folderId.value ? { folder: folderId.value } : {}) } });
}

function open(s: Suggestion) {
	if (text.value.trim()) recent.value = rememberSearch(text.value);
	router.push(`/files/${s.id}`);
}

function useRecent(q: string) {
	text.value = q;
	submit();
}

/** Puts a building block of the syntax into the field (after what is there). */
function insert(key: string) {
	const t = text.value.replace(/\s+$/, '');
	text.value = `${t}${t ? ' ' : ''}${key}`;
	nextTick(() => input.value?.focus());
}

function clearText() {
	text.value = '';
	editing.value = true;
	input.value?.focus();
}

function cancel() {
	if (submitted.value && editing.value) {
		text.value = submitted.value;
		editing.value = false;
		return;
	}
	if (window.history.length > 1) router.back();
	else router.push('/');
}

function onKey(e: KeyboardEvent) {
	const n = suggestions.value.length;
	if (e.key === 'ArrowDown' && n) {
		e.preventDefault();
		active.value = (active.value + 1) % n;
	} else if (e.key === 'ArrowUp' && n) {
		e.preventDefault();
		active.value = active.value <= 0 ? n - 1 : active.value - 1;
	} else if (e.key === 'Enter') {
		e.preventDefault();
		const s = suggestions.value[active.value];
		if (showSuggestions.value && s) open(s);
		else submit();
	} else if (e.key === 'Escape' && submitted.value) {
		cancel();
	}
}

function forget() {
	forgetSearches();
	recent.value = [];
}

// ---- Results ----
const result = ref<SearchResult | null>(null);
const more = ref<SearchHit[]>([]);
const loading = ref(false);
const error = ref('');
const folderName = ref('');

const hits = computed(() => [...(result.value?.hits ?? []), ...more.value]);
const pictures = computed(() => (typ.value || !hits.value.length ? [] : hits.value.filter((h) => thumbUrl(h, 256))));
const others = computed(() => (pictures.value.length ? hits.value.filter((h) => !thumbUrl(h, 256)) : hits.value));
const facets = computed(() => {
	const counts = new Map((result.value?.facets ?? []).map((f) => [f.typ, f.n]));
	return KINDS.filter((k) => counts.has(k.typ)).map((k) => ({ ...k, n: counts.get(k.typ)! }));
});
const all = computed(() => facets.value.reduce((s, f) => s + f.n, 0));
const pictureCount = computed(() => facets.value.find((f) => f.typ === 'bild')?.n ?? 0);

function apiQuery(offset: number): string {
	const q = typ.value ? `${submitted.value} typ:${typ.value}` : submitted.value;
	const p = new URLSearchParams({ q, limit: '30', offset: String(offset) });
	if (folderId.value) p.set('folder', String(folderId.value));
	return `/search?${p}`;
}

async function load() {
	more.value = [];
	error.value = '';
	if (!submitted.value) {
		result.value = null;
		return;
	}
	loading.value = true;
	try {
		result.value = await apiGet<SearchResult>(apiQuery(0));
	} catch (e) {
		result.value = null;
		error.value = errorMessage(e);
	} finally {
		loading.value = false;
	}
}

async function loadMore() {
	try {
		const r = await apiGet<SearchResult>(apiQuery(hits.value.length));
		more.value = [...more.value, ...r.hits];
	} catch (e) {
		error.value = errorMessage(e);
	}
}

function choose(t: string) {
	router.replace({ query: { ...route.query, typ: t || undefined } });
}

function leaveFolder() {
	const { folder: _, ...rest } = route.query;
	router.replace({ query: rest });
}

watch(
	() => [submitted.value, typ.value, folderId.value],
	() => {
		if (submitted.value) {
			text.value = submitted.value;
			editing.value = false;
		} else {
			// A fresh search (e.g. "Suche" again from the results).
			text.value = '';
			editing.value = true;
			nextTick(() => input.value?.focus());
		}
		load();
	}
);
watch(
	folderId,
	async (id) => {
		folderName.value = '';
		if (!id) return;
		try {
			const d = await apiGet<NodeDetail>(`/nodes/${id}`);
			folderName.value = d.path.length > 1 ? d.name : 'Meine Ablage';
		} catch {
			folderName.value = '';
		}
	},
	{ immediate: true }
);

onMounted(() => {
	recent.value = recentSearches();
	load();
	if (!submitted.value) input.value?.focus();
});
onBeforeUnmount(() => clearTimeout(timer));

const plural = (n: number) => (n === 1 ? '1 Treffer' : `${n.toLocaleString('de-DE')} Treffer`);
</script>

<template>
	<main class="search">
		<form class="bar" role="search" @submit.prevent="submit">
			<div class="field" :class="{ editing }">
				<Icon name="search" :size="19" />
				<label for="suchfeld" class="sr-only">Suchen</label>
				<input
					id="suchfeld"
					ref="input"
					v-model="text"
					type="search"
					enterkeyhint="search"
					autocomplete="off"
					role="combobox"
					aria-autocomplete="list"
					:aria-expanded="showSuggestions"
					aria-controls="vorschlaege"
					:aria-activedescendant="active >= 0 ? `vorschlag-${active}` : undefined"
					placeholder="Dateien und Inhalte durchsuchen"
					@focus="edit"
					@keydown="onKey"
				/>
				<button v-if="text" type="button" class="icon clear" aria-label="Eingabe löschen" @click="clearText">
					<Icon name="x" :size="16" :stroke="1.8" />
				</button>
			</div>
			<button type="button" class="text cancel" @click="cancel">Abbrechen</button>
		</form>

		<p v-if="folderId" class="scope">
			<span class="tag">In „{{ folderName || '…' }}“</span>
			<button type="button" class="link" @click="leaveFolder">Überall suchen</button>
		</p>

		<!-- While typing -->
		<section v-if="editing" class="typing">
			<ul class="ops" aria-label="Suchfilter">
				<li v-for="o in OPERATORS" :key="o.key">
					<button type="button" :title="o.hint" @click="insert(o.key)">
						{{ o.key }}<span>{{ o.example }}</span>
					</button>
				</li>
			</ul>
			<hr />
			<template v-if="suggestions.length">
				<h2>Dateinamen</h2>
				<ul id="vorschlaege" class="names" role="listbox" aria-label="Dateinamen">
					<li
						v-for="(s, i) in suggestions"
						:id="`vorschlag-${i}`"
						:key="s.id"
						role="option"
						:aria-selected="i === active"
						:class="{ active: i === active }"
						@mousedown.prevent
						@click="open(s)"
					>
						<FileMark :node="s" />
						<span class="what">
							<span class="name"><template v-for="(p, j) in s.parts" :key="j"><strong v-if="p.hit">{{ p.text }}</strong><template v-else>{{ p.text }}</template></template></span>
							<span class="where">{{ folderTrail(s.folder) }}</span>
						</span>
					</li>
				</ul>
			</template>
			<template v-if="recent.length">
				<div class="head">
					<h2>Zuletzt gesucht</h2>
					<button type="button" class="link" @click="forget">Löschen</button>
				</div>
				<ul class="recent">
					<li v-for="r in recent" :key="r">
						<button type="button" @click="useRecent(r)">
							<Icon name="history" :size="17" />
							<span>{{ r }}</span>
						</button>
					</li>
				</ul>
			</template>
			<p v-if="!suggestions.length && !recent.length" class="muted hint">
				Sucht in Namen und im Text von PDFs, Office-Dokumenten und Scans. Mit <code>typ:</code>, <code>in:</code>,
				<code>nach:</code> und <code>vor:</code> grenzt du ein, mit Anführungszeichen suchst du genau diese Wörter.
			</p>
		</section>

		<!-- Results -->
		<section v-else class="results" aria-live="polite" :aria-busy="loading">
			<p v-if="error" class="error" role="alert">{{ error }}</p>
			<template v-else-if="result">
				<ul v-if="facets.length" class="facets" aria-label="Art der Treffer">
					<li>
						<button type="button" :aria-pressed="!typ" @click="choose('')">Alle<span>{{ all }}</span></button>
					</li>
					<li v-for="f in facets" :key="f.typ">
						<button type="button" :aria-pressed="typ === f.typ" @click="choose(f.typ)">{{ f.label }}<span>{{ f.n }}</span></button>
					</li>
				</ul>
				<p class="summary">
					<template v-if="result.fuzzy">Keine genauen Treffer – ähnliche Schreibweisen: </template>{{ plural(result.total) }}<template v-if="typ"> · {{ kindLabel(typ) }}</template>
					<template v-if="result.pending"> · Der Suchindex holt gerade Änderungen nach.</template>
				</p>
				<hr />
				<template v-if="pictures.length">
					<div class="head">
						<h2>Bilder · {{ pictureCount }}</h2>
						<button v-if="pictureCount > pictures.length || others.length" type="button" class="link" @click="choose('bild')">Alle</button>
					</div>
					<ul class="tiles">
						<li v-for="p in pictures" :key="p.id">
							<NuxtLink :to="`/files/${p.id}`" :aria-label="`${p.name}, ${folderTrail(p.folder)}`">
								<img :src="thumbUrl(p, 256)!" alt="" loading="lazy" />
								<span class="tag" aria-hidden="true">{{ monthYear(p.mtime) }}</span>
							</NuxtLink>
						</li>
					</ul>
				</template>
				<template v-if="others.length">
					<h2 v-if="pictures.length">Weitere Treffer</h2>
					<ul class="hits">
						<li v-for="h in others" :key="h.id">
							<NuxtLink :to="`/files/${h.id}`" :aria-describedby="`wo-${h.id}`">
								<FileMark :node="h" />
								<span class="what">
									<span class="name">{{ h.name }}</span>
									<span v-if="h.snippet" class="snippet" aria-hidden="true">… <template v-for="(p, j) in h.snippet" :key="j"><mark v-if="p.hit">{{ p.text }}</mark><template v-else>{{ p.text }}</template></template> …</span>
									<span :id="`wo-${h.id}`" class="where" aria-hidden="true">{{ folderTrail(h.folder) }}</span>
								</span>
							</NuxtLink>
						</li>
					</ul>
				</template>
				<p v-if="!result.total" class="muted empty">
					Nichts gefunden. Andere Wörter versuchen<template v-if="typ || folderId"> oder Filter entfernen</template>.
				</p>
				<button v-else-if="hits.length < result.total" type="button" class="more" @click="loadMore">Weitere Treffer laden</button>
			</template>
		</section>
	</main>
</template>

<style scoped>
.search { max-width: 46rem; margin: 0 auto; padding: 24px 0 140px; }
.bar { display: flex; align-items: center; gap: 6px; padding: 0 12px 0 22px; }
.field {
	flex: 1; display: flex; align-items: center; gap: 10px; height: 46px; padding: 0 4px 0 15px;
	background: var(--fill); border: 1px solid transparent; border-radius: 23px; color: var(--ink);
}
.field.editing { border-color: var(--ink); }
.field input {
	flex: 1; min-width: 0; height: 100%; border: 0; outline: none; background: transparent; padding: 0;
	font-size: 15.5px; color: var(--ink); box-shadow: none;
}
.field input::-webkit-search-cancel-button { display: none; }
.field .clear { width: 38px; height: 38px; min-height: 0; color: var(--muted); }
.cancel { min-height: 44px; padding: 0 6px; font-size: 15px; }
.scope { display: flex; align-items: center; gap: 10px; padding: 12px 24px 0; margin: 0; font-size: 13px; }
.scope .link { font-size: 13px; }

.ops { list-style: none; display: flex; gap: 6px; padding: 12px 22px 16px; margin: 0; overflow-x: auto; scrollbar-width: none; }
.ops button {
	flex: none; height: 30px; min-height: 0; padding: 0 10px; border: 1px solid var(--line-strong); border-radius: 15px;
	background: transparent; font-family: var(--mono); font-size: 12px; color: var(--ink-2); white-space: nowrap;
}
.ops button span { color: var(--faint); }
hr { margin: 0; border: 0; border-top: 1px dashed var(--dash); }
h2 { margin: 0; font-size: 13px; font-weight: 400; color: var(--muted); padding: 14px 24px 6px; }
.head { display: flex; justify-content: space-between; align-items: center; padding-right: 14px; }
.head .link { font-size: 13px; min-height: 32px; }
.names, .recent, .hits, .tiles, .facets { list-style: none; margin: 0; padding: 0; }
.names li, .hits a { display: flex; align-items: center; gap: 14px; padding: 7px 14px 7px 22px; text-decoration: none; color: inherit; }
.names li { cursor: pointer; }
.names li.active, .names li:hover, .hits a:hover { background: var(--fill); }
.what { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 2px; }
.name { font-size: 15px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.names .name { color: var(--muted); }
.names .name strong { color: var(--ink); font-weight: 500; }
.where { font-size: 12px; color: var(--muted); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.recent button {
	width: 100%; display: flex; align-items: center; gap: 14px; min-height: 44px; padding: 0 14px 0 32px; border: 0;
	border-radius: 0; background: transparent; justify-content: flex-start; font-size: 15px; color: var(--ink);
}
.recent button :deep(.ico) { color: var(--muted); }
.recent button:hover { background: var(--fill); }
.hint { padding: 14px 24px; font-size: 13.5px; line-height: 1.5; }
.hint code { font-family: var(--mono); font-size: 12px; }

.facets { display: flex; gap: 6px; padding: 12px 22px 10px; overflow-x: auto; scrollbar-width: none; }
.facets button {
	flex: none; height: 30px; min-height: 0; display: flex; align-items: center; gap: 5px; padding: 0 12px;
	border-radius: 15px; border: 1px solid var(--line-strong); background: transparent; color: var(--ink-2); font-size: 13px;
}
.facets button span { font-family: var(--mono); font-size: 11px; opacity: 0.7; }
.facets button[aria-pressed='true'] { background: var(--accent); border-color: var(--accent); color: var(--accent-text); }
.summary { margin: 0; padding: 0 24px 14px; font-size: 12.5px; color: var(--muted); }
.tiles { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 3px; padding: 0 0 6px; }
.tiles a { position: relative; display: block; height: 126px; background: var(--paper-hi); overflow: hidden; }
.tiles img { width: 100%; height: 100%; object-fit: cover; display: block; }
.tiles .tag {
	position: absolute; left: 5px; bottom: 5px; font-size: 9.5px; padding: 1px 4px; border-radius: 2px; text-transform: none;
	background: var(--paper); border: 1px solid var(--line-strong); color: var(--ink);
}
.hits a { align-items: flex-start; padding-top: 9px; padding-bottom: 9px; }
.hits .what { gap: 4px; }
.snippet { font-size: 13px; line-height: 1.45; color: var(--ink-3); overflow-wrap: anywhere; }
.snippet mark { background: var(--highlight); color: var(--on-highlight); padding: 0 2px; }
.empty { padding: 18px 24px; }
.error { margin: 16px 24px; }
.more { margin: 16px 24px; }
@media (min-width: 48rem) {
	.tiles { grid-template-columns: repeat(4, minmax(0, 1fr)); padding: 0 22px 6px; }
	.tiles a { height: 150px; }
}
</style>
