//! #8483 (go de Bertrand, rc12, 12/11, options 1 et 2) — un fichier abîmé
//! dont le décodage de Tune connaît la coupure : quand la zone se fige
//! dessus, c'est la FIN de la piste. On passe à la suivante de la file (ou on
//! termine la file) et on le dit, au lieu de laisser la zone muette « en
//! lecture » puis de l'arrêter (garde #2116/#6702).
//!
//! Fil 2769 (rc11, Marantz NR1200, FLAC joué tel quel) : conteneur annonçant
//! 3:34, flux réel de 2:16. Le renderer se figeait à 2:15, Playing, et la
//! zone restait muette jusqu'à ce que le testeur l'arrête.
//!
//! La coupure vient de [`crate::audio::coupure_connue_8483`]. Deux formes de
//! « zone figée sur la coupure », les deux seules observées :
//! - la sortie reste `Playing` et sa position ne bouge plus
//!   ([`TOURS_DE_GEL`] tours ET [`DUREE_DE_GEL`]) — le renderer DLNA ;
//! - la sortie s'arrête (`Stopped`) avec son pic de position sur la coupure —
//!   la sortie locale, dont le décodeur atteint la fin du flux.
//!
//! L'avis part sur `zone.playback_error`, NON fatal, avec un code stable et
//! ses paramètres (titre, position) : le client le traduit (#7077) ; `error`
//! n'en est que le repli anglais.

use std::time::{Duration, Instant};

/// Code stable de l'avis « fichier abîmé à partir de m:ss ».
pub const CODE_FICHIER_ABIME: &str = "track_file_damaged";

/// Tours de sondage à position inchangée avant de conclure au gel.
pub(super) const TOURS_DE_GEL: u32 = 3;
/// …et durée minimale de ce gel : une rafale de tours rattrapés ne suffit
/// pas (même règle que #6702).
pub(super) const DUREE_DE_GEL: Duration = Duration::from_secs(3);
/// Un renderer peut se figer un peu AVANT la coupure : ce qu'il tient en
/// tampon et l'arrondi de `RelTime` à la seconde (fil 2769 : 2:15 pour une
/// coupure à 2:16,045).
pub(super) const TOLERANCE_AVANT_LA_COUPURE_MS: u64 = 5_000;
/// …et un peu APRÈS : dernier paquet complété par le codec, arrondi.
pub(super) const TOLERANCE_APRES_LA_COUPURE_MS: u64 = 2_000;

/// Où et depuis quand la position est figée.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SuiviDuGel {
    pub(super) position_ms: u64,
    pub(super) depuis: Instant,
    pub(super) tours: u32,
}

/// Un relevé de plus d'une sortie `Playing` : la position est-elle figée
/// depuis assez longtemps ? Toute position différente repart de zéro.
pub(super) fn gel_etabli(
    suivi: &mut Option<SuiviDuGel>,
    position_ms: u64,
    maintenant: Instant,
) -> bool {
    match suivi {
        Some(s) if s.position_ms == position_ms => {
            s.tours = s.tours.saturating_add(1);
            s.tours >= TOURS_DE_GEL && maintenant.duration_since(s.depuis) >= DUREE_DE_GEL
        }
        _ => {
            *suivi = Some(SuiviDuGel {
                position_ms,
                depuis: maintenant,
                tours: 1,
            });
            false
        }
    }
}

/// La position figée est-elle celle de la coupure connue ?
pub(super) fn position_sur_la_coupure(position_ms: u64, fin_ms: u64) -> bool {
    position_ms > 0
        && position_ms.saturating_add(TOLERANCE_AVANT_LA_COUPURE_MS) >= fin_ms
        && position_ms <= fin_ms.saturating_add(TOLERANCE_APRES_LA_COUPURE_MS)
}

/// La coupure mérite-t-elle d'être traitée en fin de piste ? Seulement LOIN
/// de la fin annoncée (mêmes seuils que le fil 1915) : un fichier à qui
/// manquent quelques trames finales finit normalement, sans avis.
pub(super) fn coupure_significative(fin_ms: u64, annonce_ms: u64) -> bool {
    super::decisions::position_loin_de_la_fin(fin_ms, annonce_ms)
}

