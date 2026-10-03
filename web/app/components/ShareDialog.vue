<script setup lang="ts">
// "Teilen" (as the "Zugriff" list in the design's details): who has access – owner or members of
// the shared root, and the people and groups it is shared with, also through a folder above.
// Whoever manages it changes roles, ends shares and adds people or groups, optionally until a day,
// and hands out public links (PLAN 9.2) – creating one needs a fresh second factor.
const props = defineProps<{ node: Pick<NodeInfo, 'id' | 'name' | 'kind'> }>();
const emit = defineEmits<{ close: []; changed: [] }>();
const { me } = useSession();

const info = ref<AccessInfo | null>(null);
const people = ref<People | null>(null);
const error = ref('');
const busy = ref(false);
const pick = ref('');
const role = ref<'viewer' | 'editor' | 'manager'>('viewer');
const until = ref('');
const links = ref<LinkInfo[]>([]);
const g = useGuard();
const linkKind = ref<LinkKind>('view');
const linkPassword = ref('');
const linkUntil = ref('');
const linkMax = ref<number | ''>('');
/** The link just created, to copy right away. */
const fresh = ref<LinkInfo | null>(null);
const copied = ref<number | null>(null);

async function load() {
	try {
		info.value = await apiGet<AccessInfo>(`/nodes/${props.node.id}/shares`);
		if (info.value.can_share && !people.value) people.value = await apiGet<People>('/people');
		if (info.value.can_share) links.value = await apiGet<LinkInfo[]>(`/nodes/${props.node.id}/links`);
	} catch (e) {
		error.value = errorMessage(e);
	}
}
onMounted(load);

async function run(fn: () => Promise<unknown>) {
	busy.value = true;
	error.value = '';
	try {
		await fn();
		await load();
		emit('changed');
	} catch (e) {
		error.value = errorMessage(e);
	} finally {
		busy.value = false;
	}
}

const add = () =>
	run(async () => {
		const [type, id] = pick.value.split(':');
		if (!type || !id) return;
		await apiPost(`/nodes/${props.node.id}/shares`, {
			type,
			id: Number(id),
			role: role.value,
			// Until the end of the day chosen.
			expires_at: until.value ? new Date(`${until.value}T23:59:59`).toISOString() : null
		});
		pick.value = '';
		until.value = '';
	});
const change = (s: ShareInfo, r: string) => run(() => apiPatch(`/shares/${s.id}`, { role: r, keep_expiry: true }));
const end = (s: ShareInfo) => run(() => apiDelete(`/shares/${s.id}`));

const kinds = computed(() =>
	(Object.keys(LINK_KIND_LABEL) as LinkKind[]).filter((k) => k !== 'upload' || props.node.kind === 'dir')
);
const counts = computed(() => linkKind.value === 'download' || linkKind.value === 'edit');
const addLink = () =>
	g.run(async () => {
		fresh.value = await apiPost<LinkInfo>(`/nodes/${props.node.id}/links`, {
			kind: linkKind.value,
			password: linkPassword.value || null,
			expires_at: linkUntil.value ? new Date(`${linkUntil.value}T23:59:59`).toISOString() : null,
			max_downloads: counts.value && linkMax.value ? Number(linkMax.value) : null
		});
		linkPassword.value = '';
		linkUntil.value = '';
		linkMax.value = '';
		await load();
		emit('changed');
	});
const endLink = (l: LinkInfo) =>
	run(async () => {
		await apiDelete(`/links/${l.id}`);
		if (fresh.value?.id === l.id) fresh.value = null;
	});
async function copy(l: LinkInfo) {
	if (!l.url) return;
	try {
		await navigator.clipboard.writeText(l.url);
		copied.value = l.id;
		setTimeout(() => (copied.value = copied.value === l.id ? null : copied.value), 2000);
	} catch {
		error.value = 'Kopieren ging nicht – bitte den Link markieren und kopieren.';
	}
}
const linkLine = (l: LinkInfo) =>
	[
		l.password ? 'mit Passwort' : 'ohne Passwort',
		l.expired ? 'abgelaufen' : l.expires_at ? `bis ${day(l.expires_at)}` : '',
		l.max_downloads ? `${l.downloads} von ${l.max_downloads} Downloads` : l.downloads ? `${l.downloads} Downloads` : ''
	]
		.filter(Boolean)
		.join(' · ');

