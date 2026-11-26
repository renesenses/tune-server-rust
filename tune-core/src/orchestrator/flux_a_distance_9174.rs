//! #9174 — une zone navigateur lue À DISTANCE (« Ce téléphone » en 5G, par le
//! relais ou l'écoute directe) reçoit un format compressé.
//!
//! Le pré-transcodage DASH (TIDAL) sert du WAV à toute zone `browser` : un
//! FLAC ré-encodé sans SEEKTABLE cale le `<audio>` sur les `Range` (0.9.42).
//! Chez soi, le WAV ne coûte rien. Par le relais, chaque octet passe par la
//! voie montante de la maison, le WebSocket du pont, puis la 5G : du WAV 24/96
//! (4,6 Mbit/s) y démarre lentement et cale.
//!
//! Le serveur ne savait pas qu'une zone était tirée de loin. Il le voit
//! pourtant passer : le relais et l'écoute directe marquent chaque requête
//! qu'ils rejouent de [`crate::cloud::ENTETE_RELAIS`]. Le lecteur d'une zone
//! navigateur parle de cette zone en continu (`browser-playing` toutes les
//! 5 s, `browser-next` avant chaque suivante) : la provenance de ses requêtes
//! dit d'où il écoute.
//!
//! Règle : seul le LECTEUR de la zone compte, c'est-à-dire ses `POST
//! …/browser-*` (signal de lecture, suivante préparée, erreur média), plus
//! une commande RELAYÉE qui prépare une piste ([`commande_relayee_qui_prepare`],
//! notée AVANT la préparation), après
//! authentification, réussis (2xx), sur une zone qui existe et qui est une
//! zone navigateur (`garde` côté serveur, `provenance_navigateur_9174`) :
//! - relayé : la zone est tirée à distance, pour [`DELAI_A_DISTANCE`] ;
//! - local : le lecteur est revenu sur le réseau de la maison.
//!
//! Une télécommande (même en 5G) qui lit l'état d'une zone ne dit rien du
//! lecteur et ne change rien. Sans signal relayé depuis
//! [`DELAI_A_DISTANCE`], la zone revient en local : un lecteur qui s'est tu
//! n'emporte pas la zone avec lui.
//!
//! Une zone tirée à distance reçoit du FLAC pré-transcodé, borné
//! (`Content-Length`, ce qu'exige CoreMedia) et indexé (SEEKTABLE, ce
//! qu'exigent les `Range` du `<audio>`). La même zone, lue chez soi, garde son
//! WAV. Aucun réglage : c'est le chemin qui décide.

use std::time::{Duration, Instant};

use super::PlaybackOrchestrator;

/// Sans `browser-*` relayé depuis ce délai, la zone revient en local. Le
/// lecteur signale sa lecture toutes les 5 s.
pub const DELAI_A_DISTANCE: Duration = Duration::from_secs(30);

/// Zones retenues au plus : au-delà, la plus ancienne est oubliée.
pub const ZONES_RETENUES_MAX: usize = 64;

/// Ce qu'une requête HTTP dit de la provenance du lecteur d'une zone :
/// `Some((zone, true))` tirée à distance, `Some((zone, false))` revenue en
/// local, `None` rien. `chemin` : celui de l'API (`/api/v1/zones/{id}/…`) ou
/// celui vu sous `/zones` (`/{id}/…`).
pub fn provenance_d_une_requete_de_zone(
    methode: &str,
    chemin: &str,
    relayee: bool,
) -> Option<(i64, bool)> {
    if !methode.eq_ignore_ascii_case("POST") {
        return None;
    }
    let reste = chemin
        .strip_prefix("/api/v1/zones")
        .unwrap_or(chemin)
        .strip_prefix('/')?;
    let (id, suite) = reste.split_once('/')?;
    let zone: i64 = id.parse().ok()?;
    suite.starts_with("browser-").then_some((zone, relayee))
}

/// #9174 (suite) — une commande RELAYÉE qui prépare une piste sur cette zone
/// (`play`, `next`, `previous`, `queue/jump`, `browser-next`) : la zone est
/// notée à distance AVANT que la piste ne soit préparée. Sans cela, la
/// première piste en 5G partait en WAV : le lecteur ne signale sa lecture
/// (`browser-playing`) qu'une fois cette piste lancée, et `browser-next`
/// n'était noté qu'après sa réponse, donc après la préparation. Locale, une
/// telle commande ne dit rien (une télécommande du salon) : `None`.
pub fn commande_relayee_qui_prepare(methode: &str, chemin: &str, relayee: bool) -> Option<i64> {
    if !relayee || !methode.eq_ignore_ascii_case("POST") {
        return None;
    }
    let reste = chemin
        .strip_prefix("/api/v1/zones")
        .unwrap_or(chemin)
        .strip_prefix('/')?;
    let (id, suite) = reste.split_once('/')?;
    let zone: i64 = id.parse().ok()?;
    matches!(
        suite,
        "play" | "next" | "previous" | "queue/jump" | "browser-next"
    )
    .then_some(zone)
}

/// Les zones tirées à distance et l'instant de leur dernier signal relayé.
#[derive(Default)]
pub(crate) struct ZonesADistance(std::sync::Mutex<std::collections::HashMap<i64, Instant>>);

impl ZonesADistance {
    pub(crate) fn noter(&self, zone_id: i64, a_distance: bool, maintenant: Instant) -> bool {
        let Ok(mut zones) = self.0.lock() else {
            return false;
        };
        zones.retain(|_, t| maintenant.saturating_duration_since(*t) < DELAI_A_DISTANCE);
        if !a_distance {
            return zones.remove(&zone_id).is_some();
        }
        let nouvelle = zones.insert(zone_id, maintenant).is_none();
        if zones.len() > ZONES_RETENUES_MAX
            && let Some((&plus_ancienne, _)) = zones.iter().min_by_key(|(_, t)| **t)
        {
            zones.remove(&plus_ancienne);
        }
        nouvelle
    }

    pub(crate) fn a_distance(&self, zone_id: i64, maintenant: Instant) -> bool {
        self.0.lock().is_ok_and(|zones| {
            zones
                .get(&zone_id)
                .is_some_and(|t| maintenant.saturating_duration_since(*t) < DELAI_A_DISTANCE)
        })
    }

    #[cfg(test)]
    pub(crate) fn nombre(&self) -> usize {
        self.0.lock().map(|z| z.len()).unwrap_or(0)
    }
}

impl PlaybackOrchestrator {
    /// Retient d'où écoute le lecteur de cette zone (voir le module).
    pub fn noter_provenance_de_zone(&self, zone_id: i64, a_distance: bool) {
        if self
            .zones_tirees_a_distance
            .noter(zone_id, a_distance, Instant::now())
        {
            tracing::info!(zone_id, a_distance, "zone_navigateur_provenance");
        }
    }

    /// Le lecteur de cette zone écoute-t-il à distance (relais, écoute
    /// directe), avec un signal relayé depuis moins de [`DELAI_A_DISTANCE`] ?
    pub fn zone_tiree_a_distance(&self, zone_id: i64) -> bool {
        self.zones_tirees_a_distance
            .a_distance(zone_id, Instant::now())
    }

    #[cfg(test)]
    pub(crate) fn zones_a_distance(&self) -> &ZonesADistance {
        &self.zones_tirees_a_distance
    }
}
