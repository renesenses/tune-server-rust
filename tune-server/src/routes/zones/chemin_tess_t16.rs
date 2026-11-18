//! TESS T16 — le bloc `tess` du chemin du signal (maquette A4).
//!
//! Le chemin du signal historique (`signal_path.rs`) décrit ce qui joue, à
//! partir de la sonde de la sortie et des octets du flux. Ce module y ajoute
//! la vue TESS : la chaîne de la zone (T02, `lire_zone`) résolue
//! (`ChaineTess::resoudre`), puis **rapprochée de ce qui tourne**
//! (`tess::chemin::chemin_du_signal`). Ce qui tourne est lu dans le chemin
//! historique déjà construit, jamais dans les réglages :
//!
//! - **greffons** : la sonde de la sortie ne dit que « un traitement a touché
//!   les échantillons » (étape DSP, crossfeed du flux, repli mono,
//!   réaffectation) ; une RI que le convolueur a retirée est connue éteinte.
//!   Tant que l'exécuteur TESS (T03, T04) n'est pas branché, c'est une
//!   [`GreffonsEnService::Sonde`] ; il publiera la liste exacte ;
//! - **headroom** : l'étage de T07 n'est pas encore branché dans la lecture,
//!   donc rien n'est en service (le headroom automatique s'affiche « not in
//!   service » plutôt qu'appliqué) ;
//! - **upsampling** : `upsampling.active` du chemin historique ;
//! - **volume** : la sonde locale (`OutputVolumeState::Applied`), sinon un
//!   gain logiciel de zone différent de l'unité, sortie locale seulement.
//!
//! Clé `tess` ABSENTE quand la chaîne de la zone ne se lit pas (clé
//! corrompue, refusée explicitement par T02) : rien plutôt qu'une chaîne
//! inventée. Une zone qui joue hors TESS (HQPlayer, Diretta) a un bloc avec
//! le badge `outside_tess`.

use std::sync::Arc;

use serde_json::Value;
use tracing::{debug, warn};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::Zone;
use tune_core::outputs::traits::OutputVolumeState;
use tune_core::playback::ZoneState;
use tune_core::tess::chemin::{EnService, GreffonsEnService, chemin_du_signal};
use tune_core::tess::greffon::Origine;
use tune_core::tess::migration::{lecture_dsd, lire_zone};
use tune_core::tess::{
    ContexteExecution, ControleVolume, LectureDsd, SourceFlux, TauxDsd, TransportZone, VolumeZone,
    catalogue_de_reference,
};

/// Le plancher publié pour un gain nul (le JSON ne porte pas −∞).
const PLANCHER_DB: f64 = -144.0;

