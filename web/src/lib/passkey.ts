// Passkeys im Browser. Der Server (webauthn-rs) liefert die Optionen als `{ publicKey: … }` mit
// Base64url-Feldern; @simplewebauthn/browser übernimmt die Umwandlung. Die Antwort geht unverändert
// zurück (webauthn-rs liest `clientExtensionResults` direkt).
import { browserSupportsWebAuthn, startAuthentication, startRegistration } from '@simplewebauthn/browser';

type Options = { publicKey: Record<string, unknown> };

export const passkeysSupported = () => browserSupportsWebAuthn();

export async function register(options: Options) {
	// eslint-disable-next-line @typescript-eslint/no-explicit-any
	return startRegistration({ optionsJSON: options.publicKey as any });
}

export async function authenticate(options: Options) {
	// eslint-disable-next-line @typescript-eslint/no-explicit-any
	return startAuthentication({ optionsJSON: options.publicKey as any });
}
