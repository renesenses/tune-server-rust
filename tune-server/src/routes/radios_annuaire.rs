//! L'annuaire radio-metadata.fr complet dans Tune (#6516).
//!
//! # Ce que fait ce module
//!
//! * **Relève** l'annuaire v2 de mozaiklabs.fr (`/api/v2/radios` : 836 stations
//!   le 19/10/2026, la fiche complète de radio-metadata.fr) et le garde dans un
//!   **cache local**, sur disque. Le relevé suivant envoie `If-None-Match` : un
//!   annuaire inchangé coûte un 304. Hors ligne, le cache est servi tel quel.
//! * **Publie** le relevé dans [`AppState::annuaire_radios`] (recherche, logos)
//!   et dans le registre du relais de titres (`tune_core::radio_metadata_relais`).
//! * **Sert** l'annuaire à l'interface (`GET /radios/directory`), avec
//!   recherche, filtres pays et genre, et pour chaque fiche la station de
//!   « Mes radios » qui lui correspond, s'il y en a une.
//! * **Rapproche** « Mes radios » de l'annuaire (`rattacher_a_l_annuaire`) sans
//!   rien écrire en base : par adresse de flux normalisée — TOUTES les adresses
//!   de la fiche —, puis par nom replié quand il est sans ambiguïté.
//!
//! # « Mes radios » reste la liste personnelle
//!
//! Rien ici ne supprime ni ne réécrit une station locale. Une fiche retirée ou
//! refusée de l'annuaire (FIP Latino, FIP Tout nouveau) disparaît du relevé —
//! [`super::radios::annuaire_depuis_json`] l'écarte — et donc de l'écran
//! « Annuaire » ; la station que l'utilisateur avait mise dans « Mes radios »
//! y reste, simplement sans fiche rattachée.
//!
//! # Aucune donnée personnelle
//!
//! Le relevé est un GET anonyme de l'annuaire public : ni compte, ni profil,
//! ni liste de stations de l'utilisateur ne part vers mozaiklabs.fr.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path as CheminDisque, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tune_core::db::radio_repo::{RadioRepo, RadioStation};
use tune_core::radio_metadata_relais::{StationSuivie, flux_normalise, nom_replie};

use super::radios::{StationAnnuaire, annuaire_depuis_json};
use crate::state::AppState;

/// L'annuaire v2 de mozaiklabs.fr.
pub const ANNUAIRE_V2_URL: &str = "https://mozaiklabs.fr/api/v2/radios";

/// Le relevé de fond se refait toutes les 6 h (#7043, décision de Bertrand du
/// 25/10/2026 ; c'était 24 h).
pub const PERIODE_DE_RELEVE: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// À l'ouverture de l'écran « Annuaire », l'annuaire est relu si la dernière
/// TENTATIVE (réussie ou non) a plus d'une heure : jamais plus d'un relevé par
/// heure, quel que soit le nombre d'ouvertures et même réseau coupé (#7043).
pub const DELAI_A_L_OUVERTURE: std::time::Duration = std::time::Duration::from_secs(3600);

/// Mention de la source, décidée le 13/10 avec Rémi : radio-metadata.fr, et sa
/// page de contribution, qui finance le serveur.
pub const ATTRIBUTION_NOM: &str = "radio-metadata.fr";
pub const ATTRIBUTION_URL: &str = "https://radio-metadata.fr";
pub const ATTRIBUTION_SOUTIEN: &str = "https://ko-fi.com/aleceiffel";

/// Ce que l'on sait du dernier relevé.
#[derive(Debug, Clone, Default, Serialize)]
pub struct EtatAnnuaire {
    /// `reseau`, `cache` ou `aucun`.
    pub origine: &'static str,
    /// Date (RFC 3339) du dernier relevé réussi sur le réseau.
    pub releve_le: Option<String>,
    #[serde(skip)]
    pub etag: Option<String>,
    #[serde(skip)]
    releve_en_cours: std::sync::Arc<AtomicBool>,
    /// Date (secondes Unix) de la dernière TENTATIVE de relevé, réussie ou non ;
    /// `0` : aucune. C'est elle, et non `releve_le`, qui borne la fréquence :
    /// un site injoignable ne doit pas être réinterrogé à chaque ouverture.
    #[serde(skip)]
    derniere_tentative: std::sync::Arc<AtomicI64>,
}

/// Le fichier du cache : le corps v2 tel que reçu, plus son `ETag` et sa date.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheAnnuaire {
    pub etag: Option<String>,
    pub releve_le: Option<String>,
    pub radios: Vec<Value>,
}

/// Ce qu'a donné un relevé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Releve {
    /// 200 : un annuaire neuf, de `n` stations publiées.
    Nouveau(usize),
    /// 304 : l'annuaire n'a pas changé depuis le cache.
    NonModifie,
    /// Réseau en défaut, ou réponse vide/illisible. Le relevé d'avant (cache
    /// ou mémoire) reste servi.
    Injoignable,
}

impl Releve {
    /// Le réseau a-t-il confirmé l'annuaire à cette passe ?
    pub fn a_jour(self) -> bool {
        matches!(self, Self::Nouveau(_) | Self::NonModifie)
    }
}

/// Le chemin du cache : à côté des pochettes, qui sont elles aussi un cache.
pub fn chemin_du_cache() -> PathBuf {
    crate::routes::library::artwork_cache_dir()
        .join("radios")
        .join("annuaire-v2.json")
}

