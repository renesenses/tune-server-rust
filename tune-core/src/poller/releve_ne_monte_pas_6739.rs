//! #6739 — un relevé du volume de l'appareil ne fait jamais grimper la
//! consigne par cliquet ; #7379 — décision de Bertrand du 02/11 : Tune SUIT
//! l'appareil monté hors de Tune tant qu'il reste sous le plafond de suivi.
//!
//! Fil 2366/2375 (Marantz NR1200 en DLNA, tweeter détruit) : le trim de
//! +8,5 dB faisait partir `SetVolume 96` pour une consigne de 36 %, et les
//! trois sites d'adoption de `tick.rs` recopiaient ensuite le 96 relu comme
//! consigne. Le geste suivant le remultipliait : cliquet vers 100 %. Le trim
//! positif ne part plus vers un appareil à volume matériel (#6749), et le
//! relevé est ramené à l'échelle de la consigne (trim retiré).
//!
//! Les trois sites passent par [`PositionPoller::releve_adoptable`] puis
//! [`PositionPoller::appliquer_la_suite_du_releve`] :
//! - plus BAS (télécommande, molette) : adopté, comme avant ;
//! - plus HAUT mais sous le plafond de suivi (la limite de sécurité de la
//!   zone, ou [`crate::audio::volume_limites::PLAFOND_DE_SUIVI_PAR_DEFAUT`]
//!   sans limite réglée) : adopté aussi (#7379). La consigne, la base et le
//!   curseur (évènement `volume`) s'alignent sur l'appareil ; rien n'est
//!   commandé. Ligne `volume_releve_plus_haut_suivi` ;
//! - au-dessus du plafond : la consigne passe AU plafond et Tune y ramène
//!   l'appareil (`volume_releve_ramene_au_plafond`). La valeur envoyée ne
//!   dépasse jamais le plafond (`borner_appareil`, trim compris).
//!
//! Débit : la règle est sur front (l'appareil doit avoir bougé depuis le tour
//! précédent), donc une ligne par mouvement, pas par tour.
use super::decisions::{JugementDuReleve, juger_le_releve_de_volume};
use super::*;

/// Ce que le sondeur fait d'un relevé jugé (#7379).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum SuiteDuReleve {
    /// La consigne devient cette valeur (échelle de consigne) ; rien n'est
    /// commandé à l'appareil.
    Adopter(f64),
    /// La consigne devient le plafond (échelle de consigne), et l'appareil y
    /// est ramené.
    RamenerAuPlafond(f64),
}

