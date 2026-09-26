//! #2742 — le crossfeed des pistes de la BIBLIOTHÈQUE sur une zone RÉSEAU.
//!
//! Jusqu'ici, sur une zone réseau dont l'opt-in `dsp_progressif_reseau` n'est
//! pas coché (Tades, 0.9.151, zone DLNA vers un renderer Diretta), seuls les
//! flux des services portaient le crossfeed. Une piste de la bibliothèque
//! partait :
//!
//! - **telle quelle** quand le crossfeed était le seul traitement de la zone :
//!   il est tenu hors de `eq_forces_transcode`, qui renvoie au ré-encodage du
//!   fichier ENTIER avant le premier octet (46 à 62 s mesurés, #3357) ;
//! - **ré-encodée en FLAC** par le fichier entier quand un égaliseur, un
//!   convolveur ou un ReplayGain l'imposait — mais `transcode_source_to_file`
//!   ne portait que ces trois étages : le crossfeed restait dehors.
//!
//! Décision de Bertrand (24/09) : « crossfeed toujours, SANS délai ». Trois
//! cas, et jamais une seconde ajoutée au premier son :
//!
//! 1. **un autre traitement ré-encode déjà la piste** : le crossfeed rejoint
//!    ce ré-encodage ([`crossfeed_cuit_dans_le_fichier`]). Même format, même
//!    chemin, même attente qu'avant — seulement un étage de plus ;
//! 2. **crossfeed seul, renderer qui a ANNONCÉ le LPCM** : la piste part en
//!    WAV progressif, le crossfeed appliqué au fil de l'eau par le relais de
//!    LAT-F1 ([`wav_progressif_consenti`]). ⚠️ Le format servi CHANGE sans
//!    l'opt-in : le crossfeed coché vaut consentement, pour ce seul cas ;
//! 3. **crossfeed seul, renderer sans LPCM** (sonde inconcluante comprise) :
//!    rien ne change, la piste part telle quelle. C'est le seul cas où le
//!    statut garde la réserve `network_progressive_off`.
//!
//! Le crossfeed ne s'applique qu'UNE fois : une sortie `local:` l'applique
//! déjà elle-même (le cumul EQ/ReplayGain de v0.9.139, voir
//! `relais_dsp_progressif`), donc aucun des deux cas n'est ouvert pour elle.
//! Les droits (Premium, greffon installé et activé) et le mode PURE passent
//! par `load_crossfeed_processor`, exactement comme sur le chemin des
//! services. « Bit-perfect strict » ne refuse qu'une conversion de fréquence
//! et ne coupe aucun traitement, sur aucun chemin : il ne gouverne donc pas
//! non plus celui-ci.

use super::PlaybackOrchestrator;
use super::regles::traitement_cuit_dans_le_fichier;

/// Cas 2 — l'opt-in du WAV progressif, tel que la décision doit le lire.
///
/// L'opt-in coché vaut pour tout traitement, comme avant. Sans lui, un
/// crossfeed actif sur une zone RÉSEAU vaut consentement au WAV progressif,
/// mais seulement quand il est le SEUL traitement : si un égaliseur, un
/// convolveur ou un ReplayGain ré-encode déjà la piste, c'est le cas 1, et le
/// format servi ne change pas. Le reste des conditions — renderer qui annonce
/// le LPCM, jamais un DSD — reste celui de `cible_wav_pour_traitement`.
pub(super) fn wav_progressif_consenti(
    opt_in: bool,
    crossfeed_reseau: bool,
    autre_traitement: bool,
) -> bool {
    opt_in || (crossfeed_reseau && !autre_traitement)
}

