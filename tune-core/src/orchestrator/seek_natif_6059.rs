//! #6059 — Yamaha R-N2000A (Cyrille, fil 2194) : en AIFF, FLAC ou DSF natif
//! de la bibliothèque, le renderer ACQUITTE le `Seek` SOAP sans l'exécuter.
//!
//! Décision de Bertrand (10/10/2026) : Tune relance alors le flux NATIF à
//! partir de l'octet qui correspond à la position (`audio::depart_natif`),
//! bit-perfect conservé, au lieu de compter sur le `Seek`.
//!
//! Trois temps, limités aux renderers dont le profil appris le dit :
//!
//! 1. **Constater** : après un `Seek` acquitté sur un fichier natif, le
//!    sondeur compare la position rapportée quelques secondes plus tard à la
//!    trajectoire d'AVANT (ancienne position + temps écoulé) et à celle
//!    d'APRÈS (cible + temps écoulé). Sur l'ancienne et loin de la cible :
//!    le `Seek` a été ignoré. C'est appris dans le profil de l'appareil
//!    (`dlna_repli_set_uri::memoriser_seek_inoperant`, persistant), et le
//!    déplacement est rattrapé aussitôt par le chemin 3.
//! 2. **Décider** : sur un appareil ainsi profilé, un déplacement dans un
//!    fichier natif ne passe plus par `Seek` : il relance la piste à la
//!    position (`replay_zone_at_position`).
//! 3. **Servir** : la résolution d'une piste locale vers ce renderer, avec un
//!    `seek_ms`, arme la carte du flux natif décalé et pose une URL portant
//!    `depart_ms` ; aucun `Seek` ne suit (`seek_output_after_replay`).

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use tracing::{info, warn};
use tune_output_api::{OutputStatus, TransportState};

use super::PlaybackOrchestrator;
use crate::audio::depart_natif::{DepartNatif, FormatNatif};
use crate::outputs::dlna_repli_set_uri as profil;

/// Délai avant de juger un `Seek` : le renderer doit avoir eu le temps de
/// rouvrir le flux et de rapporter une position neuve.
pub(crate) const DELAI_DE_CONSTAT: Duration = Duration::from_secs(4);
/// Au-delà, plus de verdict : trop de choses ont pu se passer.
const DELAI_MAX_DE_CONSTAT: Duration = Duration::from_secs(15);
/// Un déplacement de moins de 10 s ne sépare pas assez les deux trajectoires.
const ECART_MIN_MS: u64 = 10_000;
/// Tolérance autour de chaque trajectoire.
const MARGE_MS: u64 = 3_000;

/// Un `Seek` envoyé, à juger.
#[derive(Debug, Clone)]
struct SeekAJuger {
    device_id: String,
    avant_ms: u64,
    cible_ms: u64,
    envoye_a: Instant,
}

static A_JUGER: LazyLock<Mutex<HashMap<i64, SeekAJuger>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Le `Seek` a-t-il été IGNORÉ ? La position rapportée suit l'ancienne
/// trajectoire et pas la nouvelle. Pur.
pub(crate) fn seek_ignore(avant_ms: u64, cible_ms: u64, ecoule_ms: u64, rapporte_ms: u64) -> bool {
    if avant_ms.abs_diff(cible_ms) < ECART_MIN_MS {
        return false;
    }
    let sur_l_ancienne = rapporte_ms.abs_diff(avant_ms + ecoule_ms) <= MARGE_MS;
    let pres_de_la_cible = (cible_ms.saturating_sub(MARGE_MS)..=cible_ms + ecoule_ms + MARGE_MS)
        .contains(&rapporte_ms);
    sur_l_ancienne && !pres_de_la_cible
}

/// Note un `Seek` SOAP acquitté sur un fichier natif, pour le juger au
/// sondage (temps 1).
pub(crate) fn noter_seek_envoye(zone_id: i64, device_id: &str, avant_ms: u64, cible_ms: u64) {
    if let Ok(mut m) = A_JUGER.lock() {
        m.insert(
            zone_id,
            SeekAJuger {
                device_id: device_id.to_string(),
                avant_ms,
                cible_ms,
                envoye_a: Instant::now(),
            },
        );
    }
}

