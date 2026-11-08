//! #7922 — proposer un appareil pour Tune tested depuis Tune.
//!
//! Chaque zone tient un compte de lecture **sans erreur** : le temps des
//! pistes réellement rendues jusqu'à leur fin (fin observée par le sondeur ou
//! enchaînement gapless). Une erreur de transport ou une coupure de flux le
//! remet à zéro, comme un changement d'appareil. Au-delà d'une heure, la zone
//! peut proposer son appareil au site.
//!
//! Le compte est local et indépendant du consentement à la télémétrie : rien
//! ne part tant que l'utilisateur n'a pas confirmé l'envoi. Ce qui part est
//! construit ici, une seule fois, pour l'aperçu comme pour l'envoi : fabricant,
//! modèle, nom commercial du catalogue, type de sortie, formats joués, temps
//! sans erreur, version et plateforme. Jamais d'adresse, de nom de réseau, de
//! chemin, de nom de zone ni d'identifiant brut d'appareil.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::tune_tested_auto::{FormatObserve, descriptif, format_valide, protocole};
use crate::db::settings_repo::SettingsRepo;

/// Une heure de lecture sans erreur.
pub const SEUIL_MS: u64 = 3_600_000;
/// Version du contenu envoyé au site.
pub const SCHEMA_VERSION: u8 = 1;
/// Une piste ne compte jamais plus que ceci, contre une durée aberrante.
const MAX_PAR_PISTE_MS: u64 = 6 * 3_600_000;
const MAX_FORMATS: usize = 32;
const CLE_ENVOYEES: &str = "tune_tested_propositions_envoyees_v1";
const MAX_ENVOYEES: usize = 200;

fn cle_compteur(zone_id: i64) -> String {
    format!("zone_{zone_id}_tune_tested_compteur_v1")
}

/// Empreinte locale de l'identifiant d'appareil : elle sert à voir que
/// l'appareil a changé, sans garder ni envoyer l'identifiant lui-même.
fn empreinte_appareil(device_id: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"Tune tested device v1\0");
    hash.update(device_id.as_bytes());
    format!("{:x}", hash.finalize())
}

/// Le compte d'une zone, rangé dans les réglages.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompteurZone {
    pub appareil: String,
    pub millisecondes_sans_erreur: u64,
    #[serde(default)]
    pub protocole: Option<String>,
    #[serde(default)]
    pub fabricant: Option<String>,
    #[serde(default)]
    pub modele: Option<String>,
    #[serde(default)]
    pub formats: Vec<FormatObserve>,
    #[serde(default)]
    pub gapless_constate: bool,
    /// La dernière piste comptée (génération, niveau gapless) : une même fin
    /// vue par deux chemins du sondeur ne compte qu'une fois.
    #[serde(default)]
    pub derniere_piste: Option<(u64, u64)>,
}

impl CompteurZone {
    fn neuf(appareil: String) -> Self {
        Self {
            appareil,
            ..Self::default()
        }
    }

    fn remettre_a_zero(&mut self) {
        *self = Self {
            appareil: std::mem::take(&mut self.appareil),
            protocole: self.protocole.take(),
            fabricant: self.fabricant.take(),
            modele: self.modele.take(),
            ..Self::default()
        };
    }
}

/// Une piste rendue jusqu'au bout sur la zone.
pub struct LectureRendue<'a> {
    pub device_id: &'a str,
    pub protocole_brut: &'a str,
    pub identite: Option<(&'a str, &'a str)>,
    pub millisecondes: u64,
    pub format: Option<FormatObserve>,
    pub gapless: bool,
    pub cle_piste: (u64, u64),
}

fn lire(settings: &SettingsRepo, zone_id: i64) -> Option<CompteurZone> {
    serde_json::from_str(&settings.get(&cle_compteur(zone_id)).ok()??).ok()
}

fn ecrire(settings: &SettingsRepo, zone_id: i64, compteur: &CompteurZone) -> Result<(), String> {
    settings
        .set(
            &cle_compteur(zone_id),
            &serde_json::to_string(compteur).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())
}

