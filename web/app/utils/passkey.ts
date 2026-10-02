// Passkeys in the browser. The server (webauthn-rs) sends options as `{ publicKey: … }` with
// base64url fields; @simplewebauthn/browser converts them. The response goes back unchanged
// (webauthn-rs reads `clientExtensionResults` directly).
import {
	browserSupportsWebAuthn,
	startAuthentication,
	startRegistration
} from '@simplewebauthn/browser';

type Options = { publicKey: Record<string, unknown> };

export const passkeysSupported = () => browserSupportsWebAuthn();

export function registerPasskey(options: Options) {
	// eslint-disable-next-line @typescript-eslint/no-explicit-any
	return startRegistration({ optionsJSON: options.publicKey as any });
}

export function authenticatePasskey(options: Options) {
	// eslint-disable-next-line @typescript-eslint/no-explicit-any
	return startAuthentication({ optionsJSON: options.publicKey as any });
}
