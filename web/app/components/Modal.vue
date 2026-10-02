<script setup lang="ts">
// A modal dialog: backdrop, card, title; Escape or a click on the backdrop cancels.
defineProps<{ title: string }>();
const emit = defineEmits<{ cancel: [] }>();
const id = useId();

function onKey(e: KeyboardEvent) {
	if (e.key === 'Escape') emit('cancel');
}
onMounted(() => window.addEventListener('keydown', onKey));
onBeforeUnmount(() => window.removeEventListener('keydown', onKey));
</script>

<template>
	<div class="backdrop" role="presentation" @click.self="emit('cancel')">
		<div class="card dialog" role="dialog" aria-modal="true" :aria-labelledby="id">
			<h2 :id="id">{{ title }}</h2>
			<slot />
		</div>
	</div>
</template>

<style scoped>
.backdrop {
	position: fixed;
	inset: 0;
	background: rgb(0 0 0 / 40%);
	display: grid;
	place-items: center;
	padding: 1rem;
	z-index: 10;
}
.dialog { width: min(28rem, 100%); max-height: calc(100vh - 2rem); overflow: auto; }
h2 { margin-top: 0; }
</style>