/// Lit le cache local, ou `None` s'il est absent ou illisible.
pub fn lire_le_cache(chemin: &CheminDisque) -> Option<CacheAnnuaire> {
    let octets = std::fs::read(chemin).ok()?;
    serde_json::from_slice(&octets).ok()
}

/// Écrit le cache d'un seul geste : un fichier temporaire, puis un renommage.
/// Une coupure au milieu laisse l'ancien cache, jamais un fichier tronqué.
fn ecrire_le_cache(chemin: &CheminDisque, cache: &CacheAnnuaire) -> std::io::Result<()> {
    if let Some(dossier) = chemin.parent() {
        std::fs::create_dir_all(dossier)?;
    }
    let provisoire = chemin.with_extension("json.tmp");
    std::fs::write(&provisoire, serde_json::to_vec(cache)?)?;
    std::fs::rename(&provisoire, chemin)
}

fn maintenant_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Les fiches d'un corps v2 (`{"radios": […]}`) ou v1 (un tableau).
fn radios_du_corps(corps: Value) -> Vec<Value> {
    match corps {
        Value::Array(a) => a,
        Value::Object(mut o) => match o.remove("radios") {
            Some(Value::Array(a)) => a,
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Publie un relevé : dans l'état du serveur, et dans le registre du relais
/// de titres. Un relevé vide n'écrase jamais le précédent.
pub fn publier(state: &AppState, fiches: Vec<StationAnnuaire>) {
    if fiches.is_empty() {
        return;
    }
    let suivies: Vec<StationSuivie> = fiches
        .iter()
        .filter_map(|f| {
            Some(StationSuivie {
                flux: f.stream_urls.clone(),
                nom: f.name.clone(),
                cle: f.radio_metadata_key.clone()?,
                external_id: f.external_id.clone(),
            })
        })
        .collect();
    tune_core::radio_metadata_relais::publier_les_cles(&suivies);
    tracing::info!(
        stations = fiches.len(),
        suivies = suivies.len(),
        "radio_annuaire_releve"
    );
    if let Ok(mut a) = state.annuaire_radios.write() {
        *a = fiches;
    }
}

/// Charge le cache local dans l'état, si l'état est encore vide.
pub fn charger_le_cache_si_vide(state: &AppState, chemin: &CheminDisque) -> bool {
    let vide = state
        .annuaire_radios
        .read()
        .map(|a| a.is_empty())
        .unwrap_or(true);
    if !vide {
        return false;
    }
    let Some(cache) = lire_le_cache(chemin) else {
        return false;
    };
    let fiches = annuaire_depuis_json(&cache.radios);
    if fiches.is_empty() {
        return false;
    }
    publier(state, fiches);
    if let Ok(mut e) = state.annuaire_etat.write() {
        e.origine = "cache";
        e.releve_le = cache.releve_le;
        e.etag = cache.etag;
    }
    true
}

/// Relève l'annuaire à `url` et met à jour le cache `chemin`.
pub async fn rafraichir_depuis(state: &AppState, url: &str, chemin: &CheminDisque) -> Releve {
    // Le cache d'abord : un serveur qui démarre hors ligne sert l'annuaire de
    // la veille au lieu de rien.
    charger_le_cache_si_vide(state, chemin);
    noter_la_tentative(state);

    let etag = state.annuaire_etat.read().ok().and_then(|e| e.etag.clone());
    let mut requete = tune_core::http::client::shared()
        .get(url)
        .header("Accept", "application/json")
        .timeout(std::time::Duration::from_secs(20));
    if let Some(etag) = etag.as_deref() {
        requete = requete.header("If-None-Match", etag);
    }
    let Ok(reponse) = requete.send().await else {
        tracing::info!("radio_annuaire_injoignable");
        return Releve::Injoignable;
    };

    if reponse.status() == reqwest::StatusCode::NOT_MODIFIED {
        let releve_le = maintenant_rfc3339();
        if let Some(mut cache) = lire_le_cache(chemin) {
            cache.releve_le = Some(releve_le.clone());
            if let Err(e) = ecrire_le_cache(chemin, &cache) {
                tracing::warn!(erreur = %e, "radio_annuaire_cache_non_ecrit");
            }
        }
        if let Ok(mut e) = state.annuaire_etat.write() {
            e.origine = "reseau";
            e.releve_le = Some(releve_le);
        }
        tracing::info!("radio_annuaire_non_modifie");
        return Releve::NonModifie;
    }
    if !reponse.status().is_success() {
        tracing::info!(status = %reponse.status(), "radio_annuaire_refus");
        return Releve::Injoignable;
    }
    let etag_recu = reponse
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let Ok(corps) = reponse.json::<Value>().await else {
        return Releve::Injoignable;
    };
    let radios = radios_du_corps(corps);
    let fiches = annuaire_depuis_json(&radios);
    if fiches.is_empty() {
        // Un 200 vide n'apprend rien : le relevé d'avant reste servi.
        return Releve::Injoignable;
    }
    let n = fiches.len();
    let releve_le = maintenant_rfc3339();
    let cache = CacheAnnuaire {
        etag: etag_recu.clone(),
        releve_le: Some(releve_le.clone()),
        radios,
    };
    if let Err(e) = ecrire_le_cache(chemin, &cache) {
        tracing::warn!(erreur = %e, "radio_annuaire_cache_non_ecrit");
    }
    publier(state, fiches);
    if let Ok(mut e) = state.annuaire_etat.write() {
        e.origine = "reseau";
        e.releve_le = Some(releve_le);
        e.etag = etag_recu;
    }
    Releve::Nouveau(n)
}

/// Le relevé du serveur : la vraie adresse, le vrai cache.
pub async fn rafraichir(state: &AppState) -> Releve {
    rafraichir_depuis(state, ANNUAIRE_V2_URL, &chemin_du_cache()).await
}

fn maintenant_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Note qu'un relevé est tenté maintenant.
fn noter_la_tentative(state: &AppState) {
    if let Ok(e) = state.annuaire_etat.read() {
        e.derniere_tentative
            .store(maintenant_unix(), Ordering::SeqCst);
    }
}

/// Une nouvelle tentative est-elle permise ? Oui s'il n'y en a jamais eu, ou si
/// la dernière date d'au moins `delai`. Pure : se teste sans horloge.
pub fn tentative_permise(derniere: i64, maintenant: i64, delai: std::time::Duration) -> bool {
    derniere <= 0 || maintenant.saturating_sub(derniere) >= delai.as_secs() as i64
}

/// Relance un relevé en tâche de fond si la dernière tentative date de plus de
/// `delai`, sans jamais en lancer deux à la fois. L'écran n'attend pas : il
/// reçoit le relevé d'avant. Rend la tâche lancée, ou `None` si rien n'est dû.
///
/// Une seule passe, bornée : un relevé de l'annuaire (un GET conditionnel, 304
/// quand rien n'a changé) puis le rapprochement local de « Mes radios ». Aucune
/// boucle, aucune relance : la prochaine tentative attend `delai`.
pub fn relever_a_l_ouverture(
    state: &AppState,
    url: &str,
    chemin: &CheminDisque,
    delai: std::time::Duration,
) -> Option<tokio::task::JoinHandle<()>> {
    let (derniere, verrou) = match state.annuaire_etat.read() {
        Ok(e) => (e.derniere_tentative.clone(), e.releve_en_cours.clone()),
        Err(_) => return None,
    };
    if !tentative_permise(derniere.load(Ordering::SeqCst), maintenant_unix(), delai)
        || verrou.swap(true, Ordering::SeqCst)
    {
        return None;
    }
    // La place est réservée AVANT de partir : deux ouvertures successives ne
    // peuvent pas toutes deux passer entre le lancement et la première requête.
    derniere.store(maintenant_unix(), Ordering::SeqCst);
    let state = state.clone();
    let (url, chemin) = (url.to_string(), chemin.to_path_buf());
    Some(tokio::spawn(async move {
        let _ = super::radios::refresh_radio_logos_depuis(&state, &url, &chemin).await;
        verrou.store(false, Ordering::SeqCst);
    }))
}

// ---------------------------------------------------------------------------
// Rapprochement « Mes radios » ↔ annuaire
// ---------------------------------------------------------------------------

/// Comment une station de « Mes radios » a retrouvé sa fiche.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lien {
    /// Par `radio_stations.external_id` (SQLite 126 / PG 090) : le lien
    /// enregistré, qui survit à un changement d'adresse du flux.
    Id,
    /// Par l'adresse du flux, à la normalisation près.
    Flux,
    /// Par le nom replié, porté par une seule fiche.
    Nom,
}

impl Lien {
    fn as_str(self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::Flux => "flux",
            Self::Nom => "nom",
        }
    }
}

/// L'index de rapprochement d'un relevé.
pub struct IndexAnnuaire<'a> {
    fiches: &'a [StationAnnuaire],
    par_id: HashMap<String, usize>,
    par_flux: HashMap<String, usize>,
    /// `None` : nom porté par plusieurs fiches, il ne rapproche rien.
    par_nom: HashMap<String, Option<usize>>,
}