impl PositionPoller {
    /// Ce qu'il faut faire du volume `rapporte` par l'appareil, ou `None`.
    /// Jamais au-dessus du plafond de suivi de la zone.
    pub(super) fn releve_adoptable(
        &self,
        zone_id: i64,
        appareil: &str,
        precedent: Option<f64>,
        rapporte: f64,
        consigne: f64,
    ) -> Option<SuiteDuReleve> {
        // #6912 — AIR est suivi par son annonce fraîche, même arrêté.
        // Le repli get_status sur volume_voulu n'est pas une observation.
        if appareil.starts_with("devialet-air:") {
            return None;
        }
        let a_bouge = precedent.is_some_and(|p| (rapporte - p).abs() > 1e-9);
        // Le trim ne se lit en base qu'au premier relevé et quand l'appareil
        // a bougé.
        if !a_bouge && precedent.is_some() {
            return None;
        }
        let trim_db = crate::orchestrator::gain_trim_db_enregistre(&self.db, zone_id);
        let facteur = super::decisions::facteur_de_releve(trim_db);
        // #6721 — le niveau réel de l'appareil devient le repère de la garde
        // contre les montées brusques (jamais plus haut que la mémoire) : au
        // PREMIER relevé aussi, qui n'est jamais adopté (#358).
        if rapporte.is_finite() && rapporte > 0.001 && facteur > 0.0 {
            self.orchestrator
                .niveaux_des_appareils
                .noter(zone_id, rapporte / facteur);
        }
        if !a_bouge {
            return None;
        }
        // #7379 — le plafond de suivi, en échelle de l'appareil : la limite de
        // sécurité effective, sinon le plafond par défaut (jamais sous ce que
        // Tune commande lui-même).
        let plafond = crate::audio::volume_limites::lire_effectives(&self.db, zone_id)
            .plafond_de_suivi(consigne * facteur);
        let mut jugement =
            juger_le_releve_de_volume(precedent, rapporte, consigne, trim_db, plafond);
        // #7379, #8074 — une sortie à volume LOGICIEL (locale, navigateur) n'a
        // pas de bouton d'ampli : son volume relu est le gain que Tune lui a
        // posé lui-même (gain de départ, sourdine rendue). Plus haut que la
        // consigne, il n'est jamais suivi ni « ramené » vers le haut.
        if !self.suivi_a_la_hausse_permis(zone_id) {
            jugement = match jugement {
                JugementDuReleve::Adopter(v) if v > consigne => {
                    JugementDuReleve::PlusHautNonAdopte(v)
                }
                JugementDuReleve::RamenerAuPlafond(_) => {
                    JugementDuReleve::PlusHautNonAdopte(rapporte / facteur)
                }
                autre => autre,
            };
        }
        match jugement {
            JugementDuReleve::Adopter(v) => {
                if v > consigne {
                    info!(
                        zone_id,
                        appareil,
                        consigne,
                        adopte = v,
                        rapporte,
                        plafond,
                        trim_db,
                        "volume_releve_plus_haut_suivi"
                    );
                }
                Some(SuiteDuReleve::Adopter(v))
            }
            JugementDuReleve::RamenerAuPlafond(v) => {
                warn!(
                    zone_id,
                    appareil,
                    consigne,
                    rapporte,
                    plafond,
                    nouvelle_consigne = v,
                    trim_db,
                    "volume_releve_ramene_au_plafond"
                );
                Some(SuiteDuReleve::RamenerAuPlafond(v))
            }
            JugementDuReleve::PlusHautNonAdopte(releve) => {
                info!(
                    zone_id,
                    appareil,
                    consigne,
                    releve,
                    rapporte,
                    trim_db,
                    "volume_releve_plus_haut_non_adopte"
                );
                None
            }
            JugementDuReleve::Ignorer => None,
        }
    }

