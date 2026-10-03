<script setup lang="ts">
type SessionInfo = {
	id: number;
	created_at: string;
	last_seen_at: string;
	user_agent: string | null;
	ip: string | null;
	current: boolean;
};

type DeviceInfo = {
	id: number;
	name: string;
	platform: string;
	created_at: string;
	last_seen_at: string;
	last_ip: string | null;
	confirm_until: string;
};
const PLATFORMS: Record<string, string> = { macos: 'Mac', ios: 'iPhone/iPad' };

const { me, load, set } = useSession();
const live = useLive();
useHead({ title: 'Einstellungen – xlrx drive' });
const look = ref<Appearance>({ paper: 'warm', ink: 'schwarz', landscape: true });
const host = ref('');
onMounted(() => {
	look.value = readAppearance();
	host.value = location.host.toUpperCase();
});
watch(look, (a) => saveAppearance(a), { deep: true });
const initial = computed(() => (me.value?.display_name || me.value?.username || '?').trim().charAt(0).toUpperCase());

async function logout() {
	await apiPost('/auth/logout');
	live.disconnect();
	set(null);
	await navigateTo('/login');
}
const g = useGuard();
const sessions = ref<SessionInfo[]>([]);
const devices = ref<DeviceInfo[]>([]);
const newPasskey = ref('');
const totpSetup = ref<null | { ceremony: string; qr_svg: string; secret: string }>(null);
const totpCode = ref('');
const codes = ref<string[] | null>(null);
const newPassword = ref('');
const notice = ref('');
const canPasskey = ref(false);

async function refresh() {
	await load();
	sessions.value = await apiGet<SessionInfo[]>('/me/sessions');
	devices.value = await apiGet<DeviceInfo[]>('/me/devices');
}
onMounted(() => {
	canPasskey.value = passkeysSupported();
	g.run(refresh);
});

const addPasskey = () =>
	g.run(async () => {
		const begin = await apiPost<PasskeyBegin>('/me/passkeys/begin', {
			name: newPasskey.value || 'Passkey'
		});
		const credential = await registerPasskey(begin.options);
		await apiPost('/me/passkeys/finish', { ceremony: begin.ceremony, credential });
		newPasskey.value = '';
		notice.value = 'Passkey hinzugefügt.';
		await refresh();
	});

const renamePasskey = (id: number, current: string) =>
	g.run(async () => {
		const name = prompt('Neuer Name', current);
		if (!name) return;
		await apiPatch(`/me/passkeys/${id}`, { name });
		await refresh();
	});

const removePasskey = (id: number) =>
	g.run(async () => {
		if (!confirm('Diesen Passkey entfernen?')) return;
		await apiDelete(`/me/passkeys/${id}`);
		await refresh();
	});

const beginTotp = () =>
	g.run(async () => {
		totpSetup.value = await apiPost('/me/totp/begin');
	});

const confirmTotp = () =>
	g.run(async () => {
		await apiPost('/me/totp/confirm', { ceremony: totpSetup.value?.ceremony, code: totpCode.value });
		totpSetup.value = null;
		totpCode.value = '';
		notice.value = 'Authenticator-App eingerichtet.';
		await refresh();
	});

const removeTotp = () =>
	g.run(async () => {
		if (!confirm('Authenticator-App entfernen?')) return;
		await apiDelete('/me/totp');
		await refresh();
	});

const newCodes = () =>
	g.run(async () => {
		if (!confirm('Neue Codes erzeugen? Die alten werden ungültig.')) return;
		codes.value = (await apiPost<{ recovery_codes: string[] }>('/me/recovery-codes')).recovery_codes;
	});

const changePassword = () =>
	g.run(async () => {
		await apiPost('/me/password', { password: newPassword.value });
		newPassword.value = '';
		notice.value = 'Passwort geändert. Andere Sitzungen wurden beendet.';
		await refresh();
	});

const revoke = (id: number) =>
	g.run(async () => {
		await apiDelete(`/me/sessions/${id}`);
		await refresh();
	});

const leaving = ref<DeviceInfo | null>(null);
const revokeDevice = (d: DeviceInfo) =>
	g.run(async () => {
		leaving.value = null;
		await apiDelete(`/me/devices/${d.id}`);
		await refresh();
	});

function codesDone() {
	codes.value = null;
	g.run(refresh);
}
</script>

