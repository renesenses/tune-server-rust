//! #7414 — les logos de labels partagés entre serveurs Tune, par
//! mozaiklabs.fr ([`LogosPartages`] sur HTTP).
//!
//! Le contrat est celui de `renesenses/site-mozaiklabs#450` :
//!
//! - `POST /api/v1/community/label-logos/lookup` — lecture groupée,
//!   `{labels: [{name, musicbrainz_label_id?, qobuz_label_id?}]}`, 500 au
//!   plus ; rend `{logos: [{name, image_url, sha256, musicbrainz_label_id,
//!   source, provider, license_status, license, source_url, …}]}` pour les
//!   seuls labels connus. Une LECTURE en POST : 500 noms ne tiennent pas dans
//!   une URL (le 414 des bios par titre, `bio_sync.rs`).
//! - `GET <image_url>` — la vignette PNG, réencodée par le site, en cache un
//!   an. Seule une adresse sous la racine du site est suivie : le site ne
//!   peut pas faire télécharger une adresse quelconque au serveur.
//! - `POST /api/v1/community/label-logos` — la proposition d'un logo
//!   (multipart), par une installation connue (`instance_id`, celui du
//!   battement), avec `consent=1`.
//!
//! # Rien ne part sans accord
//!
//! - **Lire** envoie des noms de labels de la bibliothèque : seulement si la
//!   télémétrie n'est pas refusée (`TelemetryReporter::is_enabled_for`), la
//!   même règle que la redescente des bios communautaires.
//! - **Proposer** : seulement avec le consentement communautaire
//!   (`community_contribution_enabled`, désactivé par défaut, que le refus de
//!   télémétrie coupe aussi : [`crate::cloud::consent::contribution_autorisee`])
//!   ET un `instance_id`.
//!
//! # Jamais bloquant, toujours borné
//!
//! Le client HTTP est celui du serveur (couture unique, `http_client_seam`),
//! chaque appel a un délai court, et passe par [`rate_limit::appeler`] : un
//! 429 du site retient la portée (lecture ou dépôt) jusqu'à l'échéance
//! annoncée, redémarrage compris, sans qu'aucun appel ne parte. Une erreur,
//! un délai dépassé ou un refus ne bloquent rien : la passe continue sur le
//! web.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::cloud::rate_limit::{self, AppelCloud, CloudScope};
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;
use crate::library::logos_labels::{
    Attribution, LabelAConsulter, LogoPartage, LogosPartages, ProposeAuPartage, PropositionDeLogo,
    SourceDuLogo, StatutDeLicence, TAILLE_MAX_IMAGE,
};

/// Racine du site quand `mozaik_base_url` ne la redirige pas.
pub const RACINE_PAR_DEFAUT: &str = "https://mozaiklabs.fr";
/// Les routes, sous la racine.
pub const CHEMIN_PROPOSITION: &str = "/api/v1/community/label-logos";
pub const CHEMIN_CONSULTATION: &str = "/api/v1/community/label-logos/lookup";
/// Délai d'une lecture ou d'un téléchargement : la passe n'attend pas plus.
pub const DELAI_LECTURE: Duration = Duration::from_secs(5);
/// Délai d'un dépôt (une image part).
pub const DELAI_DEPOT: Duration = Duration::from_secs(15);

/// Le partage, vu d'un serveur Tune.
pub struct PartageHttp {
    http: reqwest::Client,
    reglages: SettingsRepo,
    racine: String,
    instance_id: Option<String>,
    consentement: bool,
}

/// La racine d'essai de ce partage pour
/// [`crate::services_factices::substituer_base`].
pub const BASE_FACTICE: &str = "logos-labels-partage";

/// La racine du site : `mozaik_base_url` s'il est réglé, sinon
/// [`RACINE_PAR_DEFAUT`]. Dans une construction de tests
/// (`services-factices`), JAMAIS le vrai site : une racine réglée, ou une
/// racine d'essai déclarée sous [`BASE_FACTICE`], sinon aucun partage — un
/// banc qui ouvre l'onglet Labels n'appelle pas mozaiklabs.fr.
fn racine(reglages: &SettingsRepo) -> Option<String> {
    let reglee = reglages
        .get("mozaik_base_url")
        .ok()
        .flatten()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty());
    #[cfg(any(test, feature = "services-factices"))]
    {
        reglee.or_else(|| {
            crate::services_factices::base_substituee(BASE_FACTICE)
                .map(|b| b.trim_end_matches('/').to_string())
        })
    }
    #[cfg(not(any(test, feature = "services-factices")))]
    {
        Some(reglee.unwrap_or_else(|| RACINE_PAR_DEFAUT.to_string()))
    }
}

