<script setup lang="ts">
type User = {
	id: number;
	username: string;
	display_name: string;
	email: string | null;
	is_admin: boolean;
	disabled: boolean;
	totp: boolean;
	passkeys: number;
	setup_pending: boolean;
	created_at: string;
};
type Audit = {
	id: number;
	at: string;
	actor: string | null;
	target: string | null;
	action: string;
	ip: string | null;
};

type SearchStatus = {
	running: boolean;
	journal: number;
	indexed: number;
	texts: number;
	jobs: { queued: number; waiting: number; failed: number };
	extract: { workers: number; pdf: boolean; ocr: boolean; tika: boolean } | null;
	problems: { state: 'failed' | 'waiting'; error: string | null; n: number }[];
};

type GroupOut = { id: number; name: string; members: { id: number; name: string }[] };
type Member = { type: 'user' | 'group'; id: number; name?: string; role: 'viewer' | 'editor' | 'manager' };
type SpaceOut = { id: number; name: string; path: string; scanned_at: string | null; members: Member[] };
type DataClasses = {
	default: DataClass;
	entries: { root: string; node_id: number; path: string; class: DataClass; set_by: string | null; set_at: string }[];
};

const { me } = useSession();
const g = useGuard();
const groups = ref<GroupOut[]>([]);
const spaces = ref<SpaceOut[]>([]);
const groupForm = ref<{ id: number | null; name: string; members: number[] }>({ id: null, name: '', members: [] });
const spaceForm = ref<{ id: number | null; name: string; path: string; members: Member[] } | null>(null);
const search = ref<SearchStatus | null>(null);
const classes = ref<DataClasses | null>(null);
/** The outside cache (PLAN 15.2). */
type CacheStatus = {
	configured: boolean;
	origin: string | null;
	bucket: string | null;
	objects: number;
	bytes: number;
	budget: number;
	uploading: number;
	by_reason: Partial<Record<'link' | 'popular' | 'prefetch', number>>;
	hits: number;
	error: string | null;
};
const cache = ref<CacheStatus | null>(null);
const REASON_LABEL = { link: 'für Links', popular: 'oft von außen geladen', prefetch: 'vorausgeladen' } as const;
const cacheReasons = computed(() =>
	(Object.keys(REASON_LABEL) as (keyof typeof REASON_LABEL)[])
		.filter((r) => cache.value?.by_reason[r])
		.map((r) => `${num(cache.value!.by_reason[r]!)} ${REASON_LABEL[r]}`)
		.join(' · ')
);
const clearCache = () =>
	g.run(async () => {
		await apiPost('/admin/cache/clear');
		await refresh();
	});
/** The AI search (PLAN 7). */
type AiStatus = {
	cloud: {
		provider: string;
		embed_model: string;
		vision_model: string | null;
		on: boolean;
		trial_left: number | null;
		paused: string | null;
		budget: number;
		budget_set: boolean;
		spent_month: number;
		error: string | null;
	} | null;
	local: { embed_model: string; clip_model: string | null; error: string | null } | null;
	progress: {
		cloud_texts: number;
		cloud_pictures: number;
		local_texts: number;
		local_pictures: number;
		queued_cloud: number;
		queued_local: number;
		failed: number;
		last_hour_cloud: number;
		last_hour_local: number;
	};
	usage: { month: string; kind: string; calls: number; tokens_in: number; tokens_out: number; cost: number }[];
	estimate: { per_text: number; per_picture: number; remaining: number } | null;
};
const ai = ref<AiStatus | null>(null);
const budgetInput = ref<number | null>(null);
const USAGE_LABEL: Record<string, string> = { embed: 'Texte einbetten', vision: 'Bilder beschreiben', query: 'Suchanfragen' };
/** Euros; tiny amounts (per text) with two significant digits. */
const euro = (v: number) =>
	v > 0 && v < 0.01
		? `${v.toLocaleString('de-DE', { maximumSignificantDigits: 2 })} €`
		: v.toLocaleString('de-DE', { style: 'currency', currency: 'EUR' });