<template>
	<main class="page with-scenery">
		<h1>Einstellungen</h1>
		<p v-if="notice" class="ok notice" role="status"><Icon name="check" :size="16" :stroke="2" />{{ notice }}</p>
		<p v-if="g.error.value" class="error" role="alert">{{ g.error.value }}</p>

		<section v-if="codes" class="card"><RecoveryCodes :codes="codes" @done="codesDone" /></section>
		<template v-else-if="me">
			<hr />
			<div class="profile">
				<span class="avatar big">{{ initial }}</span>
				<span class="who">
					<span class="display">{{ me.display_name }}</span>
					<span class="muted">{{ me.username }}</span>
					<span class="label online"><span class="dot"></span>Verbunden · {{ host }}</span>
				</span>
			</div>
			<hr />

			<section>
				<h2>Konto &amp; Sicherheit</h2>
				<h3><Icon name="key" :size="19" />Passkeys</h3>
				<ul v-if="me.passkeys.length" class="rows">
					<li v-for="pk in me.passkeys" :key="pk.id">
						<span class="text-col">
							<span class="name">{{ pk.name }}</span>
							<span class="sub">hinzugefügt {{ formatShortDate(pk.created_at) }} · zuletzt benutzt {{ formatShortDate(pk.last_used_at) || 'nie' }}</span>
						</span>
						<button class="link small" @click="renamePasskey(pk.id, pk.name)">Umbenennen</button>
						<button class="link small danger" @click="removePasskey(pk.id)">Entfernen</button>
					</li>
				</ul>
				<p v-else class="muted small">Noch kein Passkey. Mit Passkeys meldest du dich per Face ID oder Touch ID an.</p>
				<form v-if="canPasskey" class="inline" @submit.prevent="addPasskey">
					<input v-model="newPasskey" aria-label="Name des Passkeys" placeholder="Name, z.B. MacBook" />
					<button class="primary" :disabled="g.busy.value">Passkey hinzufügen</button>
				</form>

				<h3><Icon name="shield" :size="19" />Authenticator-App</h3>
				<template v-if="totpSetup">
					<p class="small">Den QR-Code scannen und den angezeigten Code eingeben.</p>
					<!-- SVG generated by the server; contains no user input as markup. -->
					<div class="qr" v-html="totpSetup.qr_svg"></div>
					<p class="muted small">Schlüssel: <span class="code">{{ totpSetup.secret }}</span></p>
					<form class="inline" @submit.prevent="confirmTotp">
						<input v-model="totpCode" class="code" inputmode="numeric" autocomplete="one-time-code" aria-label="Angezeigter Code" required />
						<button class="primary" :disabled="g.busy.value">Bestätigen</button>
						<button type="button" @click="totpSetup = null">Abbrechen</button>
					</form>
				</template>
				<p v-else-if="me.totp" class="line">
					<span>Eingerichtet.</span><span class="spacer"></span>
					<button class="link small" @click="beginTotp">Neu einrichten</button>
					<button class="link small danger" @click="removeTotp">Entfernen</button>
				</p>
				<p v-else class="line"><span>Nicht eingerichtet.</span><span class="spacer"></span><button class="link small" @click="beginTotp">Einrichten</button></p>

				<h3><Icon name="lock" :size="19" />Wiederherstellungscodes</h3>
				<p class="line">
					<span>Noch {{ me.recovery_codes_left }} von 10 übrig.</span><span class="spacer"></span>
					<button class="link small" @click="newCodes">Neue erzeugen</button>
				</p>

				<h3><Icon name="key" :size="19" />Passwort</h3>
				<form class="inline" @submit.prevent="changePassword">
					<input v-model="newPassword" type="password" autocomplete="new-password" minlength="12" placeholder="Neues Passwort" aria-label="Neues Passwort" required />
					<button :disabled="g.busy.value">Ändern</button>
				</form>
			</section>

			<section>
				<h2>Geräte</h2>
				<p v-if="!devices.length" class="muted small">Noch keine Geräte. Die Mac- und iPhone-App melden sich über den Browser an.</p>
				<ul v-else class="rows">
					<li v-for="d in devices" :key="d.id">
						<Icon name="laptop" :size="19" />
						<span class="text-col">
							<span class="name">{{ d.name }} <span class="tag">{{ PLATFORMS[d.platform] ?? d.platform }}</span></span>
							<span class="sub">zuletzt aktiv {{ formatShortDate(d.last_seen_at) }}<template v-if="d.last_ip"> · {{ d.last_ip }}</template> · Bestätigung bis {{ formatShortDate(d.confirm_until) }}</span>
						</span>
						<button class="link small danger" @click="leaving = d">Abmelden</button>
					</li>
				</ul>
			</section>

			<section>
				<h2>Angemeldete Sitzungen</h2>
				<ul class="rows">
					<li v-for="s in sessions" :key="s.id">
						<Icon name="globe" :size="19" />
						<span class="text-col">
							<span class="name">{{ s.user_agent ?? 'unbekannt' }}</span>
							<span class="sub">{{ s.ip ?? '–' }} · zuletzt aktiv {{ formatShortDate(s.last_seen_at) }}<template v-if="s.current"> · <strong>diese Sitzung</strong></template></span>
						</span>
						<button v-if="!s.current" class="link small danger" @click="revoke(s.id)">Abmelden</button>
					</li>
				</ul>
			</section>

			<section>
				<h2>Darstellung</h2>
				<div class="setting">
					<Icon name="moon" :size="19" />
					<span class="text-col"><span class="name">Papierton</span><span class="sub">Im dunklen Modus gilt immer das dunkle Papier.</span></span>
					<span class="swatches" role="radiogroup" aria-label="Papierton">
						<button v-for="p in PAPERS" :key="p.id" type="button" role="radio" :aria-checked="look.paper === p.id" :aria-label="p.label" :title="p.label" :style="{ background: p.color }" @click="look.paper = p.id"></button>
					</span>
				</div>
				<div class="setting">
					<Icon name="edit" :size="19" />
					<span class="text-col"><span class="name">Tinte</span><span class="sub">Farbe für Knöpfe und Auswahl.</span></span>
					<span class="swatches" role="radiogroup" aria-label="Tinte">
						<button v-for="i in INKS" :key="i.id" type="button" role="radio" :aria-checked="look.ink === i.id" :aria-label="i.label" :title="i.label" :style="{ background: i.color }" @click="look.ink = i.id"></button>
					</span>
				</div>
				<div class="setting">
					<Icon name="image" :size="19" />
					<span class="text-col"><span class="name">Landschaft auf Start</span><span class="sub">Nach Tageszeit, still bei „Bewegung reduzieren“.</span></span>
					<button type="button" class="switch" :aria-pressed="look.landscape" aria-label="Landschaft auf Start" @click="look.landscape = !look.landscape"></button>
				</div>
			</section>

			<button type="button" class="signout" @click="logout"><Icon name="logout" :size="19" />Abmelden</button>
			<p class="label footer">xlrx für das Web</p>
		</template>
		<Landscape class="scenery" time="Nacht" />
		<ConfirmDialog
			v-if="leaving"
			:title="`„${leaving.name}“ abmelden?`"
			text="Die App auf diesem Gerät muss sich danach neu anmelden."
			action="Gerät abmelden"
			danger
			@confirm="leaving && revokeDevice(leaving)"
			@cancel="leaving = null"
		/>
	</main>
	<StepUpDialog v-if="g.pending.value" @done="g.confirmed" @cancel="g.cancel" />
