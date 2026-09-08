//! « Tune tested » : le catalogue des appareils validés, téléchargé depuis
//! mozaiklabs (#3589).
//!
//! ## Ce que sert le site, et ce qu'on en fait
//!
//! `GET /api/v1/community/devices/tune-tested` rend un objet dont la clé utile
//! est `version` : **l'horodatage en secondes de la validation la plus
//! récente**. Il ne recule jamais. Une instance qui porte déjà cette version
//! n'a rien à réappliquer — c'est une comparaison d'entiers, pas un diff.
//! Mesuré le 08/09/2026, l'enveloppe déployée est exactement :
//!
//! ```json
//! {"version":0,"generated_at":"2026-09-08T10:14:22+02:00","count":0,
//!  "settings_vocabulary":"tune.renderer.v1","devices":[]}
//! ```
//!
//! ## 🔴 Le vocabulaire n'est PAS celui des quirks
//!
//! `settings_vocabulary` vaut `tune.renderer.v1` : ce sont les réglages de
//! l'écran **Réglages/appareil** (`dlna_native_flac`, `alac_passthrough`,
//! `gain_trim_db`…), pas les quirks du catalogue embarqué. Les deux ensembles
//! ne se recouvrent que sur trois noms. Aucune traduction n'est inventée ici :
//! [`AppareilValide::settings`] reste une carte brute, et c'est
//! [`crate::device_preconfig`] qui décide, nom par nom, ce qui se pose.
//!
//! Le module **refuse** un vocabulaire qu'il ne connaît pas : mieux vaut garder
//! le repli embarqué que poser des réglages dont on ne sait pas ce qu'ils
//! nomment.
//!
//! ## 🔴 Un trim rond revient entier
//!
//! `gain_trim_db: -3.0` traverse la base et le JSON et revient **`-3`**. Un
//! champ typé `f64` par un désérialiseur strict rejetterait la moitié des
//! trims — précisément les valeurs rondes que les gens saisissent. Les réglages
//! sont donc lus en [`serde_json::Value`] et relus par [`AppareilValide::nombre`],
//! qui accepte l'entier comme le flottant (et la chaîne, que la base rend
//! parfois pour une colonne texte).
//!
//! ## Hors ligne
//!
//! Toute erreur — réseau, statut non-2xx, JSON illisible, vocabulaire inconnu —
//! rend [`Issue::Repli`] et **ne touche à rien**. L'instance continue avec ce
//! qu'elle a : le dernier catalogue rangé, ou, si elle n'en a jamais eu, le
//! catalogue embarqué (`device_catalog.json`). Une instance sans réseau se
//! comporte exactement comme avant ce module.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// Racine du nuage, quand `mozaik_base_url` ne la redirige pas.
pub const RACINE_PAR_DEFAUT: &str = "https://mozaiklabs.fr";

/// Chemin du catalogue validé.
pub const CHEMIN: &str = "/api/v1/community/devices/tune-tested";

/// Le seul vocabulaire de réglages que ce serveur sait lire.
pub const VOCABULAIRE_CONNU: &str = "tune.renderer.v1";

/// Réglage où la version connue est rangée (entier, en secondes).
pub const CLE_VERSION: &str = "tune_tested_version";

/// Réglage où le catalogue téléchargé est rangé (JSON brut).
pub const CLE_CATALOGUE: &str = "tune_tested_catalogue";

/// Enveloppe servie par le site.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CatalogueValide {
    /// Horodatage de la validation la plus récente, en secondes. Ne recule
    /// jamais. `0` = le site n'a encore rien validé (mesuré le 08/09/2026).
    #[serde(default)]
    pub version: i64,
    #[serde(default)]
    pub generated_at: Option<String>,
    #[serde(default)]
    pub count: usize,
    #[serde(default)]
    pub settings_vocabulary: String,
    #[serde(default)]
    pub devices: Vec<AppareilValide>,
}

/// Un appareil validé et la configuration qui l'a fait valider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppareilValide {
    pub brand: String,
    pub model: String,
    #[serde(default)]
    pub output_type: Option<String>,
    /// Réglages dans le vocabulaire annoncé par l'enveloppe. Volontairement
    /// non typé : voir le piège du trim rond en tête de module.
    #[serde(default)]
    pub settings: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub households: Option<u64>,
    #[serde(default)]
    pub validated_at: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