/// Le compte de la zone pour cet appareil ; un compte neuf si l'appareil a
/// changé depuis.
fn compteur_pour(settings: &SettingsRepo, zone_id: i64, device_id: &str) -> CompteurZone {
    let appareil = empreinte_appareil(device_id);
    match lire(settings, zone_id) {
        Some(c) if c.appareil == appareil => c,
        _ => CompteurZone::neuf(appareil),
    }
}

/// Ajoute une piste rendue sans erreur au compte de la zone.
pub fn noter_lecture(
    settings: &SettingsRepo,
    zone_id: i64,
    lecture: LectureRendue<'_>,
) -> Result<CompteurZone, String> {
    let mut compteur = compteur_pour(settings, zone_id, lecture.device_id);
    if compteur.derniere_piste == Some(lecture.cle_piste) {
        return Ok(compteur);
    }
    compteur.derniere_piste = Some(lecture.cle_piste);
    compteur.millisecondes_sans_erreur = compteur
        .millisecondes_sans_erreur
        .saturating_add(lecture.millisecondes.min(MAX_PAR_PISTE_MS));
    if let Some(p) = protocole(lecture.protocole_brut) {
        compteur.protocole = Some(p.to_owned());
    }
    // L'identité détectée, sinon la marque et le modèle choisis sur la zone
    // (sortie locale) ; jamais le nom de la zone ni le nom convivial.
    let choisie = |champ: &str| {
        settings
            .get(&format!("zone_{zone_id}_{champ}"))
            .ok()
            .flatten()
    };
    let identite = match lecture.identite {
        Some((marque, modele)) => Some((marque.to_owned(), modele.to_owned())),
        None => choisie("brand").zip(choisie("model")),
    };
    if let Some((marque, modele)) = identite
        && let (Some(marque), Some(modele)) = (descriptif(&marque), descriptif(&modele))
        && marque != crate::device_catalog::CUSTOM_BRAND
    {
        compteur.fabricant = Some(marque);
        compteur.modele = Some(modele);
    }
    if let Some(format) = lecture.format.filter(format_valide)
        && !compteur.formats.contains(&format)
        && compteur.formats.len() < MAX_FORMATS
    {
        compteur.formats.push(format);
    }
    compteur.gapless_constate |= lecture.gapless;
    ecrire(settings, zone_id, &compteur)?;
    Ok(compteur)
}

/// Erreur de transport ou coupure de flux : le compte repart de zéro.
pub fn noter_erreur(settings: &SettingsRepo, zone_id: i64, device_id: &str) -> Result<(), String> {
    let mut compteur = compteur_pour(settings, zone_id, device_id);
    compteur.remettre_a_zero();
    ecrire(settings, zone_id, &compteur)
}

/// Le format réellement joué, tel que le site le reçoit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FormatJoue {
    pub codec: String,
    /// Toujours renseignée : le site exige la fréquence de chaque format
    /// (`formats.*.sample_rate`, de 8 kHz à 24,576 MHz).
    pub sample_rate: Option<u32>,
    /// Absente au-delà de 32 bits, que le site refuse.
    pub bit_depth: Option<u16>,
    /// `native`, `dop`, ou absent quand le mode n'a pas été mesuré.
    pub dsd_mode: Option<String>,
}

/// Les bornes du schéma du site (`StoreTuneTestedPropositionRequest`).
const FREQUENCE_MIN_SITE: u32 = 8_000;
const FREQUENCE_MAX_SITE: u32 = 24_576_000;
const PROFONDEUR_MAX_SITE: u16 = 32;

