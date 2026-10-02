//! Short-lived in-memory ceremony state: passkey registration and sign-in, TOTP setup while signed
//! in. It expires after 5 minutes; a restart discards it (then start over).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use webauthn_rs::prelude::{PasskeyAuthentication, PasskeyRegistration};

const TTL: Duration = Duration::from_secs(300);
const MAX_ENTRIES: usize = 10_000;

pub enum Ceremony {
    /// Add a passkey: during setup (`setup` = hash of the setup token) or while signed in.
    Register {
        user_id: i64,
        name: String,
        setup: Option<Vec<u8>>,
        state: PasskeyRegistration,
    },
    /// Sign-in or step-up via passkey.
    Authenticate {
        user_id: i64,
        purpose: AuthPurpose,
        state: PasskeyAuthentication,
    },
    /// New TOTP secret, not yet confirmed.
    TotpEnroll { user_id: i64, secret: Vec<u8> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthPurpose {
    /// Passkey-only sign-in (counts as both factors).
    Login,
    /// Second factor after the password (hash of the intermediate-step token).
    SecondFactor(Vec<u8>),
    /// Step-up of an existing session.
    StepUp(i64),
}

#[derive(Default)]
pub struct Ceremonies {
    inner: Mutex<HashMap<String, (Instant, Ceremony)>>,
}

impl Ceremonies {
    pub fn insert(&self, c: Ceremony) -> String {
        let (id, _) = super::tokens::new_token();
        let mut map = self.inner.lock().expect("Mutex");
        let now = Instant::now();
        map.retain(|_, (t, _)| now.duration_since(*t) < TTL);
        if map.len() >= MAX_ENTRIES {
            // Guard against unbounded memory growth: discard the oldest.
            if let Some(oldest) = map
                .iter()
                .min_by_key(|(_, (t, _))| *t)
                .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
            }
        }
        map.insert(id.clone(), (now, c));
        id
    }

    /// Removes and returns a pending ceremony (usable only once).
    pub fn take(&self, id: &str) -> Option<Ceremony> {
        let mut map = self.inner.lock().expect("Mutex");
        let (t, c) = map.remove(id)?;
        (t.elapsed() < TTL).then_some(c)
    }
}
