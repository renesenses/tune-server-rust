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
//! Règle :
//! - une requête RELAYÉE sur `/api/v1/zones/{id}/…` : la zone est tirée à
//!   distance ;
//! - une requête LOCALE du lecteur lui-même (`POST …/browser-…`) : il est
//!   revenu sur le réseau de la maison. Les autres requêtes locales (une
//!   télécommande du salon qui lit l'état de la zone) ne disent rien du
//!   lecteur, et ne changent rien.
//!
//! Une zone tirée à distance reçoit du FLAC pré-transcodé, borné
//! (`Content-Length`, ce qu'exige CoreMedia) et indexé (SEEKTABLE, ce
//! qu'exigent les `Range` du `<audio>`). La même zone, lue chez soi, garde son
//! WAV. Aucun réglage : c'est le chemin qui décide.

use super::PlaybackOrchestrator;

/// Ce qu'une requête HTTP dit de la provenance du lecteur d'une zone :
/// `Some((zone, true))` tirée à distance, `Some((zone, false))` revenue en
/// local, `None` rien.
pub fn provenance_d_une_requete_de_zone(
    methode: &str,
    chemin: &str,
    relayee: bool,
) -> Option<(i64, bool)> {
    let reste = chemin.strip_prefix("/api/v1/zones/")?;
    let (id, suite) = reste.split_once('/')?;
    let zone: i64 = id.parse().ok()?;
    if relayee {
        return Some((zone, true));
    }
    if methode.eq_ignore_ascii_case("POST") && suite.starts_with("browser-") {
        return Some((zone, false));
    }
    None
}

impl PlaybackOrchestrator {
    /// Retient d'où écoute le lecteur de cette zone (voir le module).
    pub fn noter_provenance_de_zone(&self, zone_id: i64, a_distance: bool) {
        let Ok(mut zones) = self.zones_tirees_a_distance.lock() else {
            return;
        };
        let changee = if a_distance {
            zones.insert(zone_id)
        } else {
            zones.remove(&zone_id)
        };
        if changee {
            tracing::info!(zone_id, a_distance, "zone_navigateur_provenance");
        }
    }

    /// Le lecteur de cette zone écoute-t-il à distance (relais, écoute
    /// directe) ?
    pub fn zone_tiree_a_distance(&self, zone_id: i64) -> bool {
        self.zones_tirees_a_distance
            .lock()
            .is_ok_and(|zones| zones.contains(&zone_id))
    }
}
