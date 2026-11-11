//! #8082 — le suivi de piste d'un FLUX CONTINU servi à un renderer DLNA.
//!
//! Quand une zone arme « Flux continu entre les pistes »
//! (`zone_{id}_dlna_flux_continu`), une suite de pistes consécutives de même
//! format part en UN seul WAV (`audio::wav_continu`). Le renderer ne voit
//! qu'une piste longue ; Tune, lui, doit continuer à dire laquelle joue.
//!
//! Ce module tient, par appareil, le PLAN du flux posé : l'identifiant de la
//! session, et pour chaque piste sa ligne de file, sa frontière dans le flux
//! et sa durée. [`corriger_le_statut`] rapporte la position du renderer à la
//! piste en cours (même mécanique que `dlna_depart_natif`, qui le précède) et
//! note tout passage d'une frontière ; le sondeur le prend par
//! [`prendre_le_passage`] et avance la file sans rien envoyer au renderer.
//!
//! Le plan meurt dès qu'une autre URI est posée sur l'appareil
//! ([`noter_url_posee`]), ou quand le renderer quitte le flux (il annonce une
//! autre URI, ou sa position recule sous la piste en cours sans geste de
//! Tune : il a enchaîné sur la suivante armée par `SetNext`).

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use tune_output_api::OutputStatus;

/// La clé de réglage de zone (`true` / absent).
pub fn cle(zone_id: i64) -> String {
    format!("zone_{zone_id}_dlna_flux_continu")
}

/// Le réglage de la zone : désactivé par défaut.
pub fn active(db: &std::sync::Arc<dyn crate::db::backend::DbBackend>, zone_id: i64) -> bool {
    crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .get(&cle(zone_id))
        .ok()
        .flatten()
        .is_some_and(|v| matches!(v.trim(), "true" | "1"))
}

/// Une piste du flux continu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PisteDuFlux {
    /// Position de sa ligne dans la file de la zone.
    pub position: i64,
    pub track_id: i64,
    /// Sa frontière dans le flux, en ms, comptée depuis le DÉBUT de la
    /// première piste (une position de départ, rapportée par
    /// `dlna_depart_natif`, est déjà ajoutée au relevé).
    pub debut_ms: u64,
    pub duree_ms: u64,
}

/// Le plan d'un flux continu posé sur un appareil.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub zone_id: i64,
    pub session_id: String,
    pub pistes: Vec<PisteDuFlux>,
}

#[derive(Debug)]
struct Suivi {
    plan: Plan,
    /// La piste où le dernier relevé a vu le renderer.
    vue: usize,
    /// Un passage de frontière pas encore pris par le sondeur.
    passage: Option<usize>,
}

static PAR_APPAREIL: LazyLock<Mutex<HashMap<String, Suivi>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Pose le plan du flux que l'appareil va recevoir.
pub fn inscrire(device_id: &str, plan: Plan) {
    if let Ok(mut m) = PAR_APPAREIL.lock() {
        m.insert(
            device_id.to_string(),
            Suivi {
                plan,
                vue: 0,
                passage: None,
            },
        );
    }
}

/// Retire le plan de l'appareil.
pub fn oublier(device_id: &str) {
    if let Ok(mut m) = PAR_APPAREIL.lock() {
        m.remove(device_id);
    }
}

/// Une URI vient d'être posée sur l'appareil (`SetAVTransportURI`) : le plan
/// ne survit que si c'est celle de son flux.
pub fn noter_url_posee(device_id: &str, url: &str) {
    if let Ok(mut m) = PAR_APPAREIL.lock()
        && let Some(s) = m.get(device_id)
        && !url.contains(&s.plan.session_id)
    {
        m.remove(device_id);
    }
}

/// Le flux continu que tient l'appareil, s'il y en a un.
pub fn plan_de(device_id: &str) -> Option<Plan> {
    PAR_APPAREIL
        .lock()
        .ok()?
        .get(device_id)
        .map(|s| s.plan.clone())
}

/// Vrai quand `session_id` est un flux continu en cours sur un appareil.
pub fn est_un_flux_continu(session_id: &str) -> bool {
    PAR_APPAREIL
        .lock()
        .map(|m| m.values().any(|s| s.plan.session_id == session_id))
        .unwrap_or(false)
}

