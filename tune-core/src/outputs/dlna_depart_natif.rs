//! #6059 — la position d'un flux natif qui commence AU MILIEU de la piste.
//!
//! Sur un renderer qui ignore les `Seek` (Yamaha R-N2000A), Tune relance le
//! flux natif à partir de la position demandée (`audio::depart_natif`). Le
//! renderer, lui, compte sa position et sa durée à partir du début de CE flux :
//! « 0:00 » veut dire « la position de départ ». Sans correction, l'écran
//! repartirait de zéro et la fin de piste serait jugée sur la durée restante.
//!
//! L'URL de ces flux porte le paramètre `depart_ms` (la route ne lit que le
//! chemin, le paramètre ne change rien à ce qui est servi). La sortie DLNA
//! rajoute ce départ à `RelTime` et `TrackDuration` tant que le renderer joue
//! ce flux-là — reconnu à l'identifiant de session dans son `TrackURI`, ou, s'il
//! ne publie pas d'URI, au dernier flux que Tune lui a posé.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use tune_output_api::OutputStatus;

/// Nom du paramètre d'URL qui porte le départ, en millisecondes.
pub const PARAMETRE: &str = "depart_ms";

/// Appareil → (session, départ en ms) du dernier flux natif décalé posé.
static PAR_APPAREIL: LazyLock<Mutex<HashMap<String, (String, u64)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Sessions → départ, pour `seek_output_after_replay` : un flux qui commence
/// déjà à la position n'a pas à recevoir de `Seek`. Borné : les sessions sont
/// éphémères, on ne garde que les plus récentes.
static PAR_SESSION: LazyLock<Mutex<Vec<(String, u64)>>> = LazyLock::new(|| Mutex::new(Vec::new()));
const SESSIONS_GARDEES: usize = 64;

/// L'URL du flux, avec son départ.
pub fn url_avec_depart(url: &str, depart_ms: u64) -> String {
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}{PARAMETRE}={depart_ms}")
}

/// Le départ qu'une URL de flux porte, s'il y en a un.
pub fn depart_de_l_url(url: &str) -> Option<u64> {
    let requete = url.split_once('?')?.1;
    requete.split(['&', '#']).find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == PARAMETRE).then(|| v.parse().ok()).flatten()
    })
}

/// Inscrit une session qui commence à `depart_ms` (à la création du flux).
pub fn inscrire_session(session_id: &str, depart_ms: u64) {
    if let Ok(mut v) = PAR_SESSION.lock() {
        v.retain(|(s, _)| s != session_id);
        v.push((session_id.to_string(), depart_ms));
        let trop = v.len().saturating_sub(SESSIONS_GARDEES);
        v.drain(..trop);
    }
}

/// Le départ de cette session, si elle commence au milieu de la piste.
pub fn depart_de_session(session_id: &str) -> Option<u64> {
    PAR_SESSION
        .lock()
        .ok()?
        .iter()
        .find(|(s, _)| s == session_id)
        .map(|(_, d)| *d)
}

/// À chaque URL posée sur l'appareil (`play_media`) : retient le départ
/// qu'elle porte, ou l'oublie si elle n'en porte pas.
pub fn noter_url_posee(device_id: &str, url: &str) {
    let Ok(mut m) = PAR_APPAREIL.lock() else {
        return;
    };
    match (
        depart_de_l_url(url),
        crate::poller::decisions::stream_id_de_l_uri(Some(url)),
    ) {
        (Some(d), Some(sid)) if d > 0 => {
            m.insert(device_id.to_string(), (sid, d));
        }
        _ => {
            m.remove(device_id);
        }
    }
}

/// Rajoute le départ à la position et à la durée rapportées, tant que le
/// renderer joue le flux décalé.
pub fn corriger_le_statut(device_id: &str, statut: &mut OutputStatus) {
    let Some((sid, depart)) = PAR_APPAREIL
        .lock()
        .ok()
        .and_then(|m| m.get(device_id).cloned())
    else {
        return;
    };
    let joue_ce_flux = match statut.current_uri.as_deref().map(str::trim) {
        None | Some("") => true,
        Some(uri) => uri.contains(&sid),
    };
    if !joue_ce_flux {
        return;
    }
    statut.position_ms = statut.position_ms.saturating_add(depart);
    if statut.duration_ms > 0 {
        statut.duration_ms = statut.duration_ms.saturating_add(depart);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tune_output_api::TransportState;

    fn statut(uri: Option<&str>, pos: u64, duree: u64) -> OutputStatus {
        OutputStatus {
            state: TransportState::Playing,
            position_ms: pos,
            duration_ms: duree,
            volume: 0.5,
            muted: false,
            current_uri: uri.map(str::to_string),
            track_title: None,
            track_artist: None,
            ended_naturally: false,
            realtime: true,
            dop_active: false,
        }
    }

    #[test]
    fn le_parametre_de_depart_aller_retour_6059() {
        let u = url_avec_depart("http://10.0.0.2:8888/stream/abc-123.flac", 93_250);
        assert_eq!(
            u,
            "http://10.0.0.2:8888/stream/abc-123.flac?depart_ms=93250"
        );
        assert_eq!(depart_de_l_url(&u), Some(93_250));
        assert_eq!(depart_de_l_url("http://h/stream/abc.flac"), None);
        assert_eq!(
            crate::poller::decisions::stream_id_de_l_uri(Some(&u)).as_deref(),
            Some("abc-123"),
            "le sondeur retrouve toujours la session"
        );
    }

    #[test]
    fn la_position_du_renderer_est_rapportee_a_la_piste_6059() {
        let dev = "uuid:yamaha-rn2000a-6059-statut";
        noter_url_posee(dev, "http://h/stream/s-6059.flac?depart_ms=120000");
        let mut s = statut(
            Some("http://h/stream/s-6059.flac?depart_ms=120000"),
            5_000,
            180_000,
        );
        corriger_le_statut(dev, &mut s);
        assert_eq!(s.position_ms, 125_000, "0:05 du flux = 2:05 de la piste");
        assert_eq!(
            s.duration_ms, 300_000,
            "durée restante + départ = durée de la piste"
        );

        // Sans URI publiée : le dernier flux posé fait foi.
        let mut s = statut(None, 1_000, 0);
        corriger_le_statut(dev, &mut s);
        assert_eq!(s.position_ms, 121_000);
        assert_eq!(s.duration_ms, 0, "une durée inconnue reste inconnue");

        // Le renderer joue un AUTRE flux (enchaînement) : rien n'est ajouté.
        let mut s = statut(Some("http://h/stream/suivante.flac"), 1_000, 200_000);
        corriger_le_statut(dev, &mut s);
        assert_eq!(s.position_ms, 1_000);

        // Une URL posée sans départ efface la correction.
        noter_url_posee(dev, "http://h/stream/autre.flac");
        let mut s = statut(None, 1_000, 200_000);
        corriger_le_statut(dev, &mut s);
        assert_eq!(s.position_ms, 1_000);
    }

    #[test]
    fn les_sessions_decalees_sont_reconnues_6059() {
        inscrire_session("sess-6059-a", 42_000);
        assert_eq!(depart_de_session("sess-6059-a"), Some(42_000));
        assert_eq!(depart_de_session("sess-6059-inconnue"), None);
    }
}
