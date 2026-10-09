//! #4382 — sondage accéléré dans la dernière seconde, pour un renderer DLNA
//! qui n'enchaîne PAS seul.
//!
//! ## Le constat (Villerio, Eversolo DMP-A6, Tune 1.0.0-rc3, fil 2193, 09/10)
//!
//! La mémoire « Next ignoré » de la rc3 fait son travail : plus de fenêtre
//! `Next` de 3 s, la fin est prononcée par l'épinglage
//! (`position_pinned_finished_uri_dlna`) puis Tune relance aussitôt
//! (`SetAVTransportURI` + `Play`). Reste un blanc de 2 à 3 s, en deux parts :
//!
//! - le renderer rouvre deux fois le flux neuf avant de jouer (~1,5 à 2 s) :
//!   c'est l'appareil, Tune n'y peut rien ;
//! - la fin elle-même n'est vue qu'à un sondage ENTIER après que la position
//!   a atteint la durée rapportée (237 000 pour 237 651) : `RelTime` est à la
//!   seconde, le sondeur tourne à 1 Hz, la phase du renderer est inconnue.
//!   La fin est prononcée entre 0,35 et 1,35 s après la dernière note.
//!
//! ## La décision
//!
//! Pour la seule sortie DLNA que Tune sait ne pas enchaîner (mémoire « Next
//! ignoré »), au sondage où la position est à moins d'une seconde de la durée
//! rapportée, un guetteur sonde la position toutes les [`PAS_DU_GUET`] jusqu'à
//! la voir atteindre cette durée : c'est la frontière, saisie à 100 ms près.
//! La dernière note tombe `durée de la file − durée rapportée` plus tard ; le
//! guetteur réveille alors le sondeur, qui conclut sans attendre un sondage
//! « inchangé ».
//!
//! Un appareil qui enchaîne seul (DMP-A8, et tout renderer absent de la
//! mémoire) ne voit aucun guetteur : son sondage ne change pas d'un octet.

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;
use tracing::info;

/// Cadence du guet dans la dernière seconde.
pub(super) const PAS_DU_GUET: Duration = Duration::from_millis(100);

/// Le guet abandonne au-delà : un renderer qui n'atteint pas sa durée en
/// deux secondes est en pause, calé, ou a changé de piste — le sondage
/// normal reprend la main.
pub(super) const PLAFOND_DU_GUET: Duration = Duration::from_millis(2_000);

/// Marge au-delà de la dernière note calculée, pour ne jamais couper la
/// queue de piste que l'arrondi de `RelTime` cache.
pub(super) const MARGE_APRES_LA_FRONTIERE: Duration = Duration::from_millis(100);

/// Fenêtre (ms) avant la durée rapportée dans laquelle le guet se lance.
pub(super) const FENETRE_DU_GUET_MS: u64 = 1_000;

/// Faut-il guetter la frontière ? Seulement pour un renderer DLNA connu pour
/// ne pas enchaîner seul, `SetNext` envoyé, durée rapportée crédible, et la
/// position dans la dernière seconde avant cette durée.
#[allow(clippy::too_many_arguments)]
pub(super) fn doit_guetter_la_frontiere(
    is_dlna: bool,
    gapless_sent: bool,
    next_ignore_connu: bool,
    track_duration_ms: u64,
    reported_duration_ms: u64,
    position_ms: u64,
) -> bool {
    is_dlna
        && gapless_sent
        && next_ignore_connu
        // Mêmes bornes que `dlna_epingle_sur_la_piste_finie` : une durée
        // rapportée crédible, au plus 2 s sous celle de la file.
        && reported_duration_ms > super::decisions::END_MARGIN_MS
        && reported_duration_ms <= track_duration_ms
        && reported_duration_ms.saturating_add(2000) >= track_duration_ms
        && position_ms < reported_duration_ms
        && position_ms.saturating_add(FENETRE_DU_GUET_MS) >= reported_duration_ms
}

