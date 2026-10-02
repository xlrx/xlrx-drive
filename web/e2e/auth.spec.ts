// Account setup, login with authenticator app and passkey, administration, browsing files – in a
// real browser.
import { createHmac } from 'node:crypto';
import { existsSync, readFileSync } from 'node:fs';
import { mkdir, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { expect, test, type Page } from '@playwright/test';

const PASSWORD = 'Wolken über dem Garten 7';

function base32Decode(s: string): Buffer {
	const a = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567';
	let bits = 0;
	let value = 0;
	const out: number[] = [];
	for (const c of s.replace(/\s/g, '')) {
		value = (value << 5) | a.indexOf(c);
		bits += 5;
		if (bits >= 8) {
			out.push((value >>> (bits - 8)) & 0xff);
			bits -= 8;
		}
	}
	return Buffer.from(out);
}

function totp(secret: Buffer, step: number): string {
	const msg = Buffer.alloc(8);
	msg.writeBigUInt64BE(BigInt(step));
	const h = createHmac('sha1', secret).update(msg).digest();
	const o = h[h.length - 1] & 0x0f;
	const bin = ((h[o] & 0x7f) << 24) | (h[o + 1] << 16) | (h[o + 2] << 8) | h[o + 3];
	return String(bin % 1_000_000).padStart(6, '0');
}

const step = () => Math.floor(Date.now() / 1000 / 30);

async function virtualAuthenticator(page: Page) {
	const cdp = await page.context().newCDPSession(page);
	await cdp.send('WebAuthn.enable');
	await cdp.send('WebAuthn.addVirtualAuthenticator', {
		options: {
			protocol: 'ctap2',
			transport: 'internal',
			hasResidentKey: true,
			hasUserVerification: true,
			isUserVerified: true,
			automaticPresenceSimulation: true
		}
	});
}

/** 8×8 pixels, blue. */
const PNG = 'iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAIAAABLbSncAAAAEUlEQVR4nGNQTX6NFTEMLQkADGRcwcht3uAAAAAASUVORK5CYII=';

const PDF =
	'%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n' +
	'3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]>>endobj\ntrailer<</Root 1 0 R>>\n%%EOF\n';

/** Browses "My Drive" of the signed-in admin: folders, preview, download, rescan. */
async function browseFiles(page: Page, data: string) {
	const drive = join(data, 'homes/admin/Drive');
	await mkdir(join(drive, 'Projekte'), { recursive: true });
	await mkdir(join(drive, 'Bilder'), { recursive: true });
	await writeFile(join(drive, 'Projekte/Plan.txt'), 'Erste Zeile\nZweite Zeile: äöü\n');
	await writeFile(join(drive, 'Projekte/seite.html'), '<script>alert("xss")</script>');
	await writeFile(join(drive, 'Bilder/Punkt.png'), Buffer.from(PNG, 'base64'));
	await writeFile(join(drive, 'Bericht.pdf'), PDF);
	page.on('dialog', () => {
		throw new Error('Benutzerinhalt hat ein Skript ausgeführt');
	});
	// Anything the browser refuses (CSP, framing) is a bug in the headers.
	const refused: string[] = [];
	page.on('console', (m) => {
		if (/refused|content security policy/i.test(m.text())) refused.push(m.text());
	});

	// The first visit imports the existing folder; the page shows the content as it arrives.
	await page.getByRole('link', { name: 'Dateien', exact: true }).click();
	await expect(page.getByRole('heading', { name: 'Meine Ablage' })).toBeVisible();
	await expect(page.getByRole('link', { name: 'Projekte', exact: true })).toBeVisible();

	// Image preview: loads under the strict CSP of the app.
	await page.getByRole('link', { name: 'Bilder', exact: true }).click();
	await page.getByRole('link', { name: 'Punkt.png', exact: true }).click();
	const img = page.getByRole('img', { name: 'Punkt.png' });
	await expect(img).toBeVisible();
	await expect.poll(() => img.evaluate((i: HTMLImageElement) => i.naturalWidth)).toBe(8);

	// Back via the breadcrumbs; text preview and download.
	await page.getByRole('navigation', { name: 'Pfad' }).getByRole('link', { name: 'Meine Ablage' }).click();
	await page.getByRole('link', { name: 'Projekte', exact: true }).click();
	await page.getByRole('link', { name: 'Plan.txt', exact: true }).click();
	await expect(page.locator('pre')).toContainText('Zweite Zeile: äöü');
	await expect(page.getByRole('link', { name: 'In neuem Tab' })).toBeVisible();
	const download = page.waitForEvent('download');
	await page.getByRole('link', { name: 'Herunterladen', exact: true }).click();
	expect((await download).suggestedFilename()).toBe('Plan.txt');

	// HTML is never shown, let alone run.
	await page.getByRole('navigation', { name: 'Pfad' }).getByRole('link', { name: 'Projekte', exact: true }).click();
	await page.getByRole('link', { name: 'seite.html', exact: true }).click();
	await expect(page.getByText('Keine Vorschau für diesen Dateityp.')).toBeVisible();
	await expect(page.getByRole('link', { name: 'In neuem Tab' })).toHaveCount(0);

	// A file added on the NAS (SMB, File Station) shows up by itself within seconds: watcher,
	// journal, live event, reload of the view.
	await page.getByRole('navigation', { name: 'Pfad' }).getByRole('link', { name: 'Projekte', exact: true }).click();
	await expect(page.getByRole('heading', { name: 'Projekte' })).toBeVisible();
	await writeFile(join(drive, 'Projekte/Neu.txt'), 'neu');
	await expect(page.getByRole('link', { name: 'Neu.txt', exact: true })).toBeVisible({ timeout: 10_000 });
	// "Neu einlesen" finds nothing left to do.
	await page.getByRole('button', { name: 'Neu einlesen' }).click();
	await expect(page.getByText('Keine Änderungen.')).toBeVisible();

	// PDF: shown in a frame of the app.
	await page.getByRole('navigation', { name: 'Pfad' }).getByRole('link', { name: 'Meine Ablage' }).click();
	// (Not "networkidle": the live event stream keeps the connection open.)
	const pdf = page.waitForResponse((r) => r.url().includes('/content?inline=true'));
	await page.getByRole('link', { name: 'Bericht.pdf', exact: true }).click();
	const frame = page.locator('iframe[title="Bericht.pdf"]');
	await expect(frame).toBeVisible();
	expect((await pdf).status()).toBe(200);
	await page.waitForTimeout(500);
	expect(refused).toEqual([]);
}

/** Changes through the web app: folder, upload with conflicts, versions, rename, move, trash. */
async function changeFiles(page: Page, data: string) {
	const drive = join(data, 'homes/admin/Drive');
	const nav = () => page.getByRole('navigation', { name: 'Pfad' });
	const link = (name: string) => page.getByRole('link', { name, exact: true });
	const menu = async (name: string, action: string) => {
		await page.getByRole('button', { name: `Aktionen für ${name}` }).click();
		await page.getByRole('menuitem', { name: action }).click();
	};
	const upload = async (name: string, content: string) => {
		await page.getByTestId('upload-input').setInputFiles({ name, mimeType: 'text/plain', buffer: Buffer.from(content) });
	};

	await page.getByRole('link', { name: 'Dateien', exact: true }).click();
	await page.getByRole('button', { name: 'Neuer Ordner' }).click();
	await page.getByLabel('Name').fill('Belege');
	await page.getByRole('button', { name: 'Anlegen' }).click();
	await link('Belege').click();
	await expect(page.getByRole('heading', { name: 'Belege' })).toBeVisible();

	// Changes from elsewhere (another device, SMB) appear without reloading the page.
	const url = new URL(page.url());
	const here = url.pathname.split('/').pop();
	const created = await page.request.post(`/api/nodes/${here}/folders`, {
		data: { name: 'Von woanders' },
		headers: { origin: url.origin }
	});
	expect(created.status()).toBe(201);
	await expect(link('Von woanders')).toBeVisible();

	// Upload; the same name again: keep both, then replace (the old content becomes a version).
	await upload('Rechnung.txt', 'Rechnung 1');
	await expect(link('Rechnung.txt')).toBeVisible();
	await upload('Rechnung.txt', 'Rechnung 2');
	await page.getByRole('button', { name: 'Beide behalten' }).click();
	await expect(link('Rechnung (1).txt')).toBeVisible();
	await upload('rechnung.TXT', 'Rechnung 3');
	await page.getByRole('button', { name: 'Ersetzen' }).click();
	await expect(page.getByText('Uploads abgeschlossen')).toBeVisible();
	await page.getByRole('button', { name: 'Uploads schließen' }).click();
	await link('Rechnung.txt').click();
	await expect(page.locator('pre')).toHaveText('Rechnung 3');
	const versions = page.locator('section.versions tbody tr');
	await expect(versions).toHaveCount(1);
	await versions.getByRole('button', { name: 'Wiederherstellen' }).click();
	await expect(page.locator('pre')).toHaveText('Rechnung 1');
	await expect(versions).toHaveCount(2);

	// Rename and move.
	await nav().getByRole('link', { name: 'Belege' }).click();
	await menu('Rechnung (1).txt', 'Umbenennen');
	await page.getByLabel('Name').fill('Quittung.txt');
	await page.getByRole('button', { name: 'Umbenennen' }).click();
	await expect(link('Quittung.txt')).toBeVisible();
	await menu('Quittung.txt', 'Verschieben');
	const dialog = page.getByRole('dialog');
	await dialog.getByRole('button', { name: 'Meine Ablage' }).click();
	await dialog.getByRole('button', { name: 'Projekte' }).click();
	await dialog.getByRole('button', { name: 'Hierher verschieben' }).click();
	await expect(page.getByText('„Quittung.txt“ wurde verschoben.')).toBeVisible();
	await expect(link('Quittung.txt')).toHaveCount(0);
	expect(readFileSync(join(drive, 'Projekte/Quittung.txt'), 'utf8')).toBe('Rechnung 2');

	// Delete, undo, delete again, restore from the trash.
	await menu('Rechnung.txt', 'Löschen');
	await expect(page.getByText('„Rechnung.txt“ liegt jetzt im Papierkorb.')).toBeVisible();
	expect(existsSync(join(drive, 'Belege/Rechnung.txt'))).toBe(false);
	await page.getByRole('button', { name: 'Rückgängig' }).click();
	await expect(link('Rechnung.txt')).toBeVisible();
	await menu('Rechnung.txt', 'Löschen');
	await page.getByRole('link', { name: 'Papierkorb', exact: true }).click();
	const row = page.getByRole('row', { name: /Rechnung\.txt/ });
	await expect(row).toContainText('Meine Ablage/Belege');
	await row.getByRole('button', { name: 'Wiederherstellen' }).click();
	await expect(page.getByText('„Rechnung.txt“ ist wieder da.')).toBeVisible();
	await expect(page.getByText('Der Papierkorb ist leer.')).toBeVisible();
	expect(readFileSync(join(drive, 'Belege/Rechnung.txt'), 'utf8')).toBe('Rechnung 1');
}

test('Einrichtung, Anmeldung, Verwaltung und Dateien', async ({ page }) => {
	const setupUrl = process.env.XLRX_SETUP_URL;
	test.skip(!setupUrl, 'XLRX_SETUP_URL fehlt (e2e/run.sh verwenden)');
	await virtualAuthenticator(page);

	// Setup from the link: password, authenticator app, recovery codes.
	await page.goto(setupUrl!);
	await expect(page.getByRole('heading', { name: 'Konto einrichten' })).toBeVisible();
	await expect(page).toHaveURL(/\/setup$/); // token removed from the address bar
	await page.getByLabel('Neues Passwort').fill(PASSWORD);
	await page.getByLabel('Passwort wiederholen').fill(PASSWORD);
	await page.getByRole('button', { name: 'Weiter' }).click();
	await page.getByRole('button', { name: 'Authenticator-App einrichten' }).click();
	const secret = base32Decode(await page.locator('p.muted .code').innerText());
	await page.getByLabel('Angezeigter Code').fill(totp(secret, step()));
	await page.getByRole('button', { name: 'Bestätigen' }).click();
	await expect(page.getByRole('heading', { name: 'Wiederherstellungscodes' })).toBeVisible();
	await expect(page.locator('.codes span')).toHaveCount(10);
	await page.getByLabel('Ich habe die Codes sicher gespeichert.').check();
	await page.getByRole('button', { name: 'Fertig' }).click();
	await expect(page.getByRole('heading', { name: /Hallo/ })).toBeVisible();

	// Add a passkey (the login counts as a fresh second factor).
	await page.getByRole('link', { name: 'Sicherheit' }).click();
	await page.getByPlaceholder('Name, z.B. MacBook').fill('Testgerät');
	await page.getByRole('button', { name: 'Passkey hinzufügen' }).click();
	await expect(page.getByRole('cell', { name: 'Testgerät' })).toBeVisible();

	// Log out, log in with the passkey alone.
	await page.getByRole('button', { name: 'Abmelden' }).first().click();
	await expect(page.getByRole('heading', { name: 'Anmelden' })).toBeVisible();
	await page.getByLabel('Benutzername').fill('admin');
	await page.getByRole('button', { name: 'Mit Passkey anmelden' }).click();
	await expect(page.getByRole('heading', { name: /Hallo/ })).toBeVisible();

	// Log out, log in with password + code (next time step: the setup code must not work again).
	await page.getByRole('button', { name: 'Abmelden' }).click();
	await page.getByLabel('Benutzername').fill('admin');
	await page.getByLabel('Passwort').fill(PASSWORD);
	await page.getByRole('button', { name: 'Weiter' }).click();
	await page.getByLabel('Code aus der Authenticator-App').fill(totp(secret, step() + 1));
	await page.getByRole('button', { name: 'Anmelden' }).click();
	await expect(page.getByRole('heading', { name: /Hallo/ })).toBeVisible();

	// Administration: new account with a setup link.
	await page.getByRole('link', { name: 'Verwaltung' }).click();
	await page.getByLabel('Benutzername').fill('bert');
	await page.getByLabel('Anzeigename').fill('Bert');
	await page.getByRole('button', { name: /Anlegen/ }).click();
	await expect(page.getByText('Einrichtungslink für bert')).toBeVisible();
	await expect(page.locator('input[readonly]')).toHaveValue(/\/setup#/);
	await expect(page.getByRole('cell', { name: 'user_created' }).first()).toBeVisible();

	const data = process.env.XLRX_E2E_DATA;
	if (data) {
		await browseFiles(page, data);
		await changeFiles(page, data);
	}
});

test('Falsches Passwort zeigt einheitliche Meldung', async ({ page }) => {
	await page.goto('/login');
	await page.getByLabel('Benutzername').fill('gibtsnicht');
	await page.getByLabel('Passwort').fill('falsch falsch falsch');
	await page.getByRole('button', { name: 'Weiter' }).click();
	await expect(page.getByRole('alert')).toHaveText('Benutzername oder Passwort ist falsch.');
});
