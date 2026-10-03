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

const { me } = useSession();
const g = useGuard();
const search = ref<SearchStatus | null>(null);
const users = ref<User[]>([]);
const audit = ref<Audit[]>([]);
const form = ref({ username: '', display_name: '', email: '', is_admin: false });
const link = ref<null | { who: string; url: string }>(null);

async function refresh() {
	users.value = await apiGet<User[]>('/admin/users');
	audit.value = await apiGet<Audit[]>('/admin/audit?limit=50');
	search.value = await apiGet<SearchStatus>('/admin/search');
}

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
	return behind > 0 ? `holt ${num(behind)} Änderungen nach` : 'aktuell';
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
.problems { margin: 12px 0; padding-left: 1.1rem; font-size: 13.5px; color: var(--ink-3); }
.grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(12rem, 1fr)); gap: 0 1rem; }
</style>
