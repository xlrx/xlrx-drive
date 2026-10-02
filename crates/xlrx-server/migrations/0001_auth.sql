-- Konten und Anmeldung (PLAN 13.3, 16.1). Läuft auf PostgreSQL 16 (Tests) und 18 (Betrieb).

CREATE TABLE users (
    id                bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    -- Stabile, nicht erratbare Kennung; dient auch als WebAuthn-User-Handle.
    uuid              uuid NOT NULL UNIQUE DEFAULT gen_random_uuid(),
    username          text NOT NULL,
    username_folded   text NOT NULL UNIQUE,
    display_name      text NOT NULL,
    email             text,
    -- argon2id im PHC-Format. NULL, bis die Einladung angenommen wurde.
    password_hash     text,
    -- AES-256-GCM (Nonce ‖ Chiffrat), Schlüssel aus einem Docker-Secret, nie in der DB.
    totp_secret_enc   bytea,
    -- Zuletzt benutzter TOTP-Zeitschritt: Jeder Code gilt nur einmal.
    totp_last_step    bigint,
    is_admin          boolean NOT NULL DEFAULT false,
    disabled_at       timestamptz,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now()
);

-- Einladungs- bzw. Einrichtungslinks (auch nach Zurücksetzen der zweiten Faktoren).
CREATE TABLE invites (
    token_hash  bytea PRIMARY KEY,
    user_id     bigint NOT NULL REFERENCES users ON DELETE CASCADE,
    created_by  bigint REFERENCES users ON DELETE SET NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    expires_at  timestamptz NOT NULL,
    used_at     timestamptz
);

CREATE TABLE passkeys (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id        bigint NOT NULL REFERENCES users ON DELETE CASCADE,
    name           text NOT NULL,
    credential_id  bytea NOT NULL UNIQUE,
    -- Serialisierter `webauthn_rs::prelude::Passkey` (öffentlicher Schlüssel, Zähler, …).
    passkey        jsonb NOT NULL,
    created_at     timestamptz NOT NULL DEFAULT now(),
    last_used_at   timestamptz
);
CREATE INDEX passkeys_user ON passkeys (user_id);

-- Einmalige Wiederherstellungscodes, nur als SHA-256 (80 Bit Zufall je Code).
CREATE TABLE recovery_codes (
    id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id    bigint NOT NULL REFERENCES users ON DELETE CASCADE,
    code_hash  bytea NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    used_at    timestamptz,
    UNIQUE (user_id, code_hash)
);

CREATE TABLE sessions (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    token_hash    bytea NOT NULL UNIQUE,
    user_id       bigint NOT NULL REFERENCES users ON DELETE CASCADE,
    created_at    timestamptz NOT NULL DEFAULT now(),
    last_seen_at  timestamptz NOT NULL DEFAULT now(),
    expires_at    timestamptz NOT NULL,
    step_up_at    timestamptz,
    user_agent    text,
    ip            text
);
CREATE INDEX sessions_user ON sessions (user_id);

-- Zwischenschritte: Passwort geprüft, zweiter Faktor fehlt noch; oder Einrichtung über eine Einladung.
CREATE TABLE login_challenges (
    token_hash        bytea PRIMARY KEY,
    user_id           bigint NOT NULL REFERENCES users ON DELETE CASCADE,
    purpose           text NOT NULL CHECK (purpose IN ('second_factor', 'setup')),
    invite_hash       bytea,
    -- Während der Einrichtung: noch nicht bestätigtes TOTP-Geheimnis (verschlüsselt).
    pending_totp_enc  bytea,
    attempts          integer NOT NULL DEFAULT 0,
    created_at        timestamptz NOT NULL DEFAULT now(),
    expires_at        timestamptz NOT NULL
);

-- Schutz vor Durchprobieren: Fehlversuche je Schlüssel („ip:…“, „name:…“).
CREATE TABLE auth_throttle (
    key               text PRIMARY KEY,
    failures          integer NOT NULL,
    first_failure_at  timestamptz NOT NULL,
    locked_until      timestamptz
);

CREATE TABLE audit_log (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    at              timestamptz NOT NULL DEFAULT now(),
    actor_user_id   bigint REFERENCES users ON DELETE SET NULL,
    target_user_id  bigint REFERENCES users ON DELETE SET NULL,
    action          text NOT NULL,
    ip              text,
    details         jsonb NOT NULL DEFAULT '{}'
);
CREATE INDEX audit_log_at ON audit_log (at);
