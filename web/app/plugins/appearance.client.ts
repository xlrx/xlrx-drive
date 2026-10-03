// Paper tone and ink chosen under "Darstellung" (kept in this browser), applied before the first
// page shows.
export default defineNuxtPlugin(() => {
	applyAppearance(readAppearance());
});