impl AppareilValide {
    /// Est-ce cet appareil-là ? Comparaison insensible à la casse et aux
    /// espaces de bord, comme [`crate::device_catalog::find_model`].
    pub fn correspond(&self, brand: &str, model: &str) -> bool {
        self.brand.trim().eq_ignore_ascii_case(brand.trim())
            && self.model.trim().eq_ignore_ascii_case(model.trim())
    }

    /// Un réglage booléen. `1`/`0` et `"true"`/`"false"` comptent aussi : la
    /// base rend un booléen comme un entier ou comme du texte selon la colonne.
    pub fn drapeau(&self, cle: &str) -> Option<bool> {
        match self.settings.get(cle)? {
            serde_json::Value::Bool(b) => Some(*b),
            serde_json::Value::Number(n) => n.as_f64().map(|v| v != 0.0),
            serde_json::Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => Some(true),
                "false" | "0" | "no" | "off" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }

    /// Un réglage numérique. 🔴 **C'est ici que se joue le piège du trim** :
    /// `-3` (entier) et `-3.0` (flottant) rendent tous deux `-3.0`.
    pub fn nombre(&self, cle: &str) -> Option<f64> {
        match self.settings.get(cle)? {
            serde_json::Value::Number(n) => n.as_f64(),
            serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
            serde_json::Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }
}

impl CatalogueValide {
    /// L'entrée qui décrit ce couple marque/modèle, s'il est validé.
    ///
    /// `output_type` n'est discriminant que s'il est renseigné **des deux
    /// côtés** : une entrée sans `output_type` vaut pour toutes les sorties.
    pub fn appareil(
        &self,
        brand: &str,
        model: &str,
        output_type: Option<&str>,
    ) -> Option<&AppareilValide> {
        self.devices.iter().find(|d| {
            d.correspond(brand, model)
                && match (d.output_type.as_deref(), output_type) {
                    (Some(a), Some(b)) => a.trim().eq_ignore_ascii_case(b.trim()),
                    (Some(_), None) => false,
                    _ => true,
                }
        })
    }

    /// Le vocabulaire annoncé est-il celui que ce serveur sait lire ?
    pub fn vocabulaire_lisible(&self) -> bool {
        // Une enveloppe muette est tolérée : c'est la forme des toutes
        // premières réponses, et elle ne porte alors aucun appareil.
        self.settings_vocabulary.is_empty() || self.settings_vocabulary == VOCABULAIRE_CONNU
    }
}

/// Ce qu'un rafraîchissement a produit.
#[derive(Debug, Clone, PartialEq)]
pub enum Issue {
    /// Le site porte la version déjà connue : rien n'a été retéléchargé ni
    /// réappliqué.
    Inchange(i64),
    /// Une version plus récente a été rangée.
    Range { avant: i64, apres: i64 },
    /// Rien n'a bougé, et la raison est dite. Le catalogue en place est
    /// conservé : hors ligne, l'instance se comporte comme avant.
    Repli(String),
}

/// La version rangée, `0` si aucune.
pub fn version_connue(settings: &SettingsRepo) -> i64 {
    settings
        .get(CLE_VERSION)
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(0)
}

/// Le catalogue téléchargé qui est rangé, `None` si aucun n'a jamais abouti —
/// auquel cas l'appelant garde le catalogue embarqué.
pub fn catalogue_range(settings: &SettingsRepo) -> Option<CatalogueValide> {
    let brut = settings.get(CLE_CATALOGUE).ok().flatten()?;
    match serde_json::from_str::<CatalogueValide>(&brut) {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::debug!(error = %e, "tune_tested_catalogue_range_illisible");
            None
        }
    }
}

/// L'adresse à interroger, `mozaik_base_url` d'abord.
pub fn adresse(settings: &SettingsRepo) -> String {
    let racine = settings
        .get("mozaik_base_url")
        .ok()
        .flatten()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| RACINE_PAR_DEFAUT.to_string());
    format!("{}{CHEMIN}", racine.trim_end_matches('/'))
}

