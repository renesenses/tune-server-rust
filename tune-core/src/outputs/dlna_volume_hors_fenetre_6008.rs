//! 🔴 #6008 (rc7, fil 2401) — la Cabasse Abyss remonte son volume à 80 %
//! HORS de la fenêtre qui suit la pose d'URI.
//!
//! Journal de pissa7 (1.0.0-rc7, Windows) : la réimposition après pose
//! (#6207) et la correction du saut dans les 15 s font leur travail
//! (`dlna_volume_reimpose_apres_saut lu_pct=80 volume_pct=22`). Puis, 33 s
//! après le début de la piste, sans aucun `SetVolume` de Tune depuis 31 s
//! (`dlna_set_volume_ok` suivant : `regroupes=0`), l'appareil rapporte de
//! nouveau 80 : `volume_adopte_du_renderer ancien=22.0 nouveau=80.0`. Hors de
//! la fenêtre, la lecture passait telle quelle : l'appareil restait à 80, et la
//! rc7 en faisait la consigne de la zone (curseur à 80, premier geste de
//! l'auditeur parti à 78). Le lot rc8 refuse déjà l'adoption (#6739, un relevé
//! ne remonte jamais la consigne), mais l'appareil restait à 80, curseur à 22.
//!
//! Règle : sur un appareil qui PORTE la règle « volume remonté à l'URI »
//! (apprise dans la fenêtre, jamais ici), une hausse de plus de 2 points
//! au-dessus de la dernière consigne de Tune est corrigée à tout moment, à
//! trois conditions :
//!
//! - Tune lit sur cet appareil : transport non arrêté, et la piste en cours
//!   vient de la même origine (schéma, hôte, port) que la dernière pose de
//!   Tune. Une source propre de l'appareil (application du fabricant, radio
//!   intégrée) n'est jamais touchée ;
//! - aucune demande de volume de Tune depuis [`GRACE_APRES_DEMANDE`] : une
//!   lecture faite pendant un glissement de curseur peut précéder
//!   l'acquittement ;
//! - une BAISSE n'est jamais corrigée.
//!
//! Prix assumé : sur cet appareil, une hausse faite hors de Tune (télécommande,
//! application Cabasse) pendant que Tune lit est ramenée à la consigne de
//! Tune. Une hausse faite depuis Tune passe : elle EST la consigne.
//!
//! ## Fin de la règle stricte (#7379, décision de Bertrand du 02/11)
//!
//! Le prix ci-dessus n'est plus payé : une règle commune à tous les appareils
//! remplace celle-ci. Hors de la fenêtre d'après pose, la sortie DLNA ne
//! corrige plus rien : elle rend le relevé tel quel au sondeur, qui SUIT
//! l'appareil sous le plafond de suivi de la zone et le ramène au plafond
//! au-dessus (`poller::releve_ne_monte_pas_6739`). L'Abyss qui remonte seule
//! à 80 % n'est donc plus tenue à la consigne : sans limite de sécurité réglée,
//! elle est SUIVIE (80 % est sous le plafond par défaut de 85 %). La correction DANS la
//! fenêtre d'après pose (#6207, l'appareil qui remonte à chaque URI) reste.
//!
//! ## Le saut connu n'est pas suivi (#8328, fil 2740, rc11)
//!
//! Journal de pissa7 en rc11 : l'Abyss remonte à 80 à chaque piste, la
//! fenêtre corrige (`lu_pct=80`, six fois), puis 0,7 s après la fin de la
//! fenêtre l'appareil rapporte encore 80 et le sondeur le SUIT
//! (`volume_releve_plus_haut_suivi consigne=0.2 adopte=0.8 plafond=0.85`).
//! La valeur du saut corrigé dans la fenêtre est donc retenue ; hors de la
//! fenêtre, une hausse d'au moins 10 points qui retombe sur elle (±1) est
//! réimposée à la consigne de Tune. Toute autre hausse reste suivie.
use std::time::Duration;

use std::sync::atomic::Ordering;

