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
    /// #5194 — profil de la MARQUE, valable pour TOUT modèle de cette marque,
    /// catalogué ou non. Un modèle catalogué le complète (ses propres valeurs
    /// passent devant) ; un modèle inconnu le reçoit tel quel.
    ///
    /// Sonos en est la raison : ses enceintes ne décodent le FLAC que jusqu'à
    /// 48 kHz, et la liste de modèles ne suivra jamais le rythme des sorties
    /// (Era 100 et Ray jouaient du silence en 96 kHz, fil forum 1978).
    #[serde(default, skip_serializing_if = "is_neutral")]
    pub default_quirks: DeviceQuirks,
    #[serde(default)]
    pub models: Vec<DeviceModel>,
}

fn is_neutral(q: &DeviceQuirks) -> bool {
    *q == DeviceQuirks::default()
}

impl DeviceQuirks {
    /// Complète ce profil (celui d'un modèle) par le profil de sa marque : un
    /// drapeau affirmé d'un côté ou de l'autre reste affirmé, une valeur posée
    /// par le modèle passe devant celle de la marque.
    fn completer_par(mut self, marque: &DeviceQuirks) -> DeviceQuirks {
        self.dlna_no_extra_headers |= marque.dlna_no_extra_headers;
        self.force_16bit |= marque.force_16bit;
        self.no_gapless |= marque.no_gapless;
        self.pcm_only |= marque.pcm_only;
        self.dlna_wav24 |= marque.dlna_wav24;
        self.dlna_native_flac |= marque.dlna_native_flac;
        self.max_sample_rate = self.max_sample_rate.or(marque.max_sample_rate);
        if self.force_mime.is_none() {
            self.force_mime = marque.force_mime.clone();
        }
        self.dlna_play_delay_ms = self.dlna_play_delay_ms.or(marque.dlna_play_delay_ms);
        self
    }
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

/// La marque du catalogue que désigne `brand`, insensible à la casse et aux
/// espaces de bord.
///
/// #5194 — accepte aussi la raison sociale que le descripteur UPnP met dans
/// `<manufacturer>` : « Sonos, Inc. » désigne « Sonos ». Le nom du catalogue
/// doit alors être suivi d'un séparateur (virgule, espace, point…), jamais
/// d'une lettre : « Sonosphere » ne désigne pas « Sonos ».
pub fn find_brand<'a>(brand: &str) -> Option<&'a DeviceBrand> {
    let brand = brand.trim();
    if brand.is_empty() {
        return None;
    }
    let cat = catalog();
    cat.brands
        .iter()
        .find(|b| b.name.eq_ignore_ascii_case(brand))
        .or_else(|| {
            cat.brands
                .iter()
                .find(|b| prefixe_suivi_d_un_separateur(brand, &b.name).is_some())
        })
}

/// Si `texte` commence par `prefixe` (casse ignorée) suivi d'un caractère non
/// alphanumérique, rend le reste débarrassé de ses séparateurs de tête.
fn prefixe_suivi_d_un_separateur<'t>(texte: &'t str, prefixe: &str) -> Option<&'t str> {
    let tete = texte.get(..prefixe.len())?;
    if !tete.eq_ignore_ascii_case(prefixe) {
        return None;
    }
    let reste = &texte[prefixe.len()..];
    let suivant = reste.chars().next()?;
    if suivant.is_alphanumeric() {
        return None;
    }
    Some(reste.trim_start_matches(|c: char| !c.is_alphanumeric()))
}

/// Le modèle de `marque` que désigne `model`. Accepte le `<modelName>` UPnP,
/// qui répète souvent la marque : « Sonos Era 100 » désigne « Era 100 ».
fn find_model_in<'a>(marque: &'a DeviceBrand, model: &str) -> Option<&'a DeviceModel> {
    let model = model.trim();
    if model.is_empty() {
        return None;
    }
    marque
        .models
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case(model))
        .or_else(|| {
            let sans_marque = prefixe_suivi_d_un_separateur(model, &marque.name)?;
            marque
                .models
                .iter()
                .find(|m| m.name.eq_ignore_ascii_case(sans_marque))
        })
}

/// Recherche un modèle par (marque, modèle), insensible à la casse et aux
/// espaces de bord. `None` si introuvable (marque libre « Autre », modèle
/// inconnu…).
pub fn find_model<'a>(brand: &str, model: &str) -> Option<&'a DeviceModel> {
    find_brand(brand).and_then(|b| find_model_in(b, model))
}

/// Profil de quirks pour un couple (marque, modèle) : celui du modèle,
/// complété par celui de sa marque ; celui de la marque seule quand le modèle
/// n'est pas au catalogue (#5194) ; neutre quand la marque ne l'est pas.
pub fn quirks_for(brand: &str, model: &str) -> DeviceQuirks {
    let Some(marque) = find_brand(brand) else {
        return DeviceQuirks::default();
    };
    match find_model_in(marque, model) {
        Some(m) => m.quirks.clone().completer_par(&marque.default_quirks),
        None => marque.default_quirks.clone(),
    }
}

