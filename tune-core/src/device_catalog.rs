//! Catalogue statique d'appareils (marque → modèles) et profils de « quirks »
//! associés.
//!
//! Le catalogue est une donnée versionnée embarquée dans le binaire
//! (`device_catalog.json`, `include_str!`). Il sert à deux choses :
//!
//! 1. **UI** : proposer à l'utilisateur, dans la config d'une zone, une marque
//!    puis un modèle via des menus déroulants (endpoint `GET /devices/catalog`).
//! 2. **Comportement** : dériver un profil de `DeviceQuirks` par modèle, pour
//!    piloter des adaptations de lecture *de façon additive* (jamais à la place
//!    de la détection auto existante — voir [`resolve_zone_quirks`]).
//!
//! Le choix utilisateur (marque + modèle) est persisté par zone dans les
//! settings clé-valeur : `zone_{id}_brand` / `zone_{id}_model`. La priorité
//! d'affichage côté serveur est : **override utilisateur > détection UPnP**.
//!
//! ## Quirks câblés vs framework
//!
//! Seuls les quirks *sûrs et additifs* sont câblés dans le chemin de lecture :
//! - [`DeviceQuirks::max_sample_rate`] : plafond de fréquence, combiné en `min`
//!   avec l'override de zone (ne fait que *baisser*, jamais monter).
//! - [`DeviceQuirks::force_16bit`] : mappé sur le flag zone `dlna_cap_16bit`
//!   existant (OR additif — ne peut que l'activer).
//!
//! Les autres champs (`force_mime`, `dlna_no_extra_headers`, `no_gapless`,
//! `pcm_only`, `dlna_wav24`, `dlna_native_flac`, `dlna_play_delay_ms`) sont
//! présents dans le profil (« framework prêt ») mais **volontairement non
//! câblés** dans le chemin de lecture tant qu'ils ne sont pas validés terrain :
//! les comportements correspondants sont déjà gérés dynamiquement ailleurs
//! (repli 714, sondes de capacités…) et un câblage naïf risquerait une
//! régression sur des zones qui fonctionnent aujourd'hui.

use serde::{Deserialize, Serialize};
use std::sync::{Arc, LazyLock};

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// Valeur libre choisie par l'utilisateur quand son appareil n'est pas au
/// catalogue. Aucun quirk n'est appliqué pour cette « marque ».
pub const CUSTOM_BRAND: &str = "Autre";

/// Profil de comportements spécifiques à un modèle. Tous les champs ont une
/// valeur neutre par défaut (aucun effet) : un modèle ne déclare que ce qui le
/// distingue.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceQuirks {
    /// Ne pas ajouter d'en-têtes HTTP DLNA supplémentaires (transferMode,
    /// contentFeatures…) — renderers stricts qui rejettent l'inconnu.
    /// **Framework only** (non câblé).
    #[serde(default)]
    pub dlna_no_extra_headers: bool,
    /// Plafond de fréquence d'échantillonnage en Hz (ex. 48000). **Câblé**
    /// (combiné en `min` avec l'override de zone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_sample_rate: Option<u32>,
    /// Forcer une orthographe/valeur MIME précise à l'annonce DLNA
    /// (ex. `audio/x-flac` pour les Sink stricts B&O). **Framework only** : le
    /// repli 714 gère déjà cela dynamiquement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_mime: Option<String>,
    /// Forcer une sortie 16-bit (renderers qui annoncent `audio/flac` mais ne
    /// décodent que 16-bit → 24-bit direct = silence, cf. Ruark R3 #1137).
    /// **Câblé** : OR additif avec le flag zone `dlna_cap_16bit`.
    #[serde(default)]
    pub force_16bit: bool,
    /// Désactiver le gapless pour ce modèle. **Framework only**.
    #[serde(default)]
    pub no_gapless: bool,
    /// Le renderer n'accepte que du PCM (jamais de FLAC/ALAC direct).
    /// **Framework only**.
    #[serde(default)]
    pub pcm_only: bool,
    /// Servir du WAV 24-bit réel plutôt que le repli LPCM 16-bit.
    /// **Framework only** (déjà exposé en flag zone après sonde de capacités).
    #[serde(default)]
    pub dlna_wav24: bool,
    /// Forcer le FLAC natif même si le Sink ne l'annonce pas (Denon Ceol N12).
    /// **Framework only** (déjà exposé en flag zone).
    #[serde(default)]
    pub dlna_native_flac: bool,
    /// Délai SetAVTransportURI→Play conseillé en ms (buffer à froid).
    /// **Framework only** (déjà exposé en flag zone/`[device_delays]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dlna_play_delay_ms: Option<u64>,
}

/// Un modèle du catalogue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceModel {
    pub name: String,
    #[serde(default)]
    pub quirks: DeviceQuirks,
}

/// Une marque et ses modèles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceBrand {
    pub name: String,
    #[serde(default)]
    pub models: Vec<DeviceModel>,
}

/// Le catalogue complet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCatalog {
    pub version: u32,
    pub brands: Vec<DeviceBrand>,
}