impl<'a> IndexAnnuaire<'a> {
    pub fn new(fiches: &'a [StationAnnuaire]) -> Self {
        let mut par_id = HashMap::new();
        let mut par_flux = HashMap::new();
        let mut par_nom: HashMap<String, Option<usize>> = HashMap::new();
        for (i, f) in fiches.iter().enumerate() {
            if let Some(id) = f.external_id.as_deref() {
                par_id.entry(id.to_string()).or_insert(i);
            }
            for u in std::iter::once(&f.stream_url).chain(f.stream_urls.iter()) {
                par_flux.entry(flux_normalise(u)).or_insert(i);
            }
            let nom = nom_replie(&f.name);
            if nom.is_empty() {
                continue;
            }
            par_nom
                .entry(nom)
                .and_modify(|v| {
                    if *v != Some(i) {
                        *v = None;
                    }
                })
                .or_insert(Some(i));
        }
        Self {
            fiches,
            par_id,
            par_flux,
            par_nom,
        }
    }

    pub fn fiche(&self, i: usize) -> Option<&'a StationAnnuaire> {
        self.fiches.get(i)
    }

    /// La fiche d'une station locale : par son lien enregistré d'abord, puis
    /// par son adresse de flux, puis par son nom quand une seule fiche le
    /// porte. Un lien vers une fiche sortie de l'annuaire ne rapproche rien —
    /// et n'autorise pas non plus à chercher ailleurs : la station a déjà sa
    /// fiche, simplement retirée.
    pub fn lien(&self, external_id: Option<&str>, nom: &str, url: &str) -> Option<(usize, Lien)> {
        if let Some(id) = external_id.map(str::trim).filter(|s| !s.is_empty()) {
            return self.par_id.get(id).map(|&i| (i, Lien::Id));
        }
        if let Some(&i) = self.par_flux.get(&flux_normalise(url)) {
            return Some((i, Lien::Flux));
        }
        self.par_nom
            .get(&nom_replie(nom))
            .copied()
            .flatten()
            .map(|i| (i, Lien::Nom))
    }

    /// La fiche d'une station locale sans lien enregistré : par son adresse de
    /// flux, sinon par son nom quand une seule fiche le porte.
    pub fn fiche_de(&self, nom: &str, url: &str) -> Option<&'a StationAnnuaire> {
        self.fiches.get(self.position_de(nom, url)?)
    }

    /// La fiche dont une adresse de flux est celle-ci, et elle seule.
    pub fn position_par_flux(&self, url: &str) -> Option<usize> {
        self.par_flux.get(&flux_normalise(url)).copied()
    }

    /// La position de cette fiche dans le relevé (sans lien enregistré).
    pub fn position_de(&self, nom: &str, url: &str) -> Option<usize> {
        self.lien(None, nom, url).map(|(i, _)| i)
    }
}