</template>

<style scoped>
.notice { display: flex; align-items: center; gap: 8px; }
.profile { display: flex; align-items: center; gap: 14px; padding: 16px 0; }
.avatar.big { width: 52px; height: 52px; font-size: 20px; font-weight: 400; }
.who { display: flex; flex-direction: column; gap: 3px; }
.display { font-size: 17px; }
.who .muted { font-size: 13px; }
.online { display: flex; align-items: center; gap: 6px; color: var(--ink-2); }
.dot { width: 6px; height: 6px; border-radius: 3px; background: var(--ok); }
section { padding: 6px 0 10px; }
h3 { display: flex; align-items: center; gap: 12px; font-weight: 400; margin: 18px 0 6px; }
.rows li { gap: 12px; min-height: 52px; padding: 6px 0; }
.text-col { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 2px; }
.name { font-size: 15px; overflow-wrap: anywhere; }
.sub { font-size: 12px; color: var(--muted); }
.small { font-size: 13px; }
.link.small { min-height: 32px; }
.line { display: flex; align-items: center; gap: 14px; margin: 0; min-height: 44px; border-bottom: 1px solid var(--line); font-size: 14px; }
.inline { display: flex; gap: 8px; flex-wrap: wrap; margin-top: 10px; }
.inline input { flex: 1; min-width: 12rem; max-width: 22rem; height: 44px; }
.setting { display: flex; align-items: center; gap: 14px; min-height: 58px; border-bottom: 1px solid var(--line); }
.setting:last-child { border-bottom: 0; }
.swatches { display: flex; gap: 8px; }
.swatches button { width: 28px; height: 28px; min-height: 0; padding: 0; border-radius: 14px; border: 1px solid var(--line-strong); }
.swatches button[aria-checked='true'] { box-shadow: 0 0 0 2px var(--paper), 0 0 0 3.5px var(--ink); }
.signout { margin-top: 14px; width: 100%; justify-content: flex-start; gap: 14px; border: 0; border-top: 1px dashed var(--dash); border-radius: 0; color: var(--danger); min-height: 52px; padding: 0; }
.footer { margin-top: 16px; }
</style>
