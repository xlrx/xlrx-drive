// Führt eine Aktion aus; verlangt der Server einen frischen zweiten Faktor, wird nach der Bestätigung
// (Dialog `StepUp`) automatisch erneut versucht.
import { ApiError, message } from './api';

export class Guard {
	pending = $state<null | (() => Promise<void>)>(null);
	error = $state('');
	busy = $state(false);

	run = async (fn: () => Promise<void>) => {
		this.error = '';
		this.busy = true;
		try {
			await fn();
		} catch (e) {
			if (e instanceof ApiError && e.stepUp) this.pending = fn;
			else this.error = message(e);
		} finally {
			this.busy = false;
		}
	};

	confirmed = async () => {
		const fn = this.pending;
		this.pending = null;
		if (fn) await this.run(fn);
	};

	cancel = () => {
		this.pending = null;
	};
}