/// L'adresse `url` est-elle l'une des adresses de flux de la fiche ?
pub fn flux_de_la_fiche(f: &StationAnnuaire, url: &str) -> bool {
    let n = flux_normalise(url);
    std::iter::once(&f.stream_url)
        .chain(f.stream_urls.iter())
        .any(|u| flux_normalise(u) == n)
}

/// Ce qu'une station de « Mes radios » gagne à sa fiche.
///
/// Le LOGO n'est prêté que si la fiche a été trouvée par son lien enregistré
/// ou par l'adresse du flux : rapprochée par son seul nom, une station
/// n'affiche jamais le logo d'une autre (fil du 19/10, « BBC Radio 3 » au logo
/// de Berliner Rundfunk). Le nom suffit, lui, à éviter un doublon à l'ajout.
///
/// `flux_change` : la station est liée à sa fiche mais son adresse n'est plus
/// celle de la fiche. L'écran PROPOSE alors « Mettre à jour l'adresse du
/// flux » (`stream_url_annuaire`) ; rien ne change tout seul.
fn rattachement(f: &StationAnnuaire, lien: Lien, url_locale: &str) -> Value {
    let logo = matches!(lien, Lien::Id | Lien::Flux);
    let flux_change = lien != Lien::Flux && !flux_de_la_fiche(f, url_locale);
    json!({
        "external_id": f.external_id,
        "radio_metadata_key": f.radio_metadata_key,
        "logo_url": if logo { f.logo_url.clone() } else { None },
        "description": f.description,
        "website_url": f.website_url,
        "rapproche_par": lien.as_str(),
        "flux_change": flux_change,
        "stream_url_annuaire": if flux_change { Some(f.stream_url.clone()) } else { None },
        // #7177 (web#2443) — de quoi remplir la fiche « Voir » : catégories,
        // et les flux publiés parmi lesquels choisir son flux préféré.
        "categories": f.categories,
        "streams": f.flux_proposes(),
    })
}

/// Joint `annuaire` à chaque station de `corps` (un tableau) rapprochée d'une
/// fiche. Une station sans fiche reste telle quelle.
pub fn rattacher_a_l_annuaire(state: &AppState, corps: &mut Value) {
    let Ok(fiches) = state.annuaire_radios.read() else {
        return;
    };
    if fiches.is_empty() {
        return;
    }
    let liens = RadioRepo::with_backend(state.backend.clone())
        .liens_annuaire()
        .unwrap_or_default();
    let index = IndexAnnuaire::new(&fiches);
    let Some(stations) = corps.as_array_mut() else {
        return;
    };
    for st in stations {
        let (Some(nom), Some(url)) = (
            st.get("name").and_then(Value::as_str),
            st.get("stream_url").and_then(Value::as_str),
        ) else {
            continue;
        };
        let ext = st
            .get("id")
            .and_then(Value::as_i64)
            .and_then(|id| liens.get(&id))
            .map(String::as_str);
        if let Some((i, lien)) = index.lien(ext, nom, url)
            && let Some(f) = index.fiche(i)
        {
            let r = rattachement(f, lien, url);
            if let Some(o) = st.as_object_mut() {
                o.insert("annuaire".into(), r);
            }
        }
    }
}