const initial = (name: string) => name.trim().charAt(0).toUpperCase() || '?';
const day = (s: string) => new Date(s).toLocaleDateString('de-DE', { day: 'numeric', month: 'short', year: 'numeric' });
const today = new Date().toISOString().slice(0, 10);
const shared = computed(() => new Set((info.value?.shares ?? []).filter((s) => !s.inherited).map((s) => `${s.to.type}:${s.to.id}`)));
</script>

<template>
	<Modal :title="`„${node.name}“ teilen`" wide @cancel="emit('close')">
		<p v-if="error" class="error" role="alert">{{ error }}</p>
		<template v-if="info">
			<h3 class="label">Zugriff</h3>
			<ul class="people">
				<li v-if="info.owner">
					<span class="avatar" aria-hidden="true">{{ initial(info.owner) }}</span>
					<span class="who"><span>{{ info.owner }}{{ info.owner === me?.display_name ? ' (du)' : '' }}</span></span>
					<span class="role">Besitzer</span>
				</li>
				<li v-for="m in info.members" :key="`m-${m.to.type}-${m.to.id}`">
					<span class="avatar" aria-hidden="true"><Icon v-if="m.to.type === 'group'" name="users" :size="16" /><template v-else>{{ initial(m.to.name) }}</template></span>
					<span class="who"><span>{{ m.to.name }}</span><span class="sub">Mitglied von „{{ info.space }}“</span></span>
					<span class="role">{{ ROLE_LABEL[m.role] }}</span>
				</li>
				<li v-for="s in info.shares" :key="s.id" :class="{ expired: s.expired }">
					<span class="avatar" aria-hidden="true"><Icon v-if="s.to.type === 'group'" name="users" :size="16" /><template v-else>{{ initial(s.to.name) }}</template></span>
					<span class="who">
						<span>{{ s.to.name }}</span>
						<span v-if="s.inherited || s.expires_at" class="sub">
							<template v-if="s.inherited">über „{{ s.node_name }}“</template>
							<template v-if="s.inherited && s.expires_at"> · </template>
							<template v-if="s.expired">abgelaufen</template>
							<template v-else-if="s.expires_at">bis {{ day(s.expires_at) }}</template>
						</span>
					</span>
					<template v-if="info.can_share && !s.inherited">
						<select :value="s.role" :aria-label="`Rolle von ${s.to.name}`" :disabled="busy" @change="change(s, ($event.target as HTMLSelectElement).value)">
							<option value="viewer">Ansehen</option>
							<option value="editor">Bearbeiten</option>
							<option value="manager">Verwalten</option>
						</select>
						<button type="button" class="icon" :aria-label="`Freigabe für ${s.to.name} beenden`" :disabled="busy" @click="end(s)">
							<Icon name="x" :size="15" :stroke="1.8" />
						</button>
					</template>
					<span v-else class="role">{{ ROLE_LABEL[s.role] }}</span>
				</li>
			</ul>
			<p v-if="!info.shares.length && !info.members.length" class="muted small">Noch mit niemandem geteilt.</p>

			<form v-if="info.can_share && people" class="add" @submit.prevent="add">
				<h3 class="label">Teilen mit</h3>
				<div class="fields">
					<select v-model="pick" aria-label="Person oder Gruppe" required>
						<option value="" disabled>Person oder Gruppe wählen</option>
						<optgroup v-if="people.users.length" label="Personen">
							<option v-for="u in people.users" :key="u.id" :value="`user:${u.id}`" :disabled="shared.has(`user:${u.id}`)">{{ u.name }}</option>
						</optgroup>
						<optgroup v-if="people.groups.length" label="Gruppen">
							<option v-for="g in people.groups" :key="g.id" :value="`group:${g.id}`" :disabled="shared.has(`group:${g.id}`)">{{ g.name }} ({{ g.members }})</option>
						</optgroup>
					</select>
					<select v-model="role" aria-label="Rolle">
						<option value="viewer">Ansehen</option>
						<option value="editor">Bearbeiten</option>
						<option value="manager">Verwalten</option>
					</select>
					<label class="until">
						<span>Bis (optional)</span>
						<input v-model="until" type="date" :min="today" />
					</label>
				</div>
				<p class="muted small">
					{{ role === 'viewer' ? 'Darf ansehen und herunterladen.' : role === 'editor' ? 'Darf auch hochladen, ändern, umbenennen und löschen – alles landet bei Bedarf im Papierkorb.' : 'Darf außerdem weiter teilen.' }}
					{{ node.kind === 'dir' ? 'Gilt für alles in diesem Ordner.' : '' }}
				</p>
				<button class="primary full" :disabled="busy || !pick">Teilen</button>
			</form>
			<p v-else-if="!info.can_share" class="muted small">Teilen kann nur, wer das verwaltet.</p>

			<section v-if="info.can_share" class="links-part">
				<h3 class="label">Link</h3>
				<ul v-if="links.length" class="people">
					<li v-for="l in links" :key="l.id" :class="{ expired: l.expired }">
						<span class="avatar" aria-hidden="true"><Icon name="link" :size="16" /></span>
						<span class="who">
							<span>{{ LINK_KIND_LABEL[l.kind] }}</span>
							<span class="sub">{{ linkLine(l) }}</span>
						</span>
						<button v-if="l.url" type="button" class="icon" :aria-label="`Link „${LINK_KIND_LABEL[l.kind]}“ kopieren`" @click="copy(l)">
							<Icon :name="copied === l.id ? 'check' : 'copy'" :size="15" :stroke="1.8" />
						</button>
						<button type="button" class="icon" :aria-label="`Link „${LINK_KIND_LABEL[l.kind]}“ beenden`" :disabled="busy" @click="endLink(l)">
							<Icon name="x" :size="15" :stroke="1.8" />
						</button>
					</li>
				</ul>
				<div v-if="fresh?.url" class="note fresh" role="status">
					<strong>Link erstellt</strong>
					<div class="copy-row">
						<input :value="fresh.url" readonly aria-label="Neuer Link" @focus="($event.target as HTMLInputElement).select()" />
						<button type="button" @click="copy(fresh)">{{ copied === fresh.id ? 'Kopiert' : 'Kopieren' }}</button>
					</div>
					<p>Wer diesen Link hat, braucht kein Konto.{{ fresh.password ? ' Das Passwort schickst du am besten auf einem anderen Weg.' : '' }}</p>
				</div>
				<form class="add" @submit.prevent="addLink">
					<div class="fields link-fields">
						<select v-model="linkKind" aria-label="Art des Links">
							<option v-for="k in kinds" :key="k" :value="k">{{ LINK_KIND_LABEL[k] }}</option>
						</select>
						<input v-model="linkPassword" type="password" autocomplete="new-password" placeholder="Passwort (optional)" aria-label="Passwort für den Link" minlength="8" />
						<label class="until">
							<span>Bis (optional)</span>
							<input v-model="linkUntil" type="date" :min="today" aria-label="Link gilt bis" />
						</label>
						<input v-if="counts" v-model="linkMax" type="number" min="1" inputmode="numeric" placeholder="Downloads (optional)" aria-label="Höchstens so viele Downloads" />
					</div>
					<p class="muted small">{{ LINK_KIND_HINT[linkKind] }}</p>
					<p v-if="g.error.value" class="error" role="alert">{{ g.error.value }}</p>
					<button class="full" :disabled="g.busy.value">Link erstellen</button>
				</form>
			</section>
		</template>
		<p v-else-if="!error" class="muted">Lädt …</p>
		<StepUpDialog v-if="g.pending.value" @done="g.confirmed" @cancel="g.cancel" />
	</Modal>
