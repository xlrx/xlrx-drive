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

const { me } = useSession();
const g = useGuard();
const users = ref<User[]>([]);
const audit = ref<Audit[]>([]);
const form = ref({ username: '', display_name: '', email: '', is_admin: false });
const link = ref<null | { who: string; url: string }>(null);

async function refresh() {
	users.value = await apiGet<User[]>('/admin/users');
	audit.value = await apiGet<Audit[]>('/admin/audit?limit=50');
}
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
			<div v-if="link" class="card stack">
				<strong>Einrichtungslink für {{ link.who }}</strong>
				<p class="muted">72 Stunden gültig und nur einmal verwendbar. Bitte auf sicherem Weg weitergeben.</p>
				<input readonly :value="link.url" class="code" />
				<div class="row">
					<button @click="copy">Kopieren</button>
					<button @click="link = null">Schließen</button>
				</div>
			</div>

			<div class="card" style="margin-top: 1.5rem">
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
			</div>

			<div class="card" style="margin-top: 1.5rem">
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
			</div>
		</template>
	</main>
	<StepUpDialog v-if="g.pending.value" @done="g.confirmed" @cancel="g.cancel" />
</template>

<style scoped>
.grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(12rem, 1fr)); gap: 0 1rem; }
</style>