/// Le rapprochement de départ : chaque station SANS lien dont l'adresse de
/// flux est, à la normalisation près, celle d'une fiche reçoit son
/// `external_id`. Idempotent : un lien déjà posé n'est jamais réécrit.
/// Rend le nombre de liens posés.
///
/// ⛔ Jamais par le NOM seul (revue de #6538). `refresh_radio_logos` lie AVANT
/// de choisir les logos, et `logo_a_poser` fait confiance au lien : un lien
/// posé par le nom prêtait donc le logo d'une homonyme d'un autre flux, ce que
/// la règle « pas de logo par le seul nom » interdit (BBC Radio 3 / Berliner
/// Rundfunk). Le nom reste un rapprochement d'AFFICHAGE (`rapproche_par:
/// "nom"`, sans logo) ; seul le geste « Ajouter » depuis l'annuaire, choisi par
/// l'utilisateur, enregistre un lien sans adresse commune.
pub fn lier_les_stations(repo: &RadioRepo, fiches: &[StationAnnuaire]) -> usize {
    let liens = repo.liens_annuaire().unwrap_or_default();
    let index = IndexAnnuaire::new(fiches);
    let mut poses = 0;
    for st in repo.list().unwrap_or_default() {
        let Some(id) = st.id else { continue };
        if liens.contains_key(&id) {
            continue;
        }
        if let Some((i, Lien::Flux)) = index.lien(None, &st.name, &st.url)
            && let Some(ext) = index.fiche(i).and_then(|f| f.external_id.as_deref())
            && repo.lier_a_l_annuaire(id, Some(ext)).is_ok()
        {
            poses += 1;
        }
    }
    if poses > 0 {
        tracing::info!(poses, "radio_annuaire_liens_poses");
    }
    poses
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/directory", get(lister))
        .route("/directory/refresh", post(forcer_le_releve))
        .route("/directory/{external_id}/add", post(ajouter))
        .route("/directory/{external_id}/play/{zone_id}", post(jouer))
        .route(
            "/directory/update-stream/{radio_id}",
            post(mettre_a_jour_le_flux),
        )
        .route(
            "/directory/preferred-stream/{radio_id}",
            post(choisir_le_flux),
        )
}

#[derive(Debug, Deserialize, Default)]
pub struct FiltresAnnuaire {
    pub q: Option<String>,
    pub country: Option<String>,
    /// #7466 — un genre, ou plusieurs séparés par des virgules (en OU).
    pub genre: Option<String>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
    /// #7466 — les critères de [`super::radios_criteres_7466::Criteres`].
    pub codec: Option<String>,
    pub bitrate_min: Option<i32>,
    pub lossless: Option<bool>,
    pub hires: Option<bool>,
    pub favorites_only: Option<bool>,
    pub rated_min: Option<f64>,
    pub metadata_plus: Option<bool>,
}

impl FiltresAnnuaire {
    pub fn criteres(&self) -> super::radios_criteres_7466::Criteres {
        super::radios_criteres_7466::Criteres::depuis(
            self.genre.as_deref(),
            self.country.as_deref(),
            self.codec.as_deref(),
            self.bitrate_min,
            self.lossless,
            self.hires,
            self.favorites_only,
            self.rated_min,
            self.metadata_plus,
        )
    }
}

const LIMITE_PAR_DEFAUT: usize = 120;
const LIMITE_MAX: usize = 1000;

use super::radios_criteres_7466::Criteres;

fn repli(s: &str) -> String {
    tune_core::db::engine::fold_diacritics(s).to_lowercase()
}

/// La fiche répond-elle à la recherche ? Nom, genre, catégories, pays,
/// description et mots-clés (le diffuseur, #6964), sans accents ni casse.
/// Tous les mots doivent figurer dans un même champ, dans n'importe quel ordre.
fn correspond(f: &StationAnnuaire, motif: &str) -> bool {
    if motif.is_empty() {
        return true;
    }
    std::iter::once(f.name.as_str())
        .chain(f.genre.as_deref())
        .chain(f.genres.iter().map(String::as_str))
        .chain(f.country.as_deref())
        .chain(f.description.as_deref())
        .chain(f.categories.iter().map(String::as_str))
        .chain(f.keywords.iter().map(String::as_str))
        .any(|champ| {
            let champ = repli(champ);
            motif.split_whitespace().all(|mot| champ.contains(mot))
        })
}

fn meme_pays(f: &StationAnnuaire, criteres: &Criteres) -> bool {
    criteres.pays_ok(f.country.as_deref())
}

/// #7215 — vrai si le genre demandé est L'UN des genres de la fiche
/// (`genres`, sinon `genre`) : « OuiFM Blues & Rock » sort sous blues ET rock.
/// #7466 — plusieurs genres demandés : l'un d'eux suffit.
fn meme_genre(f: &StationAnnuaire, criteres: &Criteres) -> bool {
    criteres.genre_ok(&f.genres_ou_genre())
}

/// Le corps de `GET /radios/directory`, calculé hors de toute requête réseau.
pub fn corps_annuaire(
    fiches: &[StationAnnuaire],
    mes_radios: &[RadioStation],
    filtres: &FiltresAnnuaire,
    etat: &EtatAnnuaire,
) -> Value {
    corps_annuaire_avec_liens(fiches, mes_radios, &HashMap::new(), filtres, etat)
}