fn etapes(sp: &Value) -> &[Value] {
    sp.get("steps")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn a_le_code(e: &Value, code: &str) -> bool {
    e.get("code").and_then(Value::as_str) == Some(code)
}

fn a_le_nom(e: &Value, nom: &str) -> bool {
    e.get("name").and_then(Value::as_str) == Some(nom)
}

fn altere(e: &Value) -> bool {
    e.get("bit_perfect").and_then(Value::as_bool) == Some(false)
}

/// Ce que le chemin historique dit avoir réellement en service.
pub(crate) fn en_service_d_apres_le_chemin(sp: &Value, volume_logiciel: bool) -> EnService {
    let steps = etapes(sp);
    let correction_appliquee = steps
        .iter()
        .any(|e| a_le_code(e, "room_correction") && e.get("applied") == Some(&Value::Bool(true)));
    let actif = correction_appliquee
        || steps.iter().any(|e| {
            (a_le_nom(e, "DSP") || a_le_nom(e, "Mono") || a_le_code(e, "crossfeed")) && altere(e)
                || a_le_code(e, "channel_remap")
        });
    let mut hors_service = Vec::new();
    if sp
        .get("room_correction")
        .and_then(|r| r.get("applied"))
        .and_then(Value::as_bool)
        == Some(false)
    {
        hors_service.extend(["convolution".to_owned(), "room-correction".to_owned()]);
    }
    let upsampling = sp
        .get("upsampling")
        .and_then(|u| u.get("active"))
        .and_then(Value::as_bool)
        == Some(true);
    EnService {
        greffons: GreffonsEnService::Sonde {
            actif,
            hors_service,
        },
        headroom_db: None,
        upsampling,
        volume_logiciel,
        latences: Vec::new(),
    }
}

fn taux_dsd(sample_rate: u32) -> TauxDsd {
    match sample_rate {
        r if r >= 22_579_200 => TauxDsd::Unknown,
        r if r >= 11_289_600 => TauxDsd::Dsd256,
        r if r >= 5_644_800 => TauxDsd::Dsd128,
        _ => TauxDsd::Dsd64,
    }
}

/// Le flux qui entre dans la chaîne, et ce que la zone fait d'un DSD (lu
/// d'abord dans le chemin servi, puis dans `dsd_mode`).
fn source_flux(ps: &ZoneState, dsd_mode: &str, sp: &Value) -> Option<SourceFlux> {
    let np = ps.now_playing.as_ref()?;
    let rate = np.sample_rate.filter(|r| *r > 0)?;
    let bits = np.bit_depth.unwrap_or(0);
    let channels = np.channels.unwrap_or(2);
    if super::signal_path::is_dsd_resolution(rate as i32, bits as i32) {
        let steps = etapes(sp);
        let played_as = if steps.iter().any(|e| a_le_code(e, "dop")) {
            LectureDsd::Dop
        } else if steps.iter().any(|e| a_le_code(e, "dsd_natif")) {
            LectureDsd::Native
        } else if steps
            .iter()
            .any(|e| a_le_code(e, "dsd_pcm_repli") || a_le_code(e, "oaat_dsd_pcm_fallback"))
        {
            LectureDsd::ConvertedToPcm
        } else {
            lecture_dsd(dsd_mode).unwrap_or(LectureDsd::Unknown)
        };
        return Some(SourceFlux::Dsd {
            rate: taux_dsd(rate),
            channels,
            played_as,
        });
    }
    Some(SourceFlux::Pcm {
        sample_rate_hz: rate,
        bits: u8::try_from(bits).unwrap_or(0),
        channels,
    })
}

/// Le volume de la zone, et s'il multiplie réellement les échantillons.
fn volume_de_la_zone(
    backend: &Arc<dyn DbBackend>,
    zone: &Zone,
    ps: &ZoneState,
) -> (VolumeZone, bool) {
    let zid = zone.id.unwrap_or(0);
    if zone.fixed_volume {
        return (
            VolumeZone {
                control: ControleVolume::Fixed,
                db: 0.0,
            },
            false,
        );
    }
    if zone.output_type.as_deref().unwrap_or("local") != "local" {
        return (
            VolumeZone {
                control: ControleVolume::Device,
                db: 0.0,
            },
            false,
        );
    }
    // Pure + verrou : tenu à 100 % (`effective_volume`), donc 0 dB.
    let gain = tune_core::audio::audiophile::effective_volume(backend, zid, zone.volume / 100.0);
    let db = if gain <= 0.0 {
        PLANCHER_DB
    } else {
        (20.0 * gain.log10()).max(PLANCHER_DB)
    };
    let db = if gain >= 1.0 { 0.0 } else { db };
    // La sonde de la sortie locale tranche quand elle existe.
    let applique = match ps.output_signal_path.as_ref() {
        Some(s) => s.volume == OutputVolumeState::Applied && gain < 1.0,
        None => gain > 0.0 && gain < 1.0,
    };
    (
        VolumeZone {
            control: ControleVolume::Software,
            db,
        },
        applique,
    )
}

/// Le bloc `tess` d'une zone qui joue, ou `None` (voir l'en-tête).
pub(crate) fn chemin_tess_de_la_zone(
    backend: &Arc<dyn DbBackend>,
    zone: &Zone,
    ps: &ZoneState,
    sp: &Value,
    premium: bool,
) -> Option<Value> {
    let zid = zone.id?;
    let settings = SettingsRepo::with_backend(backend.clone());
    let lecture = match lire_zone(&settings, zid, premium) {
        Ok(l) => l,
        Err(e) => {
            warn!(zone_id = zid, error = %e, "tess_chemin_chaine_illisible");
            return None;
        }
    };
    let catalogue = catalogue_de_reference();
    let disponible = |id: &str| match catalogue.get(id).map(|d| d.origin) {
        Some(Origine::Package) => tune_core::audio::premium_plugins::enabled(&settings, id),
        Some(_) => true,
        None => false,
    };
    let (volume, volume_logiciel) = volume_de_la_zone(backend, zone, ps);
    let ctx = ContexteExecution {
        transport: TransportZone::depuis_type_de_sortie(zone.output_type.as_deref()),
        source: source_flux(ps, &lecture.dsd_mode, sp),
        premium,
        greffon_disponible: &disponible,
        pure: lecture.pure,
        volume,
    };
    let resolution = lecture.chaine.resoudre(&catalogue, &ctx);
    let en_service = en_service_d_apres_le_chemin(sp, volume_logiciel);
    let intact = sp.get("bit_perfect").and_then(Value::as_bool);
    let chemin = chemin_du_signal(
        &lecture.chaine,
        &catalogue,
        resolution,
        &en_service,
        intact,
        None,
    );
    if !chemin.discrepancies.is_empty() {
        debug!(zone_id = zid, ecarts = ?chemin.discrepancies, "tess_chemin_ecarts");
    }
    serde_json::to_value(chemin).ok()
}

/// Ajoute `tess` au chemin du signal déjà construit (licence de l'hôte).
pub async fn avec_le_chemin_tess(
    license: &tune_core::license::LicenseManager,
    backend: &Arc<dyn DbBackend>,
    zone: &Zone,
    ps: &ZoneState,
    signal_path: Option<Value>,
) -> Option<Value> {
    let mut sp = signal_path?;
    let premium = license.is_premium().await;
    if let Some(t) = chemin_tess_de_la_zone(backend, zone, ps, &sp, premium)
        && let Some(o) = sp.as_object_mut()
    {
        o.insert("tess".into(), t);
    }
    Some(sp)
}