const aiState = computed(() => {
	const c = ai.value?.cloud;
	if (!c) return '';
	if (!c.paused) return c.trial_left ? `Probelauf: noch ${num(c.trial_left)} Inhalte` : 'Läuft';
	if (c.trial_left === 0 && !c.on) return 'Probelauf beendet';
	return c.paused;
});
/** How long the rest takes at the pace of the last hour. */
const eta = (queued: number, perHour: number) => {
	if (!queued || !perHour) return '';
	const h = queued / perHour;
	return h < 1 ? ' · unter einer Stunde' : h < 48 ? ` · etwa ${Math.round(h)} Std.` : ` · etwa ${Math.round(h / 24)} Tage`;
};
const aiAction = (action: 'start' | 'stop' | 'trial') =>
	g.run(async () => {
		if (action === 'start' && !confirm('Die Analyse in der Cloud starten? Sie kostet Geld – bis zum Monatsbudget.')) return;
		ai.value = await apiPost<AiStatus>('/admin/ai', action === 'trial' ? { action, trial: 500 } : { action });
	});
const saveBudget = (reset = false) =>
	g.run(async () => {
		ai.value = await api<AiStatus>('PUT', '/admin/ai/budget', { budget: reset ? null : budgetInput.value });
		budgetInput.value = ai.value.cloud?.budget ?? null;
	});
const users = ref<User[]>([]);
const audit = ref<Audit[]>([]);
const form = ref({ username: '', display_name: '', email: '', is_admin: false });
const link = ref<null | { who: string; url: string }>(null);

async function refresh() {
	users.value = await apiGet<User[]>('/admin/users');
	audit.value = await apiGet<Audit[]>('/admin/audit?limit=50');
	search.value = await apiGet<SearchStatus>('/admin/search');
	groups.value = await apiGet<GroupOut[]>('/admin/groups');
	spaces.value = await apiGet<SpaceOut[]>('/admin/spaces');
	classes.value = await apiGet<DataClasses>('/admin/data-classes');
	cache.value = await apiGet<CacheStatus>('/admin/cache');
	ai.value = await apiGet<AiStatus>('/admin/ai');
	budgetInput.value = ai.value.cloud?.budget ?? null;
}