/// [`corps_annuaire`], les liens `external_id` enregistrés compris : une
/// station liée retrouve sa fiche même quand l'adresse du flux a changé.
pub fn corps_annuaire_avec_liens(
    fiches: &[StationAnnuaire],
    mes_radios: &[RadioStation],
    liens: &HashMap<i64, String>,
    filtres: &FiltresAnnuaire,
    etat: &EtatAnnuaire,
) -> Value {
    let motif = repli(filtres.q.as_deref().unwrap_or("").trim());
    let criteres = filtres.criteres();

    // L'inverse du rapprochement : pour chaque fiche, la station locale.
    let index = IndexAnnuaire::new(fiches);
    let mut locales: HashMap<usize, &RadioStation> = HashMap::new();
    for st in mes_radios {
        let ext = st.id.and_then(|id| liens.get(&id)).map(String::as_str);
        if let Some((i, _)) = index.lien(ext, &st.name, &st.url) {
            locales.entry(i).or_insert(st);
        }
    }

    // #7466 — les critères sans facette (format, débit, favoris…) bornent la
    // recherche ELLE-MÊME : les facettes de pays et de genre se comptent
    // dessus, et le compteur dit ce que l'écran montrera.
    let qui_cherchent: Vec<(usize, &StationAnnuaire)> = fiches
        .iter()
        .enumerate()
        .filter(|(_, f)| correspond(f, &motif))
        .filter(|(i, f)| criteres.autres_ok_pour_la_fiche(f, locales.get(i).map(|s| s.is_favorite)))
        .collect();
    // Les facettes se comptent sur la recherche ET l'autre filtre : choisir un
    // pays montre les genres de ce pays, et réciproquement.
    let mut pays_comptes: BTreeMap<String, usize> = BTreeMap::new();
    let mut genres_comptes: BTreeMap<String, usize> = BTreeMap::new();
    for (_, f) in &qui_cherchent {
        if meme_genre(f, &criteres)
            && let Some(c) = f.country.as_deref()
        {
            *pays_comptes.entry(c.to_ascii_uppercase()).or_default() += 1;
        }
        // #7215 — une fiche compte dans CHACUN de ses genres (une fois chacun).
        if meme_pays(f, &criteres) {
            let mut vus: Vec<String> = Vec::new();
            for g in f.genres_ou_genre() {
                let cle = g.to_ascii_lowercase();
                if !vus.contains(&cle) {
                    *genres_comptes.entry(cle.clone()).or_default() += 1;
                    vus.push(cle);
                }
            }
        }
    }
    let retenues: Vec<(usize, &StationAnnuaire)> = qui_cherchent
        .into_iter()
        .filter(|(_, f)| meme_pays(f, &criteres) && meme_genre(f, &criteres))
        .collect();

    let total = retenues.len();
    let offset = filtres.offset.unwrap_or(0).min(total);
    let limit = filtres
        .limit
        .unwrap_or(LIMITE_PAR_DEFAUT)
        .clamp(1, LIMITE_MAX);
    let items: Vec<Value> = retenues
        .iter()
        .skip(offset)
        .take(limit)
        .map(|(i, f)| {
            let locale = locales.get(i);
            json!({
                "external_id": f.external_id,
                "name": f.name,
                "description": f.description,
                "logo_url": f.logo_url,
                "country_code": f.country.as_deref().map(str::to_ascii_uppercase),
                "genre": f.genre,
                // #7215 — tous les genres de la fiche (repli : `[genre]`).
                "genres": f.genres_ou_genre(),
                "categories": f.categories,
                // #7194 (web#2400) — les mots-clés de la fiche : le diffuseur
                // (NPR, BBC, Radio France…), dont la carte fait ses réseaux.
                "keywords": f.keywords,
                "quality": f.quality,
                "codec": f.codec,
                "bitrate": f.bitrate,
                "website_url": f.website_url,
                "stream_url": f.stream_url,
                // #7177 (web#2443) — tous les flux publiés de la fiche.
                "streams": f.flux_proposes(),
                "radio_metadata_key": f.radio_metadata_key,
                "now_playing": f.radio_metadata_key.is_some(),
                "my_radio_id": locale.and_then(|s| s.id),
                "my_radio_favorite": locale.map(|s| s.is_favorite),
                // #7466 — la note radio-metadata.fr, quand la fiche en a une.
                "avg_rating": f.note_centiemes.map(|n| f64::from(n) / 100.0),
                "rating_count": f.nombre_de_notes,
            })
        })
        .collect();

    let par_compte = |m: BTreeMap<String, usize>, cle: &str| -> Vec<Value> {
        let mut v: Vec<(String, usize)> = m.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.into_iter()
            .map(|(k, n)| json!({ cle: k, "count": n }))
            .collect()
    };
    json!({
        "total": total,
        "offset": offset,
        "limit": limit,
        "count": items.len(),
        "items": items,
        "countries": par_compte(pays_comptes, "code"),
        "genres": par_compte(genres_comptes, "key"),
        "stations": fiches.len(),
        "origine": if fiches.is_empty() { "aucun" } else { etat.origine },
        "releve_le": etat.releve_le,
        "attribution": {
            "name": ATTRIBUTION_NOM,
            "url": ATTRIBUTION_URL,
            "support_url": ATTRIBUTION_SOUTIEN,
        },
    })
}

async fn lister(
    State(state): State<AppState>,
    Query(filtres): Query<FiltresAnnuaire>,
) -> Json<Value> {
    // Premier affichage d'un serveur qui n'a pas encore relevé : le cache.
    charger_le_cache_si_vide(&state, &chemin_du_cache());
    relever_a_l_ouverture(
        &state,
        ANNUAIRE_V2_URL,
        &chemin_du_cache(),
        DELAI_A_L_OUVERTURE,
    );
    let fiches = state
        .annuaire_radios
        .read()
        .map(|a| a.clone())
        .unwrap_or_default();
    let etat = state
        .annuaire_etat
        .read()
        .map(|e| e.clone())
        .unwrap_or_default();
    let repo = RadioRepo::with_backend(state.backend.clone());
    let mes_radios = repo.list().unwrap_or_default();
    let liens = repo.liens_annuaire().unwrap_or_default();
    Json(corps_annuaire_avec_liens(
        &fiches,
        &mes_radios,
        &liens,
        &filtres,
        &etat,
    ))
}