/// Sonde la position toutes les `pas` jusqu'à la voir atteindre
/// `duree_rapportee_ms` ; rend l'instant de la frontière. `None` si le
/// renderer ne répond plus ou n'y arrive pas avant `plafond`.
pub(super) async fn guetter_la_frontiere<F, Fut>(
    mut sonder: F,
    duree_rapportee_ms: u64,
    pas: Duration,
    plafond: Duration,
) -> Option<Instant>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<u64>>,
{
    let limite = Instant::now() + plafond;
    loop {
        let position = sonder().await?;
        let maintenant = Instant::now();
        if position >= duree_rapportee_ms {
            return Some(maintenant);
        }
        if maintenant + pas > limite {
            return None;
        }
        tokio::time::sleep(pas).await;
    }
}

/// L'instant où le sondeur peut conclure : la frontière, plus la queue de
/// piste que l'arrondi cache, plus la marge.
pub(super) fn instant_de_conclusion(
    frontiere: Instant,
    track_duration_ms: u64,
    reported_duration_ms: u64,
) -> Instant {
    let queue = Duration::from_millis(track_duration_ms.saturating_sub(reported_duration_ms));
    frontiere + queue + MARGE_APRES_LA_FRONTIERE
}

/// Une fin précise posée par le guetteur, pour un flux donné.
#[derive(Debug, Clone)]
pub(super) struct FinPrecise {
    pub flux: String,
    /// `None` tant que la frontière n'est pas saisie.
    pub conclure_a: Option<Instant>,
}

/// Le sondeur peut-il conclure maintenant, sur la foi du guetteur ?
pub(super) fn conclusion_permise(
    fin: Option<&FinPrecise>,
    flux: Option<&str>,
    maintenant: Instant,
) -> bool {
    match (fin, flux.filter(|f| !f.is_empty())) {
        (Some(fin), Some(flux)) => {
            fin.flux == flux && fin.conclure_a.is_some_and(|t| maintenant >= t)
        }
        _ => false,
    }
}

/// La carte des fins précises, par zone : partagée avec les guetteurs.
pub(super) type FinsPrecises =
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<i64, FinPrecise>>>;

impl super::PositionPoller {
    /// L'appareil est-il dans la mémoire « Next ignoré » ? Muet, à la
    /// différence de `next_deja_ignore`, qui le journalise à chaque fin.
    fn next_ignore_connu(&self, device_id: &str) -> bool {
        self.appareils_qui_ignorent_next
            .lock()
            .map(|a| a.contains(device_id))
            .unwrap_or(false)
    }

