-- Accounts and sign-in (PLAN 13.3, 16.1). Runs on PostgreSQL 16 (tests) and 18 (production).

CREATE TABLE users (
    id                bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    -- Stable, unguessable identifier; also serves as the WebAuthn user handle.
    uuid              uuid NOT NULL UNIQUE DEFAULT gen_random_uuid(),
    username          text NOT NULL,
    username_folded   text NOT NULL UNIQUE,
    display_name      text NOT NULL,
    email             text,
    -- argon2id in PHC format. NULL until the invite has been accepted.
    password_hash     text,
    -- AES-256-GCM (nonce ‖ ciphertext), key from a Docker secret, never in the DB.
    totp_secret_enc   bytea,
    -- Last used TOTP time step: each code is valid only once.
    totp_last_step    bigint,
    is_admin          boolean NOT NULL DEFAULT false,
    disabled_at       timestamptz,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now()
);

-- Invite or setup links (also after the second factors have been reset).
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
    -- Serialized `webauthn_rs::prelude::Passkey` (public key, counter, …).
    passkey        jsonb NOT NULL,
    created_at     timestamptz NOT NULL DEFAULT now(),
    last_used_at   timestamptz
);
CREATE INDEX passkeys_user ON passkeys (user_id);

-- Single-use recovery codes, stored only as SHA-256 (80 bits of randomness per code).
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

-- Intermediate steps: password verified, second factor still missing; or setup via an invite.
CREATE TABLE login_challenges (
    token_hash        bytea PRIMARY KEY,
    user_id           bigint NOT NULL REFERENCES users ON DELETE CASCADE,
    purpose           text NOT NULL CHECK (purpose IN ('second_factor', 'setup')),
    invite_hash       bytea,
    -- During setup: TOTP secret not yet confirmed (encrypted).
    pending_totp_enc  bytea,
    attempts          integer NOT NULL DEFAULT 0,
    created_at        timestamptz NOT NULL DEFAULT now(),
    expires_at        timestamptz NOT NULL
);

-- Brute-force protection: failed attempts per key ("ip:…", "name:…").
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