/// Cas 1 — le ré-encodage par le fichier doit-il cuire le crossfeed ?
///
/// Seulement vers une zone RÉSEAU, et jamais vers une sortie locale (qui
/// l'applique elle-même : [`traitement_cuit_dans_le_fichier`]). Le navigateur
/// et les sorties PULL passent aussi par ce bras ; le statut les déclare
/// `non_local_output`, sans mesure qui dise le contraire : ils n'en
/// reçoivent pas.
pub(super) fn crossfeed_cuit_dans_le_fichier(
    sortie_est_reseau: bool,
    sortie_est_locale: bool,
) -> bool {
    sortie_est_reseau && traitement_cuit_dans_le_fichier(sortie_est_locale)
}

/// Le crossfeed ENTRE dans la clé du cache de transcodage.
///
/// Il change les octets encodés, comme l'égaliseur (LAT-F2) : sans ce
/// brassage, une rendition sans crossfeed — celle du pré-chauffage de
/// `queue.rs`, ou d'avant ce correctif — serait servie à une zone qui l'a
/// coché, et inversement. `None` rend la clé d'avant, inchangée.
pub(super) fn empreinte_avec_crossfeed(
    dsp: Option<[u8; 32]>,
    crossfeed: Option<(f32, f32)>,
) -> Option<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let Some((amount, delay_ms)) = crossfeed else {
        return dsp;
    };
    let mut h = Sha256::new();
    h.update(b"crossfeed\0");
    h.update(amount.to_le_bytes());
    h.update(delay_ms.to_le_bytes());
    if let Some(d) = dsp {
        h.update(b"dsp\0");
        h.update(d);
    }
    Some(h.finalize().into())
}

/// #5081 — l'ombre de la tête du crossfeed ENTRE à son tour dans la clé du
/// cache de transcodage : deux réglages de filtre donnent deux renditions.
/// Brassée APRÈS [`empreinte_avec_crossfeed`], sans en changer la signature ;
/// `None` (filtre éteint, réglage d'avant #5081) rend la clé inchangée — les
/// renditions déjà en cache restent bonnes.
pub(super) fn empreinte_avec_ombre(
    dsp: Option<[u8; 32]>,
    ombre: Option<tune_plugin_crossfeed::OmbreDeTete>,
) -> Option<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let Some(ombre) = ombre else {
        return dsp;
    };
    let mut h = Sha256::new();
    h.update(b"crossfeed-ombre\0");
    h.update(ombre.cutoff_hz.to_le_bytes());
    h.update(ombre.slope_db_per_octave.to_le_bytes());
    if let Some(d) = dsp {
        h.update(b"dsp\0");
        h.update(d);
    }
    Some(h.finalize().into())
}