    /// Lance le guet de la frontière si la zone s'y prête — au plus un guet
    /// par flux. Ne fait rien, sans même toucher la mémoire, pour une sortie
    /// qui n'est pas DLNA ou dont la position est loin de la fin.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn lancer_le_guet_si_besoin(
        &self,
        zone_id: i64,
        device_id: &str,
        flux: Option<&str>,
        is_dlna: bool,
        gapless_sent: bool,
        track_duration_ms: u64,
        reported_duration_ms: u64,
        position_ms: u64,
    ) {
        let Some(flux) = flux.filter(|f| !f.is_empty()) else {
            return;
        };
        if !is_dlna || !gapless_sent {
            return;
        }
        if !doit_guetter_la_frontiere(
            is_dlna,
            gapless_sent,
            self.next_ignore_connu(device_id),
            track_duration_ms,
            reported_duration_ms,
            position_ms,
        ) {
            return;
        }
        {
            let mut carte = self.fins_precises.lock().unwrap_or_else(|e| e.into_inner());
            if carte.get(&zone_id).is_some_and(|f| f.flux == flux) {
                return;
            }
            carte.insert(
                zone_id,
                FinPrecise {
                    flux: flux.to_string(),
                    conclure_a: None,
                },
            );
        }
        let output_arc = {
            let outputs = self.outputs.lock().await;
            match outputs.get(device_id) {
                Some(o) => o,
                None => return,
            }
        };
        info!(
            zone_id,
            device = %device_id,
            position_ms,
            duree_rapportee_ms = reported_duration_ms,
            "sondage_accelere_fin_de_piste"
        );
        let carte = self.fins_precises.clone();
        let flux = flux.to_string();
        tokio::spawn(async move {
            let sonder = || {
                let arc = output_arc.clone();
                async move {
                    let sortie = arc.lock().await;
                    tokio::time::timeout(Duration::from_millis(500), sortie.get_status())
                        .await
                        .ok()?
                        .ok()
                        .map(|s| s.position_ms)
                }
            };
            let Some(frontiere) =
                guetter_la_frontiere(sonder, reported_duration_ms, PAS_DU_GUET, PLAFOND_DU_GUET)
                    .await
            else {
                info!(zone_id, "sondage_accelere_abandonne");
                return;
            };
            let conclure_a =
                instant_de_conclusion(frontiere, track_duration_ms, reported_duration_ms);
            {
                let mut carte = carte.lock().unwrap_or_else(|e| e.into_inner());
                match carte.get_mut(&zone_id) {
                    Some(f) if f.flux == flux => f.conclure_a = Some(conclure_a),
                    _ => return,
                }
            }
            info!(
                zone_id,
                attente_ms = conclure_a
                    .saturating_duration_since(Instant::now())
                    .as_millis() as u64,
                "sondage_accelere_frontiere_saisie"
            );
            tokio::time::sleep_until(conclure_a).await;
            super::TRACK_END_NOTIFY.notify_one();
        });
    }

    /// Le guetteur a-t-il établi la fin de CE flux, et son heure est-elle
    /// venue ? Une entrée laissée par un autre flux est jetée au passage.
    pub(super) fn fin_precise_atteinte(&self, zone_id: i64, flux: Option<&str>) -> bool {
        let mut carte = self.fins_precises.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(f) = carte.get(&zone_id)
            && flux.is_none_or(|fl| fl != f.flux)
        {
            carte.remove(&zone_id);
            return false;
        }
        conclusion_permise(carte.get(&zone_id), flux, Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUREE_FILE: u64 = 237_651;
    const DUREE_RAPPORTEE: u64 = 237_000;

    /// Faux DMP-A6 : `RelTime` à la seconde, position épinglée sur sa durée
    /// rapportée, horloge partie à `debut` (Tune ne connaît pas sa phase).
    fn position_du_renderer(debut: Instant, maintenant: Instant) -> u64 {
        let ms = maintenant.saturating_duration_since(debut).as_millis() as u64;
        (ms / 1000 * 1000).min(DUREE_RAPPORTEE)
    }

    /// Pour toutes les phases, à 50 ms près : la fin est conclue au plus
    /// 200 ms après la dernière note, et jamais avant.
    #[tokio::test(start_paused = true)]
    async fn fin_detectee_en_200_ms_au_plus_sur_un_rendu_fige() {
        for phase_ms in (0..1000).step_by(50) {
            // On se place au sondage qui lit 236 000, à la phase voulue.
            let maintenant = Instant::now();
            let debut = maintenant - Duration::from_millis(236_000 + phase_ms);
            assert_eq!(position_du_renderer(debut, maintenant), 236_000);
            assert!(doit_guetter_la_frontiere(
                true,
                true,
                true,
                DUREE_FILE,
                DUREE_RAPPORTEE,
                236_000
            ));
            let derniere_note = debut + Duration::from_millis(DUREE_FILE);

            let frontiere = guetter_la_frontiere(
                || async { Some(position_du_renderer(debut, Instant::now())) },
                DUREE_RAPPORTEE,
                PAS_DU_GUET,
                PLAFOND_DU_GUET,
            )
            .await
            .expect("la frontière doit être saisie");
            let conclusion = instant_de_conclusion(frontiere, DUREE_FILE, DUREE_RAPPORTEE);

            assert!(
                conclusion >= derniere_note,
                "phase {phase_ms} : la queue de piste serait coupée"
            );
            let retard = conclusion - derniere_note;
            assert!(
                retard <= Duration::from_millis(200),
                "phase {phase_ms} : fin conclue {retard:?} après la dernière note"
            );

            // Aujourd'hui : un sondage entier après la première lecture de
            // 237 000 — entre 0,35 et 1,35 s de retard.
            let premiere_lecture = maintenant + Duration::from_secs(1);
            let aujourd_hui = premiere_lecture + Duration::from_secs(1);
            assert!(aujourd_hui - derniere_note >= Duration::from_millis(349));
        }
    }

    /// Contre-épreuve : un appareil qui enchaîne seul (DMP-A8, absent de la
    /// mémoire « Next ignoré ») garde le sondage normal, comme tout ce qui
    /// n'est pas DLNA, sans `SetNext`, ou loin de la fin.
    #[test]
    fn un_appareil_qui_enchaine_seul_garde_le_sondage_normal() {
        // DMP-A8 : DLNA, SetNext envoyé, dernière seconde — mais il enchaîne.
        assert!(!doit_guetter_la_frontiere(
            true,
            true,
            false,
            DUREE_FILE,
            DUREE_RAPPORTEE,
            236_000
        ));
        // DMP-A6 connu : oui.
        assert!(doit_guetter_la_frontiere(
            true,
            true,
            true,
            DUREE_FILE,
            DUREE_RAPPORTEE,
            236_000
        ));
        // Loin de la fin, déjà à la durée, pas de SetNext, pas DLNA : non.
        assert!(!doit_guetter_la_frontiere(
            true,
            true,
            true,
            DUREE_FILE,
            DUREE_RAPPORTEE,
            200_000
        ));
        assert!(!doit_guetter_la_frontiere(
            true,
            true,
            true,
            DUREE_FILE,
            DUREE_RAPPORTEE,
            237_000
        ));
        assert!(!doit_guetter_la_frontiere(
            true,
            false,
            true,
            DUREE_FILE,
            DUREE_RAPPORTEE,
            236_000
        ));
        assert!(!doit_guetter_la_frontiere(
            false,
            true,
            true,
            DUREE_FILE,
            DUREE_RAPPORTEE,
            236_000
        ));
        // Durée rapportée absente ou incohérente avec la file : non.
        assert!(!doit_guetter_la_frontiere(
            true, true, true, DUREE_FILE, 0, 236_000
        ));
        assert!(!doit_guetter_la_frontiere(
            true, true, true, DUREE_FILE, 200_000, 199_500
        ));
    }

    /// Le guet abandonne si le renderer n'atteint jamais sa durée (pause).
    #[tokio::test(start_paused = true)]
    async fn le_guet_abandonne_un_renderer_en_pause() {
        let r = guetter_la_frontiere(
            || async { Some(236_000) },
            DUREE_RAPPORTEE,
            PAS_DU_GUET,
            PLAFOND_DU_GUET,
        )
        .await;
        assert!(r.is_none());
        let r = guetter_la_frontiere(
            || async { None },
            DUREE_RAPPORTEE,
            PAS_DU_GUET,
            PLAFOND_DU_GUET,
        )
        .await;
        assert!(r.is_none());
    }

    /// Le sondeur ne conclut sur la foi du guetteur que pour LE flux guetté,
    /// et une fois l'instant atteint.
    #[tokio::test(start_paused = true)]
    async fn la_conclusion_vaut_pour_le_flux_guette_et_a_l_heure() {
        let t = Instant::now();
        let fin = FinPrecise {
            flux: "abc".into(),
            conclure_a: Some(t + Duration::from_millis(500)),
        };
        assert!(!conclusion_permise(Some(&fin), Some("abc"), t));
        assert!(conclusion_permise(
            Some(&fin),
            Some("abc"),
            t + Duration::from_millis(500)
        ));
        assert!(!conclusion_permise(
            Some(&fin),
            Some("autre"),
            t + Duration::from_secs(1)
        ));
        let en_guet = FinPrecise {
            flux: "abc".into(),
            conclure_a: None,
        };
        assert!(!conclusion_permise(
            Some(&en_guet),
            Some("abc"),
            t + Duration::from_secs(1)
        ));
        assert!(!conclusion_permise(None, Some("abc"), t));
    }
}
