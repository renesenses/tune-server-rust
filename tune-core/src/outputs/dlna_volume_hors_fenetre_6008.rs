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
use std::time::Duration;

use tracing::info;

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
    pub(super) fn noter_demande_de_volume(&self) {
        *self
            .volume_demande_a
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
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

    /// Hors de la fenêtre d'après pose : corrige une hausse sur un appareil
    /// qui porte la règle, sinon rend la lecture telle quelle (#6472 : elle ne
    /// devient jamais la consigne).
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
        let en_grace = self
            .volume_demande_a
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|t| t.elapsed() < GRACE_APRES_DEMANDE);
        if en_grace {
            return lu;
        }
        match self.set_volume(connu).await {
            Ok(()) => {
                info!(
                    device = %self.name,
                    device_id = %self.device_id,
                    lu_pct = (lu * 100.0).round() as u64,
                    volume_pct = (connu * 100.0).round() as u64,
                    "dlna_volume_reimpose_hors_fenetre"
                );
                connu
            }
            Err(e) => {
                tracing::warn!(
                    device = %self.name,
                    error = %e,
                    "dlna_volume_reimpose_hors_fenetre_echoue"
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