/// Au sondage : juge le dernier `Seek` noté sur la zone. `Some(position)` :
/// il a été ignoré, l'appareil est désormais profilé, et la piste est à
/// relancer à `position` (la cible, plus le temps écoulé depuis).
pub(crate) fn constater(zone_id: i64, statut: &OutputStatus) -> Option<u64> {
    let mut m = A_JUGER.lock().ok()?;
    let a_juger = m.get(&zone_id)?;
    let ecoule = a_juger.envoye_a.elapsed();
    if ecoule < DELAI_DE_CONSTAT {
        return None;
    }
    let a_juger = m.remove(&zone_id)?;
    if ecoule > DELAI_MAX_DE_CONSTAT || statut.state != TransportState::Playing {
        return None;
    }
    let ecoule_ms = ecoule.as_millis() as u64;
    if !seek_ignore(
        a_juger.avant_ms,
        a_juger.cible_ms,
        ecoule_ms,
        statut.position_ms,
    ) {
        return None;
    }
    warn!(
        zone_id,
        device_id = %a_juger.device_id,
        avant_ms = a_juger.avant_ms,
        cible_ms = a_juger.cible_ms,
        ecoule_ms,
        rapporte_ms = statut.position_ms,
        "seek_acquitte_mais_ignore_par_le_renderer"
    );
    profil::memoriser_seek_inoperant(&a_juger.device_id);
    Some(a_juger.cible_ms + ecoule_ms)
}

/// Un `Seek` attend-il d'être jugé sur cette zone ? (bancs)
#[cfg(test)]
pub(crate) fn seek_en_attente(zone_id: i64) -> bool {
    A_JUGER.lock().is_ok_and(|m| m.contains_key(&zone_id))
}

/// Oublie le `Seek` en attente de jugement (nouvelle commande, autre piste).
pub(crate) fn oublier(zone_id: i64) {
    if let Ok(mut m) = A_JUGER.lock() {
        m.remove(&zone_id);
    }
}

impl PlaybackOrchestrator {
    /// Le fichier de la session de flux, s'il est d'un format natif traité.
    pub(super) async fn fichier_natif_de_session(&self, stream_id: &str) -> Option<String> {
        let sessions = self.streamer.sessions_state();
        let sessions = sessions.lock().await;
        let session = sessions.get(stream_id)?.clone();
        drop(sessions);
        let chemin = session.file_path.lock().await.clone()?;
        FormatNatif::du_chemin(std::path::Path::new(&chemin))?;
        Some(chemin)
    }

    /// Temps 2 : sur un renderer profilé « Seek inopérant », un déplacement
    /// dans un fichier natif relance la piste à la position. `None` : pas
    /// concerné, le `Seek` ordinaire s'applique.
    pub(super) async fn deplacer_en_flux_natif_6059(
        &self,
        zone_id: i64,
        did: &str,
        position_ms: u64,
        state: &crate::playback::ZoneState,
    ) -> Option<Result<(), String>> {
        if !profil::seek_inoperant(did) {
            return None;
        }
        let np = state.now_playing.as_ref()?;
        if np.source != "local" {
            return None;
        }
        let sid = np.stream_id.as_deref()?;
        let chemin = self.fichier_natif_de_session(sid).await?;
        oublier(zone_id);
        info!(
            zone_id,
            device_id = %did,
            position_ms,
            fichier = %chemin,
            "seek_natif_relance_du_flux_a_la_position"
        );
        Some(
            self.replay_zone_at_position(zone_id, position_ms, "seek_natif")
                .await,
        )
    }

