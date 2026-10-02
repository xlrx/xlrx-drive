<script setup lang="ts">
// Entry point: forwards to the root folder of "My Drive".
const error = ref('');

onMounted(async () => {
	try {
		const roots = await apiGet<RootInfo[]>('/roots');
		const home = roots[0];
		if (home) await navigateTo(`/files/${home.node_id}`, { replace: true });
		else error.value = 'Auf diesem Server ist noch kein Datenverzeichnis eingerichtet (XLRX_DATA_DIR).';
	} catch (e) {
		error.value = errorMessage(e);
	}
});
</script>

<template>
	<main class="page">
		<p v-if="error" class="error" role="alert">{{ error }}</p>
		<p v-else class="muted">Wird geladen …</p>
	</main>
</template>
