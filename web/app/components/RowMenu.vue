<script setup lang="ts">
// "⋯" menu of a row: rename, move, delete (and whatever the slot adds).
defineProps<{ label: string }>();
const emit = defineEmits<{ rename: []; move: []; remove: [] }>();
const open = ref(false);
const root = ref<HTMLElement | null>(null);

function close(e: Event) {
	if (root.value && !root.value.contains(e.target as Node)) open.value = false;
}
function onKey(e: KeyboardEvent) {
	if (e.key === 'Escape') open.value = false;
}
onMounted(() => {
	document.addEventListener('click', close);
	document.addEventListener('keydown', onKey);
});
onBeforeUnmount(() => {
	document.removeEventListener('click', close);
	document.removeEventListener('keydown', onKey);
});

function pick(what: 'rename' | 'move' | 'remove') {
	open.value = false;
	if (what === 'rename') emit('rename');
	else if (what === 'move') emit('move');
	else emit('remove');
}
</script>

<template>
	<div ref="root" class="menu">
		<button
			class="dots"
			type="button"
			:aria-label="`Aktionen für ${label}`"
			aria-haspopup="menu"
			:aria-expanded="open"
			@click="open = !open"
		>
			⋯
		</button>
		<div v-if="open" class="popup card" role="menu">
			<button role="menuitem" type="button" @click="pick('rename')">Umbenennen</button>
			<button role="menuitem" type="button" @click="pick('move')">Verschieben</button>
			<button role="menuitem" type="button" class="danger" @click="pick('remove')">Löschen</button>
		</div>
	</div>
</template>

<style scoped>
.menu { position: relative; display: inline-block; }
.dots { border: 0; background: none; padding: 0.2rem 0.5rem; font-size: 1.1rem; line-height: 1; }
.popup { position: absolute; right: 0; top: 100%; z-index: 5; padding: 0.3rem; min-width: 10rem; display: flex; flex-direction: column; }
.popup button { border: 0; text-align: left; border-radius: 6px; }
.popup button:hover { background: var(--bg); }
</style>
