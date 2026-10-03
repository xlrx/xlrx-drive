<script setup lang="ts">
// Frame of the app (as in the design): on wide screens a bar on top with the brand, the sections,
// "Neu" and the account; on phones the brand on top and a floating bar at the bottom with "+".
// Uploads and their questions belong to the whole app, so they go on while browsing.
const route = useRoute();
const { me } = useSession();
const live = useLive();
const uploads = useUploads();
const isPublic = computed(() => ['/login', '/setup'].includes(route.path.replace(/\/+$/, '')));
const showNew = ref(false);
const initial = computed(() => (me.value?.display_name || me.value?.username || '?').trim().charAt(0).toUpperCase());

// Live updates while signed in.
watch(
	me,
	(m) => {
		if (m) live.connect();
		else live.disconnect();
	},
	{ immediate: true }
);

const sections = computed(() => [
	{ to: '/', label: 'Start', icon: 'home' as const, exact: true },
	{ to: '/files', label: 'Dateien', icon: 'folder' as const },
	{ to: '/search', label: 'Suche', icon: 'search' as const },
	{ to: '/shared', label: 'Geteilt', icon: 'shared' as const },
	{ to: '/activity', label: 'Aktivität', icon: 'clock' as const },
	{ to: '/trash', label: 'Papierkorb', icon: 'trash' as const }
]);
// The bar at the bottom of phones holds four, as in the design; the trash is in the folder view,
// the activity on the start page.
const docked = computed(() => sections.value.filter((s) => s.to !== '/trash' && s.to !== '/activity'));
const current = (s: { to: string; exact?: boolean }) =>
	s.exact ? route.path === s.to : route.path === s.to || route.path.startsWith(`${s.to}/`);
</script>

<template>
	<div v-if="me && !isPublic" class="frame">
		<header class="top">
			<NuxtLink to="/" class="brand" aria-label="xlrx – Start"><span class="mark" aria-hidden="true"><span></span></span>xlrx</NuxtLink>
			<nav class="sections" aria-label="Hauptnavigation">
				<NuxtLink v-for="s in sections" :key="s.to" :to="s.to" :aria-current="current(s) ? 'page' : undefined">{{ s.label }}</NuxtLink>
				<NuxtLink v-if="me.is_admin" to="/admin" :aria-current="current({ to: '/admin' }) ? 'page' : undefined">Verwaltung</NuxtLink>
			</nav>
			<span class="spacer"></span>
			<button type="button" class="primary new" @click="showNew = true"><Icon name="plus" :size="18" :stroke="1.7" />Neu</button>
			<Bell />
			<NuxtLink to="/settings/security" class="icon account" aria-label="Konto und Einstellungen">
				<span class="avatar">{{ initial }}</span>
			</NuxtLink>
		</header>
		<slot />
		<div class="dock">
			<nav class="tabs-bottom" aria-label="Navigation">
				<NuxtLink v-for="s in docked" :key="s.to" :to="s.to" :aria-current="current(s) ? 'page' : undefined">
					<Icon :name="s.icon" /><span>{{ s.label }}</span>
				</NuxtLink>
			</nav>
			<button type="button" class="plus" aria-label="Neu: Hochladen oder Ordner" @click="showNew = true">
				<Icon name="plus" :size="22" :stroke="1.7" />
			</button>
		</div>
		<NewSheet v-if="showNew" @close="showNew = false" />
		<ConflictDialog
			v-if="uploads.question.value"
			:name="uploads.question.value.name"
			:can-replace="uploads.question.value.canReplace"
			:more="uploads.question.value.more"
			@choose="(c, all) => uploads.question.value?.answer(c, all)"
		/>
		<UploadQueue v-if="!showNew" :items="uploads.items.value" :active="uploads.active.value" @close="uploads.clear()" @resume="uploads.resume" />
	</div>
	<slot v-else />
</template>

<style scoped>
.frame { min-height: 100vh; }
.top {
	position: sticky; top: 0; z-index: 20; display: flex; align-items: center; gap: 28px;
	padding: 10px 20px 10px 24px; background: color-mix(in srgb, var(--paper) 92%, transparent);
	backdrop-filter: blur(8px); border-bottom: 1px dashed var(--dash);
}
.brand { display: flex; align-items: center; gap: 9px; font-size: 18px; font-weight: 500; letter-spacing: -0.01em; text-decoration: none; }
.mark { width: 18px; height: 18px; background: var(--accent); display: flex; align-items: center; justify-content: center; }
.mark span { width: 7px; height: 7px; background: var(--paper); }
.sections { display: flex; gap: 6px; }
.sections a { padding: 8px 12px; border-radius: 18px; font-size: 14px; text-decoration: none; color: var(--ink-3); }
.sections a:hover { background: var(--fill); color: var(--ink); }
.sections a[aria-current='page'] { background: var(--accent); color: var(--accent-text); }
.new { min-height: 40px; padding: 0 16px 0 12px; font-size: 14px; }
.account { margin-left: -18px; }
.top :deep(.bell) { margin-left: -12px; }

.dock { display: none; }
@media (max-width: 47.99rem) {
	.top { padding: 8px 6px 8px 20px; gap: 12px; }
	.sections, .new { display: none; }
	.dock {
		position: fixed; left: 14px; right: 14px; bottom: calc(18px + env(safe-area-inset-bottom)); z-index: 25;
		display: flex; gap: 8px; align-items: center;
	}
	.tabs-bottom {
		flex: 1; height: 58px; display: flex; align-items: center; padding: 0 4px; background: var(--paper);
		border: 1px solid var(--line-strong); border-radius: 29px; box-shadow: var(--shadow-float);
	}
	.tabs-bottom a {
		flex: 1; height: 48px; border-radius: 24px; display: flex; flex-direction: column; align-items: center; justify-content: center;
		gap: 2px; text-decoration: none; color: var(--ink-3); font-size: 10.5px; font-weight: 500; letter-spacing: 0.01em;
	}
	.tabs-bottom a[aria-current='page'] { background: var(--accent); color: var(--accent-text); }
	.plus {
		flex: none; width: 58px; height: 58px; min-height: 0; padding: 0; border-radius: 29px; border: 0;
		background: var(--accent); color: var(--accent-text); box-shadow: var(--shadow-float);
	}
	.plus:hover:not(:disabled) { background: var(--accent); }
}
</style>