/// Vrai quand la ligne `position` de la file est DÉJÀ dans le flux, après la
/// piste en cours : l'armer par `SetNext` la ferait jouer deux fois.
pub fn couvre_la_suivante(device_id: &str, position: i64) -> bool {
    PAR_APPAREIL
        .lock()
        .ok()
        .and_then(|m| {
            m.get(device_id).map(|s| {
                s.plan
                    .pistes
                    .iter()
                    .skip(s.vue + 1)
                    .any(|p| p.position == position)
            })
        })
        .unwrap_or(false)
}

/// Recul toléré sous le début de la piste vue avant de conclure que le
/// renderer a quitté le flux.
const GIGUE_TOLEREE_MS: u64 = 3_000;

/// L'index de la piste qui porte la position `ms` du flux.
fn piste_a(pistes: &[PisteDuFlux], ms: u64) -> usize {
    pistes.iter().rposition(|p| p.debut_ms <= ms).unwrap_or(0)
}

/// Rapporte le relevé du renderer à la piste en cours du flux continu, et
/// note un passage de frontière pour le sondeur.
pub fn corriger_le_statut(device_id: &str, statut: &mut OutputStatus) {
    let Ok(mut m) = PAR_APPAREIL.lock() else {
        return;
    };
    let Some(s) = m.get_mut(device_id) else {
        return;
    };
    // Une URI annoncée qui n'est pas la nôtre : le renderer joue autre chose
    // (la suivante armée après la dernière piste du flux, ou un tiers).
    if let Some(uri) = statut.current_uri.as_deref().map(str::trim)
        && !uri.is_empty()
        && !uri.contains(&s.plan.session_id)
    {
        tracing::info!(
            device_id,
            session = %s.plan.session_id,
            uri,
            "flux_continu_quitte_par_le_renderer"
        );
        m.remove(device_id);
        return;
    }
    let brute = statut.position_ms;
    let mut i = piste_a(&s.plan.pistes, brute);
    // Une gigue du renderer à la frontière (relevé arrondi à la seconde, non
    // monotone) n'est pas une sortie : on reste sur la piste vue.
    if i < s.vue && brute.saturating_add(GIGUE_TOLEREE_MS) >= s.plan.pistes[s.vue].debut_ms {
        i = s.vue;
    }
    if i < s.vue {
        // Le renderer recule sous la piste en cours sans geste de Tune (un
        // déplacement de Tune pose un NOUVEAU flux, donc un nouveau plan) :
        // il a quitté le flux. Le relevé reste brut.
        tracing::info!(
            device_id,
            session = %s.plan.session_id,
            position_ms = brute,
            piste_vue = s.vue,
            "flux_continu_recul_hors_du_flux"
        );
        m.remove(device_id);
        return;
    }
    if i > s.vue {
        s.vue = i;
        s.passage = Some(i);
    }
    let p = &s.plan.pistes[i];
    statut.position_ms = brute.saturating_sub(p.debut_ms).min(p.duree_ms);
    statut.duration_ms = p.duree_ms;
}

/// Le passage de frontière pas encore pris : la piste où le renderer est
/// entré. Rendu une seule fois.
pub fn prendre_le_passage(device_id: &str) -> Option<(i64, PisteDuFlux)> {
    let mut m = PAR_APPAREIL.lock().ok()?;
    let s = m.get_mut(device_id)?;
    let i = s.passage.take()?;
    Some((s.plan.zone_id, s.plan.pistes[i].clone()))
}