    /// Temps 3 : la carte du flux natif décalé, quand la piste se résout vers
    /// un renderer profilé avec un `seek_ms`. `None` : flux entier, comme avant.
    pub(super) async fn preparer_depart_natif_6059(
        &self,
        device_id: Option<&str>,
        seek_ms: Option<u64>,
        file_path: &str,
    ) -> Option<DepartNatif> {
        let seek_ms = seek_ms.filter(|s| *s > 0)?;
        let did = device_id?;
        if !profil::seek_inoperant(did) {
            return None;
        }
        FormatNatif::du_chemin(std::path::Path::new(file_path))?;
        let chemin = std::path::PathBuf::from(file_path);
        let depart = tokio::task::spawn_blocking(move || {
            crate::audio::depart_natif::preparer(&chemin, seek_ms)
        })
        .await
        .ok()
        .flatten();
        match &depart {
            Some(d) => info!(
                device_id = %did,
                seek_ms,
                depart_ms = d.depart_ms,
                octets = d.carte.total,
                fichier = %file_path,
                "seek_natif_flux_arme"
            ),
            None => warn!(
                device_id = %did,
                seek_ms,
                fichier = %file_path,
                "seek_natif_flux_impossible_fichier_entier"
            ),
        }
        depart
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_seek_ignore_suit_l_ancienne_trajectoire_6059() {
        // 0:30 → 3:00 demandé ; 5 s après, le Yamaha dit 0:35.
        assert!(seek_ignore(30_000, 180_000, 5_000, 35_000));
        // Recul ignoré : 3:00 → 0:30, il dit 3:05.
        assert!(seek_ignore(180_000, 30_000, 5_000, 185_000));
    }

    #[test]
    fn un_seek_execute_n_est_pas_un_seek_ignore_6059() {
        assert!(!seek_ignore(30_000, 180_000, 5_000, 184_000));
        // Position encore à la cible (tampon du renderer) : exécuté.
        assert!(!seek_ignore(30_000, 180_000, 5_000, 180_000));
        // Déplacement trop court pour trancher.
        assert!(!seek_ignore(30_000, 36_000, 5_000, 35_000));
        // Ni l'une ni l'autre (le renderer a rouvert à zéro) : pas ce défaut.
        assert!(!seek_ignore(30_000, 180_000, 5_000, 2_000));
    }

    fn statut(position_ms: u64) -> OutputStatus {
        OutputStatus {
            state: TransportState::Playing,
            position_ms,
            duration_ms: 300_000,
            volume: 0.5,
            muted: false,
            current_uri: None,
            track_title: None,
            track_artist: None,
            ended_naturally: false,
            realtime: true,
            dop_active: false,
        }
    }

    #[test]
    fn le_constat_profile_l_appareil_et_rend_la_position_a_rattraper_6059() {
        let _ = profil::base_de_test();
        let dev = "dlna:uuid-rn2000a-constat-6059";
        assert!(!profil::seek_inoperant(dev));
        let zone = 60_591;
        noter_seek_envoye(zone, dev, 30_000, 180_000);
        // Trop tôt : aucun verdict, le seek reste à juger.
        assert_eq!(constater(zone, &statut(31_000)), None);
        // Quatre secondes et plus tard, sur l'ancienne trajectoire.
        if let Ok(mut m) = A_JUGER.lock() {
            m.get_mut(&zone).unwrap().envoye_a = Instant::now() - Duration::from_secs(5);
        }
        let rattrapage = constater(zone, &statut(35_000)).expect("seek ignoré constaté");
        assert!((185_000..186_000).contains(&rattrapage), "{rattrapage}");
        assert!(profil::seek_inoperant(dev), "appris dans le profil");
        assert_eq!(
            constater(zone, &statut(36_000)),
            None,
            "jugé une seule fois"
        );
        assert!(
            profil::compatibilite_de(dev)
                .seek_inoperant_depuis
                .is_some(),
            "le diagnostic de la zone le montre"
        );
    }

    #[test]
    fn un_seek_execute_ne_profile_rien_6059() {
        let _ = profil::base_de_test();
        let dev = "dlna:uuid-renderer-sage-6059";
        let zone = 60_592;
        noter_seek_envoye(zone, dev, 30_000, 180_000);
        if let Ok(mut m) = A_JUGER.lock() {
            m.get_mut(&zone).unwrap().envoye_a = Instant::now() - Duration::from_secs(5);
        }
        assert_eq!(constater(zone, &statut(184_500)), None);
        assert!(!profil::seek_inoperant(dev));
    }

    #[test]
    fn le_profil_seek_inoperant_survit_au_redemarrage_et_au_reset_6059() {
        let _ = profil::base_de_test();
        let dev = "dlna:uuid-rn2000a-persistance-6059";
        profil::memoriser_seek_inoperant(dev);
        // L'écriture part sur un fil bloquant hors exécuteur : synchrone ici.
        profil::oublier_en_memoire_seulement(dev);
        assert!(
            profil::seek_inoperant(dev),
            "relu en base après redémarrage"
        );
        assert!(
            profil::valeur_en_base(dev).is_some_and(|v| v.contains("seek_inoperant")),
            "rangé en base"
        );
        assert_eq!(profil::reinitialiser_compatibilite(dev), Ok(1));
        assert!(!profil::seek_inoperant(dev));
    }
}
