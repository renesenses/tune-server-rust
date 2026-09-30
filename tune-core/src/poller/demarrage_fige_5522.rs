//! #5522 — une piste « en lecture » dont la position reste à 0.
//!
//! # Le constat
//!
//! Fil forum 2048 (Windows, Qobuz) : certains morceaux restent à 0:00, sans
//! aucun son et sans bandeau. Le compteur de l'écran monte à 0:02 puis retombe,
//! en boucle. Ce n'est pas une relance : le client fait avancer le compteur
//! tout seul tant que la zone est `playing`, et se recale sur la position du
//! serveur dès que l'écart dépasse 2 s (`tune-web-client`,
//! `src/lib/v2Live.ts`, `DERIVE_MAX_MS`). Or le sondeur publie 0 à chaque tour.
//!
//! Aucune garde ne voyait ce démarrage-là :
//! - le chien de garde « en lecture mais morte » (#2116) n'est armé qu'après
//!   5 s de position atteinte (`dlna_playing_stall_eligible`) ;
//! - le « démarrage mort » (#2394) n'est jugé que dans le bras `Stopped` ;
//! - la sortie locale dit `Playing` dès `play_url`, même quand son amont
//!   ne répond pas.
//!
//! # La règle (go de Bertrand, 30/09/2026)
//!
//! Une zone qui JOUE, dont la sortie dit `Playing` ou `Transitioning` et dont
//! la position n'a JAMAIS quitté 0 depuis [`DEMARRAGE_FIGE_SECS`] :
//! 1. première fois : UNE relance automatique de la même piste ;
//! 2. si la relance reste figée à 0 à son tour : arrêt de la zone, avec le
//!    bandeau « Le morceau n'a pas démarré » (`zone.playback_error`, `fatal`).
//!
//! Jamais de boucle : la relance est notée HORS de l'état de sondage (que la
//! relance recrée), et la seconde détection sur la même ligne de file arrête.
//!
//! Ne déclenchent RIEN : une pause (l'horloge repart de zéro), une position
//! qui démarre avant 8 s, un flux dont les octets servis progressent encore
//! (le renderer tamponne), une radio, un déplacement récent, et un renderer
//! qui n'a jamais prouvé qu'il rapporte sa position (certains rendent 0 en
//! permanence tout en jouant).

use std::time::{Duration, Instant};

use tracing::warn;

/// Délai au-delà duquel une position restée à 0 n'est plus un démarrage lent.
pub(super) const DEMARRAGE_FIGE_SECS: u64 = 8;

/// Durée pendant laquelle une relance reste « la relance de cette piste ».
/// Couvre largement la résolution du flux puis les 8 s d'observation ; au-delà
/// la note est périmée et ne peut plus changer une relance en arrêt.
pub(super) const RELANCE_FIGEE_VALIDE_SECS: u64 = 120;

/// Ce que le sondeur voit à ce tour, pour cette zone.
#[derive(Debug, Clone, Copy)]
pub(super) struct Constat {
    /// Tune tient la zone en lecture (pas en pause, pas à l'arrêt).
    pub(super) tune_joue: bool,
    /// La sortie dit `Playing` ou `Transitioning`.
    pub(super) sortie_joue: bool,
    /// La sortie consomme à 1x (tout sauf un enregistreur).
    pub(super) temps_reel: bool,
    /// Flux de radio : une position à 0 n'y veut rien dire.
    pub(super) radio: bool,
    /// Un déplacement vient d'avoir lieu.
    pub(super) en_grace_deplacement: bool,
    pub(super) position_ms: u64,
    /// Position la plus haute atteinte sur cette piste.
    pub(super) position_max_ms: u64,
    /// La sortie sait rapporter sa position : locale par construction, ou
    /// renderer qui a déjà rapporté une position non nulle.
    pub(super) position_prouvee: bool,
    /// Les octets servis ont avancé depuis le tour précédent.
    pub(super) octets_progressent: bool,
}