/// Exactement ce qui part vers le site : l'aperçu montré à l'utilisateur et
/// le corps de l'envoi sont cette même valeur.
///
/// #8084 — les noms des champs sont ceux que la route du site lit
/// (`POST /api/v1/tune-tested/propositions`, renesenses/site-mozaiklabs
/// `StoreTuneTestedPropositionRequest`) : `brand`, `gapless`, `bit_depth`,
/// `dsd`. Avant, `manufacturer` et `gapless_observed` partaient vers une
/// adresse que le site ne sert pas (#8033).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Proposition {
    pub schema_version: u8,
    /// Clé stable de l'appareil (fabricant, modèle, type de sortie), gardée
    /// localement pour `already_proposed`. Le site, lui, garde une
    /// proposition par fiche du catalogue et par identifiant d'instance.
    pub device_key: String,
    #[serde(rename = "brand")]
    pub manufacturer: String,
    pub model: String,
    pub commercial_name: Option<String>,
    /// Le type de sortie, dans le vocabulaire du site (`airplay` pour
    /// AirPlay 1).
    pub output_type: String,
    pub formats: Vec<FormatJoue>,
    /// La plus grande profondeur PCM jouée, si elle est connue.
    pub bit_depth: Option<u16>,
    /// `native` si du DSD natif a été joué, sinon `dop`, sinon absent.
    pub dsd: Option<String>,
    #[serde(rename = "gapless")]
    pub gapless_observed: bool,
    pub error_free_seconds: u64,
    pub tune_version: String,
    pub platform: String,
}

/// Ce que la zone peut proposer, pour `GET /zones/{id}/tune-tested`.
#[derive(Clone, Debug, Serialize)]
pub struct EtatProposition {
    pub eligible: bool,
    pub seconds_without_error: u64,
    pub threshold_seconds: u64,
    pub remaining_seconds: u64,
    pub already_proposed: bool,
    /// #8084 — l'appareil figure déjà au catalogue Tune tested téléchargé :
    /// la proposition reste permise, elle ajoute le foyer de l'utilisateur
    /// (et sa configuration, si elle diffère, est jointe pour examen).
    pub already_tune_tested: bool,
    /// Pourquoi la proposition n'est pas possible : `below_threshold`,
    /// `no_device` ou `device_unidentified`.
    pub reason: Option<&'static str>,
    pub preview: Option<Proposition>,
}

pub fn cle_appareil(fabricant: &str, modele: &str, protocole: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"Tune tested proposition v1\0");
    hash.update(fabricant.to_lowercase().as_bytes());
    hash.update(b"\0");
    hash.update(modele.to_lowercase().as_bytes());
    hash.update(b"\0");
    hash.update(protocole.as_bytes());
    format!("{:x}", hash.finalize())
}

fn nom_commercial(fabricant: &str, modele: &str) -> Option<String> {
    let marque = crate::device_catalog::find_brand(fabricant)?;
    let modele = crate::device_catalog::find_model(fabricant, modele)?;
    descriptif(&format!("{} {}", marque.name, modele.name))
}

/// Le type de sortie tel que le site le nomme : il dit `airplay` là où Tune
/// distingue `airplay1`.
fn sortie_du_site(protocole: &str) -> &str {
    match protocole {
        "airplay1" => "airplay",
        autre => autre,
    }
}

