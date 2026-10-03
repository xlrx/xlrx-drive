<script setup lang="ts">
// "Teilen" (as the "Zugriff" list in the design's details): who has access – owner or members of
// the shared root, and the people and groups it is shared with, also through a folder above.
// Whoever manages it changes roles, ends shares and adds people or groups, optionally until a day.
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

async function load() {
	try {
		info.value = await apiGet<AccessInfo>(`/nodes/${props.node.id}/shares`);
		if (info.value.can_share && !people.value) people.value = await apiGet<People>('/people');
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
		</template>
		<p v-else-if="!error" class="muted">Lädt …</p>
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
.add .primary { margin-top: 10px; }
@media (min-width: 48rem) {
	.fields { grid-template-columns: 2fr 1fr 1fr; align-items: end; }
}
</style>