/// Range une enveloppe déjà obtenue, si et seulement si sa version a avancé.
///
/// Séparé du téléchargement pour être vérifiable sans réseau : c'est ici que
/// vivent la comparaison d'entiers et le refus d'un vocabulaire inconnu.
pub fn ranger(settings: &SettingsRepo, catalogue: &CatalogueValide) -> Issue {
    if !catalogue.vocabulaire_lisible() {
        return Issue::Repli(format!(
            "vocabulaire inconnu : {}",
            catalogue.settings_vocabulary
        ));
    }
    let avant = version_connue(settings);
    if catalogue.version <= avant {
        return Issue::Inchange(avant);
    }
    let brut = match serde_json::to_string(catalogue) {
        Ok(b) => b,
        Err(e) => return Issue::Repli(format!("sérialisation impossible : {e}")),
    };
    if let Err(e) = settings.set(CLE_CATALOGUE, &brut) {
        return Issue::Repli(format!("écriture du catalogue impossible : {e}"));
    }
    // La version n'est posée qu'APRÈS le catalogue : l'inverse laisserait une
    // instance persuadée d'avoir la version N sans en porter les appareils, et
    // elle ne retéléchargerait jamais.
    if let Err(e) = settings.set(CLE_VERSION, &catalogue.version.to_string()) {
        return Issue::Repli(format!("écriture de la version impossible : {e}"));
    }
    Issue::Range {
        avant,
        apres: catalogue.version,
    }
}