async fn forcer_le_releve(State(state): State<AppState>) -> Json<Value> {
    let bilan = super::radios::refresh_radio_logos(&state).await;
    let etat = state
        .annuaire_etat
        .read()
        .map(|e| e.clone())
        .unwrap_or_default();
    let stations = state.annuaire_radios.read().map(|a| a.len()).unwrap_or(0);
    Json(json!({
        "stations": stations,
        "a_jour": bilan.annuaire_a_jour,
        "origine": if stations == 0 { "aucun" } else { etat.origine },
        "releve_le": etat.releve_le,
    }))
}

fn fiche_par_id(state: &AppState, external_id: &str) -> Option<StationAnnuaire> {
    state
        .annuaire_radios
        .read()
        .ok()?
        .iter()
        .find(|f| f.external_id.as_deref() == Some(external_id))
        .cloned()
}

/// La station de « Mes radios » que l'annuaire créerait pour une fiche.
pub fn station_depuis_fiche(f: &StationAnnuaire) -> RadioStation {
    RadioStation {
        id: None,
        name: f.name.clone(),
        url: f.stream_url.clone(),
        homepage: f.website_url.clone(),
        logo_url: f.logo_url.clone(),
        country: f.country.clone(),
        language: None,
        genre: f.genre.clone(),
        codec: f.codec.as_deref().map(str::to_ascii_lowercase),
        bitrate: f.bitrate,
        is_favorite: false,
        last_played: None,
        play_count: 0,
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct CorpsAjout {
    /// Ajouter ET mettre en favori (le cœur de l'écran).
    #[serde(default)]
    pub favorite: bool,
}

/// `POST /radios/directory/{external_id}/add` — met la fiche dans « Mes
/// radios », sans doublon : une station déjà rapprochée est rendue telle
/// quelle (200), seule une station neuve est créée (201).
async fn ajouter(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(external_id): Path<String>,
    corps: Option<Json<CorpsAjout>>,
) -> Response {
    let favori = corps.map(|Json(c)| c.favorite).unwrap_or(false);
    let Some(fiche) = fiche_par_id(&state, &external_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "radio_annuaire_inconnue"})),
        )
            .into_response();
    };
    let repo = RadioRepo::with_backend(state.backend.clone());
    let mes_radios = match repo.list() {
        Ok(l) => l,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    // La station déjà dans « Mes radios » : par son lien enregistré d'abord,
    // sinon — parmi les stations SANS lien — par l'adresse ou le nom. Une
    // station liée à une AUTRE fiche n'est jamais prise pour celle-ci.
    let liens = repo.liens_annuaire().unwrap_or_default();
    let unique = [fiche.clone()];
    let index = IndexAnnuaire::new(&unique);
    let existante = mes_radios
        .iter()
        .find(|st| st.id.and_then(|id| liens.get(&id)) == Some(&external_id))
        .or_else(|| {
            mes_radios.iter().find(|st| {
                st.id.is_some_and(|id| !liens.contains_key(&id))
                    && index.fiche_de(&st.name, &st.url).is_some()
            })
        })
        .cloned();

    let (id, statut) = match existante {
        Some(st) => (st.id, StatusCode::OK),
        None => {
            let mut station = station_depuis_fiche(&fiche);
            // Une fiche peut rester en favori même si aucun flux n'est
            // accessible aujourd'hui ; c'est sa lecture qui sera refusée.
            if let Ok(flux) = super::radios::geofencing::stream_for_directory(&state, &fiche, None)
            {
                station.url = flux.url;
                station.codec = flux.codec.map(|c| c.to_ascii_lowercase());
                station.bitrate = flux.bitrate;
            }
            match repo.create(&station) {
                Ok(id) => {
                    state.event_bus.emit(
                        "library.radios_changed",
                        json!({"action": "created", "id": id, "external_id": external_id}),
                    );
                    (Some(id), StatusCode::CREATED)
                }
                Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
            }
        }
    };
    let Some(id) = id else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    // Le lien est enregistré dès la création, ou au premier ajout d'une
    // station retrouvée par l'adresse ou le nom.
    if !liens.contains_key(&id)
        && let Err(e) = repo.lier_a_l_annuaire(id, Some(&external_id))
    {
        tracing::warn!(erreur = %e, id, "radio_annuaire_lien_non_pose");
    }
    if favori && repo.set_favorite(id, true).is_ok() {
        state.event_bus.emit(
            "library.radios_changed",
            json!({"action": "favorite", "id": id}),
        );
    }
    let Some(station) = repo.get(id).ok().flatten() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let mut sortie = json!(station);
    super::radios_libelles::enrichir(&mut sortie, &crate::i18n::lang_from_header(&headers));
    if let Some(o) = sortie.as_object_mut() {
        o.insert(
            "annuaire".into(),
            rattachement(&fiche, Lien::Id, &station.url),
        );
    }
    (statut, Json(sortie)).into_response()
}