impl PartageHttp {
    /// Le partage de ce serveur, ou `None` quand la télémétrie est refusée
    /// (ou, dans un banc, sans racine d'essai) — et alors, aucun appel, ni
    /// lecture ni dépôt.
    pub fn depuis_reglages(backend: &Arc<dyn DbBackend>, http: reqwest::Client) -> Option<Self> {
        let reglages = SettingsRepo::with_backend(backend.clone());
        if !crate::cloud::telemetry::TelemetryReporter::is_enabled_for(&reglages) {
            return None;
        }
        let instance_id = reglages
            .get("instance_id")
            .ok()
            .flatten()
            .filter(|v| !v.trim().is_empty());
        let consentement = crate::cloud::consent::contribution_autorisee(&reglages);
        Some(Self {
            http,
            racine: racine(&reglages)?,
            reglages,
            instance_id,
            consentement,
        })
    }

    /// L'adresse est-elle sous la racine du site ? Seules celles-là sont
    /// téléchargées (comparaison de l'origine, pas d'un préfixe de chaîne :
    /// `https://mozaiklabs.fr.example` n'est pas le site).
    fn sous_la_racine(&self, url: &str) -> bool {
        let (Ok(u), Ok(r)) = (reqwest::Url::parse(url), reqwest::Url::parse(&self.racine)) else {
            return false;
        };
        u.scheme() == r.scheme()
            && u.host_str() == r.host_str()
            && u.port_or_known_default() == r.port_or_known_default()
    }
}

fn source(s: &str) -> Option<SourceDuLogo> {
    Some(match s {
        "commons" => SourceDuLogo::Commons,
        "wikipedia" => SourceDuLogo::Wikipedia,
        "official_site" => SourceDuLogo::SiteOfficiel,
        "discogs" => SourceDuLogo::Discogs,
        "qobuz" => SourceDuLogo::Qobuz,
        _ => return None,
    })
}

fn source_vers_site(s: SourceDuLogo) -> &'static str {
    match s {
        SourceDuLogo::Commons => "commons",
        SourceDuLogo::Wikipedia => "wikipedia",
        SourceDuLogo::SiteOfficiel => "official_site",
        SourceDuLogo::Discogs => "discogs",
        SourceDuLogo::Qobuz => "qobuz",
    }
}

fn statut(s: &str) -> StatutDeLicence {
    match s {
        "free" => StatutDeLicence::Libre,
        "non_free_identification" => StatutDeLicence::NonLibreIdentification,
        _ => StatutDeLicence::Inconnue,
    }
}

fn statut_vers_site(s: StatutDeLicence) -> &'static str {
    match s {
        StatutDeLicence::Libre => "free",
        StatutDeLicence::NonLibreIdentification => "non_free_identification",
        StatutDeLicence::Inconnue => "unknown",
    }
}

