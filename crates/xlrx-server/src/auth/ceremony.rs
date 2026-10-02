//! Kurzlebige Zwischenstände im Speicher: Passkey-Registrierung und -Anmeldung, TOTP-Einrichtung im
//! angemeldeten Zustand. Nach 5 Minuten verfallen sie; ein Neustart verwirft sie (dann neu beginnen).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use webauthn_rs::prelude::{PasskeyAuthentication, PasskeyRegistration};

const TTL: Duration = Duration::from_secs(300);
const MAX_ENTRIES: usize = 10_000;

pub enum Ceremony {
    /// Passkey hinzufügen: bei der Einrichtung (`setup` = Hash des Einrichtungs-Tokens) oder angemeldet.
    Register {
        user_id: i64,
        name: String,
        setup: Option<Vec<u8>>,
        state: PasskeyRegistration,
    },
    /// Anmeldung oder Step-up per Passkey.
    Authenticate {
        user_id: i64,
        purpose: AuthPurpose,
        state: PasskeyAuthentication,
    },
    /// Neues TOTP-Geheimnis, noch nicht bestätigt.
    TotpEnroll { user_id: i64, secret: Vec<u8> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthPurpose {
    /// Anmeldung nur mit Passkey (zählt als beide Faktoren).
    Login,
    /// Zweiter Faktor nach dem Passwort (Hash des Zwischenschritt-Tokens).
    SecondFactor(Vec<u8>),
    /// Step-up einer bestehenden Sitzung.
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
            // Schutz vor Speicherüberlauf: die ältesten verwerfen.
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

    /// Entnimmt einen Zwischenstand (nur einmal verwendbar).
    pub fn take(&self, id: &str) -> Option<Ceremony> {
        let mut map = self.inner.lock().expect("Mutex");
        let (t, c) = map.remove(id)?;
        (t.elapsed() < TTL).then_some(c)
    }
}