impl PlaybackOrchestrator {
    /// Le crossfeed à cuire dans le fichier ré-encodé, avec son réglage pour
    /// la clé du cache. `None` hors réseau, sur une sortie locale, en PURE,
    /// sans les droits du greffon, ou case décochée.
    pub(super) fn crossfeed_du_fichier(
        &self,
        zone_id: i64,
        sample_rate: u32,
        sortie_est_reseau: bool,
        sortie_est_locale: bool,
    ) -> Option<(crate::audio::crossfeed::CrossfeedProcessor, (f32, f32))> {
        if !crossfeed_cuit_dans_le_fichier(sortie_est_reseau, sortie_est_locale) {
            return None;
        }
        let processeur = self.load_crossfeed_processor(zone_id, sample_rate)?;
        let reglage = self.crossfeed_configure(zone_id)?;
        Some((processeur, reglage))
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod ombre_5081_tests {
    use super::*;

    fn ombre(
        cutoff_hz: f32,
        slope_db_per_octave: f32,
    ) -> Option<tune_plugin_crossfeed::OmbreDeTete> {
        Some(tune_plugin_crossfeed::OmbreDeTete {
            cutoff_hz,
            slope_db_per_octave,
        })
    }

    /// Un réglage de filtre change la clé du cache ; filtre éteint, la clé
    /// d'avant #5081 est rendue telle quelle.
    #[test]
    fn un_reglage_de_filtre_change_la_cle_du_cache_5081() {
        let base = empreinte_avec_crossfeed(None, Some((0.3, 0.3)));
        assert_eq!(empreinte_avec_ombre(base, None), base);
        let a = empreinte_avec_ombre(base, ombre(700.0, 6.0));
        let b = empreinte_avec_ombre(base, ombre(1200.0, 6.0));
        let c = empreinte_avec_ombre(base, ombre(700.0, 3.0));
        assert_ne!(a, base, "allumer le filtre ne change pas la clé");
        assert_ne!(a, b, "la coupure n'entre pas dans la clé");
        assert_ne!(a, c, "la pente n'entre pas dans la clé");
        assert_ne!(
            empreinte_avec_ombre(None, ombre(700.0, 6.0)),
            a,
            "le reste de la clé est perdu"
        );
    }

    /// L'orchestrateur charge le filtre enregistré sur la zone, et
    /// l'empreinte du traitement (relance d'un flux) le distingue. Un réglage
    /// d'avant #5081 se charge filtre éteint.
    #[test]
    fn l_orchestrateur_charge_l_ombre_de_la_zone_5081() {
        use std::sync::Arc;
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let orch = PlaybackOrchestrator::new(
            db.clone(),
            Arc::new(crate::playback::PlaybackManager::new()),
            Arc::new(crate::http::streamer::AudioStreamer::new(0)),
            Arc::new(tokio::sync::Mutex::new(
                crate::streaming::registry::ServiceRegistry::new(),
            )),
            Arc::new(tokio::sync::Mutex::new(
                crate::outputs::registry::OutputRegistry::new(),
            )),
            None,
        );
        let settings = crate::db::settings_repo::SettingsRepo::with_backend(db);
        settings
            .set(crate::audio::premium_plugins::MIGRATION, "complete")
            .unwrap();
        settings.set("plugin_crossfeed_installed", "true").unwrap();
        settings.set("plugin_crossfeed_enabled", "true").unwrap();

        settings
            .set(
                "zone_1_crossfeed",
                r#"{"enabled":true,"amount":0.3,"delay_ms":0.3}"#,
            )
            .unwrap();
        let p = orch.load_crossfeed_processor(1, 48_000).expect("crossfeed");
        assert_eq!(p.ombre(), None, "un réglage d'avant #5081 allume le filtre");
        let sans = orch.empreinte_du_traitement(1, None);

        let reglage = |fc: f32| {
            format!(
                r#"{{"enabled":true,"amount":0.3,"delay_ms":0.3,"head_shadow_enabled":true,"cutoff_hz":{fc},"slope_db_per_octave":4.5}}"#
            )
        };
        settings.set("zone_1_crossfeed", &reglage(1200.0)).unwrap();
        let p = orch.load_crossfeed_processor(1, 48_000).expect("crossfeed");
        assert_eq!(
            p.ombre(),
            ombre(1200.0, 4.5),
            "le filtre enregistré n'est pas chargé"
        );
        let a = orch.empreinte_du_traitement(1, None);
        settings.set("zone_1_crossfeed", &reglage(700.0)).unwrap();
        let b = orch.empreinte_du_traitement(1, None);
        assert!(
            sans != a && a != b,
            "l'empreinte ignore le filtre : {sans} / {a} / {b}"
        );
    }

    /// La clé est brassée par `resolve_local`, sur le processeur réellement
    /// chargé.
    #[test]
    fn resolve_local_brasse_l_ombre_du_processeur_charge_5081() {
        const RESOLVE_LOCAL: &str = include_str!("resolve_local.rs");
        assert!(
            RESOLVE_LOCAL.contains(
                "super::crossfeed_bibliotheque_reseau::empreinte_avec_ombre(\n                empreinte_dsp,\n                crossfeed.as_ref().and_then(|(p, _)| p.ombre()),"
            ),
            "le filtre d'ombre n'entre plus dans la clé du cache de transcodage"
        );
    }
}
