// Einrichtung, Anmeldung mit Authenticator-App und Passkey, Verwaltung – im echten Browser.
import { createHmac } from 'node:crypto';
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

test('Einrichtung, Anmeldung und Verwaltung', async ({ page }) => {
	const setupUrl = process.env.XLRX_SETUP_URL;
	test.skip(!setupUrl, 'XLRX_SETUP_URL fehlt (e2e/run.sh verwenden)');
	await virtualAuthenticator(page);

	// Einrichtung über den Link: Passwort, Authenticator-App, Wiederherstellungscodes.
	await page.goto(setupUrl!);
	await expect(page.getByRole('heading', { name: 'Konto einrichten' })).toBeVisible();
	await expect(page).toHaveURL(/\/setup$/); // Token aus der Adresszeile entfernt
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

	// Passkey hinzufügen (die Anmeldung gilt als frischer zweiter Faktor).
	await page.getByRole('link', { name: 'Sicherheit' }).click();
	await page.getByPlaceholder('Name, z.B. MacBook').fill('Testgerät');
	await page.getByRole('button', { name: 'Passkey hinzufügen' }).click();
	await expect(page.getByRole('cell', { name: 'Testgerät' })).toBeVisible();

	// Abmelden, mit Passkey allein anmelden.
	await page.getByRole('button', { name: 'Abmelden' }).first().click();
	await expect(page.getByRole('heading', { name: 'Anmelden' })).toBeVisible();
	await page.getByLabel('Benutzername').fill('admin');
	await page.getByRole('button', { name: 'Mit Passkey anmelden' }).click();
	await expect(page.getByRole('heading', { name: /Hallo/ })).toBeVisible();

	// Abmelden, mit Passwort + Code anmelden (nächster Zeitschritt: der Einrichtungscode gilt nicht erneut).
	await page.getByRole('button', { name: 'Abmelden' }).click();
	await page.getByLabel('Benutzername').fill('admin');
	await page.getByLabel('Passwort').fill(PASSWORD);
	await page.getByRole('button', { name: 'Weiter' }).click();
	await page.getByLabel('Code aus der Authenticator-App').fill(totp(secret, step() + 1));
	await page.getByRole('button', { name: 'Anmelden' }).click();
	await expect(page.getByRole('heading', { name: /Hallo/ })).toBeVisible();

	// Verwaltung: neues Konto mit Einrichtungslink.
	await page.getByRole('link', { name: 'Verwaltung' }).click();
	await page.getByLabel('Benutzername').fill('bert');
	await page.getByLabel('Anzeigename').fill('Bert');
	await page.getByRole('button', { name: /Anlegen/ }).click();
	await expect(page.getByText('Einrichtungslink für bert')).toBeVisible();
	await expect(page.locator('input[readonly]')).toHaveValue(/\/setup#/);
	await expect(page.getByRole('cell', { name: 'user_created' }).first()).toBeVisible();
});

test('Falsches Passwort zeigt einheitliche Meldung', async ({ page }) => {
	await page.goto('/login');
	await page.getByLabel('Benutzername').fill('gibtsnicht');
	await page.getByLabel('Passwort').fill('falsch falsch falsch');
	await page.getByRole('button', { name: 'Weiter' }).click();
	await expect(page.getByRole('alert')).toHaveText('Benutzername oder Passwort ist falsch.');
});