/// `m:ss` (ou `h:mm:ss`), forme neutre que toutes les langues lisent.
pub(super) fn position_lisible(ms: u64) -> String {
    let s = ms / 1000;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// La charge de `zone.playback_error` : code, paramètres, repli anglais.
pub(super) fn charge_de_l_avis(zone_id: i64, titre: &str, fin_ms: u64) -> serde_json::Value {
    let position = position_lisible(fin_ms);
    serde_json::json!({
        "zone_id": zone_id,
        "code": CODE_FICHIER_ABIME,
        "params": { "title": titre, "position": position },
        "track_title": titre,
        "position_ms": fin_ms,
        // La zone continue : piste suivante, ou fin de file.
        "fatal": false,
        "error": format!(
            "“{titre}”: the file is damaged from {position} on. Tune skipped the rest of the track."
        ),
    })
}

impl super::PositionPoller {
    /// #8483 — la coupure connue du fichier de la piste en cours, s'il en a
    /// une. Piste de bibliothèque seulement, hors feuille CUE (sa position
    /// dans la piste n'est pas celle du fichier). Chemin en base, puis sa
    /// graphie résolue sur disque — celle que le décodeur a ouverte.
    fn coupure_connue_de_la_piste(
        &self,
        zone_state: &crate::playback::ZoneState,
    ) -> Option<crate::audio::coupure_connue_8483::CoupureConnue> {
        let track_id = zone_state.now_playing.as_ref()?.track_id?;
        let piste = crate::db::track_repo::TrackRepo::with_backend(self.db.clone())
            .get(track_id)
            .ok()??;
        if piste.cue_start_ms.is_some() {
            return None;
        }
        let chemin = piste.file_path?;
        crate::audio::coupure_connue_8483::coupure_de(&chemin).or_else(|| {
            crate::library::local_path::resolve_existing_local_path(&chemin)
                .and_then(|resolu| crate::audio::coupure_connue_8483::coupure_de(&resolu))
        })
    }

    /// La zone est figée à `position` : si c'est la coupure connue du fichier
    /// en cours, dire l'avis et passer à la suivante (ou finir la file).
    /// Rend `true` quand la piste a été quittée.
    pub(super) async fn passer_le_fichier_abime(
        &self,
        zone_id: i64,
        zone_state: &crate::playback::ZoneState,
        device_id: &str,
        position: u64,
        transport: crate::outputs::traits::TransportState,
    ) -> bool {
        let Some(coupure) = self.coupure_connue_de_la_piste(zone_state) else {
            return false;
        };
        if !coupure_significative(coupure.fin_ms, coupure.annonce_ms)
            || !position_sur_la_coupure(position, coupure.fin_ms)
        {
            return false;
        }
        let titre = zone_state
            .now_playing
            .as_ref()
            .map(|np| np.title.clone())
            .unwrap_or_default();
        tracing::warn!(
            zone_id,
            device = %device_id,
            position_ms = position,
            coupure_ms = coupure.fin_ms,
            annonce_ms = coupure.annonce_ms,
            transport = ?transport,
            queue_pos = zone_state.queue_position,
            "fichier_abime_fige_sur_la_coupure_passage_a_la_suivante"
        );
        if let Some(ref bus) = self.event_bus {
            bus.emit(
                "zone.playback_error",
                charge_de_l_avis(zone_id, &titre, coupure.fin_ms),
            );
            bus.emit(
                "playback.track_skipped",
                serde_json::json!({
                    "zone_id": zone_id,
                    "position": zone_state.queue_position,
                    "reason": CODE_FICHIER_ABIME,
                }),
            );
        }
        // Fil 2694 — fin sur ERREUR : pas de radio de fin de file.
        crate::playback::radio_fin_de_file::noter_fin_sur_erreur(zone_id);
        self.handle_track_end(zone_id, zone_state).await;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_gel_demande_trois_tours_et_trois_secondes_8483() {
        let t0 = Instant::now();
        let mut suivi = None;
        assert!(!gel_etabli(&mut suivi, 135_000, t0));
        assert!(!gel_etabli(
            &mut suivi,
            135_000,
            t0 + Duration::from_secs(1)
        ));
        // Trois tours, mais en rafale : pas encore.
        assert!(!gel_etabli(
            &mut suivi,
            135_000,
            t0 + Duration::from_millis(1_200)
        ));
        assert!(gel_etabli(&mut suivi, 135_000, t0 + Duration::from_secs(3)));
        // Une position qui bouge repart de zéro.
        assert!(!gel_etabli(
            &mut suivi,
            136_000,
            t0 + Duration::from_secs(4)
        ));
    }

    #[test]
    fn une_lecture_qui_avance_n_est_jamais_figee_8483() {
        let t0 = Instant::now();
        let mut suivi = None;
        for i in 0..40u64 {
            assert!(!gel_etabli(
                &mut suivi,
                100_000 + i * 1_000,
                t0 + Duration::from_secs(i)
            ));
        }
    }

    #[test]
    fn la_position_du_fil_2769_est_sur_la_coupure_8483() {
        assert!(position_sur_la_coupure(135_000, 136_045));
        assert!(position_sur_la_coupure(137_000, 136_045));
        assert!(!position_sur_la_coupure(60_000, 136_045));
        assert!(!position_sur_la_coupure(139_000, 136_045));
        assert!(!position_sur_la_coupure(0, 2_000));
    }

    #[test]
    fn quelques_trames_manquantes_a_la_fin_ne_sont_pas_une_coupure_8483() {
        assert!(coupure_significative(136_045, 214_826));
        assert!(!coupure_significative(214_700, 214_826));
    }

    #[test]
    fn l_avis_porte_un_code_et_ses_parametres_sans_phrase_francaise_8483() {
        let c = charge_de_l_avis(3, "Lecture coupée", 136_045);
        assert_eq!(c["code"], CODE_FICHIER_ABIME);
        assert_eq!(c["params"]["title"], "Lecture coupée");
        assert_eq!(c["params"]["position"], "2:16");
        assert_eq!(c["fatal"], false);
        assert!(c["error"].as_str().unwrap().contains("damaged from 2:16"));
        assert_eq!(position_lisible(3_725_000), "1:02:05");
    }
}