use tracing::{debug, warn};

use super::DlnaOutput;
use crate::outputs::traits::{OutputTarget, TransportState};

/// Délai, après une demande de volume de Tune, pendant lequel une lecture de
/// l'appareil hors fenêtre n'est pas jugée.
pub(crate) const GRACE_APRES_DEMANDE: Duration = Duration::from_secs(3);

/// Schéma + autorité (`http://hote:port`) d'une URI, en minuscules.
fn origine(uri: &str) -> Option<String> {
    let uri = uri.trim();
    let (schema, reste) = uri.split_once("://")?;
    let autorite = reste.split(['/', '?', '#']).next()?;
    if autorite.is_empty() {
        return None;
    }
    Some(format!(
        "{}://{}",
        schema.to_ascii_lowercase(),
        autorite.to_ascii_lowercase()
    ))
}

/// La piste que l'appareil dit jouer vient-elle du même serveur que la
/// dernière pose de Tune ?
pub(crate) fn meme_origine(piste_de_l_appareil: &str, pose_de_tune: &str) -> bool {
    matches!(
        (origine(piste_de_l_appareil), origine(pose_de_tune)),
        (Some(a), Some(b)) if a == b
    )
}

impl DlnaOutput {
    /// Retient qu'une demande de volume de Tune part maintenant.
    pub(super) fn noter_demande_de_volume(&self, volume: f64) {
        self.volume_demande_pct.store(
            (volume.clamp(0.0, 1.0) * 100.0).round() as u64,
            Ordering::Relaxed,
        );
        *self
            .volume_demande_a
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
    }

    /// Banc #8328 : une demande de volume de Tune partie, dont l'acquittement
    /// n'est pas encore revenu (la consigne connue n'a pas suivi).
    #[cfg(test)]
    pub(crate) fn demande_de_volume_en_vol(&self, volume: f64) {
        self.noter_demande_de_volume(volume);
    }

    /// Banc : fait comme si la dernière demande de volume datait d'au-delà de
    /// [`GRACE_APRES_DEMANDE`].
    #[cfg(test)]
    pub(crate) fn vieillir_la_demande_de_volume(&self) {
        let mut d = self
            .volume_demande_a
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *d = d.and_then(|t| t.checked_sub(GRACE_APRES_DEMANDE + Duration::from_secs(1)));
    }

    /// Tune lit-il sur cet appareil ? Transport non arrêté, et piste de la
    /// même origine que la dernière pose acceptée.
    pub(super) fn lecture_de_tune(&self, etat: TransportState, piste: Option<&str>) -> bool {
        if etat == TransportState::Stopped {
            return false;
        }
        let (Some(piste), Some(pose)) = (piste, self.pose_courante()) else {
            return false;
        };
        meme_origine(piste, &pose.url)
    }

