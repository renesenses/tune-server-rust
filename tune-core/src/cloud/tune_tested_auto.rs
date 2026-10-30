//! Observations consenties, agrégées localement, sans identité d'appareil exportée.

use std::collections::{HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::db::settings_repo::SettingsRepo;

const CLE: &str = "telemetry_tune_tested_v1";
const MAX_MODELES: usize = 200;
const MAX_TENTATIVES: usize = 8192;
const MAX_COMPTEUR: u32 = i32::MAX as u32;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatObserve {
    pub codec: String,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u16>,
    pub dsd_mode: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModeleObserve {
    pub manufacturer: String,
    pub model: String,
    pub protocol: String,
    pub successful_plays: u32,
    pub failed_plays: u32,
    pub best_format: Option<FormatObserve>,
}

#[derive(Debug, Serialize)]
pub struct Contribution {
    pub schema_version: u8,
    pub devices: Vec<ModeleObserve>,
}

pub fn pseudonyme_serveur(identite: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"Tune tested telemetry v1\0");
    hash.update(identite.as_bytes());
    format!("{:x}", hash.finalize())
}

fn descriptif(valeur: &str) -> Option<String> {
    static IPV4: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?:[0-9]{1,3}\.){3}[0-9]{1,3}").expect("expression IPv4 constante")
    });
    let valeur = valeur.trim();
    if valeur.is_empty()
        || valeur.chars().count() > 120
        || !valeur.chars().next()?.is_alphanumeric()
        || !valeur
            .chars()
            .all(|c| c.is_alphanumeric() || " .&()+,_-".contains(c))
        || valeur.parse::<std::net::IpAddr>().is_ok()
        || IPV4
            .find_iter(valeur)
            .any(|partie| partie.as_str().parse::<std::net::Ipv4Addr>().is_ok())
        || uuid::Uuid::parse_str(valeur).is_ok()
        || valeur.as_bytes().windows(36).any(|partie| {
            std::str::from_utf8(partie).is_ok_and(|s| uuid::Uuid::parse_str(s).is_ok())
        })
    {
        return None;
    }
    if valeur.as_bytes().windows(17).any(|partie| {
        partie.iter().enumerate().all(|(i, c)| {
            if i % 3 == 2 {
                *c == b'-' || *c == b':'
            } else {
                c.is_ascii_hexdigit()
            }
        })
    }) {
        return None;
    }
    Some(valeur.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn protocole(brut: &str) -> Option<&'static str> {
    Some(match brut {
        "local" | "alsa" | "asio" | "wasapi" | "coreaudio" => "local",
        "airplay" | "airplay1" => "airplay1",
        "airplay2" => "airplay2",
        "dlna" => "dlna",
        "openhome" => "openhome",
        "chromecast" => "chromecast",
        "sonos" => "sonos",
        "bluos" => "bluos",
        "devialet" | "devialet_air" => "devialet",
        "hqplayer" => "hqplayer",
        "squeezebox" => "squeezebox",
        "slimproto" => "slimproto",
        "sendspin" => "sendspin",
        "oaat" => "oaat",
        "oaat-multiroom" => "oaat-multiroom",
        _ => return None,
    })
}

fn rang(format: &FormatObserve) -> (bool, u32, u16, &str, &str) {
    (
        format.codec == "dsd" || format.dsd_mode.is_some(),
        format.sample_rate.unwrap_or(0),
        format.bit_depth.unwrap_or(0),
        &format.codec,
        format.dsd_mode.as_deref().unwrap_or(""),
    )
}

pub struct AppareilObserve<'a> {
    pub protocole: &'a str,
    pub identite_detectee: Option<(&'a str, &'a str)>,
}

/// Le cache des tentatives ne vit qu'en mémoire, jamais dans la contribution.
#[derive(Default)]
pub struct Observateur {
    terminees: HashSet<(i64, u64, u64)>,
    ordre: VecDeque<(i64, u64, u64)>,
}