fn texte(v: &Value) -> Option<String> {
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Un élément de `logos` du site, ou `None` s'il lui manque l'essentiel.
pub fn logo_du_site(v: &Value) -> Option<LogoPartage> {
    let nom = texte(&v["name"])?;
    let image_url = texte(&v["image_url"])?;
    let source = source(v["source"].as_str()?)?;
    let fournisseur = texte(&v["provider"]).unwrap_or_else(|| String::from("Tune"));
    Some(LogoPartage {
        nom,
        image_url,
        mb_label_id: texte(&v["musicbrainz_label_id"]),
        attribution: Attribution {
            source,
            statut_licence: statut(v["license_status"].as_str().unwrap_or("")),
            fournisseur,
            licence: texte(&v["license"]),
            licence_url: None,
            auteur: None,
            source_url: texte(&v["source_url"]).unwrap_or_default(),
            fichier: None,
            douteux: false,
        },
    })
}

/// Le corps de la lecture groupée.
pub fn corps_de_consultation(labels: &[LabelAConsulter]) -> Value {
    let labels: Vec<Value> = labels
        .iter()
        .map(|l| {
            let mut o = json!({ "name": l.nom });
            if let Some(m) = &l.mb_label_id {
                o["musicbrainz_label_id"] = json!(m);
            }
            if let Some(q) = &l.qobuz_label_id {
                o["qobuz_label_id"] = json!(q);
            }
            o
        })
        .collect();
    json!({ "labels": labels })
}

#[async_trait::async_trait]
impl LogosPartages for PartageHttp {
    async fn consulter(&self, labels: &[LabelAConsulter]) -> Result<Vec<LogoPartage>, String> {
        if labels.is_empty() {
            return Ok(Vec::new());
        }
        let requete = self
            .http
            .post(format!("{}{CHEMIN_CONSULTATION}", self.racine))
            .timeout(DELAI_LECTURE)
            .json(&corps_de_consultation(labels));
        match rate_limit::appeler(&self.reglages, CloudScope::CommunityLabelLogosRead, requete)
            .await
        {
            AppelCloud::Retenu(b) => Err(format!("retenu jusqu'à {}", b.until_epoch)),
            AppelCloud::Erreur(e) => Err(e.to_string()),
            AppelCloud::Reponse(r) if !r.status().is_success() => {
                Err(format!("statut {}", r.status()))
            }
            AppelCloud::Reponse(r) => {
                let corps: Value = r.json().await.map_err(|e| e.to_string())?;
                let logos: Vec<LogoPartage> = corps["logos"]
                    .as_array()
                    .map(|a| a.iter().filter_map(logo_du_site).collect())
                    .unwrap_or_default();
                debug!(
                    demandes = labels.len(),
                    rendus = logos.len(),
                    "logo_label_partage_consulte"
                );
                Ok(logos)
            }
        }
    }

    async fn telecharger(&self, url: &str) -> Result<Vec<u8>, String> {
        if !self.sous_la_racine(url) {
            return Err(format!("adresse hors du site : {url}"));
        }
        let mut r = self
            .http
            .get(url)
            .timeout(DELAI_LECTURE)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !r.status().is_success() {
            return Err(format!("statut {}", r.status()));
        }
        let mut octets = Vec::new();
        while let Some(morceau) = r.chunk().await.map_err(|e| e.to_string())? {
            octets.extend_from_slice(&morceau);
            if octets.len() > TAILLE_MAX_IMAGE {
                return Err(String::from("image trop lourde"));
            }
        }
        Ok(octets)
    }

    fn peut_proposer(&self) -> bool {
        self.consentement && self.instance_id.is_some()
    }

    async fn proposer(&self, p: &PropositionDeLogo) -> ProposeAuPartage {
        let Some(instance_id) = self.instance_id.as_deref().filter(|_| self.consentement) else {
            return ProposeAuPartage::Plus;
        };
        let mime = crate::library::artwork::cache_mime(p.ext);
        let Ok(image) = reqwest::multipart::Part::bytes(p.octets.clone())
            .file_name(format!("logo.{}", p.ext))
            .mime_str(mime)
        else {
            return ProposeAuPartage::Traitee;
        };
        let a = &p.attribution;
        let mut form = reqwest::multipart::Form::new()
            .text("instance_id", instance_id.to_string())
            .text("consent", "1")
            .text("name", p.nom.clone())
            .text("source", source_vers_site(a.source))
            .text("license_status", statut_vers_site(a.statut_licence))
            .text("provider", a.fournisseur.clone());
        for (cle, valeur) in [
            ("musicbrainz_label_id", &p.mb_label_id),
            ("qobuz_label_id", &p.qobuz_label_id),
            ("license", &a.licence),
        ] {
            if let Some(v) = valeur {
                form = form.text(cle, v.clone());
            }
        }
        // Le site n'accepte qu'une page d'origine en https.
        if a.source_url.starts_with("https://") {
            form = form.text("source_url", a.source_url.clone());
        }
        let requete = self
            .http
            .post(format!("{}{CHEMIN_PROPOSITION}", self.racine))
            .timeout(DELAI_DEPOT)
            .multipart(form.part("image", image));
        match rate_limit::appeler(
            &self.reglages,
            CloudScope::CommunityLabelLogosWrite,
            requete,
        )
        .await
        {
            AppelCloud::Reponse(r) if r.status().is_success() => {
                info!(label = %p.nom, "logo_label_propose");
                ProposeAuPartage::Traitee
            }
            // Refusée pour de bon (image, champ) : ne pas la reproposer.
            AppelCloud::Reponse(r) if r.status().as_u16() == 422 => {
                debug!(label = %p.nom, "logo_label_proposition_refusee");
                ProposeAuPartage::Traitee
            }
            AppelCloud::Reponse(r) => {
                warn!(label = %p.nom, status = %r.status(), "logo_label_proposition_differee");
                ProposeAuPartage::Plus
            }
            AppelCloud::Retenu(_) => ProposeAuPartage::Plus,
            AppelCloud::Erreur(e) => {
                debug!(label = %p.nom, erreur = %e, "logo_label_proposition_echec");
                ProposeAuPartage::Plus
            }
        }
    }
}

#[cfg(test)]
#[path = "logos_labels_partages_tests.rs"]
mod tests;