/// La piste est-elle, à ce tour, un démarrage encore à zéro ?
pub(super) fn encore_a_zero(c: &Constat) -> bool {
    c.tune_joue
        && c.sortie_joue
        && c.temps_reel
        && !c.radio
        && !c.en_grace_deplacement
        && c.position_prouvee
        && c.position_ms == 0
        && c.position_max_ms == 0
        && !c.octets_progressent
}

/// Ce qu'il faut faire de la zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Rien, ou pas encore.
    Attendre,
    /// Première détection : relancer la même piste, une fois.
    Relancer,
    /// La relance est restée figée : arrêter la zone, avec le bandeau.
    Arreter,
}

/// Tient l'horloge du démarrage figé et rend le verdict du tour.
///
/// `fige_depuis` est l'état tenu par le sondeur : posé au premier tour à zéro,
/// effacé dès que le constat cesse (pause, position qui démarre, octets qui
/// avancent). `deja_relancee` : une relance de CETTE ligne de file est encore
/// valide (voir [`relance_encore_valide`]).
pub(super) fn verdict(
    c: &Constat,
    fige_depuis: &mut Option<Instant>,
    maintenant: Instant,
    deja_relancee: bool,
) -> Verdict {
    if !encore_a_zero(c) {
        *fige_depuis = None;
        return Verdict::Attendre;
    }
    let depuis = *fige_depuis.get_or_insert(maintenant);
    if maintenant.saturating_duration_since(depuis) < Duration::from_secs(DEMARRAGE_FIGE_SECS) {
        return Verdict::Attendre;
    }
    if deja_relancee {
        Verdict::Arreter
    } else {
        Verdict::Relancer
    }
}

/// La relance notée pour cette zone vaut-elle encore pour cette ligne de file ?
pub(super) fn relance_encore_valide(
    note: Option<(i64, Instant)>,
    ligne_de_file: i64,
    maintenant: Instant,
) -> bool {
    note.is_some_and(|(ligne, a)| {
        ligne == ligne_de_file
            && maintenant.saturating_duration_since(a)
                < Duration::from_secs(RELANCE_FIGEE_VALIDE_SECS)
    })
}

/// Le texte du bandeau, quand la relance n'a rien changé.
pub(super) fn message_d_arret(titre: &str) -> String {
    format!(
        "Le morceau n'a pas démarré : « {titre} » est resté à 0:00 pendant \
         {DEMARRAGE_FIGE_SECS} s, même après une relance automatique. \
         La zone a été arrêtée."
    )
}

/// Le texte du bandeau, quand la relance elle-même a échoué.
pub(super) fn message_de_relance_echouee(titre: &str, cause: &str) -> String {
    format!(
        "Le morceau n'a pas démarré : « {titre} » est resté à 0:00 pendant \
         {DEMARRAGE_FIGE_SECS} s, et sa relance a échoué ({cause}). \
         La zone a été arrêtée."
    )
}

impl super::PositionPoller {
    /// Exécute le verdict : relance de la même ligne de file, ou arrêt avec
    /// bandeau. L'état de sondage de la zone a déjà été retiré par l'appelant.
    pub(super) async fn agir_sur_un_demarrage_fige(
        &self,
        zone_id: i64,
        zone_state: &crate::playback::ZoneState,
        verdict: Verdict,
    ) {
        let titre = zone_state
            .now_playing
            .as_ref()
            .map(|np| np.title.clone())
            .unwrap_or_default();
        let ligne = zone_state.queue_position;
        let appareil = self.get_zone_device_id(zone_id);
        match verdict {
            Verdict::Attendre => {}
            Verdict::Relancer => {
                // Noter AVANT d'agir : la relance recrée l'état de sondage, et
                // c'est cette note seule qui changera la prochaine détection
                // en arrêt.
                if let Ok(mut r) = self.relances_demarrage_fige.lock() {
                    r.insert(zone_id, (ligne, Instant::now()));
                }
                warn!(
                    zone_id,
                    ligne,
                    titre = %titre,
                    fige_secs = DEMARRAGE_FIGE_SECS,
                    "demarrage_fige_relance_automatique_5522"
                );
                self.orchestrator.stop(zone_id, appareil.as_deref()).await;
                if let Err(e) = self.orchestrator.play_from_queue(zone_id, ligne).await {
                    warn!(zone_id, ligne, error = %e, "demarrage_fige_relance_echouee_5522");
                    self.oublier_la_relance(zone_id);
                    self.arreter_avec_bandeau(
                        zone_id,
                        appareil.as_deref(),
                        message_de_relance_echouee(&titre, &e),
                    )
                    .await;
                }
            }
            Verdict::Arreter => {
                warn!(
                    zone_id,
                    ligne,
                    titre = %titre,
                    fige_secs = DEMARRAGE_FIGE_SECS,
                    "demarrage_fige_apres_relance_arret_5522"
                );
                self.oublier_la_relance(zone_id);
                self.arreter_avec_bandeau(zone_id, appareil.as_deref(), message_d_arret(&titre))
                    .await;
            }
        }
    }