const CATALOG_JSON: &str = include_str!("device_catalog.json");

static CATALOG: LazyLock<DeviceCatalog> = LazyLock::new(|| {
    serde_json::from_str(CATALOG_JSON)
        .expect("device_catalog.json embarqué doit être un JSON valide")
});

/// Accès au catalogue embarqué (parsé une seule fois).
pub fn catalog() -> &'static DeviceCatalog {
    &CATALOG
}

/// Recherche un modèle par (marque, modèle), insensible à la casse et aux
/// espaces de bord. `None` si introuvable (marque libre « Autre », modèle
/// inconnu…).
pub fn find_model<'a>(brand: &str, model: &str) -> Option<&'a DeviceModel> {
    let brand = brand.trim();
    let model = model.trim();
    catalog()
        .brands
        .iter()
        .find(|b| b.name.eq_ignore_ascii_case(brand))
        .and_then(|b| b.models.iter().find(|m| m.name.eq_ignore_ascii_case(model)))
}

/// Profil de quirks pour un couple (marque, modèle). Profil neutre par défaut
/// si le modèle n'est pas au catalogue.
pub fn quirks_for(brand: &str, model: &str) -> DeviceQuirks {
    find_model(brand, model)
        .map(|m| m.quirks.clone())
        .unwrap_or_default()
}

/// Résout les quirks *effectifs* d'une zone depuis son override utilisateur
/// persisté (`zone_{id}_brand` / `zone_{id}_model`). Profil neutre si l'un des
/// deux est absent, ou si le modèle n'est pas au catalogue.
///
/// C'est le SEUL point d'entrée du chemin de lecture : un quirk ne s'active que
/// si l'utilisateur a explicitement choisi un modèle catalogué pour la zone.
pub fn resolve_zone_quirks(db: &Arc<dyn DbBackend>, zone_id: i64) -> DeviceQuirks {
    let settings = SettingsRepo::with_backend(db.clone());
    let brand = settings
        .get(&format!("zone_{zone_id}_brand"))
        .ok()
        .flatten();
    let model = settings
        .get(&format!("zone_{zone_id}_model"))
        .ok()
        .flatten();
    match (brand, model) {
        (Some(b), Some(m)) if !b.trim().is_empty() && !m.trim().is_empty() => quirks_for(&b, &m),
        _ => DeviceQuirks::default(),
    }
}

