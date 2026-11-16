//! #7330 — fin naturelle OAAT : ne plus envoyer `stop` avant l'échéance du
//! dernier échantillon.
//!
//! En fin de piste, la sortie envoyait `LAST_PACKET` puis `stop` dans la
//! foulée. Or Tune cadence ses paquets en avance sur leur PTS (l'avance de
//! départ #7331, de 0,5 à 4 s, plus la rafale initiale) : au moment du
//! `LAST_PACKET`, le récepteur tient encore cette avance dans sa file. Le RFC
//! fait de `stop` une PURGE : la fin de la piste était coupée.
//!
//! Ce que fait ce module, et seulement cela : retarder le `stop` jusqu'au PTS
//! du dernier paquet envoyé, plus une marge, borné. Le PTS final est une donnée
//! de Tune, pas du récepteur : rien ici ne suppose une réponse du fabricant.
//!
//! Ce que ce module ne fait PAS : il ne connaît pas la latence du récepteur
//! après le PTS (DAC, tampon matériel). Le contrat OAAT (c96021f0) n'a ni
//! acquittement de fin consommée, ni latence annoncée (`buffer_size_ms` est une
//! capacité). La marge couvre la durée du dernier paquet et une latence de
//! sortie courte ; le reste attend la réponse du fabricant, et le protocole
//! n'est pas touché.
//!
//! Gardes : l'attente n'existe qu'en fin NATURELLE (un `stop()` de
//! l'utilisateur interrompt la tâche, donc l'attente, sur-le-champ) ; elle est
//! bornée à [`PLAFOND_ATTENTE`] (les endpoints n'arment pas un PTS au-delà de
//! cinq secondes) ; et `TUNE_OAAT_STOP_IMMEDIAT_7330=1` rend le comportement
//! d'avant.

use std::time::Duration;

use tracing::info;

/// Marge après le PTS du dernier paquet : sa propre durée (quelques dizaines
/// de millisecondes au plus) et une latence de sortie courte.
pub(super) const MARGE_APRES_LE_DERNIER_PTS: Duration = Duration::from_millis(300);

/// Plafond de l'attente, quoi que dise le PTS.
pub(super) const PLAFOND_ATTENTE: Duration = Duration::from_secs(5);

/// Variable d'environnement qui rend le `stop` immédiat d'avant #7330.
pub(super) const STOP_IMMEDIAT: &str = "TUNE_OAAT_STOP_IMMEDIAT_7330";

/// Le PTS du dernier paquet audio réellement envoyé sur le flux en cours.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct FinJouee {
    dernier_pts_ns: Option<u64>,
}

impl FinJouee {
    /// À appeler après chaque paquet audio envoyé. Le DERNIER fait foi, pas le
    /// plus grand : un déplacement réancre les PTS plus tôt.
    pub(super) fn observer(&mut self, pts_ns: u64) {
        self.dernier_pts_ns = Some(pts_ns);
    }

    /// Ce qu'il reste à attendre avant `stop`, à l'instant `maintenant_ns`
    /// (même horloge que les PTS : `helpers::now_ns`).
    pub(super) fn attente(&self, maintenant_ns: u64, stop_immediat: bool) -> Duration {
        if stop_immediat {
            return Duration::ZERO;
        }
        let Some(pts) = self.dernier_pts_ns else {
            return Duration::ZERO;
        };
        let echeance =
            pts.saturating_add(MARGE_APRES_LE_DERNIER_PTS.as_nanos().min(u64::MAX as u128) as u64);
        Duration::from_nanos(echeance.saturating_sub(maintenant_ns)).min(PLAFOND_ATTENTE)
    }

    /// Attend l'échéance du dernier échantillon, puis rend la main pour `stop`.
    pub(super) async fn attendre_avant_stop(&self, device: &str) {
        let attente = self.attente(super::helpers::now_ns(), stop_immediat_demande());
        if attente.is_zero() {
            return;
        }
        info!(
            device,
            attente_ms = attente.as_millis() as u64,
            "oaat_fin_naturelle_attend_le_dernier_echantillon_7330"
        );
        tokio::time::sleep(attente).await;
    }
}

fn stop_immediat_demande() -> bool {
    std::env::var(STOP_IMMEDIAT).is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "oui"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    #[test]
    fn sans_paquet_envoye_rien_a_attendre() {
        assert_eq!(FinJouee::default().attente(10 * S, false), Duration::ZERO);
    }

    #[test]
    fn attend_jusqu_au_dernier_pts_plus_la_marge() {
        let mut fin = FinJouee::default();
        fin.observer(12 * S);
        assert_eq!(
            fin.attente(10 * S, false),
            Duration::from_secs(2) + MARGE_APRES_LE_DERNIER_PTS
        );
    }

    #[test]
    fn un_pts_deja_passe_ne_fait_pas_attendre() {
        let mut fin = FinJouee::default();
        fin.observer(9 * S);
        assert_eq!(fin.attente(10 * S, false), Duration::ZERO);
    }

    #[test]
    fn l_attente_est_bornee() {
        let mut fin = FinJouee::default();
        fin.observer(60 * S);
        assert_eq!(fin.attente(10 * S, false), PLAFOND_ATTENTE);
    }

    #[test]
    fn le_dernier_pts_fait_foi_apres_un_deplacement() {
        let mut fin = FinJouee::default();
        fin.observer(14 * S);
        fin.observer(11 * S);
        assert_eq!(
            fin.attente(10 * S, false),
            Duration::from_secs(1) + MARGE_APRES_LE_DERNIER_PTS
        );
    }

    #[test]
    fn le_stop_immediat_rend_le_comportement_d_avant() {
        let mut fin = FinJouee::default();
        fin.observer(12 * S);
        assert_eq!(fin.attente(10 * S, true), Duration::ZERO);
    }
}