/// #3660 — la clé du vide FORCÉ sur l'identité d'appareil d'une zone :
/// « l'appareil détecté n'est PAS celui de cette zone ». Une seule définition
/// pour la route qui l'écrit et la lecture qui la respecte.
pub fn cle_identite_effacee(zone_id: i64) -> String {
    format!("zone_{zone_id}_identite_effacee")
}

/// Clé de réglage du magasin des renderers connus, écrit par la découverte
/// SSDP à chaque renderer enregistré (#1126, identité #2639) : un tableau JSON
/// de `{device_id, location, name, mac, manufacturer, model}`.
pub const KNOWN_RENDERERS_KEY: &str = "known_renderers";

/// La marque et le modèle DÉTECTÉS (`<manufacturer>`, `<modelName>` UPnP) de
/// l'appareil de la zone, lus dans le magasin des renderers connus. `None`
/// quand la zone n'a pas d'appareil, que le magasin ne le connaît pas, ou que
/// l'un des deux champs est vide.
fn detected_identity(db: &Arc<dyn DbBackend>, zone_id: i64) -> Option<(String, String)> {
    let device_id = crate::db::zone_repo::ZoneRepo::with_backend(db.clone())
        .get(zone_id)
        .ok()
        .flatten()?
        .output_device_id?;
    let magasin = SettingsRepo::with_backend(db.clone())
        .get(KNOWN_RENDERERS_KEY)
        .ok()
        .flatten()?;
    let entrees: Vec<serde_json::Value> = serde_json::from_str(&magasin).ok()?;
    let entree = entrees
        .iter()
        .find(|e| e.get("device_id").and_then(|v| v.as_str()) == Some(device_id.as_str()))?;
    let champ = |k: &str| {
        entree
            .get(k)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    Some((champ("manufacturer")?, champ("model")?))
}

/// Résout les quirks *effectifs* d'une zone : depuis son override utilisateur
/// persisté (`zone_{id}_brand` / `zone_{id}_model`) quand les deux sont posés,
/// sinon depuis l'identité DÉTECTÉE de son appareil (magasin
/// [`KNOWN_RENDERERS_KEY`]), sauf si l'utilisateur l'a récusée
/// (`zone_{id}_identite_effacee`, #3660). Profil neutre si rien n'est connu
/// ou que la marque n'est pas au catalogue.
///
/// C'est le SEUL point d'entrée du chemin de lecture.
///
/// 🔴 #5194 — la retombée sur la détection manquait : un quirk ne s'activait
/// que si l'utilisateur avait choisi un modèle à la main. Aucun Sonos
/// découvert ne recevait donc son plafond de 48 kHz, et un FLAC Qobuz 96 kHz
/// partait tel quel vers un Era 100 ou un Ray, qui bouclaient sans un son.
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
        _ => {
            let recusee = settings
                .get(&cle_identite_effacee(zone_id))
                .ok()
                .flatten()
                .as_deref()
                == Some("true");
            if recusee {
                return DeviceQuirks::default();
            }
            detected_identity(db, zone_id)
                .map(|(b, m)| quirks_for(&b, &m))
                .unwrap_or_default()
        }
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

    /// Weiss — #4178 (Kimon, fil 1799) : la marque manquait aux 25 du
    /// catalogue, donc absente des menus marque/modèle et ramenée à la saisie
    /// libre « Autre », sans profil. Modèles sourcés sur weiss.ch/products le
    /// 19/09/2026 (DAC501/502 et leurs MK2, DAC204/205, DSP501/502, HELIOS,
    /// MAN301/301R, DAC301, DAC202, MEDUS), profil de quirks **neutre** :
    /// aucune mesure terrain sur ce matériel — en poser un serait une
    /// supposition, même règle que NAD/Samsung ci-dessus.
    #[test]
    fn weiss_est_catalogue_sans_quirk_invente() {
        let names: Vec<&str> = catalog().brands.iter().map(|b| b.name.as_str()).collect();
        assert!(
            names.contains(&"Weiss"),
            "Weiss absent du catalogue : {names:?}"
        );
        for model in [
            "DAC501",
            "DAC502",
            "DAC501-MK2",
            "DAC502-MK2",
            "DAC204",
            "DAC204-MK2",
            "DAC205-MK2",
            "DSP501",
            "DSP502",
            "HELIOS",
            "MAN301",
            "MAN301R",
            "DAC301",
            "DAC202",
            "MEDUS",
        ] {
            assert!(
                find_model("Weiss", model).is_some(),
                "modèle Weiss absent : {model}"
            );
            assert_eq!(
                quirks_for("Weiss", model),
                DeviceQuirks::default(),
                "Weiss {model} ne doit porter aucun quirk supposé"
            );
        }
        // Même tolérance de casse que le reste du catalogue.
        assert!(find_model("weiss", "dac502-mk2").is_some());
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
        // Modèle inconnu d'une marque SANS profil de marque → profil neutre.
        assert_eq!(quirks_for("NAD", "inconnu"), DeviceQuirks::default());
        // #5194 — modèle Sonos inconnu : le profil de la MARQUE s'applique.
        assert_eq!(quirks_for("Sonos", "inconnu").max_sample_rate, Some(48000));
        assert_eq!(
            quirks_for(CUSTOM_BRAND, "quoi-que-ce-soit"),
            DeviceQuirks::default()
        );
    }

    /// #5194 — l'identité telle que le descripteur UPnP d'un Sonos la donne :
    /// `<manufacturer>Sonos, Inc.</manufacturer>` et un `<modelName>` qui
    /// répète la marque. Tout Sonos, catalogué ou non, est plafonné à 48 kHz,
    /// sans contrainte de profondeur (24 bits acceptés : pas de `force_16bit`).
    ///
    /// Sabotage qui rend ce témoin ROUGE : rendre `DeviceQuirks::default()` au
    /// lieu de `marque.default_quirks.clone()` dans `quirks_for`.
    #[test]
    fn tout_sonos_est_plafonne_a_48k_par_sa_marque_5194() {
        for modele in [
            "Sonos Era 100",
            "Sonos Ray",
            "Era 300",
            "Sonos Arc Ultra",
            "Sonos Zzz 2031",
        ] {
            let q = quirks_for("Sonos, Inc.", modele);
            assert_eq!(q.max_sample_rate, Some(48000), "{modele}");
            assert!(!q.force_16bit, "{modele} : le 24 bits reste permis");
        }
        // Le modèle catalogué est bien RECONNU sous son `<modelName>` UPnP.
        assert_eq!(
            find_model("Sonos, Inc.", "Sonos Era 100").map(|m| m.name.as_str()),
            Some("Era 100")
        );
        assert_eq!(
            find_model("Sonos, Inc.", "Sonos Ray").map(|m| m.name.as_str()),
            Some("Ray")
        );
        // Un nom qui ne fait que COMMENCER par les mêmes lettres n'est pas la
        // marque.
        assert!(find_brand("Sonosphere").is_none());
        assert_eq!(quirks_for("Sonosphere", "X"), DeviceQuirks::default());
        // Le profil de la marque complète celui d'un modèle, sans l'écraser.
        let ruark = quirks_for("Ruark Audio", "R3");
        assert!(ruark.force_16bit);
        assert_eq!(ruark.max_sample_rate, None);
    }

    /// #5194 — sans override, la zone prend les quirks de l'appareil DÉTECTÉ
    /// (magasin `known_renderers`) ; une identité récusée (#3660) les coupe.
    ///
    /// Sabotage qui rend ce témoin ROUGE : rendre `DeviceQuirks::default()`
    /// dans la branche « sans override » de `resolve_zone_quirks`.
    #[test]
    fn resolve_zone_quirks_retombe_sur_l_appareil_detecte_5194() {
        use crate::db::migrations;
        use crate::db::sqlite::SqliteDb;
        use crate::db::zone_repo::ZoneRepo;

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let settings = SettingsRepo::with_backend(backend.clone());
        let zid = ZoneRepo::with_backend(backend.clone())
            .create("Salon", Some("dlna"), Some("uuid:RINCON_RAY"))
            .unwrap();

        // Aucun appareil connu : neutre.
        assert_eq!(resolve_zone_quirks(&backend, zid), DeviceQuirks::default());

        settings
            .set(
                KNOWN_RENDERERS_KEY,
                r#"[{"device_id":"uuid:RINCON_RAY","location":"http://h/x.xml","name":"Sonos Ray","manufacturer":"Sonos, Inc.","model":"Sonos Ray"}]"#,
            )
            .unwrap();
        assert_eq!(
            resolve_zone_quirks(&backend, zid).max_sample_rate,
            Some(48000)
        );

        // L'override reste roi : « Autre » ⇒ aucun quirk.
        settings
            .set(&format!("zone_{zid}_brand"), CUSTOM_BRAND)
            .unwrap();
        settings
            .set(&format!("zone_{zid}_model"), "Mon DAC")
            .unwrap();
        assert_eq!(resolve_zone_quirks(&backend, zid), DeviceQuirks::default());
        settings.delete(&format!("zone_{zid}_brand")).unwrap();
        settings.delete(&format!("zone_{zid}_model")).unwrap();

        // Identité récusée : la détection ne compte plus.
        settings.set(&cle_identite_effacee(zid), "true").unwrap();
        assert_eq!(resolve_zone_quirks(&backend, zid), DeviceQuirks::default());
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
