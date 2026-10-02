<script setup lang="ts">
const route = useRoute();
const { me, set } = useSession();
const isPublic = computed(() => ['/login', '/setup'].includes(route.path.replace(/\/+$/, '')));

async function logout() {
	await apiPost('/auth/logout');
	set(null);
	await navigateTo('/login');
}
</script>

<template>
	<header v-if="me && !isPublic">
		<nav class="page row">
			<NuxtLink class="brand" to="/">xlrx drive</NuxtLink>
			<NuxtLink to="/files">Dateien</NuxtLink>
			<NuxtLink to="/trash">Papierkorb</NuxtLink>
			<span class="spacer"></span>
			<NuxtLink to="/settings/security">Sicherheit</NuxtLink>
			<NuxtLink v-if="me.is_admin" to="/admin">Verwaltung</NuxtLink>
			<span class="muted">{{ me.display_name }}</span>
			<button @click="logout">Abmelden</button>
		</nav>
	</header>
	<slot />
</template>

<style scoped>
header { background: var(--surface); border-bottom: 1px solid var(--border); }
nav { margin-top: 0; margin-bottom: 0; padding-top: 0.6rem; padding-bottom: 0.6rem; gap: 1rem; }
.brand { font-weight: 700; text-decoration: none; color: var(--text); }
.spacer { flex: 1; }
nav a { text-decoration: none; }
</style>
