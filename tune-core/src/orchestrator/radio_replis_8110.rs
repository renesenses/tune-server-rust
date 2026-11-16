//! #8110 — les flux de REPLI d'une station, retenus au lancement.
//!
//! Une fiche d'annuaire publie plusieurs flux ; la demande de lecture n'en
//! porte qu'un (`PlayRequest::source_id`). Quand le diffuseur refuse ce flux
//! depuis le pays d'écoute (403, 451 : BBC Radio 6 Music, flux `uk` écouté
//! depuis la France), la lecture s'arrêtait sur « refuse l'accès à son flux »
//! et l'auditeur devait choisir lui-même un autre flux de la fiche, alors
//! qu'elle en propose d'autres qui marchent (fil forum 2556, reste de #7515).
//!
//! La route retient ici, par adresse de flux (à la normalisation près, comme
//! [`crate::poller::logo_de_lancement`]), la liste ORDONNÉE des autres flux
//! autorisés de la fiche, du meilleur au moins bon ; le décodeur
//! (`decode_radio_stream_to_pcm_avec_profondeur`) la relève au départ et, sur
//! un refus du diffuseur SEULEMENT, passe au flux suivant. Une coupure réseau
//! ou une station morte gardent la reconnexion sur le même flux.
//!
//! `PlayRequest` n'a pas gagné de champ : plus de soixante-dix constructions
//! sans `..Default::default()` auraient bougé pour une seule lecture radio.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use crate::radio_metadata_relais::flux_normalise;

/// Au-delà, le lancement le plus ANCIEN est oublié, et lui seul.
pub(crate) const STATIONS_RETENUES_AU_PLUS: usize = 256;

type Replis = HashMap<String, (u64, Vec<String>)>;

/// Mémoire du processus, partagée : les tests qui s'en servent prennent chacun
/// une adresse de flux à eux.
static REPLIS: LazyLock<Mutex<(Replis, u64)>> = LazyLock::new(|| Mutex::new((HashMap::new(), 0)));

/// Retient les flux de repli `replis` (dans l'ordre d'essai) du flux
/// `url_flux` qui vient d'être lancé. Une liste vide efface ce qui était
/// retenu pour ce flux : une fiche qui ne publie plus d'autre flux autorisé
/// ne laisse pas un repli périmé.
pub fn retenir_les_replis_radio(url_flux: &str, replis: Vec<String>) {
    let cle = flux_normalise(url_flux);
    if cle.is_empty() {
        return;
    }
    let Ok(mut memoire) = REPLIS.lock() else {
        return;
    };
    let (retenus, rang) = &mut *memoire;
    // Le flux lancé n'est jamais son propre repli, et une adresse ne s'essaie
    // qu'une fois.
    let mut vus = vec![cle.clone()];
    let replis: Vec<String> = replis
        .into_iter()
        .filter(|r| {
            let n = flux_normalise(r);
            if n.is_empty() || vus.contains(&n) {
                false
            } else {
                vus.push(n);
                true
            }
        })
        .collect();
    if replis.is_empty() {
        retenus.remove(&cle);
        return;
    }
    *rang += 1;
    if retenus.len() >= STATIONS_RETENUES_AU_PLUS
        && !retenus.contains_key(&cle)
        && let Some(ancien) = retenus
            .iter()
            .min_by_key(|(_, (r, _))| *r)
            .map(|(c, _)| c.clone())
    {
        retenus.remove(&ancien);
    }
    retenus.insert(cle, (*rang, replis));
}

/// Les flux de repli retenus au lancement du flux `url_flux`, dans l'ordre
/// d'essai ; vide quand rien n'a été retenu.
pub(crate) fn replis_retenus(url_flux: Option<&str>) -> Vec<String> {
    let Some(url_flux) = url_flux else {
        return Vec::new();
    };
    let cle = flux_normalise(url_flux);
    REPLIS
        .lock()
        .ok()
        .and_then(|m| m.0.get(&cle).map(|(_, r)| r.clone()))
        .unwrap_or_default()
}

/// Le diffuseur a REFUSÉ ce flux (403, 451 sur une liste ou un segment HLS,
/// `radio_flux_interdit`) : le seul échec qui justifie d'essayer le flux
/// suivant de la fiche. Tout autre échec — réseau, page web, HLS illisible —
/// garde le comportement d'avant.
pub(crate) fn flux_refuse_par_le_diffuseur(erreur: &str) -> bool {
    erreur.starts_with(super::RADIO_HLS_INTERDIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn les_replis_sont_retenus_par_flux_normalise_et_sans_doublon() {
        let flux = "https://a.example/8110/uk.m3u8";
        retenir_les_replis_radio(
            flux,
            vec![
                "https://a.example/8110/ww.m3u8".into(),
                "https://A.EXAMPLE/8110/uk.m3u8".into(),
                "https://a.example/8110/ww.m3u8".into(),
                "https://a.example/8110/nonuk.m3u8".into(),
            ],
        );
        assert_eq!(
            replis_retenus(Some("https://A.EXAMPLE/8110/uk.m3u8")),
            vec![
                "https://a.example/8110/ww.m3u8".to_string(),
                "https://a.example/8110/nonuk.m3u8".to_string()
            ],
            "l'ordre est gardé, le flux lancé et les doublons sont écartés"
        );
        assert!(replis_retenus(None).is_empty());
        assert!(replis_retenus(Some("https://a.example/8110/autre.m3u8")).is_empty());
    }

    #[test]
    fn une_liste_vide_efface_le_repli_retenu() {
        let flux = "https://b.example/8110/uk.m3u8";
        retenir_les_replis_radio(flux, vec!["https://b.example/8110/ww.m3u8".into()]);
        assert_eq!(replis_retenus(Some(flux)).len(), 1);
        retenir_les_replis_radio(flux, Vec::new());
        assert!(replis_retenus(Some(flux)).is_empty());
    }

    #[test]
    fn seul_le_refus_du_diffuseur_justifie_un_repli() {
        assert!(flux_refuse_par_le_diffuseur(
            "radio_flux_interdit: le diffuseur refuse l'accès à ce flux HLS (HTTP 403)"
        ));
        assert!(!flux_refuse_par_le_diffuseur(
            "radio HTTP error: 404 Not Found"
        ));
        assert!(!flux_refuse_par_le_diffuseur("radio_not_audio: text/html"));
        assert!(!flux_refuse_par_le_diffuseur("radio_hls_unsupported: DRM"));
    }
}