/// La durée de la PREMIÈRE piste d'un flux continu, en ms : la résolution
/// rend la durée du flux entier (la DIDL la veut ainsi, #1132), mais
/// l'écran, l'historique et le sondeur parlent de la piste.
pub fn duree_de_la_premiere_piste(session_id: Option<&str>) -> Option<i64> {
    let sid = session_id?;
    PAR_APPAREIL.lock().ok()?.values().find_map(|s| {
        (s.plan.session_id == sid)
            .then(|| s.plan.pistes.first().map(|p| p.duree_ms as i64))
            .flatten()
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use tune_output_api::TransportState;

    fn statut(uri: Option<&str>, pos: u64) -> OutputStatus {
        OutputStatus {
            state: TransportState::Playing,
            position_ms: pos,
            duration_ms: 900_000,
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

    fn plan(sid: &str) -> Plan {
        Plan {
            zone_id: 10,
            session_id: sid.into(),
            pistes: vec![
                PisteDuFlux {
                    position: 3,
                    track_id: 31,
                    debut_ms: 0,
                    duree_ms: 198_800,
                },
                PisteDuFlux {
                    position: 4,
                    track_id: 32,
                    debut_ms: 198_800,
                    duree_ms: 150_000,
                },
                PisteDuFlux {
                    position: 5,
                    track_id: 33,
                    debut_ms: 348_800,
                    duree_ms: 20_000,
                },
            ],
        }
    }

    #[test]
    fn la_position_est_rapportee_a_la_piste_et_le_passage_note_8082() {
        let dev = "uuid:dmp-a8-8082-a";
        let uri = "http://h/stream/fc-8082-a.wav";
        inscrire(dev, plan("fc-8082-a"));
        noter_url_posee(dev, uri);
        assert!(plan_de(dev).is_some(), "l'URI du flux garde le plan");

        let mut s = statut(Some(uri), 120_000);
        corriger_le_statut(dev, &mut s);
        assert_eq!((s.position_ms, s.duration_ms), (120_000, 198_800));
        assert_eq!(prendre_le_passage(dev), None);
        assert!(
            couvre_la_suivante(dev, 4),
            "piste 4 déjà dans le flux : pas de SetNext"
        );
        assert!(couvre_la_suivante(dev, 5));

        let mut s = statut(Some(uri), 200_000);
        corriger_le_statut(dev, &mut s);
        assert_eq!((s.position_ms, s.duration_ms), (1_200, 150_000));
        let (zone, p) = prendre_le_passage(dev).expect("frontière franchie");
        assert_eq!((zone, p.position, p.track_id), (10, 4, 32));
        assert_eq!(prendre_le_passage(dev), None, "rendu une seule fois");
        assert!(!couvre_la_suivante(dev, 4));

        // Une piste courte sautée entre deux relevés : on va à la dernière.
        let mut s = statut(None, 349_000);
        corriger_le_statut(dev, &mut s);
        assert_eq!(s.position_ms, 200);
        assert_eq!(prendre_le_passage(dev).map(|(_, p)| p.position), Some(5));
        assert!(
            !couvre_la_suivante(dev, 6),
            "dernière piste du flux : la suivante s'arme normalement"
        );

        // Le renderer passe à la suivante armée : autre URI, plan retiré,
        // relevé brut.
        let mut s = statut(Some("http://h/stream/autre.flac"), 1_000);
        corriger_le_statut(dev, &mut s);
        assert_eq!(s.position_ms, 1_000);
        assert!(plan_de(dev).is_none());
    }

    #[test]
    fn un_recul_sans_geste_de_tune_sort_du_flux_8082() {
        let dev = "uuid:dmp-a8-8082-b";
        inscrire(dev, plan("fc-8082-b"));
        let mut s = statut(None, 250_000);
        corriger_le_statut(dev, &mut s);
        let _ = prendre_le_passage(dev);
        let mut s = statut(None, 500);
        corriger_le_statut(dev, &mut s);
        assert_eq!(s.position_ms, 500, "relevé laissé brut");
        assert!(plan_de(dev).is_none());
    }

    #[test]
    fn une_gigue_a_la_frontiere_ne_sort_pas_du_flux_8082() {
        let dev = "uuid:dmp-a8-8082-d";
        inscrire(dev, plan("fc-8082-d"));
        let mut s = statut(None, 199_000);
        corriger_le_statut(dev, &mut s);
        assert!(prendre_le_passage(dev).is_some());
        let mut s = statut(None, 198_000);
        corriger_le_statut(dev, &mut s);
        assert_eq!(
            (s.position_ms, s.duration_ms),
            (0, 150_000),
            "reste sur la piste 2"
        );
        assert!(plan_de(dev).is_some(), "plan gardé");
        assert_eq!(prendre_le_passage(dev), None);
    }

    #[test]
    fn une_autre_uri_posee_retire_le_plan_8082() {
        let dev = "uuid:dmp-a8-8082-c";
        inscrire(dev, plan("fc-8082-c"));
        assert!(est_un_flux_continu("fc-8082-c"));
        noter_url_posee(dev, "http://h/stream/xyz.flac");
        assert!(plan_de(dev).is_none());
        assert!(!est_un_flux_continu("fc-8082-c"));
    }
}