    /// #7379 — le suivi à la HAUSSE ne vaut que pour un appareil à volume
    /// matériel (DLNA, OpenHome, AirPlay…) : un geste au bouton de l'ampli.
    /// Une sortie à volume logiciel (`local`, `browser` : échelle `db60`) n'a
    /// pas de bouton hors de Tune ; sa consigne ne monte que par Tune.
    pub(super) fn suivi_a_la_hausse_permis(&self, zone_id: i64) -> bool {
        let output_type = crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_type);
        suivi_a_la_hausse_pour(output_type.as_deref())
    }

    /// Applique `suite`.
    ///
    /// - `Adopter` : la consigne de la zone en mémoire (évènement `volume`
    ///   vers les clients : le curseur suit) et en base. Rien n'est commandé.
    /// - `RamenerAuPlafond` : la commande part D'ABORD, la consigne d'avant
    ///   le relevé (`ancien`) restant le repère de la garde de hausse
    ///   (#6919, #6721) ; la consigne devient ce que la garde a retenu
    ///   (`VolumeRetenu::retenu`). L'appareil finit au plafond, ou plus bas,
    ///   jamais plus haut que la consigne d'avant augmentée du seuil.
    ///
    /// Rien n'est fait si une consigne de Tune est arrivée depuis le relevé
    /// (grâce de volume ouverte, ou mémoire qui n'est plus `ancien`) : le
    /// geste de l'auditeur l'emporte sur un relevé déjà périmé.
    pub(super) async fn appliquer_la_suite_du_releve(
        &self,
        zone_id: i64,
        appareil: &str,
        ancien: f64,
        suite: SuiteDuReleve,
    ) {
        if self.consigne_posee_depuis_le_releve(zone_id, ancien).await {
            info!(
                zone_id,
                appareil,
                ancien,
                ?suite,
                "volume_releve_perime_ecarte"
            );
            return;
        }
        match suite {
            SuiteDuReleve::Adopter(nouveau) => {
                self.adopter_la_consigne(zone_id, nouveau).await;
                // #6008 — l'adoption se dit au journal.
                volume_adopte_6008::journaliser_volume_adopte(zone_id, appareil, ancien, nouveau);
            }
            SuiteDuReleve::RamenerAuPlafond(cible) => {
                if let Some(retenu) = self.commander_le_plafond(zone_id, cible).await {
                    volume_adopte_6008::journaliser_volume_adopte(
                        zone_id, appareil, ancien, retenu,
                    );
                }
            }
        }
    }

    /// #7379 — vrai quand une consigne a été posée depuis le relevé : la
    /// grâce de volume est ouverte, ou la mémoire n'est plus `ancien`.
    pub(super) async fn consigne_posee_depuis_le_releve(&self, zone_id: i64, ancien: f64) -> bool {
        let etat = self.playback.get_state(zone_id).await;
        etat.last_volume_set_at
            .is_some_and(|t| t.elapsed().as_secs() < VOLUME_GRACE_SECS)
            || (etat.volume - ancien).abs() > 1e-3
    }

    /// La consigne de la zone devient `volume`, en mémoire (évènement
    /// `volume`) et en base. Rien n'est commandé.
    pub(super) async fn adopter_la_consigne(&self, zone_id: i64, volume: f64) {
        self.playback.set_volume(zone_id, volume).await;
        // #2886 — `as i32` TRONQUAIT : le volume adopte du renderer
        // tombait a 0 sous 0,01 lineaire (-40 dB).
        crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
            .update_volume(zone_id, volume * 100.0)
            .ok();
    }

    /// #7379 — ramène l'appareil vers `cible` (échelle de consigne) par le
    /// point de passage unique, SOUS la garde de hausse : son repère est la
    /// consigne en mémoire, celle d'avant le relevé, jamais le relevé. La
    /// limite de sécurité borne la valeur envoyée, trim compris. En cas de
    /// succès, l'orchestrateur a déjà écrit la valeur retenue en mémoire et
    /// en base ; elle est rendue. `None` en cas d'échec : rien n'est adopté.
    pub(super) async fn commander_le_plafond(&self, zone_id: i64, cible: f64) -> Option<f64> {
        let device_id = self.get_zone_device_id(zone_id);
        match self
            .orchestrator
            .set_volume_avec_garde(
                zone_id,
                cible,
                device_id.as_deref(),
                crate::audio::garde_de_hausse::Hausse::Gardee,
            )
            .await
        {
            Ok(r) => {
                info!(
                    zone_id,
                    cible,
                    retenu = r.retenu,
                    "volume_ramene_au_plafond_ok"
                );
                Some(r.retenu)
            }
            Err(e) => {
                warn!(zone_id, cible, error = %e, "volume_ramene_au_plafond_echec");
                None
            }
        }
    }
}

/// #7379 — `true` pour une sortie à volume matériel connue, `false` pour une
/// sortie à volume logiciel (même partage que
/// [`crate::audio::volume_scale::EchelleDeVolume::de_la_sortie`]).
/// Un type de sortie inconnu (`NULL` en base) n'est pas suivi vers le haut :
/// la prudence.
pub(super) fn suivi_a_la_hausse_pour(output_type: Option<&str>) -> bool {
    output_type.is_some()
        && crate::audio::volume_scale::EchelleDeVolume::de_la_sortie(output_type)
            != crate::audio::volume_scale::EchelleDeVolume::Perceptive
}