impl Observateur {
    pub fn terminer(
        &mut self,
        settings: &SettingsRepo,
        cle_tentative: (i64, u64, u64),
        protocole_brut: &str,
        succes: bool,
        format: Option<FormatObserve>,
    ) -> Result<bool, String> {
        self.terminer_avec_identite(
            settings,
            cle_tentative,
            AppareilObserve {
                protocole: protocole_brut,
                identite_detectee: None,
            },
            succes,
            format,
        )
    }

    pub fn terminer_avec_identite(
        &mut self,
        settings: &SettingsRepo,
        cle_tentative: (i64, u64, u64),
        appareil: AppareilObserve<'_>,
        succes: bool,
        format: Option<FormatObserve>,
    ) -> Result<bool, String> {
        // Avant toute lecture d'identité, d'agrégat ou écriture de télémétrie.
        if !super::telemetry::TelemetryReporter::is_enabled_for(settings) {
            return Ok(false);
        }
        let (zone, _, _) = cle_tentative;
        if self.terminees.contains(&cle_tentative) {
            return Ok(false);
        }
        let Some(protocol) = protocole(appareil.protocole) else {
            return Ok(false);
        };
        // Ces champs techniques sont alimentés par la découverte ; aucun repli
        // sur le nom de zone, nom convivial ou identifiant du périphérique.
        let (manufacturer, model) = match appareil.identite_detectee {
            Some((marque, modele)) => (descriptif(marque), descriptif(modele)),
            None => (
                settings
                    .get(&format!("zone_{zone}_brand"))
                    .map_err(|e| e.to_string())?
                    .and_then(|v| descriptif(&v)),
                settings
                    .get(&format!("zone_{zone}_model"))
                    .map_err(|e| e.to_string())?
                    .and_then(|v| descriptif(&v)),
            ),
        };
        let (Some(manufacturer), Some(model)) = (manufacturer, model) else {
            return Ok(false);
        };
        if manufacturer == crate::device_catalog::CUSTOM_BRAND {
            return Ok(false);
        }
        let mut devices: Vec<ModeleObserve> = match settings.get(CLE).map_err(|e| e.to_string())? {
            Some(json) => serde_json::from_str(&json).map_err(|e| e.to_string())?,
            None => Vec::new(),
        };
        let index = devices.iter().position(|d| {
            d.manufacturer.eq_ignore_ascii_case(&manufacturer)
                && d.model.eq_ignore_ascii_case(&model)
                && d.protocol == protocol
        });
        let index = match index {
            Some(index) => index,
            None if devices.len() < MAX_MODELES => {
                devices.push(ModeleObserve {
                    manufacturer,
                    model,
                    protocol: protocol.into(),
                    successful_plays: 0,
                    failed_plays: 0,
                    best_format: None,
                });
                devices.len() - 1
            }
            None => return Ok(false),
        };
        let device = &mut devices[index];
        if succes {
            device.successful_plays = device.successful_plays.saturating_add(1).min(MAX_COMPTEUR);
            if let Some(format) = format.filter(format_valide)
                && device
                    .best_format
                    .as_ref()
                    .is_none_or(|ancien| rang(&format) > rang(ancien))
            {
                device.best_format = Some(format);
            }
        } else {
            device.failed_plays = device.failed_plays.saturating_add(1).min(MAX_COMPTEUR);
        }
        settings
            .set(
                CLE,
                &serde_json::to_string(&devices).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
        self.terminees.insert(cle_tentative);
        self.ordre.push_back(cle_tentative);
        if self.ordre.len() > MAX_TENTATIVES
            && let Some(ancienne) = self.ordre.pop_front()
        {
            self.terminees.remove(&ancienne);
        }
        Ok(true)
    }
}

fn format_valide(format: &FormatObserve) -> bool {
    [
        "pcm", "wav", "aiff", "flac", "alac", "mp3", "aac", "ogg", "opus", "dsd",
    ]
    .contains(&format.codec.as_str())
        && format
            .sample_rate
            .is_none_or(|n| (8000..=49152000).contains(&n))
        && format.bit_depth.is_none_or(|n| (1..=64).contains(&n))
        && format
            .dsd_mode
            .as_deref()
            .is_none_or(|s| ["native", "dop"].contains(&s))
}

pub fn contribution(settings: &SettingsRepo) -> Option<Contribution> {
    if !super::telemetry::TelemetryReporter::is_enabled_for(settings) {
        return None;
    }
    let devices: Vec<ModeleObserve> = serde_json::from_str(&settings.get(CLE).ok()??).ok()?;
    // Défense à l'envoi contre un réglage local modifié hors du producteur.
    let devices: Vec<_> = devices
        .into_iter()
        .take(MAX_MODELES)
        .filter(|d| {
            d.manufacturer != crate::device_catalog::CUSTOM_BRAND
                && descriptif(&d.manufacturer).as_deref() == Some(d.manufacturer.as_str())
                && descriptif(&d.model).as_deref() == Some(d.model.as_str())
                && protocole(&d.protocol) == Some(d.protocol.as_str())
                && d.successful_plays <= MAX_COMPTEUR
                && d.failed_plays <= MAX_COMPTEUR
                && d.best_format.as_ref().is_none_or(format_valide)
        })
        .collect();
    (!devices.is_empty()).then_some(Contribution {
        schema_version: 1,
        devices,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::backend::DbBackend;
    use crate::db::sqlite::SqliteDb;
    use std::sync::Arc;

    fn settings() -> SettingsRepo {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let db: Arc<dyn DbBackend> = Arc::new(db);
        let settings = SettingsRepo::with_backend(db);
        settings.set("telemetry_enabled", "true").unwrap();
        settings.set("zone_7_brand", "Test Audio").unwrap();
        settings.set("zone_7_model", "Model 7291").unwrap();
        settings
    }

    fn format(rate: u32, bits: u16) -> FormatObserve {
        FormatObserve {
            codec: "flac".into(),
            sample_rate: Some(rate),
            bit_depth: Some(bits),
            dsd_mode: None,
        }
    }

    #[test]
    fn tune_tested_auto_refus_ne_collecte_ni_ne_prepare_un_envoi() {
        let settings = settings();
        settings.set("telemetry_enabled", "false").unwrap();
        let mut observateur = Observateur::default();
        assert!(
            !observateur
                .terminer(&settings, (7, 1, 0), "dlna", true, Some(format(96000, 24)))
                .unwrap()
        );
        assert!(
            settings.get(CLE).unwrap().is_none(),
            "refus : aucun agrégat ne doit être créé"
        );
        assert!(
            contribution(&settings).is_none(),
            "refus : aucune charge utile ne doit partir"
        );
    }

    #[test]
    fn tune_tested_auto_agrege_une_tentative_une_fois_et_garde_un_format_entier() {
        let settings = settings();
        let mut observateur = Observateur::default();
        assert!(
            observateur
                .terminer(&settings, (7, 1, 0), "dlna", true, Some(format(96000, 24)))
                .unwrap()
        );
        assert!(
            !observateur
                .terminer(&settings, (7, 1, 0), "dlna", true, Some(format(96000, 24)))
                .unwrap()
        );
        assert!(
            observateur
                .terminer(
                    &settings,
                    (7, 2, 0),
                    "dlna",
                    false,
                    Some(format(384000, 32))
                )
                .unwrap()
        );
        observateur
            .terminer(&settings, (7, 3, 0), "dlna", true, Some(format(192000, 16)))
            .unwrap();
        let devices = contribution(&settings).unwrap().devices;
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].successful_plays, 2);
        assert_eq!(devices[0].failed_plays, 1);
        assert_eq!(
            devices[0].best_format,
            Some(format(192000, 16)),
            "ne pas créer un 192/24 jamais lu ni publier le format échoué"
        );
    }

    #[test]
    fn tune_tested_auto_conserve_les_autres_protocoles_publics_sans_format_invente() {
        let settings = settings();
        let mut observateur = Observateur::default();
        for (index, protocol) in [
            "hqplayer",
            "squeezebox",
            "slimproto",
            "sendspin",
            "oaat",
            "oaat-multiroom",
        ]
        .into_iter()
        .enumerate()
        {
            assert!(
                observateur
                    .terminer(&settings, (7, index as u64 + 1, 0), protocol, true, None)
                    .unwrap(),
                "le protocole public {protocol} est perdu"
            );
        }
        let devices = contribution(&settings).unwrap().devices;
        assert_eq!(
            devices.len(),
            6,
            "les chemins publics distincts ne doivent pas être fusionnés"
        );
        for (protocol, count) in [
            ("hqplayer", 1),
            ("squeezebox", 1),
            ("slimproto", 1),
            ("sendspin", 1),
            ("oaat", 1),
            ("oaat-multiroom", 1),
        ] {
            let device = devices
                .iter()
                .find(|device| device.protocol == protocol)
                .unwrap();
            assert_eq!(device.successful_plays, count);
            assert_eq!(
                device.best_format, None,
                "aucun maximum ne se déduit du nom de protocole"
            );
        }
    }

    #[test]
    fn tune_tested_auto_gapless_meme_generation_compte_chaque_piste_une_fois() {
        let settings = settings();
        let mut observateur = Observateur::default();
        for passage in 0..3 {
            assert!(
                observateur
                    .terminer(&settings, (7, 1, passage), "local", true, None)
                    .unwrap()
            );
            assert!(
                !observateur
                    .terminer(&settings, (7, 1, passage), "local", true, None)
                    .unwrap()
            );
        }
        assert_eq!(
            contribution(&settings).unwrap().devices[0].successful_plays,
            3
        );
    }

    #[test]
    fn tune_tested_auto_persiste_et_separe_les_protocoles_sans_exporter_les_identites() {
        let settings = settings();
        settings.set("zone_7_name", "PRIVATE kitchen").unwrap();
        settings
            .set("zone_7_device_id", "123e4567-e89b-12d3-a456-426614174000")
            .unwrap();
        Observateur::default()
            .terminer(&settings, (7, 1, 0), "dlna", true, None)
            .unwrap();
        Observateur::default()
            .terminer(&settings, (7, 2, 0), "openhome", false, None)
            .unwrap();
        let contribution = contribution(&settings).unwrap();
        assert_eq!(contribution.devices.len(), 2);
        let json = serde_json::to_value(contribution).unwrap();
        let device = json["devices"][0].as_object().unwrap();
        let mut keys: Vec<_> = device.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "best_format",
                "failed_plays",
                "manufacturer",
                "model",
                "protocol",
                "successful_plays"
            ]
        );
        let texte = json.to_string();
        for interdit in [
            "PRIVATE kitchen",
            "123e4567-e89b-12d3-a456-426614174000",
            "zone_id",
            "device_id",
            "serial_number",
            "mac_address",
            "ip_address",
        ] {
            assert!(
                !texte.contains(interdit),
                "champ ou valeur interdite : {interdit}"
            );
        }
    }

    #[test]
    fn tune_tested_auto_ne_devine_pas_le_modele_depuis_un_nom_ou_une_adresse() {
        let settings = settings();
        let mut observateur = Observateur::default();
        for interdit in [
            "",
            "192.0.2.1",
            "DAC 192.0.2.1",
            "DAC-192.0.2.1",
            "DAC aa-bb-cc-dd-ee-ff",
            "aa-bb-cc-dd-ee-ff",
            "123e4567-e89b-12d3-a456-426614174000",
            "Model 123e4567-e89b-12d3-a456-426614174000",
        ] {
            settings.set("zone_7_model", interdit).unwrap();
            assert!(
                !observateur
                    .terminer(&settings, (7, 1, 0), "dlna", true, None)
                    .unwrap(),
                "identité refusée : {interdit}"
            );
        }
        assert!(contribution(&settings).is_none());
    }

    #[test]
    fn tune_tested_auto_borne_les_modeles_et_les_compteurs_sans_ecraser_les_observations() {
        let settings = settings();
        let appareils: Vec<_> = (0..199)
            .map(|n| ModeleObserve {
                manufacturer: "Test Audio".into(),
                model: format!("DAC {n}"),
                protocol: "dlna".into(),
                successful_plays: MAX_COMPTEUR,
                failed_plays: MAX_COMPTEUR,
                best_format: None,
            })
            .collect();
        settings
            .set(CLE, &serde_json::to_string(&appareils).unwrap())
            .unwrap();
        let mut observateur = Observateur::default();
        // La deux-centième identité est conservée ; la suivante ne remplace
        // aucune identité et n'augmente pas la charge utile.
        assert!(
            observateur
                .terminer(&settings, (7, 1, 0), "dlna", true, None)
                .unwrap()
        );
        settings.set("zone_7_model", "DAC supplémentaire").unwrap();
        assert!(
            !observateur
                .terminer(&settings, (7, 2, 0), "dlna", true, None)
                .unwrap()
        );
        let contribution = contribution(&settings).unwrap();
        assert_eq!(contribution.devices.len(), 200);
        assert!(contribution.devices.iter().any(|d| d.model == "Model 7291"));
        assert!(
            !contribution
                .devices
                .iter()
                .any(|d| d.model == "DAC supplémentaire")
        );
        // Une identité déjà présente continue d'être observée à la borne,
        // avec saturation des deux compteurs et sans débordement signé.
        settings.set("zone_7_model", "DAC 0").unwrap();
        assert!(
            observateur
                .terminer(&settings, (7, 3, 0), "dlna", true, None)
                .unwrap()
        );
        assert!(
            observateur
                .terminer(&settings, (7, 4, 0), "dlna", false, None)
                .unwrap()
        );
        let contribution = super::contribution(&settings).unwrap();
        let appareil = contribution
            .devices
            .iter()
            .find(|d| d.model == "DAC 0")
            .unwrap();
        assert_eq!(appareil.successful_plays, i32::MAX as u32);
        assert_eq!(appareil.failed_plays, i32::MAX as u32);
        assert_eq!(contribution.devices.len(), 200);
    }

    #[test]
    fn tune_tested_auto_pseudonyme_stable_distinct_du_brut_et_du_domaine_general() {
        let brut = "123e4567-e89b-12d3-a456-426614174000";
        let hash = pseudonyme_serveur(brut);
        assert_eq!(hash.len(), 64);
        assert_eq!(hash, pseudonyme_serveur(brut));
        assert_ne!(hash, format!("{:x}", Sha256::digest(brut.as_bytes())));
        assert!(!hash.contains(brut));
        assert_ne!(hash, pseudonyme_serveur("autre installation"));
    }

    #[test]
    fn tune_tested_auto_garde_la_famille_dsd_sans_inventer_le_mode_ni_la_resolution() {
        let settings = settings();
        let mut observateur = Observateur::default();
        observateur
            .terminer(&settings, (7, 1, 0), "dlna", true, Some(format(192000, 24)))
            .unwrap();
        let dsd = FormatObserve {
            codec: "dsd".into(),
            sample_rate: None,
            bit_depth: Some(1),
            dsd_mode: None,
        };
        observateur
            .terminer(&settings, (7, 2, 0), "dlna", true, Some(dsd.clone()))
            .unwrap();
        assert_eq!(
            contribution(&settings).unwrap().devices[0].best_format,
            Some(dsd),
            "ne ni perdre la famille DSD ni lui attribuer la résolution ou le mode d'une autre lecture"
        );
    }
}