/** The record of where data may go, as a table for Numbers or Excel. */
function exportClasses() {
	const c = classes.value;
	if (!c) return;
	// Quoted; a leading = + - @ would be run as a formula by spreadsheets.
	const cell = (v: string) => `"${(/^[=+\-@]/.test(v) ? `'${v}` : v).replaceAll('"', '""')}"`;
	const rows = [
		['Ablage', 'Ordner', 'Datenklasse', 'Festgelegt von', 'Am'],
		...c.entries.map((e) => [e.root, e.path || '(ganze Ablage)', CLASS_LABEL[e.class], e.set_by ?? '', formatDate(e.set_at)]),
		['(alle anderen Ordner)', '', CLASS_LABEL[c.default], 'Voreinstellung des Servers', '']
	];
	const csv = '\uFEFF' + rows.map((r) => r.map(cell).join(';')).join('\r\n');
	const a = document.createElement('a');
	a.href = URL.createObjectURL(new Blob([csv], { type: 'text/csv;charset=utf-8' }));
	a.download = `xlrx-datenklassen-${new Date().toISOString().slice(0, 10)}.csv`;
	a.click();
	URL.revokeObjectURL(a.href);
}

const saveGroup = () =>
	g.run(async () => {
		const body = { name: groupForm.value.name, members: groupForm.value.members };
		if (groupForm.value.id) await api('PUT', `/admin/groups/${groupForm.value.id}`, body);
		else await apiPost('/admin/groups', body);
		groupForm.value = { id: null, name: '', members: [] };
		await refresh();
	});
const editGroup = (gr: GroupOut) => (groupForm.value = { id: gr.id, name: gr.name, members: gr.members.map((m) => m.id) });
const deleteGroup = (gr: GroupOut) =>
	g.run(async () => {
		if (!confirm(`Gruppe „${gr.name}“ löschen? Was mit ihr geteilt ist, sehen ihre Mitglieder dann nicht mehr.`)) return;
		await apiDelete(`/admin/groups/${gr.id}`);
		await refresh();
	});

const newSpace = () => (spaceForm.value = { id: null, name: '', path: '', members: [] });
const editSpace = (sp: SpaceOut) =>
	(spaceForm.value = { id: sp.id, name: sp.name, path: sp.path, members: sp.members.map((m) => ({ ...m })) });
const addMember = () => spaceForm.value?.members.push({ type: 'user', id: users.value[0]?.id ?? 0, role: 'editor' });
const memberKey = (m: Member) => `${m.type}:${m.id}`;
function setMember(m: Member, key: string) {
	const [type, id] = key.split(':');
	m.type = type as Member['type'];
	m.id = Number(id);
}
const saveSpace = () =>
	g.run(async () => {
		const f = spaceForm.value;
		if (!f) return;
		const body = { name: f.name, path: f.path, members: f.members.map((m) => ({ type: m.type, id: m.id, role: m.role })) };
		if (f.id) await api('PUT', `/admin/spaces/${f.id}`, body);
		else await apiPost('/admin/spaces', body);
		spaceForm.value = null;
		await refresh();
	});
const memberText = (sp: SpaceOut) =>
	sp.members.map((m) => `${m.name} (${ROLE_LABEL[m.role]})`).join(', ') || 'noch keine Mitglieder';

const retryJobs = () =>
	g.run(async () => {
		await apiPost('/admin/jobs/retry');
		await refresh();
	});

const num = (n: number) => n.toLocaleString('de-DE');
const indexState = computed(() => {
	const s = search.value;
	if (!s?.running) return 'läuft nicht';
	const behind = s.journal - s.indexed;
	return behind > 0 ? `holt ${num(behind)} ${behind === 1 ? 'Änderung' : 'Änderungen'} nach` : 'aktuell';
});
const readers = computed(() => {
	const e = search.value?.extract;
	if (!e) return 'Textextraktion ausgeschaltet';
	return [
		`PDF ${e.pdf ? '✓' : 'fehlt'}`,
		`Texterkennung ${e.ocr ? '✓' : 'fehlt'}`,
		`Office (Tika) ${e.tika ? '✓' : 'nicht eingerichtet'}`
	].join(' · ');
});
onMounted(() => {
	if (me.value?.is_admin) g.run(refresh);
});

const create = () =>
	g.run(async () => {
		const r = await apiPost<{ setup_url: string }>('/admin/users', {
			...form.value,
			email: form.value.email || null
		});
		link.value = { who: form.value.username, url: r.setup_url };
		form.value = { username: '', display_name: '', email: '', is_admin: false };
		await refresh();
	});

const newLink = (u: User, reset: boolean) =>
	g.run(async () => {
		if (
			reset &&
			!confirm(`Alle zweiten Faktoren von ${u.username} zurücksetzen? Alle Sitzungen werden beendet.`)
		)
			return;
		const r = await apiPost<{ setup_url: string }>(
			`/admin/users/${u.id}/${reset ? 'reset-factors' : 'invite'}`
		);
		link.value = { who: u.username, url: r.setup_url };
		await refresh();
	});

const toggle = (u: User) =>
	g.run(async () => {
		await apiPost(`/admin/users/${u.id}/disabled`, { disabled: !u.disabled });
		await refresh();
	});

const factors = (u: User) =>
	[u.totp ? 'App' : null, u.passkeys ? `${u.passkeys} Passkey(s)` : null]
		.filter(Boolean)
		.join(', ') || '–';
const status = (u: User) => (u.disabled ? 'gesperrt' : u.setup_pending ? 'Einrichtung offen' : 'aktiv');
const copy = () => link.value && navigator.clipboard.writeText(link.value.url);
</script>

<template>
	<main class="page">
		<h1>Verwaltung</h1>
		<p v-if="!me?.is_admin" class="error">Nur für Administratoren.</p>
		<template v-else>
			<p v-if="g.error.value" class="error" role="alert">{{ g.error.value }}</p>
			<div v-if="link" class="note stack">
				<strong>Einrichtungslink für {{ link.who }}</strong>
				<p class="muted">72 Stunden gültig und nur einmal verwendbar. Bitte auf sicherem Weg weitergeben.</p>
				<input readonly :value="link.url" class="code" />
				<div class="row">
					<button @click="copy">Kopieren</button>
					<button @click="link = null">Schließen</button>
				</div>
			</div>

			<section class="card">
				<h2 style="margin-top: 0">Konten</h2>
				<table>
					<thead>
						<tr><th>Konto</th><th>Faktoren</th><th>Status</th><th></th></tr>
					</thead>
					<tbody>
						<tr v-for="u in users" :key="u.id">
							<td>
								<strong>{{ u.display_name }}</strong><br /><span class="muted">{{ u.username }}</span>
								<span v-if="u.is_admin" class="badge">Admin</span>
							</td>
							<td>{{ factors(u) }}</td>
							<td>{{ status(u) }}</td>
							<td class="row">
								<button @click="newLink(u, false)">Neuer Link</button>
								<button @click="newLink(u, true)">Faktoren zurücksetzen</button>
								<button v-if="u.id !== me?.id" class="danger" @click="toggle(u)">
									{{ u.disabled ? 'Entsperren' : 'Sperren' }}
								</button>
							</td>
						</tr>
					</tbody>
				</table>

				<h2>Neues Konto</h2>
				<form @submit.prevent="create">
					<div class="grid">
						<div><label for="nu">Benutzername</label><input id="nu" v-model="form.username" required /></div>
						<div><label for="nd">Anzeigename</label><input id="nd" v-model="form.display_name" required /></div>
						<div><label for="ne">E-Mail (optional)</label><input id="ne" v-model="form.email" type="email" /></div>
					</div>
					<label class="row" style="color: var(--text)">
						<input v-model="form.is_admin" type="checkbox" /> Administrator
					</label>
					<button class="primary" :disabled="g.busy.value">Anlegen und Einrichtungslink erzeugen</button>
				</form>
			</section>

			<section v-if="search" class="card">
				<h2 style="margin-top: 0">Suche</h2>
				<div class="kv"><span>Suchindex</span><span>{{ indexState }}</span></div>
				<div class="kv"><span>Gelesene Inhalte</span><span>{{ num(search.texts) }}</span></div>
				<div class="kv">
					<span>Warteschlange</span>
					<span>{{ num(search.jobs.queued) }} offen · {{ num(search.jobs.waiting) }} warten · {{ num(search.jobs.failed) }} fehlgeschlagen</span>
				</div>
				<div class="kv"><span>Liest</span><span>{{ readers }}</span></div>
				<ul v-if="search.problems.length" class="problems">
					<li v-for="(p, i) in search.problems" :key="i">
						{{ num(p.n) }} × {{ p.state === 'failed' ? 'fehlgeschlagen' : 'wartet' }}: {{ p.error ?? 'ohne Angabe' }}
					</li>
				</ul>
				<button v-if="search.jobs.failed" type="button" :disabled="g.busy.value" @click="retryJobs">
					Fehlgeschlagene erneut versuchen
				</button>
			</section>

			<section class="card">
				<h2 style="margin-top: 0">Gruppen</h2>
				<ul v-if="groups.length" class="plain">
					<li v-for="gr in groups" :key="gr.id">
						<span class="grow"><strong>{{ gr.name }}</strong><span class="muted"> · {{ gr.members.map((m) => m.name).join(', ') || 'leer' }}</span></span>
						<button class="link" type="button" @click="editGroup(gr)">Bearbeiten</button>
						<button class="link danger" type="button" @click="deleteGroup(gr)">Löschen</button>
					</li>
				</ul>
				<p v-else class="muted">Noch keine Gruppen. Mit einer Gruppe („Familie“, „Eltern“) teilt man mit mehreren auf einmal.</p>
				<form class="stack" @submit.prevent="saveGroup">
					<label>Name der Gruppe<input v-model="groupForm.name" required maxlength="80" /></label>
					<fieldset class="checks">
						<legend>Mitglieder</legend>
						<label v-for="u in users" :key="u.id"><input v-model="groupForm.members" type="checkbox" :value="u.id" />{{ u.display_name }}</label>
					</fieldset>
					<div class="row">
						<button class="primary" :disabled="g.busy.value">{{ groupForm.id ? 'Gruppe speichern' : 'Gruppe anlegen' }}</button>
						<button v-if="groupForm.id" type="button" @click="groupForm = { id: null, name: '', members: [] }">Abbrechen</button>
					</div>
				</form>
			</section>

			<section class="card">
				<h2 style="margin-top: 0">Geteilte Ablagen</h2>
				<p class="muted">
					Ein Ordner auf dem NAS, den mehrere gemeinsam nutzen – zum Beispiel ein Teamordner von Synology Drive. Der Ordner
					bleibt, wo er ist; xlrx bindet ihn nur ein.
				</p>
				<ul v-if="spaces.length" class="plain">
					<li v-for="sp in spaces" :key="sp.id">
						<span class="grow"><strong>{{ sp.name }}</strong><span class="muted"> · {{ sp.path }} · {{ memberText(sp) }}</span></span>
						<button class="link" type="button" @click="editSpace(sp)">Mitglieder ändern</button>
					</li>
				</ul>
				<form v-if="spaceForm" class="stack" @submit.prevent="saveSpace">
					<label>Name<input v-model="spaceForm.name" required placeholder="z. B. Familie" /></label>
					<label v-if="!spaceForm.id">
						Ordner auf dem NAS
						<input v-model="spaceForm.path" required placeholder="z. B. Familie für /volume1/Familie" />
					</label>
					<fieldset class="members">
						<legend>Mitglieder</legend>
						<div v-for="(m, i) in spaceForm.members" :key="i" class="member">
							<select :value="memberKey(m)" aria-label="Person oder Gruppe" @change="setMember(m, ($event.target as HTMLSelectElement).value)">
								<optgroup label="Personen">
									<option v-for="u in users" :key="u.id" :value="`user:${u.id}`">{{ u.display_name }}</option>
								</optgroup>
								<optgroup v-if="groups.length" label="Gruppen">
									<option v-for="gr in groups" :key="gr.id" :value="`group:${gr.id}`">{{ gr.name }}</option>
								</optgroup>
							</select>
							<select v-model="m.role" aria-label="Rolle">
								<option value="viewer">Ansehen</option>
								<option value="editor">Bearbeiten</option>
								<option value="manager">Verwalten</option>
							</select>
							<button type="button" class="icon" aria-label="Mitglied entfernen" @click="spaceForm.members.splice(i, 1)"><Icon name="x" :size="15" /></button>
						</div>
						<button type="button" class="link" @click="addMember">Mitglied hinzufügen</button>
					</fieldset>
					<div class="row">
						<button class="primary" :disabled="g.busy.value">{{ spaceForm.id ? 'Speichern' : 'Einbinden' }}</button>
						<button type="button" @click="spaceForm = null">Abbrechen</button>
					</div>
				</form>
				<button v-else type="button" @click="newSpace">Ablage einbinden</button>
			</section>

			<section v-if="cache" class="card">
				<h2 style="margin-top: 0">Außen-Beschleuniger</h2>
				<p v-if="!cache.configured" class="muted">
					Aus. Mit einem S3-Bucket (z. B. bei Hetzner) laden Personen unterwegs große Dateien von dort statt über den
					Heimanschluss – nur aus Ordnern mit „Cloud erlaubt“. Einrichtung: <code>XLRX_S3_BUCKET</code>, siehe
					<code>deploy/synology.md</code>.
				</p>
				<template v-else>
					<div class="kv"><span>Bucket</span><span>{{ cache.bucket }} · {{ cache.origin }}</span></div>
					<div class="kv">
						<span>Belegt</span>
						<span>{{ formatSize(cache.bytes) }} von {{ formatSize(cache.budget) }} · {{ num(cache.objects) }} {{ cache.objects === 1 ? 'Datei' : 'Dateien' }}</span>
					</div>
					<div v-if="cacheReasons" class="kv"><span>Warum</span><span>{{ cacheReasons }}</span></div>
					<div class="kv"><span>Von dort geladen</span><span>{{ num(cache.hits) }}-mal</span></div>
					<div v-if="cache.uploading" class="kv"><span>Lädt gerade hoch</span><span>{{ num(cache.uploading) }}</span></div>
					<p v-if="cache.error" class="error" role="alert">{{ cache.error }}</p>
					<p class="muted small">„Nur lokal“-Inhalte kommen nie dorthin. Im Heimnetz wird immer direkt vom NAS geladen.</p>
					<button type="button" :disabled="g.busy.value || !cache.objects" @click="clearCache">Leeren</button>
				</template>
			</section>

			<section v-if="ai" class="card" aria-labelledby="ki">
				<h2 id="ki" style="margin-top: 0">KI-Suche</h2>
				<p class="muted">
					Findet Dateien nach ihrer Bedeutung und Bilder nach ihrem Inhalt. Inhalte aus Ordnern mit „Cloud erlaubt“ gehen an
					den Anbieter in der EU, alles andere wird nur auf dem NAS verarbeitet.
				</p>
				<template v-if="ai.cloud">
					<h3>Cloud · {{ ai.cloud.provider }}</h3>
					<div class="kv"><span>Modelle</span><span>{{ ai.cloud.embed_model }} · {{ ai.cloud.vision_model ?? 'keine Bildanalyse' }}</span></div>
					<div class="kv"><span>Zustand</span><span>{{ aiState }}</span></div>
					<div class="kv"><span>Diesen Monat</span><span>{{ euro(ai.cloud.spent_month) }} von {{ euro(ai.cloud.budget) }}</span></div>
					<form class="row" @submit.prevent="saveBudget()">
						<label class="budget">Budget pro Monat (€)<input v-model.number="budgetInput" type="number" min="0" step="1" required /></label>
						<button :disabled="g.busy.value">Speichern</button>
						<button v-if="ai.cloud.budget_set" type="button" class="link" :disabled="g.busy.value" @click="saveBudget(true)">Voreinstellung</button>
					</form>
					<p v-if="ai.cloud.error" class="error" role="alert">{{ ai.cloud.error }}</p>
					<div class="row">
						<template v-if="!ai.cloud.on">
							<button type="button" class="primary" :disabled="g.busy.value" @click="aiAction('start')">Starten</button>
							<button v-if="!ai.cloud.trial_left" type="button" :disabled="g.busy.value" @click="aiAction('trial')">Probelauf (500 Inhalte)</button>
							<button v-else type="button" :disabled="g.busy.value" @click="aiAction('stop')">Probelauf abbrechen</button>
						</template>
						<button v-else type="button" :disabled="g.busy.value" @click="aiAction('stop')">Anhalten</button>
					</div>
					<p class="muted small">Ein Probelauf zeigt die echten Kosten je Inhalt, bevor alles analysiert wird.</p>
				</template>
				<p v-else class="muted">
					Kein Anbieter für „Cloud erlaubt“ eingerichtet (<code>XLRX_AI_URL</code>, siehe <code>deploy/synology.md</code>).
				</p>
				<template v-if="ai.local">
					<h3>Heimnetz · embed-local</h3>
					<div class="kv"><span>Modelle</span><span>{{ ai.local.embed_model }}{{ ai.local.clip_model ? ` · ${ai.local.clip_model}` : '' }}</span></div>
					<p v-if="ai.local.error" class="error" role="alert">{{ ai.local.error }}</p>
				</template>
				<h3>Fortschritt</h3>
				<div class="kv"><span>Texte</span><span>{{ num(ai.progress.cloud_texts) }} in der Cloud · {{ num(ai.progress.local_texts) }} im Heimnetz</span></div>
				<div class="kv"><span>Bilder</span><span>{{ num(ai.progress.cloud_pictures) }} beschrieben · {{ num(ai.progress.local_pictures) }} im Heimnetz</span></div>
				<div class="kv">
					<span>Ausstehend</span>
					<span>
						{{ num(ai.progress.queued_cloud) }} Cloud{{ eta(ai.progress.queued_cloud, ai.progress.last_hour_cloud) }} ·
						{{ num(ai.progress.queued_local) }} Heimnetz{{ eta(ai.progress.queued_local, ai.progress.last_hour_local) }}
					</span>
				</div>
				<div v-if="ai.progress.failed" class="kv"><span>Fehlgeschlagen</span><span>{{ num(ai.progress.failed) }}</span></div>
				<div v-if="ai.estimate" class="kv">
					<span>Hochrechnung</span>
					<span>{{ euro(ai.estimate.per_text) }} je Text · {{ euro(ai.estimate.per_picture) }} je Bild · Rest ≈ {{ euro(ai.estimate.remaining) }}</span>
				</div>
				<table v-if="ai.usage.length">
					<thead>
						<tr><th>Monat</th><th>Art</th><th>Aufrufe</th><th>Tokens</th><th>Kosten</th></tr>
					</thead>
					<tbody>
						<tr v-for="u in ai.usage" :key="`${u.month}-${u.kind}`">
							<td>{{ new Date(u.month).toLocaleDateString('de-DE', { month: 'long', year: 'numeric' }) }}</td>
							<td>{{ USAGE_LABEL[u.kind] ?? u.kind }}</td>
							<td>{{ num(u.calls) }}</td>
							<td>{{ num(u.tokens_in + u.tokens_out) }}</td>
							<td>{{ euro(u.cost) }}</td>
						</tr>
					</tbody>
				</table>
			</section>

			<section v-if="classes" class="card">
				<h2 style="margin-top: 0">Datenklassen</h2>
				<p class="muted">
					Was „Cloud erlaubt“ ist, darf für die KI-Suche an einen Dienst in der EU – alles andere bleibt auf dem NAS.
					Ohne eigene Einstellung gilt <strong>{{ CLASS_LABEL[classes.default] }}</strong> (<code>XLRX_DEFAULT_DATA_CLASS</code>).
					Festlegen kann es, wer einen Ordner verwaltet – im Ordner über die Datenklasse.
				</p>
				<ul v-if="classes.entries.length" class="plain">
					<li v-for="e in classes.entries" :key="e.node_id">
						<span class="grow">
							<strong>{{ e.root }}{{ e.path ? ` › ${e.path.replaceAll('/', ' › ')}` : '' }}</strong>
							<span class="muted"> · {{ e.set_by ?? '–' }}, {{ formatDate(e.set_at) }}</span>
						</span>
						<span class="tag"><Icon :name="e.class === 'local' ? 'lock' : 'cloud'" :size="10" :stroke="2" />{{ CLASS_LABEL[e.class] }}</span>
					</li>
				</ul>
				<p v-else class="muted">Noch keine Ordner mit eigener Einstellung.</p>
				<button type="button" @click="exportClasses"><Icon name="download" :size="16" />Als Tabelle sichern</button>
			</section>

			<section class="card">
				<h2 style="margin-top: 0">Protokoll</h2>
				<table>
					<thead>
						<tr><th>Zeit</th><th>Aktion</th><th>Wer</th><th>Betrifft</th><th>IP</th></tr>
					</thead>
					<tbody>
						<tr v-for="a in audit" :key="a.id">
							<td>{{ formatDate(a.at) }}</td>
							<td>{{ a.action }}</td>
							<td>{{ a.actor ?? '–' }}</td>
							<td>{{ a.target ?? '–' }}</td>
							<td>{{ a.ip ?? '–' }}</td>
						</tr>
					</tbody>
				</table>
			</section>
		</template>
	</main>
	<StepUpDialog v-if="g.pending.value" @done="g.confirmed" @cancel="g.cancel" />
</template>

<style scoped>
.plain { list-style: none; margin: 0 0 14px; padding: 0; }
.plain li { display: flex; align-items: center; gap: 12px; min-height: 48px; border-bottom: 1px solid var(--line); font-size: 14px; }
.plain .grow { flex: 1; min-width: 0; }
.checks, .members { border: 0; padding: 0; margin: 0; display: flex; flex-wrap: wrap; gap: 6px 18px; }
.checks legend, .members legend { font-size: 13px; color: var(--muted); margin-bottom: 6px; }
.checks label { display: flex; align-items: center; gap: 8px; font-size: 14px; }
.members { flex-direction: column; }
.member { display: flex; gap: 8px; align-items: center; }
.member select { flex: 1; }
.problems { margin: 12px 0; padding-left: 1.1rem; font-size: 13.5px; color: var(--ink-3); }
.budget { display: flex; flex-direction: column; gap: 4px; font-size: 13px; color: var(--muted); }
.budget input { width: 8rem; }
h3 { font-size: 14px; margin: 18px 0 6px; }
.grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(12rem, 1fr)); gap: 0 1rem; }
</style>