fn proposition(compteur: &CompteurZone) -> Option<Proposition> {
    let fabricant = compteur.fabricant.as_deref()?;
    let modele = compteur.modele.as_deref()?;
    let sortie = compteur.protocole.as_deref()?;
    let formats: Vec<FormatJoue> = compteur
        .formats
        .iter()
        // Le site exige une fréquence dans ses bornes : un format sans
        // fréquence connue ne part pas, plutôt que de faire refuser le tout.
        .filter(|f| {
            f.sample_rate
                .is_some_and(|n| (FREQUENCE_MIN_SITE..=FREQUENCE_MAX_SITE).contains(&n))
        })
        .map(|f| FormatJoue {
            codec: f.codec.clone(),
            sample_rate: f.sample_rate,
            bit_depth: f.bit_depth.filter(|b| *b <= PROFONDEUR_MAX_SITE),
            dsd_mode: f.dsd_mode.clone(),
        })
        .collect();
    let bit_depth = formats
        .iter()
        .filter(|f| f.codec != "dsd")
        .filter_map(|f| f.bit_depth)
        .max();
    let dsd = if formats
        .iter()
        .any(|f| f.dsd_mode.as_deref() == Some("native"))
    {
        Some("native".to_owned())
    } else if formats.iter().any(|f| f.dsd_mode.as_deref() == Some("dop")) {
        Some("dop".to_owned())
    } else {
        None
    };
    Some(Proposition {
        schema_version: SCHEMA_VERSION,
        device_key: cle_appareil(fabricant, modele, sortie),
        manufacturer: fabricant.to_owned(),
        model: modele.to_owned(),
        commercial_name: nom_commercial(fabricant, modele),
        output_type: sortie_du_site(sortie).to_owned(),
        formats,
        bit_depth,
        dsd,
        gapless_observed: compteur.gapless_constate,
        error_free_seconds: compteur.millisecondes_sans_erreur / 1000,
        tune_version: crate::version().to_owned(),
        platform: std::env::consts::OS.to_owned(),
    })
}

/// #8084 — l'appareil est-il déjà au catalogue Tune tested téléchargé ?
/// Par marque et modèle seulement : « déjà Tune tested » vaut pour
/// l'appareil, quelle que soit la sortie par laquelle on l'écoute.
fn deja_au_catalogue(settings: &SettingsRepo, apercu: &Proposition) -> bool {
    super::tune_tested::catalogue_range(settings).is_some_and(|c| {
        c.devices
            .iter()
            .any(|d| d.correspond(&apercu.manufacturer, &apercu.model))
    })
}

fn envoyees(settings: &SettingsRepo) -> Vec<String> {
    settings
        .get(CLE_ENVOYEES)
        .ok()
        .flatten()
        .and_then(|brut| serde_json::from_str(&brut).ok())
        .unwrap_or_default()
}

/// L'état de la zone. `device_id` est l'appareil actuel de la zone : un
/// compte tenu pour un autre appareil vaut zéro.
pub fn etat(settings: &SettingsRepo, zone_id: i64, device_id: Option<&str>) -> EtatProposition {
    let compteur = device_id.map(|d| compteur_pour(settings, zone_id, d));
    let ms = compteur.as_ref().map_or(0, |c| c.millisecondes_sans_erreur);
    let apercu = compteur.as_ref().and_then(proposition);
    let reason = if device_id.is_none() {
        Some("no_device")
    } else if ms < SEUIL_MS {
        Some("below_threshold")
    } else if apercu.is_none() {
        Some("device_unidentified")
    } else {
        None
    };
    let already_proposed = apercu
        .as_ref()
        .is_some_and(|p| envoyees(settings).contains(&p.device_key));
    // Jamais un motif de refus : un appareil déjà validé se propose encore,
    // pour ajouter un foyer (#8084).
    let already_tune_tested = apercu
        .as_ref()
        .is_some_and(|p| deja_au_catalogue(settings, p));
    EtatProposition {
        eligible: reason.is_none(),
        seconds_without_error: ms / 1000,
        threshold_seconds: SEUIL_MS / 1000,
        remaining_seconds: SEUIL_MS.saturating_sub(ms).div_ceil(1000),
        already_proposed,
        already_tune_tested,
        reason,
        preview: apercu,
    }
}

/// Retient qu'une proposition est partie pour cet appareil.
pub fn marquer_proposee(settings: &SettingsRepo, device_key: &str) -> Result<(), String> {
    let mut liste = envoyees(settings);
    if !liste.iter().any(|k| k == device_key) {
        liste.push(device_key.to_owned());
        if liste.len() > MAX_ENVOYEES {
            liste.remove(0);
        }
    }
    settings
        .set(
            CLE_ENVOYEES,
            &serde_json::to_string(&liste).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())
}

#[cfg(test)]
#[path = "tune_tested_proposition_7922_tests.rs"]
mod tests;