/// Télécharge le catalogue validé et le range si sa version a avancé.
///
/// Ne rend jamais d'erreur : hors ligne, l'instance garde ce qu'elle a.
pub async fn rafraichir(db: &Arc<dyn DbBackend>) -> Issue {
    let settings = SettingsRepo::with_backend(db.clone());
    let url = adresse(&settings);
    let client = match crate::http::client::builder()
        .timeout(Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => return Issue::Repli(format!("client HTTP indisponible : {e}")),
    };
    let reponse = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => return Issue::Repli(format!("hôte injoignable : {e}")),
    };
    if !reponse.status().is_success() {
        return Issue::Repli(format!("statut {}", reponse.status().as_u16()));
    }
    let catalogue = match reponse.json::<CatalogueValide>().await {
        Ok(c) => c,
        Err(e) => return Issue::Repli(format!("réponse illisible : {e}")),
    };
    ranger(&settings, &catalogue)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;

    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().expect("base mémoire");
        db.init_schema().expect("schéma");
        migrations::run_migrations(&db).expect("migrations");
        Arc::new(db)
    }

    fn enveloppe(version: i64) -> CatalogueValide {
        serde_json::from_str(&format!(
            r#"{{"version":{version},"generated_at":"2026-09-08T09:12:00+00:00","count":1,
                "settings_vocabulary":"tune.renderer.v1",
                "devices":[{{"brand":"Eversolo","model":"DMP-A8","output_type":"dlna",
                  "settings":{{"dlna_native_flac":true,"gain_trim_db":-3}},
                  "households":20,"validated_at":"2026-09-08T09:10:00+00:00",
                  "note":"Le 192 sature sur firmware 1.4."}}]}}"#
        ))
        .expect("enveloppe de référence lisible")
    }

    /// 🔴 Le piège nommé par #3589 : `-3` arrive ENTIER du JSON. Un champ typé
    /// `f64` strict le rejetterait, et avec lui la moitié des trims du parc.
    #[test]
    fn un_trim_rond_arrive_entier_et_se_lit_quand_meme() {
        let cat = enveloppe(1788800000);
        let dev = &cat.devices[0];
        // La preuve que la valeur est bien un ENTIER dans le JSON, sinon le
        // test se contenterait de relire un flottant et ne garderait rien.
        assert!(
            dev.settings["gain_trim_db"].is_i64(),
            "le JSON de référence doit porter un entier, sinon ce test ne \
             garde pas le piège : {:?}",
            dev.settings["gain_trim_db"]
        );
        assert_eq!(dev.nombre("gain_trim_db"), Some(-3.0));
        // Et la forme flottante passe aussi, évidemment.
        let flottant: AppareilValide =
            serde_json::from_str(r#"{"brand":"X","model":"Y","settings":{"gain_trim_db":-3.5}}"#)
                .unwrap();
        assert_eq!(flottant.nombre("gain_trim_db"), Some(-3.5));
    }

    #[test]
    fn les_drapeaux_acceptent_booleen_entier_et_texte() {
        let dev: AppareilValide = serde_json::from_str(
            r#"{"brand":"X","model":"Y","settings":{"a":true,"b":1,"c":"false","d":0}}"#,
        )
        .unwrap();
        assert_eq!(dev.drapeau("a"), Some(true));
        assert_eq!(dev.drapeau("b"), Some(true));
        assert_eq!(dev.drapeau("c"), Some(false));
        assert_eq!(dev.drapeau("d"), Some(false));
        assert_eq!(dev.drapeau("absent"), None);
    }

    /// L'enveloppe RÉELLEMENT servie le 08/09/2026, catalogue vide. Elle doit
    /// se lire sans rien casser : c'est l'état du site le jour du chantier.
    #[test]
    fn l_enveloppe_vide_du_site_se_lit() {
        let cat: CatalogueValide = serde_json::from_str(
            r#"{"version":0,"generated_at":"2026-09-08T10:14:22+02:00","count":0,
                "settings_vocabulary":"tune.renderer.v1","devices":[]}"#,
        )
        .expect("l'enveloppe déployée est lisible");
        assert_eq!(cat.version, 0);
        assert!(cat.devices.is_empty());
        assert!(cat.vocabulaire_lisible());
    }

    #[test]
    fn une_version_qui_n_a_pas_bouge_ne_reapplique_rien() {
        let db = base();
        let settings = SettingsRepo::with_backend(db.clone());
        assert_eq!(version_connue(&settings), 0);

        let cat = enveloppe(1788800000);
        assert_eq!(
            ranger(&settings, &cat),
            Issue::Range {
                avant: 0,
                apres: 1788800000
            }
        );
        assert_eq!(version_connue(&settings), 1788800000);

        // Même version : inchangé, et le catalogue rangé n'est pas réécrit.
        assert_eq!(
            ranger(&settings, &cat),
            Issue::Inchange(1788800000),
            "une version identique ne doit rien réappliquer"
        );
        // Une version PLUS ANCIENNE ne doit pas faire reculer l'instance.
        assert_eq!(
            ranger(&settings, &enveloppe(1788700000)),
            Issue::Inchange(1788800000),
            "la version ne recule jamais"
        );
        assert_eq!(version_connue(&settings), 1788800000);
    }

    #[test]
    fn le_catalogue_range_se_relit_avec_ses_appareils() {
        let db = base();
        let settings = SettingsRepo::with_backend(db.clone());
        ranger(&settings, &enveloppe(1788800000));
        let relu = catalogue_range(&settings).expect("catalogue rangé relisible");
        let dev = relu
            .appareil("eversolo", "  dmp-a8 ", Some("dlna"))
            .expect("recherche insensible à la casse et aux espaces");
        assert_eq!(dev.nombre("gain_trim_db"), Some(-3.0));
        assert_eq!(dev.drapeau("dlna_native_flac"), Some(true));
        // Sortie qui ne correspond pas : pas de préconfiguration par erreur.
        assert!(relu.appareil("Eversolo", "DMP-A8", Some("local")).is_none());
        // Modèle inconnu : rien.
        assert!(relu.appareil("Eversolo", "DMP-A6", Some("dlna")).is_none());
    }

    /// Un vocabulaire inconnu ne se traduit pas : on garde ce qu'on a.
    #[test]
    fn un_vocabulaire_inconnu_est_refuse_sans_rien_ecraser() {
        let db = base();
        let settings = SettingsRepo::with_backend(db.clone());
        ranger(&settings, &enveloppe(1788800000));

        let mut etrange = enveloppe(1799000000);
        etrange.settings_vocabulary = "tune.quirks.v9".into();
        assert!(matches!(ranger(&settings, &etrange), Issue::Repli(_)));

        // Ni la version ni le catalogue n'ont bougé.
        assert_eq!(version_connue(&settings), 1788800000);
        assert_eq!(
            catalogue_range(&settings).map(|c| c.version),
            Some(1788800000)
        );
    }

    #[test]
    fn l_adresse_suit_mozaik_base_url() {
        let db = base();
        let settings = SettingsRepo::with_backend(db.clone());
        assert_eq!(
            adresse(&settings),
            format!("{RACINE_PAR_DEFAUT}/api/v1/community/devices/tune-tested")
        );
        settings
            .set("mozaik_base_url", "http://127.0.0.1:9099/")
            .unwrap();
        assert_eq!(
            adresse(&settings),
            "http://127.0.0.1:9099/api/v1/community/devices/tune-tested",
            "la barre finale ne doit pas se doubler"
        );
    }
}