/// `POST /radios/directory/update-stream/{radio_id}` — « Mettre à jour
/// l'adresse du flux » : si son ancien flux a été retiré, la station liée
/// prend l'adresse principale et ses métadonnées. Un flux secondaire encore
/// publié reste choisi, comme lors du rattrapage de fond (#7043).
async fn mettre_a_jour_le_flux(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(radio_id): Path<i64>,
) -> Response {
    let repo = RadioRepo::with_backend(state.backend.clone());
    let Some(mut station) = repo.get(radio_id).ok().flatten() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "radio_inconnue"})),
        )
            .into_response();
    };
    let lien = repo
        .liens_annuaire()
        .unwrap_or_default()
        .get(&radio_id)
        .cloned();
    let Some(fiche) = lien.as_deref().and_then(|ext| fiche_par_id(&state, ext)) else {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "radio_sans_fiche_liee"})),
        )
            .into_response();
    };
    if !flux_de_la_fiche(&fiche, &station.url) {
        // #6973 : les métadonnées de l'ancienne adresse ne décrivent pas
        // le nouveau flux. Un format inconnu reste inconnu.
        station.codec = None;
        station.bitrate = None;
        poser_le_flux_prefere(&fiche, &mut station, &fiche.stream_url);
        if let Err(e) = repo.update(&station) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
        state.event_bus.emit(
            "library.radios_changed",
            json!({"action": "updated", "id": radio_id}),
        );
    }
    let mut sortie = json!(station);
    super::radios_libelles::enrichir(&mut sortie, &crate::i18n::lang_from_header(&headers));
    if let Some(o) = sortie.as_object_mut() {
        o.insert(
            "annuaire".into(),
            rattachement(&fiche, Lien::Id, &station.url),
        );
    }
    Json(sortie).into_response()
}

#[derive(Debug, Deserialize)]
pub struct CorpsFluxPrefere {
    pub stream_url: String,
}

/// #7177 (web#2443) — pose `url` comme flux de `station`, s'il est l'un des
/// flux publiés de `fiche`, avec son format et son débit. `false` : ce flux
/// n'est pas de la fiche, rien ne change.
///
/// Le flux préféré EST l'adresse de la station : aucune colonne de plus. Le
/// rattrapage de fond (#7043, [`super::radios::flux_a_poser`]) garde une
/// adresse tant que la fiche la publie, même secondaire ; il ne revient au
/// flux principal que si la fiche retire celle-ci.
pub fn poser_le_flux_prefere(
    fiche: &StationAnnuaire,
    station: &mut RadioStation,
    url: &str,
) -> bool {
    let voulu = flux_normalise(url);
    let Some(flux) = fiche
        .flux_proposes()
        .into_iter()
        .find(|f| flux_normalise(&f.url) == voulu)
    else {
        return false;
    };
    station.url = flux.url;
    if let Some(c) = flux.codec.as_deref() {
        station.codec = Some(c.to_ascii_lowercase());
    }
    if let Some(b) = flux.bitrate {
        station.bitrate = Some(b);
    }
    true
}

/// `POST /radios/directory/preferred-stream/{radio_id}` `{"stream_url": …}`
/// — la station de « Mes radios » liée à sa fiche écoute désormais ce flux,
/// choisi parmi ceux que la fiche publie (#7177, web#2443). Une adresse qui
/// n'est pas de la fiche est refusée (422) : pour une autre adresse, il y a la
/// saisie manuelle.
async fn choisir_le_flux(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(radio_id): Path<i64>,
    Json(corps): Json<CorpsFluxPrefere>,
) -> Response {
    let repo = RadioRepo::with_backend(state.backend.clone());
    let Some(mut station) = repo.get(radio_id).ok().flatten() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "radio_inconnue"})),
        )
            .into_response();
    };
    let lien = repo
        .liens_annuaire()
        .unwrap_or_default()
        .get(&radio_id)
        .cloned();
    let Some(fiche) = lien.as_deref().and_then(|ext| fiche_par_id(&state, ext)) else {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "radio_sans_fiche_liee"})),
        )
            .into_response();
    };
    let avant = (station.url.clone(), station.codec.clone(), station.bitrate);
    if !poser_le_flux_prefere(&fiche, &mut station, &corps.stream_url) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": "flux_hors_fiche"})),
        )
            .into_response();
    }
    if (station.url.clone(), station.codec.clone(), station.bitrate) != avant {
        if let Err(e) = repo.update(&station) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
        state.event_bus.emit(
            "library.radios_changed",
            json!({"action": "updated", "id": radio_id}),
        );
    }
    let mut sortie = json!(station);
    super::radios_libelles::enrichir(&mut sortie, &crate::i18n::lang_from_header(&headers));
    if let Some(o) = sortie.as_object_mut() {
        o.insert(
            "annuaire".into(),
            rattachement(&fiche, Lien::Id, &station.url),
        );
    }
    Json(sortie).into_response()
}

/// `POST /radios/directory/{external_id}/play/{zone_id}` — l'écoute en un clic
/// d'une fiche qui n'est pas dans « Mes radios ». Rien n'est ajouté.
async fn jouer(
    State(state): State<AppState>,
    Path((external_id, zone_id)): Path<(String, i64)>,
) -> Response {
    let Some(fiche) = fiche_par_id(&state, &external_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "radio_annuaire_inconnue"})),
        )
            .into_response();
    };
    let flux = match super::radios::geofencing::stream_for_directory(&state, &fiche, None) {
        Ok(flux) => flux,
        Err(response) => return response.into_response(),
    };
    Json(
        super::radios::jouer_un_flux(
            &state,
            zone_id,
            &fiche.name,
            &flux.url,
            fiche.logo_url.clone(),
        )
        .await,
    )
    .into_response()
}

#[cfg(test)]
#[path = "radios_annuaire_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "radios_annuaire_recherche_7198_tests.rs"]
mod tests_recherche_7198;

#[cfg(test)]
#[path = "radios_format_mis_a_jour_6973_tests.rs"]
mod format_mis_a_jour_6973;