/// Combine deux plafonds de fréquence en prenant le plus contraignant (le
/// `min`). `None` = pas de plafond. Sert à appliquer le plafond catalogue
/// *en plus* de l'override de zone, sans jamais l'assouplir.
pub fn combine_max_sample_rate(zone: Option<u32>, quirk: Option<u32>) -> Option<u32> {
    match (zone, quirk) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_deserialises_and_is_non_empty() {
        let cat = catalog();
        assert!(cat.version >= 1);
        assert!(!cat.brands.is_empty());
        // Chaque marque a au moins un modèle, noms non vides.
        for b in &cat.brands {
            assert!(!b.name.trim().is_empty(), "marque sans nom");
            assert!(!b.models.is_empty(), "marque {} sans modèle", b.name);
            for m in &b.models {
                assert!(!m.name.trim().is_empty(), "modèle sans nom dans {}", b.name);
            }
        }
    }

    #[test]
    fn contains_seed_brands() {
        let names: Vec<&str> = catalog().brands.iter().map(|b| b.name.as_str()).collect();
        for expected in ["Sonos", "Bang & Olufsen", "WiiM", "Ruark Audio"] {
            assert!(
                names.contains(&expected),
                "marque attendue absente: {expected}"
            );
        }
    }

    /// Aucun identifiant en double, marque comme modèle.
    ///
    /// Ce n'est pas cosmétique : [`find_model`] résout par `find()`, donc sur
    /// le PREMIER élément qui correspond. Un doublon masquerait silencieusement
    /// le second — et si les deux ne portent pas les mêmes quirks, c'est le
    /// mauvais profil qui s'appliquerait à la lecture. La comparaison est
    /// insensible à la casse, comme la recherche.
    #[test]
    fn catalog_has_no_duplicate_identifiers() {
        let cat = catalog();

        let mut seen_brands: Vec<String> = Vec::new();
        for b in &cat.brands {
            let key = b.name.trim().to_ascii_lowercase();
            assert!(
                !seen_brands.contains(&key),
                "marque en double dans le catalogue: {}",
                b.name
            );
            seen_brands.push(key);

            let mut seen_models: Vec<String> = Vec::new();
            for m in &b.models {
                let mkey = m.name.trim().to_ascii_lowercase();
                assert!(
                    !seen_models.contains(&mkey),
                    "modèle en double chez {}: {}",
                    b.name,
                    m.name
                );
                seen_models.push(mkey);
            }
        }

        // La marque libre « Autre » ne doit jamais être catalogée : elle
        // signifie « hors catalogue, aucun quirk ».
        assert!(
            !seen_brands.contains(&CUSTOM_BRAND.to_ascii_lowercase()),
            "« {CUSTOM_BRAND} » est une saisie libre, pas une marque du catalogue"
        );
    }

    /// NAD (BluOS) et Samsung (TV DLNA) — #2136.
    ///
    /// Les deux marques sont ajoutées avec des modèles *sourcés* et un profil
    /// de quirks **neutre** : aucune capacité n'a été constatée sur ce matériel
    /// (ni plafond de fréquence, ni contrainte 16-bit). Le test verrouille cette
    /// neutralité — poser un quirk ici exige une mesure terrain, pas une
    /// supposition, sinon le diagnostic de tous les possesseurs est faussé.
    #[test]
    fn nad_and_samsung_are_catalogued_without_invented_quirks() {
        let names: Vec<&str> = catalog().brands.iter().map(|b| b.name.as_str()).collect();
        for expected in ["NAD", "Samsung"] {
            assert!(
                names.contains(&expected),
                "marque attendue absente: {expected}"
            );
        }

        // Modèles réellement sélectionnables (une marque nue n'offre rien).
        assert!(find_model("NAD", "M10 V3").is_some());
        assert!(find_model("NAD", "C 700").is_some());
        assert!(find_model("Samsung", "S95B").is_some());

        // Profil neutre : l'appareil se comporte exactement comme aujourd'hui.
        for (brand, model) in [
            ("NAD", "C 700"),
            ("NAD", "M10"),
            ("NAD", "M10 V2"),
            ("NAD", "M10 V3"),
            ("NAD", "M33"),
            ("NAD", "M66"),
            ("Samsung", "S95B"),
        ] {
            assert_eq!(
                quirks_for(brand, model),
                DeviceQuirks::default(),
                "{brand} {model} ne doit porter aucun quirk supposé"
            );
        }
    }

    #[test]
    fn lookup_is_case_insensitive() {
        assert!(find_model("sonos", "one").is_some());
        assert!(find_model("  Sonos  ", "  One  ").is_some());
        assert!(find_model("Sonos", "inconnu-xyz").is_none());
        assert!(find_model("MarqueInconnue", "One").is_none());
    }

    #[test]
    fn quirks_lookup_returns_expected_profiles() {
        // Sonos One : plafond 48 kHz câblé.
        assert_eq!(quirks_for("Sonos", "One").max_sample_rate, Some(48000));
        // Ruark R3 : force 16-bit câblé.
        assert!(quirks_for("Ruark Audio", "R3").force_16bit);
        // B&O Beoplay A9 : force_mime (framework only).
        assert_eq!(
            quirks_for("Bang & Olufsen", "Beoplay A9")
                .force_mime
                .as_deref(),
            Some("audio/x-flac")
        );
        // Modèle sans quirk / inconnu → profil neutre.
        assert_eq!(quirks_for("Sonos", "inconnu"), DeviceQuirks::default());
        assert_eq!(
            quirks_for(CUSTOM_BRAND, "quoi-que-ce-soit"),
            DeviceQuirks::default()
        );
    }

    #[test]
    fn combine_max_sample_rate_takes_the_stricter_bound() {
        assert_eq!(combine_max_sample_rate(None, None), None);
        assert_eq!(combine_max_sample_rate(Some(96000), None), Some(96000));
        assert_eq!(combine_max_sample_rate(None, Some(48000)), Some(48000));
        assert_eq!(
            combine_max_sample_rate(Some(96000), Some(48000)),
            Some(48000)
        );
        assert_eq!(
            combine_max_sample_rate(Some(44100), Some(48000)),
            Some(44100)
        );
    }

    #[test]
    fn resolve_zone_quirks_reads_override_from_settings() {
        use crate::db::migrations;
        use crate::db::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let settings = SettingsRepo::with_backend(backend.clone());

        // Sans override → profil neutre (aucun quirk actif).
        assert_eq!(resolve_zone_quirks(&backend, 1), DeviceQuirks::default());

        // Override utilisateur explicite → quirks du modèle catalogué.
        settings.set("zone_1_brand", "Sonos").unwrap();
        settings.set("zone_1_model", "One").unwrap();
        let q = resolve_zone_quirks(&backend, 1);
        assert_eq!(q.max_sample_rate, Some(48000));

        // Marque libre « Autre » → aucun quirk (texte utilisateur non catalogué).
        settings.set("zone_2_brand", CUSTOM_BRAND).unwrap();
        settings.set("zone_2_model", "Mon DAC maison").unwrap();
        assert_eq!(resolve_zone_quirks(&backend, 2), DeviceQuirks::default());

        // Marque seule sans modèle → profil neutre (les deux sont requis).
        settings.set("zone_3_brand", "Ruark Audio").unwrap();
        assert_eq!(resolve_zone_quirks(&backend, 3), DeviceQuirks::default());
    }

    #[test]
    fn quirks_json_roundtrip() {
        let q = quirks_for("Sonos", "One");
        let s = serde_json::to_string(&q).unwrap();
        let back: DeviceQuirks = serde_json::from_str(&s).unwrap();
        assert_eq!(q, back);
    }
}
