<script setup lang="ts">
// A sheet (as in the design): slides up from the bottom on narrow screens, sits in the middle on
// wide ones. Escape or a click beside it cancels. The head shows the title and a close button,
// unless the slot `head` brings its own (e.g. "Abbrechen · Titel · Fertig").
withDefaults(defineProps<{ title: string; role?: 'dialog' | 'alertdialog'; wide?: boolean }>(), {
	role: 'dialog',
	wide: false
});
const emit = defineEmits<{ cancel: [] }>();
const id = useId();

function onKey(e: KeyboardEvent) {
	if (e.key === 'Escape') emit('cancel');
}
onMounted(() => window.addEventListener('keydown', onKey));
onBeforeUnmount(() => window.removeEventListener('keydown', onKey));
</script>

<template>
	<Teleport to="body">
		<div class="scrim" role="presentation" @click.self="emit('cancel')">
			<div class="sheet" :class="{ wide }" :role="role" aria-modal="true" :aria-labelledby="id">
				<span class="handle" aria-hidden="true"></span>
				<slot name="head" :id="id">
					<div class="head">
						<h2 :id="id">{{ title }}</h2>
						<button type="button" class="icon close" aria-label="Schließen" @click="emit('cancel')">
							<span class="ring"><Icon name="x" :size="13" :stroke="2" /></span>
						</button>
					</div>
				</slot>
				<slot />
			</div>
		</div>
	</Teleport>
</template>

<style scoped>
.scrim {
	position: fixed; inset: 0; z-index: 40; background: var(--scrim);
	display: flex; align-items: center; justify-content: center; padding: 24px;
}
.sheet {
	position: relative; width: min(30rem, 100%); max-height: calc(100vh - 48px); overflow: auto;
	background: var(--paper); color: var(--ink); border: 1px solid var(--line-strong); border-radius: 18px;
	box-shadow: var(--shadow-float); padding: 8px 24px 26px;
}
.sheet.wide { width: min(40rem, 100%); }
.handle { display: none; }
.head { display: flex; align-items: center; justify-content: space-between; gap: 12px; padding: 10px 0 8px; margin-right: -14px; }
.head h2 { margin: 0; font-size: 22px; font-weight: 400; letter-spacing: -0.02em; color: var(--ink); }
.ring { width: 30px; height: 30px; border-radius: 15px; border: 1px solid var(--line-strong); display: flex; align-items: center; justify-content: center; }

@media (max-width: 40rem) {
	.scrim { align-items: flex-end; padding: 0; }
	.sheet, .sheet.wide {
		width: 100%; max-height: calc(100vh - 24px); border: 0; border-top: 1px solid var(--line-strong);
		border-radius: 18px 18px 0 0; box-shadow: var(--shadow-sheet); padding-bottom: calc(28px + env(safe-area-inset-bottom));
		animation: rise 0.18s ease-out;
	}
	.handle { display: block; margin: 0 auto 4px; width: 34px; height: 4px; border-radius: 2px; background: var(--line-strong); }
}
@keyframes rise { from { transform: translateY(24px); opacity: 0; } }
@media (prefers-reduced-motion: reduce) { .sheet { animation: none !important; } }
</style>
