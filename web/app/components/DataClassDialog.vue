<script setup lang="ts">
// Data class of a folder (PLAN 7.4, "Datenklassen der Ordner" in the design's settings): may its
// content ever be analysed outside the house? Set per folder, valid for everything inside unless a
// folder further down says otherwise. Changing it needs a fresh second factor and is logged.
const props = defineProps<{ nodeId: number }>();
const emit = defineEmits<{ close: []; changed: [] }>();
const g = useGuard();
const node = ref<NodeDetail | null>(null);
const choice = ref<'local' | 'cloud' | 'inherit'>('local');

async function load() {
	try {
		node.value = await apiGet<NodeDetail>(`/nodes/${props.nodeId}`);
		choice.value = node.value.data_class.explicit ? node.value.data_class.class : 'inherit';
	} catch (e) {
		g.error.value = errorMessage(e);
	}
}
onMounted(load);

const isRoot = computed(() => (node.value?.path.length ?? 0) <= 1 && !node.value?.shared);

const save = () =>
	g.run(async () => {
		node.value = await api<NodeDetail>('PUT', `/nodes/${props.nodeId}/data-class`, { class: choice.value });
		emit('changed');
		emit('close');
	});
</script>

<template>
	<Modal :title="node ? `Datenklasse von „${node.name}“` : 'Datenklasse'" @cancel="emit('close')">
		<p v-if="g.error.value" class="error" role="alert">{{ g.error.value }}</p>
		<template v-if="node">
			<p class="now">
				Zurzeit <strong>{{ CLASS_LABEL[node.data_class.class] }}</strong> · {{ classSource(node.data_class) }}
			</p>
			<fieldset :disabled="!node.data_class.can_change || g.busy.value">
				<legend class="sr-only">Datenklasse</legend>
				<label class="option" :class="{ on: choice === 'local' }">
					<input v-model="choice" type="radio" value="local" />
					<span class="ico-box"><Icon name="lock" :size="20" /></span>
					<span class="text">
						<strong>Nur lokal</strong>
						<span>Alles bleibt auf dem NAS – Suche, Texterkennung und später auch die KI-Suche laufen nur hier. Für Gesundheitsdaten, Verträge und Daten Dritter.</span>
					</span>
				</label>
				<label class="option" :class="{ on: choice === 'cloud' }">
					<input v-model="choice" type="radio" value="cloud" />
					<span class="ico-box"><Icon name="cloud" :size="20" /></span>
					<span class="text">
						<strong>Cloud erlaubt</strong>
						<span>Für die KI-Suche (kommt in einem späteren Schritt) dürfen Texte und verkleinerte Bilder – ohne Dateinamen und Ortsdaten – an einen Dienst in der EU. Die Dateien selbst bleiben auf dem NAS.</span>
					</span>
				</label>
				<label v-if="!isRoot" class="option" :class="{ on: choice === 'inherit' }">
					<input v-model="choice" type="radio" value="inherit" />
					<span class="ico-box"><Icon name="folder" :size="20" /></span>
					<span class="text">
						<strong>Wie der Ordner darüber</strong>
						<span>Keine eigene Einstellung: gilt, was weiter oben festgelegt ist.</span>
					</span>
				</label>
			</fieldset>
			<template v-if="node.data_class.can_change">
				<p class="muted small">Gilt für alles in diesem Ordner. Ändern verlangt eine erneute Bestätigung und wird protokolliert.</p>
				<button class="primary full" type="button" :disabled="g.busy.value" @click="save">Speichern</button>
			</template>
			<p v-else class="muted small">Ändern kann nur, wer den Ordner verwaltet.</p>
		</template>
		<StepUpDialog v-if="g.pending.value" @done="g.confirmed" @cancel="g.cancel" />
	</Modal>
</template>

<style scoped>
.now { margin: 0 0 12px; font-size: 14px; color: var(--ink-3); }
fieldset { border: 0; padding: 0; margin: 0 0 12px; display: flex; flex-direction: column; gap: 8px; }
.option {
	display: flex; gap: 12px; align-items: flex-start; margin: 0; padding: 12px 14px; border: 1px solid var(--line-strong);
	border-radius: 14px; cursor: pointer; color: var(--ink);
}
.option.on { border-color: var(--ink); background: var(--fill); }
.option input { margin-top: 4px; accent-color: var(--accent); }
.ico-box { flex: none; color: var(--ink-3); margin-top: 1px; }
.text { display: flex; flex-direction: column; gap: 3px; font-size: 13.5px; line-height: 1.45; }
.text strong { font-size: 15px; font-weight: 500; }
.text span { color: var(--muted); }
.small { font-size: 13px; }
</style>