</template>

<style scoped>
.label { margin: 6px 0 4px; }
.people { list-style: none; margin: 0 0 8px; padding: 0; }
.people li { display: flex; align-items: center; gap: 12px; min-height: 56px; border-bottom: 1px solid var(--line); }
.people li.expired .who { opacity: 0.6; }
.who { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 2px; font-size: 15px; }
.who > span:first-child { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.sub { font-size: 12px; color: var(--muted); }
.role { font-size: 13.5px; color: var(--muted); }
.people select { width: auto; height: 38px; padding: 0 36px 0 14px; font-size: 13.5px; }
.people .icon { width: 36px; height: 36px; min-height: 0; color: var(--muted); }
.add { margin-top: 18px; }
.fields { display: grid; grid-template-columns: 1fr; gap: 8px; }
.until { display: flex; flex-direction: column; gap: 4px; font-size: 12.5px; color: var(--muted); }
.small { font-size: 13px; }
.add .primary, .add .full { margin-top: 10px; }
.links-part { margin-top: 26px; }
.fresh { margin: 10px 0; }
.copy-row { display: flex; gap: 8px; margin-top: 8px; }
.copy-row input { flex: 1; min-width: 0; font-family: var(--mono); font-size: 12.5px; }
@media (min-width: 48rem) {
	.fields { grid-template-columns: 2fr 1fr 1fr; align-items: end; }
	.link-fields { grid-template-columns: 1fr 1fr; }
}
</style>