    fn oublier_la_relance(&self, zone_id: i64) {
        if let Ok(mut r) = self.relances_demarrage_fige.lock() {
            r.remove(&zone_id);
        }
    }

    /// Le même canal que les autres gardes du sondeur : `zone.playback_error`,
    /// `fatal` pour que la fenêtre de grâce du client ne l'avale pas.
    async fn arreter_avec_bandeau(&self, zone_id: i64, appareil: Option<&str>, message: String) {
        if let Some(ref bus) = self.event_bus {
            bus.emit(
                "zone.playback_error",
                serde_json::json!({
                    "zone_id": zone_id,
                    "error": message,
                    "fatal": true,
                }),
            );
        }
        self.orchestrator.stop(zone_id, appareil).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fige() -> Constat {
        Constat {
            tune_joue: true,
            sortie_joue: true,
            temps_reel: true,
            radio: false,
            en_grace_deplacement: false,
            position_ms: 0,
            position_max_ms: 0,
            position_prouvee: true,
            octets_progressent: false,
        }
    }

    #[test]
    fn huit_secondes_a_zero_relancent_puis_arretent() {
        let t0 = Instant::now();
        let mut depuis = None;
        assert_eq!(verdict(&fige(), &mut depuis, t0, false), Verdict::Attendre);
        let t7 = t0 + Duration::from_secs(7);
        assert_eq!(verdict(&fige(), &mut depuis, t7, false), Verdict::Attendre);
        let t8 = t0 + Duration::from_secs(8);
        assert_eq!(verdict(&fige(), &mut depuis, t8, false), Verdict::Relancer);
        assert_eq!(verdict(&fige(), &mut depuis, t8, true), Verdict::Arreter);
    }

    #[test]
    fn chaque_temoin_de_vie_efface_l_horloge() {
        let t0 = Instant::now();
        let tard = t0 + Duration::from_secs(30);
        let cas: [fn(&mut Constat); 9] = [
            |c| c.tune_joue = false,
            |c| c.sortie_joue = false,
            |c| c.temps_reel = false,
            |c| c.radio = true,
            |c| c.en_grace_deplacement = true,
            |c| c.position_ms = 1,
            |c| c.position_max_ms = 1_000,
            |c| c.position_prouvee = false,
            |c| c.octets_progressent = true,
        ];
        for (i, modifier) in cas.iter().enumerate() {
            let mut c = fige();
            modifier(&mut c);
            let mut depuis = Some(t0);
            assert_eq!(
                verdict(&c, &mut depuis, tard, false),
                Verdict::Attendre,
                "cas {i}"
            );
            assert!(
                depuis.is_none(),
                "cas {i} : l'horloge doit repartir de zéro"
            );
        }
    }

    #[test]
    fn la_relance_ne_vaut_que_pour_sa_ligne_et_un_temps() {
        let t0 = Instant::now();
        assert!(relance_encore_valide(
            Some((3, t0)),
            3,
            t0 + Duration::from_secs(20)
        ));
        assert!(!relance_encore_valide(Some((3, t0)), 4, t0));
        assert!(!relance_encore_valide(
            Some((3, t0)),
            3,
            t0 + Duration::from_secs(RELANCE_FIGEE_VALIDE_SECS)
        ));
        assert!(!relance_encore_valide(None, 3, t0));
    }
}
