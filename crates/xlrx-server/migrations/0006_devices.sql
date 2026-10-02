-- Devices (Mac, iPhone): sign-in through the browser with PKCE, rotating refresh tokens and
-- short-lived access tokens (PLAN 16.1). Only hashes of codes and tokens are stored.

CREATE TABLE devices (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id         bigint NOT NULL REFERENCES users ON DELETE CASCADE,
    name            text NOT NULL,
    platform        text NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    -- Last confirmation with a second factor; refreshing stops a configured number of days later.
    confirmed_at    timestamptz NOT NULL DEFAULT now(),
    last_seen_at    timestamptz NOT NULL DEFAULT now(),
    last_ip         text,
    revoked_at      timestamptz,
    revoked_reason  text
);
CREATE INDEX devices_user ON devices (user_id);

-- One pair of tokens per rotation.
CREATE TABLE device_grants (
    id                 bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    device_id          bigint NOT NULL REFERENCES devices ON DELETE CASCADE,
    -- The grant whose refresh token was exchanged for this one.
    parent_id          bigint REFERENCES device_grants ON DELETE SET NULL,
    refresh_hash       bytea NOT NULL UNIQUE,
    access_hash        bytea NOT NULL UNIQUE,
    access_expires_at  timestamptz NOT NULL,
    created_at         timestamptz NOT NULL DEFAULT now(),
    -- First use of either token: proves that the device received this grant.
    used_at            timestamptz,
    -- Replaced by a newer grant: its tokens no longer work, and presenting its refresh token again
    -- counts as reuse. Kept for a while to recognize that.
    superseded_at      timestamptz
);
CREATE INDEX device_grants_device ON device_grants (device_id);
-- At most one valid grant per device.
CREATE UNIQUE INDEX device_grants_current ON device_grants (device_id) WHERE superseded_at IS NULL;

-- Codes from the browser flow: single use, valid for two minutes, bound to the PKCE challenge.
CREATE TABLE device_codes (
    code_hash     bytea PRIMARY KEY,
    user_id       bigint NOT NULL REFERENCES users ON DELETE CASCADE,
    challenge     text NOT NULL,
    redirect_uri  text NOT NULL,
    name          text NOT NULL,
    platform      text NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    expires_at    timestamptz NOT NULL
);