    /// Hors de la fenêtre d'après pose : la lecture passe telle quelle au
    /// sondeur (#7379), et elle ne devient la consigne que par sa règle
    /// commune (`poller::releve_ne_monte_pas_6739`). Une hausse sur un
    /// appareil qui porte la règle « volume remonté à l'URI » se dit au
    /// journal de débogage, sans commande.
    pub(super) async fn volume_hors_fenetre(
        &self,
        lu: f64,
        connu: Option<f64>,
        lecture_de_tune: bool,
    ) -> f64 {
        let Some(connu) = connu else {
            return lu;
        };
        if !lecture_de_tune || lu - connu <= 0.02 {
            return lu;
        }
        if !crate::outputs::dlna_repli_set_uri::volume_remonte_a_l_uri(&self.device_id) {
            return lu;
        }
        // #8328 — le saut connu de l'appareil n'est pas un geste au bouton.
        if self.est_le_saut_connu(lu, connu) {
            return self.reimposer_hors_fenetre(lu, connu).await;
        }
        let en_grace = self
            .volume_demande_a
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|t| t.elapsed() < GRACE_APRES_DEMANDE);
        if !en_grace {
            debug!(
                device = %self.name,
                device_id = %self.device_id,
                lu_pct = (lu * 100.0).round() as u64,
                volume_pct = (connu * 100.0).round() as u64,
                "dlna_volume_hausse_hors_fenetre_laissee_au_sondeur"
            );
        }
        lu
    }

    /// #8328 — la lecture est-elle le saut que l'appareil fait de lui-même
    /// (vu et corrigé dans la fenêtre d'après pose) ? Une hausse d'au moins
    /// [`super::HAUSSE_QUI_APPREND`] au-dessus de la consigne de Tune, qui
    /// retombe à un point près sur la valeur du saut retenu. Un geste au
    /// bouton monte pas à pas et ne retombe pas sur cette valeur fixe : il
    /// reste suivi (#7379).
    pub(super) fn est_le_saut_connu(&self, lu: f64, connu: f64) -> bool {
        let saut = self.saut_de_l_appareil_pct.load(Ordering::Relaxed);
        if saut == u64::MAX || lu - connu < super::HAUSSE_QUI_APPREND {
            return false;
        }
        let lu_pct = (lu * 100.0).round();
        if (lu_pct - saut as f64).abs() > 1.0 {
            return false;
        }
        // Relecture : une demande de Tune à cette valeur, encore dans la
        // grâce (acquittement pas revenu, consigne connue pas encore à jour),
        // est l'auditeur qui pose 80 depuis Tune, pas l'appareil.
        let demande = self.volume_demande_pct.load(Ordering::Relaxed);
        let en_grace = self
            .volume_demande_a
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|t| t.elapsed() < GRACE_APRES_DEMANDE);
        !(en_grace && demande != u64::MAX && (lu_pct - demande as f64).abs() <= 1.0)
    }

    /// #8328 — repose la consigne de Tune sur un appareil qui a refait son
    /// saut hors de la fenêtre ; le sondeur reçoit la consigne, jamais le saut.
    /// Vaut aussi dans la grâce d'une demande : la consigne connue est la
    /// dernière de Tune, et la valeur reposée n'est jamais plus haute que le
    /// relevé.
    async fn reimposer_hors_fenetre(&self, lu: f64, connu: f64) -> f64 {
        // #8151 — en sourdine, seule la sourdine est reposée, comme dans la
        // fenêtre : un `SetVolume` à la consigne rendrait le son à un appareil
        // tenu muet par un volume nul, ou à un firmware qui lève sa sourdine
        // sur `SetVolume`.
        if self.en_sourdine() {
            self.reimposer_la_sourdine("saut_connu_hors_fenetre").await;
            if self.sourdine_par_volume_nul.load(Ordering::Relaxed) {
                return 0.0;
            }
            return connu;
        }
        match self.set_volume(connu).await {
            Ok(()) => {
                warn!(
                    device = %self.name,
                    device_id = %self.device_id,
                    lu_pct = (lu * 100.0).round() as u64,
                    volume_pct = (connu * 100.0).round() as u64,
                    "dlna_volume_saut_connu_reimpose_hors_fenetre"
                );
                connu
            }
            Err(e) => {
                warn!(
                    device = %self.name,
                    error = %e,
                    "dlna_volume_saut_connu_reimpose_hors_fenetre_echoue"
                );
                lu
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::meme_origine;

    #[test]
    fn l_origine_compare_schema_hote_et_port() {
        assert!(meme_origine(
            "http://192.0.2.10:8888/stream/b.flac",
            "http://192.0.2.10:8888/stream/a.flac"
        ));
        assert!(meme_origine(
            "HTTP://Host:8888/stream/b.flac?x=1",
            "http://host:8888/stream/a.flac"
        ));
        assert!(!meme_origine(
            "http://192.0.2.10:9000/radio",
            "http://192.0.2.10:8888/stream/a.flac"
        ));
        assert!(!meme_origine(
            "x-rincon-stream:abc",
            "http://192.0.2.10:8888/stream/a.flac"
        ));
        assert!(!meme_origine("", "http://192.0.2.10:8888/a"));
    }
}
