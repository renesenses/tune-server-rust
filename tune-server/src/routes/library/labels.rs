//! `GET /library/labels` (#6736) — les labels de la bibliothèque, avec leur
//! logo Qobuz quand il est connu.
//!
//! La route n'attend jamais Qobuz : elle rend ce que l'index des logos sait
//! déjà ([`tune_core::library::logos_labels`]) et, si un compte Qobuz est
//! connecté, lance en arrière-plan une passe bornée sur les labels encore
//! inconnus. Le logo apparaît donc à une ouverture suivante de l'onglet.
//! Sans compte Qobuz (service absent, désactivé ou non connecté) : aucune
//! passe Qobuz, aucun appel Qobuz.
//!
//! #7089 — les logos du web ouvert, sans compte
//! ([`tune_core::library::logos_web`]) : après un rapprochement MusicBrainz
//! sûr, Commons (libre), puis Wikipédia, puis le site officiel, puis
//! Discogs. Chaque serveur les récupère lui-même, pour son usage local
//! d'identification. Ils ont leur propre index et leur propre passe, bornée
//! de même, qui ne tourne pas quand la tâche de fond « Enrichissement » est en
//! pause. Ordre de préférence : ces sources, puis Qobuz. Chaque logo est rendu
//! avec sa source et sa licence connue (`image_credit`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};
use tokio::sync::RwLock;
use tune_core::db::album_repo::{AlbumRepo, LabelDeBibliotheque};
use tune_core::library::artistes_du_label::{
    ArtisteADecouvrir, CacheDecouverte, CatalogueDuLabel, artistes_de_la_bibliotheque,
    cles_de_la_bibliotheque, regrouper,
};
use tune_core::library::logos_labels::{
    Attribution, CacheLogosLabels, CatalogueDeLabels, Demande, LabelAConsulter, LabelDuCatalogue,
    LogosPartages, PASSE_MAX, PAUSE_ENTRE_LABELS, PROPOSITIONS_PAR_PASSE, ProposeAuPartage,
    PropositionDeLogo, SourceDuLogo, Statut, StatutDeLicence, cle_de_label,
};
use tune_core::library::logos_web::{CatalogueLogosWeb, FICHIER_INDEX_WEB, transport_du_serveur};
use tune_core::streaming::StreamingService;
use tune_core::streaming::qobuz::QobuzService;
use tune_core::streaming::traits::StreamAlbum;
use tune_http_types::panne_sql::OuDefautJournalise;

use crate::state::AppState;

/// Un index par dossier de cache — le processus n'en a qu'un en production.
pub(crate) fn cache_de(dossier: &Path) -> Arc<CacheLogosLabels> {
    static CACHES: OnceLock<Mutex<HashMap<PathBuf, Arc<CacheLogosLabels>>>> = OnceLock::new();
    let mut caches = CACHES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    caches
        .entry(dossier.to_path_buf())
        .or_insert_with(|| Arc::new(CacheLogosLabels::ouvrir(dossier)))
        .clone()
}

/// #7089 — l'index des logos du web, à côté de celui de Qobuz.
pub(crate) fn cache_web_de(dossier: &Path) -> Arc<CacheLogosLabels> {
    static CACHES: OnceLock<Mutex<HashMap<PathBuf, Arc<CacheLogosLabels>>>> = OnceLock::new();
    let mut caches = CACHES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    caches
        .entry(dossier.to_path_buf())
        .or_insert_with(|| Arc::new(CacheLogosLabels::ouvrir_index(dossier, FICHIER_INDEX_WEB)))
        .clone()
}

/// #7089 — le catalogue des logos du web, ou `None` quand la tâche de fond
/// « Enrichissement » est en pause (ou, dans un banc, sans racine d'essai) —
/// et alors, aucune passe.
pub(crate) fn catalogue_web(state: &AppState) -> Option<Arc<dyn CatalogueDeLabels>> {
    if tune_core::taches_de_fond::est_en_pause(tune_core::taches_de_fond::Tache::Enrichissement) {
        return None;
    }
    let transport = transport_du_serveur(state.http_client.clone())?;
    Some(Arc::new(CatalogueLogosWeb::new(
        transport,
        Some(state.backend.clone()),
    )))
}

/// #7414 — le partage communautaire des logos (mozaiklabs.fr), ou `None`
/// quand la télémétrie est refusée (et, dans un banc, sans racine d'essai).
pub(crate) fn partage_de(state: &AppState) -> Option<Arc<dyn LogosPartages>> {
    tune_core::cloud::logos_labels_partages::PartageHttp::depuis_reglages(
        &state.backend,
        state.http_client.clone(),
    )
    .map(|p| Arc::new(p) as Arc<dyn LogosPartages>)
}

/// La pause entre deux téléchargements du partage : ses images sont en cache
/// chez le site, un quart de seconde suffit à ne pas faire de rafale.
const PAUSE_PARTAGE: Duration = Duration::from_millis(250);

fn maintenant() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Le service Qobuz, s'il est actif ET connecté à un compte. Il est relu à
/// chaque appel : un compte déconnecté en cours de passe arrête les appels.
struct QobuzConnecte(Arc<RwLock<Box<dyn StreamingService>>>);

/// Le service sous lecture, s'il est encore actif et connecté.
async fn connecte(
    svc: &RwLock<Box<dyn StreamingService>>,
) -> Result<tokio::sync::RwLockReadGuard<'_, Box<dyn StreamingService>>, String> {
    let garde = svc.read().await;
    if !garde.enabled() || !qobuz(garde.as_ref())?.auth_status().await.authenticated {
        return Err(String::from("compte Qobuz non connecté"));
    }
    Ok(garde)
}

fn qobuz(garde: &dyn StreamingService) -> Result<&QobuzService, String> {
    garde
        .as_any()
        .downcast_ref::<QobuzService>()
        .ok_or_else(|| String::from("service qobuz inattendu"))
}

#[async_trait::async_trait]
impl CatalogueDeLabels for QobuzConnecte {
    async fn trouver_label(
        &self,
        nom: &str,
        album: Option<&str>,
    ) -> Result<Option<LabelDuCatalogue>, String> {
        let garde = connecte(&self.0).await?;
        qobuz(garde.as_ref())?.trouver_label(nom, album).await
    }

    async fn telecharger_image(&self, url: &str) -> Result<Vec<u8>, String> {
        let garde = connecte(&self.0).await?;
        qobuz(garde.as_ref())?.telecharger_image(url).await
    }
}

#[async_trait::async_trait]
impl CatalogueDuLabel for QobuzConnecte {
    async fn albums_du_label(&self, label_id: &str) -> Result<Vec<StreamAlbum>, String> {
        let garde = connecte(&self.0).await?;
        CatalogueDuLabel::albums_du_label(qobuz(garde.as_ref())?, label_id).await
    }
}

/// Le service Qobuz actif ET connecté, ou `None` — et alors, aucun appel.
async fn qobuz_connecte(state: &AppState) -> Option<Arc<QobuzConnecte>> {
    let svc = state.services.lock().await.get_actif("qobuz").await?;
    drop(connecte(&svc).await.ok()?);
    Some(Arc::new(QobuzConnecte(svc)))
}

/// Le catalogue Qobuz à interroger, ou `None` sans compte Qobuz connecté —
/// et alors, aucune passe.
pub(crate) async fn catalogue_qobuz(state: &AppState) -> Option<Arc<dyn CatalogueDeLabels>> {
    Some(qobuz_connecte(state).await? as Arc<dyn CatalogueDeLabels>)
}

/// L'attribution d'un logo, telle que la liste la rend (#7089).
pub(crate) fn credit(a: &Attribution) -> Value {
    let source = match a.source {
        SourceDuLogo::Commons => "commons",
        SourceDuLogo::Wikipedia => "wikipedia",
        SourceDuLogo::SiteOfficiel => "official_site",
        SourceDuLogo::Discogs => "discogs",
        SourceDuLogo::Qobuz => "qobuz",
    };
    let statut = match a.statut_licence {
        StatutDeLicence::Libre => "free",
        StatutDeLicence::NonLibreIdentification => "non_free_identification",
        StatutDeLicence::Inconnue => "unknown",
    };
    let mut o = json!({
        "source": source,
        "provider": a.fournisseur,
        "license_status": statut,
        "source_url": a.source_url,
    });
    for (cle, valeur) in [
        ("license", &a.licence),
        ("license_url", &a.licence_url),
        ("author", &a.auteur),
        ("file", &a.fichier),
    ] {
        if let Some(v) = valeur {
            o[cle] = json!(v);
        }
    }
    if a.douteux {
        o["doubtful"] = json!(true);
    }
    o
}

/// L'attribution d'un logo Qobuz : source connue, licence inconnue.
fn credit_qobuz() -> Value {
    json!({ "source": "qobuz", "provider": "Qobuz", "license_status": "unknown" })
}

/// Une entrée de la liste : `image_url`, `image_credit` et `qobuz_label_id`
/// sont ABSENTS quand rien n'est connu (jamais `null`, jamais une adresse qui
/// rend 404). Ordre de préférence (#7089) : le logo du web — Commons,
/// Wikipédia, site officiel, Discogs —, puis Qobuz. Un logo est TOUJOURS rendu
/// avec son attribution ; un logo du web sans attribution n'est pas rendu.
pub(crate) fn element(
    cache: &CacheLogosLabels,
    web: &CacheLogosLabels,
    label: &LabelDeBibliotheque,
) -> Value {
    let mut o = json!({ "name": label.nom, "album_count": label.album_count });
    if let Some(condensat) = web.image(&label.nom)
        && let Some(a) = web.entree(&label.nom).and_then(|e| e.attribution)
    {
        o["image_url"] = json!(format!("/api/v1/library/artwork/{condensat}"));
        o["image_credit"] = credit(&a);
        // #7414 — reçu du partage communautaire (mozaiklabs.fr).
        if web.entree(&label.nom).is_some_and(|e| e.communaute) {
            o["image_credit"]["via"] = json!("community");
        }
    } else if let Some(condensat) = cache.image(&label.nom) {
        o["image_url"] = json!(format!("/api/v1/library/artwork/{condensat}"));
        o["image_credit"] = credit_qobuz();
    }
    if let Some(id) = cache.entree(&label.nom).and_then(|e| e.qobuz_label_id) {
        o["qobuz_label_id"] = json!(id);
    }
    o
}

/// Lance, si besoin et si aucune ne tourne, une passe sur les labels encore
/// inconnus. Rend la tâche, pour les essais.
///
/// #7414 — la passe ne s'arrête plus après [`PASSE_MAX`] labels : avec 338
/// labels sur le .18, il fallait une quarantaine d'ouvertures de l'onglet
/// pour tous les demander. Elle les prend TOUS, en arrière-plan, un label
/// après l'autre (`pause` entre deux, et les limiteurs de chaque source) ;
/// [`PASSE_MAX`] n'est plus que la taille d'une tranche, entre deux
/// tranches de laquelle `continuer` est relu — la pause de l'Enrichissement
/// arrête ainsi la passe du web en cours de route. Chaque label reste
/// demandé une seule fois par passe et inscrit dès qu'il est résolu.
pub(crate) fn lancer_passe(
    cache: Arc<CacheLogosLabels>,
    catalogue: Arc<dyn CatalogueDeLabels>,
    demandes: Vec<Demande>,
    pause: Duration,
    continuer: fn() -> bool,
) -> Option<tokio::task::JoinHandle<usize>> {
    let a_faire = cache.a_resoudre(&demandes, maintenant(), usize::MAX);
    if a_faire.is_empty() {
        return None;
    }
    Some(tokio::spawn(async move {
        let Some(_passe) = cache.reserver_passe() else {
            return 0;
        };
        parcourir(&cache, &*catalogue, &a_faire, pause, continuer).await
    }))
}

/// Le corps d'une passe : `a_faire`, tranche par tranche (#7414).
async fn parcourir(
    cache: &CacheLogosLabels,
    catalogue: &dyn CatalogueDeLabels,
    a_faire: &[Demande],
    pause: Duration,
    continuer: fn() -> bool,
) -> usize {
    let mut trouves = 0;
    for (i, tranche) in a_faire.chunks(PASSE_MAX).enumerate() {
        if !continuer() {
            break;
        }
        if i > 0 && !pause.is_zero() {
            tokio::time::sleep(pause).await;
        }
        trouves += cache
            .resoudre(catalogue, tranche.to_vec(), maintenant, pause)
            .await;
        // Une tranche entièrement en erreur (compte déconnecté, service
        // en panne) arrête la passe : le reste sera demandé plus tard,
        // au lieu d'être inscrit en erreur pour un jour.
        if tranche.iter().all(|d| {
            cache
                .entree(&d.nom)
                .is_some_and(|e| e.statut == Statut::Erreur)
        }) {
            break;
        }
    }
    trouves
}

/// #7414 — la passe du web, avec le partage communautaire :
///
/// 1. AVANT toute recherche sur le web, les labels à demander sont demandés
///    au partage, en UN appel ; chaque logo rendu est inscrit aussitôt, et
///    ce label n'est pas cherché sur le web. Un partage muet ne coûte qu'un
///    délai court : la passe continue sur le web.
/// 2. La passe du web, comme [`lancer_passe`], sur ce qui reste.
/// 3. Si l'utilisateur a donné son consentement communautaire, les logos
///    trouvés ici (avec leur attribution, jamais ceux reçus du partage) sont
///    proposés, [`PROPOSITIONS_PAR_PASSE`] au plus, `pause` entre deux ; un
///    site qui ne répond pas arrête les propositions jusqu'à la passe
///    suivante.
///
/// Sans partage (`None`), c'est exactement [`lancer_passe`]. Rend la tâche et
/// le nombre de logos inscrits (partage compris), pour les essais.
pub(crate) fn lancer_passe_web(
    web: Arc<CacheLogosLabels>,
    qobuz: Arc<CacheLogosLabels>,
    catalogue: Arc<dyn CatalogueDeLabels>,
    partage: Option<Arc<dyn LogosPartages>>,
    demandes: Vec<Demande>,
    pause: Duration,
    continuer: fn() -> bool,
) -> Option<tokio::task::JoinHandle<usize>> {
    let a_faire = web.a_resoudre(&demandes, maintenant(), usize::MAX);
    let proposer = partage
        .as_ref()
        .is_some_and(|p| p.peut_proposer() && !web.a_proposer(1).is_empty());
    if a_faire.is_empty() && !proposer {
        return None;
    }
    Some(tokio::spawn(async move {
        let Some(_passe) = web.reserver_passe() else {
            return 0;
        };
        let mut a_faire = a_faire;
        let mut trouves = 0;
        if let Some(partage) = partage.as_deref()
            && !a_faire.is_empty()
        {
            let labels: Vec<LabelAConsulter> = a_faire
                .iter()
                .map(|d| LabelAConsulter {
                    nom: d.nom.clone(),
                    mb_label_id: web.entree(&d.nom).and_then(|e| e.mb_label_id),
                    qobuz_label_id: qobuz.entree(&d.nom).and_then(|e| e.qobuz_label_id),
                })
                .collect();
            let combles = web
                .combler_depuis_le_partage(partage, &labels, maintenant, PAUSE_PARTAGE.min(pause))
                .await;
            trouves += combles.len();
            a_faire.retain(|d| !combles.contains(&cle_de_label(&d.nom)));
        }
        trouves += parcourir(&web, &*catalogue, &a_faire, pause, continuer).await;
        if let Some(partage) = partage.as_deref()
            && partage.peut_proposer()
            && continuer()
        {
            proposer_au_partage(&web, &qobuz, partage, &demandes, pause).await;
        }
        trouves
    }))
}

/// #7414 — propose au partage les logos trouvés ici. Seuls les labels encore
/// dans la bibliothèque (`demandes`) partent, sous leur nom affiché.
async fn proposer_au_partage(
    web: &CacheLogosLabels,
    qobuz: &CacheLogosLabels,
    partage: &dyn LogosPartages,
    demandes: &[Demande],
    pause: Duration,
) {
    let noms: HashMap<String, &str> = demandes
        .iter()
        .map(|d| (cle_de_label(&d.nom), d.nom.as_str()))
        .collect();
    let mut envoyees = 0usize;
    for (cle, entree, octets, ext) in web.a_proposer(PROPOSITIONS_PAR_PASSE) {
        let (Some(nom), Some(attribution)) = (noms.get(&cle), entree.attribution) else {
            continue;
        };
        if envoyees > 0 && !pause.is_zero() {
            tokio::time::sleep(pause).await;
        }
        envoyees += 1;
        let proposition = PropositionDeLogo {
            nom: (*nom).to_string(),
            mb_label_id: entree.mb_label_id,
            qobuz_label_id: qobuz.entree(nom).and_then(|e| e.qobuz_label_id),
            attribution,
            octets,
            ext,
        };
        match partage.proposer(&proposition).await {
            ProposeAuPartage::Traitee => web.marquer_partage(nom),
            ProposeAuPartage::Plus => break,
        }
    }
}

/// La passe du web continue tant que l'Enrichissement n'est pas en pause.
fn enrichissement_actif() -> bool {
    !tune_core::taches_de_fond::est_en_pause(tune_core::taches_de_fond::Tache::Enrichissement)
}

/// La passe Qobuz n'a pas d'interrupteur : un compte déconnecté se voit à
/// chaque appel, et une tranche tout en erreur arrête la passe.
fn toujours() -> bool {
    true
}

/// Les demandes de la passe Qobuz : l'album Qobuz de la bibliothèque.
pub(crate) fn demandes_qobuz(labels: &[LabelDeBibliotheque]) -> Vec<Demande> {
    labels
        .iter()
        .map(|l| Demande {
            nom: l.nom.clone(),
            album_du_catalogue: l.album_qobuz.clone(),
        })
        .collect()
}

/// Les demandes de la passe du web (#7089) : le MBID d'une release de la
/// bibliothèque. Le web passe avant Qobuz : un label qui a déjà un logo
/// Qobuz est demandé aussi.
pub(crate) fn demandes_web(labels: &[LabelDeBibliotheque]) -> Vec<Demande> {
    labels
        .iter()
        .map(|l| Demande {
            nom: l.nom.clone(),
            album_du_catalogue: l.release_mb.clone(),
        })
        .collect()
}

pub(super) async fn list_labels(State(state): State<AppState>) -> Json<Value> {
    let labels = AlbumRepo::with_backend(state.backend.clone())
        .labels_de_la_bibliotheque()
        .ou_defaut_journalise();
    let dossier = super::artwork_cache_dir();
    let cache = cache_de(&dossier);
    let web = cache_web_de(&dossier);
    let items: Vec<Value> = labels.iter().map(|l| element(&cache, &web, l)).collect();
    // L'ouverture de l'onglet lance toujours la passe (#6736, #7414), mais
    // par le même chemin que `POST /library/labels/logos/enrich` : inscrite
    // au registre sous `label_logos`, une seule à la fois.
    let sources = SourcesDeLogos::du_serveur(&state).await;
    lancer_passes_logos(&state, labels, sources);
    Json(Value::Array(items))
}

// ---------------------------------------------------------------------------
// Écran « État du serveur » (01/11/2026) — la passe des logos, lancée et
// suivie explicitement
// ---------------------------------------------------------------------------

/// L'identifiant de la passe des logos de labels au registre
/// `background_tasks`.
pub(crate) const TACHE_LOGOS_LABELS: &str = "label_logos";

/// La clé `settings` du bilan de la dernière passe.
const CLE_DERNIERE_PASSE_LOGOS: &str = "label_logos_last_run";

/// Toutes les combien l'avancement de la passe est relu dans les index.
const PERIODE_AVANCEMENT: Duration = Duration::from_secs(1);

/// Les sources d'une passe de logos. `None` : cette source ne tourne pas.
pub(crate) struct SourcesDeLogos {
    pub(crate) web: Option<Arc<dyn CatalogueDeLabels>>,
    pub(crate) qobuz: Option<Arc<dyn CatalogueDeLabels>>,
    pub(crate) partage: Option<Arc<dyn LogosPartages>>,
    pub(crate) pause: Duration,
}

impl SourcesDeLogos {
    /// Celles du serveur : le web (sauf Enrichissement en pause), Qobuz
    /// (seulement avec un compte connecté), le partage communautaire.
    pub(crate) async fn du_serveur(state: &AppState) -> Self {
        Self {
            web: catalogue_web(state),
            qobuz: catalogue_qobuz(state).await,
            partage: partage_de(state),
            pause: PAUSE_ENTRE_LABELS,
        }
    }
}

/// Ce qu'une demande de passe a produit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LancementLogos {
    /// La passe est partie ; tant de labels à demander.
    Lancee { a_traiter: usize },
    /// Une passe tourne déjà : rien de plus n'est lancé.
    DejaEnCours,
    /// Aucun label à demander (et rien à proposer au partage).
    RienAFaire,
    /// Aucune source : pas de compte Qobuz et la passe du web suspendue.
    SansSource,
}

/// Le nombre de labels qui ONT un logo affichable, selon la même règle que
/// [`element`] (un logo du web n'est rendu qu'avec son attribution).
pub(crate) fn compter_logos(
    cache: &CacheLogosLabels,
    web: &CacheLogosLabels,
    labels: &[LabelDeBibliotheque],
) -> usize {
    labels
        .iter()
        .filter(|l| {
            (web.image(&l.nom).is_some()
                && web.entree(&l.nom).is_some_and(|e| e.attribution.is_some()))
                || cache.image(&l.nom).is_some()
        })
        .count()
}

/// Lance la passe des logos (web puis Qobuz, comme à l'ouverture de
/// l'onglet), inscrite au registre sous [`TACHE_LOGOS_LABELS`] avec son
/// avancement, et range son bilan dans `label_logos_last_run` à la fin.
///
/// Une seule passe à la fois : l'inscription au registre (`try_begin`) est
/// le verrou, prise AVANT tout le reste.
pub(crate) fn lancer_passes_logos(
    state: &AppState,
    labels: Vec<LabelDeBibliotheque>,
    sources: SourcesDeLogos,
) -> LancementLogos {
    let Some(garde) = state.background_tasks.try_begin(
        TACHE_LOGOS_LABELS,
        "Recherche des logos de labels…",
        "enrichment",
    ) else {
        return LancementLogos::DejaEnCours;
    };
    if sources.web.is_none() && sources.qobuz.is_none() {
        return LancementLogos::SansSource;
    }
    let dossier = super::artwork_cache_dir();
    let cache = cache_de(&dossier);
    let web = cache_web_de(&dossier);
    let debut = maintenant();
    let (demandes_w, demandes_q) = (demandes_web(&labels), demandes_qobuz(&labels));
    // Ce que chaque passe va demander : c'est le total de l'avancement.
    let suivis_web: Vec<String> = if sources.web.is_some() {
        web.a_resoudre(&demandes_w, debut, usize::MAX)
            .into_iter()
            .map(|d| d.nom)
            .collect()
    } else {
        Vec::new()
    };
    let suivis_qobuz: Vec<String> = if sources.qobuz.is_some() {
        cache
            .a_resoudre(&demandes_q, debut, usize::MAX)
            .into_iter()
            .map(|d| d.nom)
            .collect()
    } else {
        Vec::new()
    };
    let mut passes = Vec::new();
    if let Some(catalogue) = sources.web {
        passes.extend(lancer_passe_web(
            web.clone(),
            cache.clone(),
            catalogue,
            sources.partage,
            demandes_w,
            sources.pause,
            enrichissement_actif,
        ));
    }
    if let Some(catalogue) = sources.qobuz {
        passes.extend(lancer_passe(
            cache.clone(),
            catalogue,
            demandes_q,
            sources.pause,
            toujours,
        ));
    }
    if passes.is_empty() {
        return LancementLogos::RienAFaire;
    }
    let total = (suivis_web.len() + suivis_qobuz.len()) as u64;
    let a_traiter = suivis_web.len() + suivis_qobuz.len();
    let taches = state.background_tasks.clone();
    let backend = state.backend.clone();
    // Un label est traité quand son entrée a été réécrite depuis le début.
    let traites = {
        let (cache, web) = (cache.clone(), web.clone());
        move || -> u64 {
            let n = suivis_web
                .iter()
                .filter(|n| web.entree(n).is_some_and(|e| e.verifie_le >= debut))
                .count()
                + suivis_qobuz
                    .iter()
                    .filter(|n| cache.entree(n).is_some_and(|e| e.verifie_le >= debut))
                    .count();
            (n as u64).min(total)
        }
    };
    taches.update_progress(TACHE_LOGOS_LABELS, 0, total, "labels");
    tokio::spawn(async move {
        let _garde = garde;
        let mut toutes = Box::pin(futures_util::future::join_all(passes));
        let trouves: usize = loop {
            tokio::select! {
                fins = &mut toutes => break fins.into_iter().map(|r| r.unwrap_or(0)).sum(),
                _ = tokio::time::sleep(PERIODE_AVANCEMENT) => {
                    taches.update_progress(TACHE_LOGOS_LABELS, traites(), total, "labels");
                }
            }
        };
        let traites = traites();
        taches.update_progress(TACHE_LOGOS_LABELS, traites, total, "labels");
        let bilan = json!({
            "started_at": debut,
            "finished_at": maintenant(),
            "to_process": total,
            "processed": traites,
            "found": trouves,
            "labels_total": labels.len(),
            "with_logo": compter_logos(&cache, &web, &labels),
        });
        if let Err(e) = tune_core::db::settings_repo::SettingsRepo::with_backend(backend)
            .set(CLE_DERNIERE_PASSE_LOGOS, &bilan.to_string())
        {
            tracing::warn!(erreur = %e, "logos_labels_bilan_non_ecrit");
        }
        tracing::info!(%bilan, "logos_labels_passe_terminee");
    });
    LancementLogos::Lancee { a_traiter }
}

/// `POST /library/labels/logos/enrich` — lance la passe des logos.
///
/// - 202 `{"status": "accepted", "labels_to_process": N}` ;
/// - 409 `{"status": "already_running"}` : une passe tourne, rien de plus ;
/// - 200 `{"status": "skipped", "reason": "nothing_to_do" | "paused" |
///   "no_source"}`.
pub(super) async fn lancer_logos(State(state): State<AppState>) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    let labels = AlbumRepo::with_backend(state.backend.clone())
        .labels_de_la_bibliotheque()
        .ou_defaut_journalise();
    let sources = SourcesDeLogos::du_serveur(&state).await;
    match lancer_passes_logos(&state, labels, sources) {
        LancementLogos::Lancee { a_traiter } => (
            StatusCode::ACCEPTED,
            Json(json!({"status": "accepted", "labels_to_process": a_traiter})),
        )
            .into_response(),
        LancementLogos::DejaEnCours => (
            StatusCode::CONFLICT,
            Json(json!({
                "status": "already_running",
                "message": "label logo pass already running",
            })),
        )
            .into_response(),
        LancementLogos::RienAFaire => {
            Json(json!({"status": "skipped", "reason": "nothing_to_do"})).into_response()
        }
        LancementLogos::SansSource => {
            let raison = if enrichissement_actif() {
                "no_source"
            } else {
                "paused"
            };
            Json(json!({"status": "skipped", "reason": raison})).into_response()
        }
    }
}

/// `GET /library/labels/logos/status` — combien de labels ont un logo, la
/// passe en cours et la dernière passe.
///
/// ```json
/// { "labels_total": 338, "with_logo": 133, "without_logo": 205,
///   "running": true, "progress": {"processed": 40, "total": 205},
///   "last_run": {"started_at": …, "finished_at": …, "to_process": 205,
///                "processed": 205, "found": 12, "labels_total": 338,
///                "with_logo": 145} }
/// ```
///
/// Une lecture SQL (les labels regroupés) et les deux index en mémoire :
/// aucun appel réseau, aucune passe lancée. `progress` n'est là que pendant
/// la passe, `last_run` est `null` avant la première.
pub(super) async fn statut_logos(State(state): State<AppState>) -> Json<Value> {
    let labels = AlbumRepo::with_backend(state.backend.clone())
        .labels_de_la_bibliotheque()
        .ou_defaut_journalise();
    let dossier = super::artwork_cache_dir();
    let avec = compter_logos(&cache_de(&dossier), &cache_web_de(&dossier), &labels);
    let tache = state.background_tasks.get(TACHE_LOGOS_LABELS);
    let derniere = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .get(CLE_DERNIERE_PASSE_LOGOS)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    let mut corps = json!({
        "labels_total": labels.len(),
        "with_logo": avec,
        "without_logo": labels.len().saturating_sub(avec),
        "running": tache.is_some(),
        "last_run": derniere,
    });
    if let Some(p) = tache.and_then(|t| t.progress) {
        corps["progress"] = json!({ "processed": p.processed, "total": p.total });
    }
    Json(corps)
}

// ---------------------------------------------------------------------------
// #6782 — les artistes du même label
// ---------------------------------------------------------------------------

/// Les deux faces de Qobuz dont la découverte a besoin : rapprocher le label
/// (lot L), puis lire son catalogue.
pub(crate) struct QobuzPourLeLabel {
    pub(crate) labels: Arc<dyn CatalogueDeLabels>,
    pub(crate) catalogue: Arc<dyn CatalogueDuLabel>,
}

/// La section « À découvrir » d'un label : `None` sans Qobuz, sans label
/// Qobuz rapproché, ou quand son catalogue n'a pas pu être lu.
///
/// Le label Qobuz est celui que le lot L garde dans l'index des logos. Pour
/// un label que la liste n'a pas encore rapproché (page ouverte depuis une
/// fiche album), on le rapproche ici, pour ce seul label — trois appels au
/// plus, gardés comme ceux de la passe — et seulement si aucune passe ne
/// tourne.
pub(crate) async fn section_decouvrir(
    logos: &CacheLogosLabels,
    decouverte: &CacheDecouverte,
    qobuz: Option<QobuzPourLeLabel>,
    label: &LabelDeBibliotheque,
    deja: &HashSet<String>,
) -> Option<(String, Vec<ArtisteADecouvrir>)> {
    let qobuz = qobuz?;
    let mut id = logos.entree(&label.nom).and_then(|e| e.qobuz_label_id);
    if id.is_none() {
        let demande = Demande {
            nom: label.nom.clone(),
            album_du_catalogue: label.album_qobuz.clone(),
        };
        let a_faire = logos.a_resoudre(&[demande], maintenant(), 1);
        if !a_faire.is_empty()
            && let Some(_passe) = logos.reserver_passe()
        {
            logos
                .resoudre(&*qobuz.labels, a_faire, maintenant, Duration::ZERO)
                .await;
        }
        id = logos.entree(&label.nom).and_then(|e| e.qobuz_label_id);
    }
    let id = id?;
    let albums = decouverte
        .albums(&*qobuz.catalogue, &id, maintenant)
        .await
        .ok()?;
    Some((id, regrouper(&albums, deja)))
}

/// `GET /library/labels/{nom}/artists` (#6782) — les artistes du label.
///
/// ```json
/// { "name": "ECM",
///   "bibliotheque": [{"id": 1, "name": "Keith Jarrett", "album_count": 4,
///                     "image_url": "/api/v1/library/artists/1/image"}],
///   "qobuz_label_id": "9",
///   "decouvrir": [{"name": "Jan Garbarek", "qobuz_artist_id": "20",
///                  "album_count": 3,
///                  "album_phare": {"id": "…", "title": "Rarum",
///                                  "cover_url": "https://…", "year": 2002}}] }
/// ```
///
/// `bibliotheque` est toujours là (vide pour un label inconnu). `decouvrir`
/// et `qobuz_label_id` sont ABSENTS sans compte Qobuz connecté — aucun appel
/// alors — ou quand le label n'a pas de correspondant Qobuz, ou que son
/// catalogue n'a pas pu être lu. `decouvrir` ne contient aucun artiste déjà
/// présent dans la bibliothèque, sous ce label ou un autre.
pub(super) async fn artistes_du_label(
    State(state): State<AppState>,
    axum::extract::Path(nom): axum::extract::Path<String>,
) -> Json<Value> {
    let nom = nom.trim().to_string();
    let bibliotheque = artistes_de_la_bibliotheque(&state.backend, &nom).ou_defaut_journalise();
    let mut corps = json!({ "name": nom, "bibliotheque": bibliotheque });
    let qobuz = qobuz_connecte(&state).await.map(|q| QobuzPourLeLabel {
        labels: q.clone(),
        catalogue: q,
    });
    if qobuz.is_some() {
        let label = AlbumRepo::with_backend(state.backend.clone())
            .labels_de_la_bibliotheque()
            .ou_defaut_journalise()
            .into_iter()
            .find(|l| l.nom == nom)
            .unwrap_or(LabelDeBibliotheque {
                nom: nom.clone(),
                album_count: 0,
                album_qobuz: None,
                release_mb: None,
            });
        let deja = cles_de_la_bibliotheque(&state.backend).ou_defaut_journalise();
        let logos = cache_de(&super::artwork_cache_dir());
        if let Some((id, decouvrir)) =
            section_decouvrir(&logos, CacheDecouverte::global(), qobuz, &label, &deja).await
        {
            corps["qobuz_label_id"] = json!(id);
            corps["decouvrir"] = json!(decouvrir);
        }
    }
    Json(corps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;
    use tune_core::db::backend::ToSqlValue;
    use tune_core::library::logos_labels::EntreeLogo;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n-logo-6736-";

    fn demarrer(chemin: &str) -> (axum::Router, AppState) {
        let state = AppState::new(chemin, 0, Default::default()).expect("démarrage");
        (crate::routes::router(state.clone()), state)
    }

    fn album(
        state: &AppState,
        titre: &str,
        label: Option<&str>,
        source: &str,
        source_id: Option<&str>,
    ) {
        state
            .backend
            .execute(
                "INSERT INTO albums (title, artist_id, source, source_id, track_count, year, label) \
                 VALUES (?, NULL, ?, ?, 0, 1970, ?)",
                &[
                    &titre as &dyn ToSqlValue,
                    &source,
                    &source_id.map(String::from),
                    &label.map(String::from),
                ],
            )
            .unwrap();
    }

    async fn labels(app: &axum::Router) -> Vec<Value> {
        let reponse = app
            .clone()
            .oneshot(
                Request::get("/api/v1/library/labels")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(reponse.status().is_success(), "{}", reponse.status());
        let corps = axum::body::to_bytes(reponse.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice::<Value>(&corps)
            .unwrap()
            .as_array()
            .expect("une liste")
            .clone()
    }

    /// La liste regroupe par nom (espaces de bord retirés), compte les albums,
    /// rend le logo connu en adresse du cache des pochettes, et n'écrit AUCUN
    /// `image_url` pour un label inconnu.
    #[tokio::test(flavor = "multi_thread")]
    async fn la_liste_des_labels_rend_le_compte_et_le_logo_connu_seulement() {
        let dossier = tune_core::test_scratch::scratch_dir("labels-6736-route");
        let chemin = dossier.path().join("tune.db");
        let (app, state) = demarrer(chemin.to_str().unwrap());
        let (logo, sans) = ("Verve 6736 route", "Sans Logo 6736 route");
        album(&state, "A", Some(logo), "local", None);
        album(&state, "B", Some(&format!(" {logo} ")), "local", None);
        album(&state, "C", Some(sans), "local", None);
        album(&state, "D", None, "local", None);
        album(&state, "E", Some("  "), "local", None);

        let cache = cache_de(&crate::routes::library::artwork_cache_dir());
        let condensat =
            tune_core::library::artwork::cache_fetched_image(PNG, cache.dossier(), "png").unwrap();
        cache.inscrire(
            logo,
            EntreeLogo {
                statut: Statut::Trouve,
                qobuz_label_id: Some("77".into()),
                mb_label_id: None,
                attribution: None,
                image: Some(condensat.clone()),
                verifie_le: maintenant(),
                communaute: false,
                partage: false,
            },
        );

        let liste = labels(&app).await;
        let par_nom = |n: &str| liste.iter().find(|l| l["name"] == n).cloned();
        assert_eq!(
            liste.len(),
            2,
            "ni album sans label ni label blanc : {liste:?}"
        );
        let v = par_nom(logo).expect("label avec logo");
        assert_eq!(v["album_count"], 2);
        assert_eq!(
            v["image_url"],
            format!("/api/v1/library/artwork/{condensat}")
        );
        assert_eq!(v["qobuz_label_id"], "77");
        let s = par_nom(sans).expect("label sans logo");
        assert_eq!(s["album_count"], 1);
        assert!(s.get("image_url").is_none(), "absent, pas null : {s}");
        assert!(s.get("qobuz_label_id").is_none());

        // Et l'adresse rendue sert bien l'image.
        let reponse = app
            .clone()
            .oneshot(
                Request::get(v["image_url"].as_str().unwrap())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(reponse.status().is_success());
    }

    /// Sans compte Qobuz connecté — le cas d'un démarrage neuf — aucun
    /// catalogue, donc aucune passe et aucun appel.
    #[tokio::test(flavor = "multi_thread")]
    async fn sans_compte_qobuz_aucun_catalogue_n_est_interroge() {
        let dossier = tune_core::test_scratch::scratch_dir("labels-6736-sans-compte");
        let chemin = dossier.path().join("tune.db");
        let (_app, state) = demarrer(chemin.to_str().unwrap());
        let svc = state
            .services
            .lock()
            .await
            .get("qobuz")
            .expect("le service Qobuz est enregistré");
        // Désactivé (aucun identifiant d'application) : aucun catalogue.
        assert!(catalogue_qobuz(&state).await.is_none());
        // Activé, mais sans compte connecté : toujours aucun catalogue.
        svc.write().await.set_enabled(true);
        assert!(!svc.read().await.auth_status().await.authenticated);
        assert!(
            catalogue_qobuz(&state).await.is_none(),
            "service actif sans compte connecté : aucun appel ne doit partir"
        );
    }

    struct Compteur(AtomicUsize);

    #[async_trait::async_trait]
    impl CatalogueDeLabels for Compteur {
        async fn trouver_label(
            &self,
            nom: &str,
            album: Option<&str>,
        ) -> Result<Option<LabelDuCatalogue>, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok((album == Some("q-1")).then(|| LabelDuCatalogue {
                id: "1".into(),
                nom: nom.into(),
                image_url: Some("http://img.test/l.png".into()),
                attribution: None,
            }))
        }
        async fn telecharger_image(&self, _: &str) -> Result<Vec<u8>, String> {
            Ok(PNG.to_vec())
        }
    }

    /// #7414 — UNE ouverture de la liste lance une passe qui demande TOUS les
    /// labels encore inconnus (pas seulement `PASSE_MAX`), chacun une seule
    /// fois, l'album Qobuz de la bibliothèque transmis ; une seconde liste ne
    /// redemande rien.
    #[tokio::test(flavor = "multi_thread")]
    async fn une_passe_demande_tous_les_labels_inconnus_une_fois_7414() {
        let dossier = tune_core::test_scratch::scratch_dir("labels-6736-passe");
        let chemin = dossier.path().join("tune.db");
        let (_app, state) = demarrer(chemin.to_str().unwrap());
        album(&state, "Q", Some("Lbl 6736 passe 00"), "qobuz", Some("q-1"));
        for i in 1..20 {
            album(
                &state,
                &format!("L{i}"),
                Some(&format!("Lbl 6736 passe {i:02}")),
                "local",
                None,
            );
        }
        let liste = AlbumRepo::with_backend(state.backend.clone())
            .labels_de_la_bibliotheque()
            .unwrap();
        assert_eq!(liste[0].album_qobuz.as_deref(), Some("q-1"));
        assert!(liste.len() > PASSE_MAX, "l'essai dépasse une tranche");

        let cache_dir = tune_core::test_scratch::scratch_dir("labels-6736-passe-cache");
        let cache = Arc::new(CacheLogosLabels::ouvrir(cache_dir.path()));
        let compteur = Arc::new(Compteur(AtomicUsize::new(0)));
        let trouves = lancer_passe(
            cache.clone(),
            compteur.clone(),
            demandes_qobuz(&liste),
            Duration::ZERO,
            toujours,
        )
        .expect("une passe")
        .await
        .unwrap();
        assert_eq!(trouves, 1);
        assert_eq!(
            compteur.0.load(Ordering::SeqCst),
            20,
            "#7414 : une seule ouverture doit demander les 20 labels, pas \
             seulement les PASSE_MAX premiers"
        );
        assert!(cache.image("Lbl 6736 passe 00").is_some());
        assert!(
            lancer_passe(
                cache,
                compteur.clone(),
                demandes_qobuz(&liste),
                Duration::ZERO,
                toujours,
            )
            .is_none(),
            "tout est connu, absence comprise : plus aucun appel"
        );
        assert_eq!(compteur.0.load(Ordering::SeqCst), 20);
    }

    /// #7414 — la passe s'arrête quand on le lui dit (Enrichissement mis en
    /// pause) : aucun label n'est demandé, et ils restent à demander.
    #[tokio::test(flavor = "multi_thread")]
    async fn la_passe_s_arrete_quand_continuer_dit_non_7414() {
        fn jamais() -> bool {
            false
        }
        let cache_dir = tune_core::test_scratch::scratch_dir("labels-7414-arret");
        let cache = Arc::new(CacheLogosLabels::ouvrir(cache_dir.path()));
        let compteur = Arc::new(Compteur(AtomicUsize::new(0)));
        let demandes: Vec<Demande> = (0..12)
            .map(|i| Demande {
                nom: format!("Lbl 7414 arret {i:02}"),
                album_du_catalogue: None,
            })
            .collect();
        lancer_passe(
            cache.clone(),
            compteur.clone(),
            demandes.clone(),
            Duration::ZERO,
            jamais,
        )
        .expect("des labels à demander")
        .await
        .unwrap();
        assert_eq!(compteur.0.load(Ordering::SeqCst), 0);
        assert_eq!(
            cache.a_resoudre(&demandes, maintenant(), usize::MAX).len(),
            12
        );
    }

    struct EnPanne(AtomicUsize);

    #[async_trait::async_trait]
    impl CatalogueDeLabels for EnPanne {
        async fn trouver_label(
            &self,
            _: &str,
            _: Option<&str>,
        ) -> Result<Option<LabelDuCatalogue>, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err("compte déconnecté".into())
        }
        async fn telecharger_image(&self, _: &str) -> Result<Vec<u8>, String> {
            Err("compte déconnecté".into())
        }
    }

    /// #7414 — une source en panne n'est pas sollicitée pour tous les labels :
    /// une tranche entièrement en erreur arrête la passe.
    #[tokio::test(flavor = "multi_thread")]
    async fn une_tranche_tout_en_erreur_arrete_la_passe_7414() {
        let cache_dir = tune_core::test_scratch::scratch_dir("labels-7414-panne");
        let cache = Arc::new(CacheLogosLabels::ouvrir(cache_dir.path()));
        let panne = Arc::new(EnPanne(AtomicUsize::new(0)));
        let demandes: Vec<Demande> = (0..3 * PASSE_MAX)
            .map(|i| Demande {
                nom: format!("Lbl 7414 panne {i:02}"),
                album_du_catalogue: None,
            })
            .collect();
        lancer_passe(cache, panne.clone(), demandes, Duration::ZERO, toujours)
            .unwrap()
            .await
            .unwrap();
        assert_eq!(panne.0.load(Ordering::SeqCst), PASSE_MAX);
    }

    // -----------------------------------------------------------------------
    // #7089 — les logos du web : Commons, Wikipédia, site officiel, Discogs
    // -----------------------------------------------------------------------

    /// Un faux MusicBrainz / Wikidata / Commons sous une seule racine, au
    /// format que [`tune_core::library::logos_web::Transport::sous_base`]
    /// attend. Journalise les chemins demandés.
    async fn web_simule() -> (String, Arc<Mutex<Vec<String>>>) {
        use axum::extract::{Path as Chemin, Query};
        use axum::routing::get;
        let journal = Arc::new(Mutex::new(Vec::new()));
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let racine = format!("http://{}", ecoute.local_addr().unwrap());
        let vignette = format!("{racine}/images/bn.png");
        let (j1, j2, j3, j4) = (
            journal.clone(),
            journal.clone(),
            journal.clone(),
            journal.clone(),
        );
        let app = axum::Router::new()
            .route(
                "/mb/release/{id}",
                get(move |Chemin(id): Chemin<String>| {
                    j1.lock().unwrap().push(format!("release/{id}"));
                    async move {
                        Json(json!({"id": id, "label-info": [
                            {"label": {"id": "mb-bn-7089", "name": "Blue Note 7089"}}]}))
                    }
                }),
            )
            .route(
                "/mb/label/{id}",
                get(move |Chemin(id): Chemin<String>| {
                    j2.lock().unwrap().push(format!("label/{id}"));
                    async move {
                        Json(json!({"id": id, "relations": [
                            {"type": "wikidata", "url": {"resource": "https://www.wikidata.org/wiki/Q885833"}}]}))
                    }
                }),
            )
            .route(
                "/wikidata",
                get(|| async {
                    Json(json!({"entities": {"Q885833": {"claims": {"P154": [
                        {"rank": "normal", "mainsnak": {"datavalue": {"value": "Blue Note Records.svg"}}}]}}}}))
                }),
            )
            .route(
                "/commons",
                get(move |Query(q): Query<HashMap<String, String>>| {
                    j3.lock().unwrap().push(format!("commons {}", q["titles"]));
                    let vignette = vignette.clone();
                    async move {
                        Json(json!({"query": {"pages": [{
                            "title": "File:Blue Note Records.svg",
                            "categories": [{"title": "Category:PD-self"}],
                            "imageinfo": [{
                                "thumburl": vignette,
                                "descriptionurl": "https://commons.wikimedia.org/wiki/File:Blue_Note_Records.svg",
                                "extmetadata": {
                                    "License": {"value": "pd"},
                                    "LicenseShortName": {"value": "Public domain"},
                                    "Artist": {"value": "Reid Miles"}}}]}]}}))
                    }
                }),
            )
            .route(
                "/images/bn.png",
                get(move || {
                    j4.lock().unwrap().push("image".into());
                    async { PNG.to_vec() }
                }),
            );
        tokio::spawn(async move {
            axum::serve(ecoute, app).await.unwrap();
        });
        (racine, journal)
    }

    /// De bout en bout, par le VRAI routeur : la première liste lance la
    /// passe du web (release → label → Wikidata → Commons), la suivante
    /// rend le logo libre ET son attribution — douteux compris.
    #[tokio::test(flavor = "multi_thread")]
    async fn la_liste_rend_le_logo_libre_de_commons_avec_son_attribution_7089() {
        let (racine, journal) = web_simule().await;
        tune_core::services_factices::substituer_base(
            tune_core::library::logos_web::BASE_FACTICE,
            &racine,
        );
        let dossier = tune_core::test_scratch::scratch_dir("labels-7089-route");
        let chemin = dossier.path().join("tune.db");
        let (app, state) = demarrer(chemin.to_str().unwrap());
        state
            .backend
            .execute(
                "INSERT INTO albums (title, artist_id, source, track_count, label, musicbrainz_release_id) \
                 VALUES ('Blue Train', NULL, 'local', 0, 'Blue Note 7089', 'rel-7089')",
                &[],
            )
            .unwrap();

        let premiere = labels(&app).await;
        let bn = premiere
            .iter()
            .find(|l| l["name"] == "Blue Note 7089")
            .unwrap();
        assert!(
            bn.get("image_url").is_none(),
            "la liste n'attend jamais la passe"
        );

        let web = cache_web_de(&crate::routes::library::artwork_cache_dir());
        let debut = std::time::Instant::now();
        // Une passe d'un autre banc peut tenir l'index : chaque liste relance
        // la nôtre dès qu'aucune ne tourne.
        while web.entree("Blue Note 7089").is_none() {
            assert!(
                debut.elapsed() < Duration::from_secs(60),
                "la passe n'a rien inscrit"
            );
            tokio::time::sleep(Duration::from_millis(300)).await;
            labels(&app).await;
        }

        let seconde = labels(&app).await;
        let bn = seconde
            .iter()
            .find(|l| l["name"] == "Blue Note 7089")
            .unwrap();
        assert!(
            bn["image_url"]
                .as_str()
                .is_some_and(|u| u.starts_with("/api/v1/library/artwork/")),
            "{bn}"
        );
        assert_eq!(bn["image_credit"]["provider"], "Wikimedia Commons");
        assert_eq!(bn["image_credit"]["source"], "commons");
        assert_eq!(bn["image_credit"]["license_status"], "free");
        assert_eq!(bn["image_credit"]["license"], "Public domain");
        assert_eq!(bn["image_credit"]["author"], "Reid Miles");
        assert_eq!(bn["image_credit"]["file"], "Blue Note Records.svg");
        assert_eq!(
            bn["image_credit"]["source_url"],
            "https://commons.wikimedia.org/wiki/File:Blue_Note_Records.svg"
        );
        assert_eq!(bn["image_credit"]["doubtful"], true, "PD-self : à vérifier");
        assert!(
            bn.get("qobuz_label_id").is_none(),
            "un MBID n'est pas un identifiant Qobuz"
        );
        let j = journal.lock().unwrap().clone();
        assert_eq!(&j[..2], ["release/rel-7089", "label/mb-bn-7089"], "{j:?}");
        assert!(j.contains(&"image".to_string()));
    }

    /// Ordre de préférence : le logo du web prime sur Qobuz ; un logo Qobuz
    /// seul est rendu avec son attribution « Qobuz, licence inconnue » ; un
    /// logo du web sans attribution n'est jamais rendu ; et le web est demandé
    /// même pour un label qui a déjà un logo Qobuz.
    #[test]
    fn le_web_prime_sur_qobuz_et_chaque_logo_porte_son_attribution_7089() {
        let dossier = tune_core::test_scratch::scratch_dir("labels-7089-ordre");
        let qobuz = CacheLogosLabels::ouvrir(dossier.path());
        let web = CacheLogosLabels::ouvrir_index(dossier.path(), FICHIER_INDEX_WEB);
        let condensat =
            tune_core::library::artwork::cache_fetched_image(PNG, dossier.path(), "png").unwrap();
        let entree = |attribution: Option<Attribution>| EntreeLogo {
            statut: Statut::Trouve,
            qobuz_label_id: None,
            mb_label_id: None,
            attribution,
            image: Some(condensat.clone()),
            verifie_le: maintenant(),
            communaute: false,
            partage: false,
        };
        let wikipedia = Attribution {
            source: SourceDuLogo::Wikipedia,
            statut_licence: StatutDeLicence::NonLibreIdentification,
            fournisseur: "Wikipedia".into(),
            licence: Some("Fair use".into()),
            licence_url: None,
            auteur: None,
            source_url: "https://en.wikipedia.org/wiki/File:ECM.png".into(),
            fichier: Some("ECM.png".into()),
            douteux: false,
        };
        let discogs = Attribution {
            source: SourceDuLogo::Discogs,
            statut_licence: StatutDeLicence::Inconnue,
            fournisseur: "Discogs".into(),
            licence: None,
            licence_url: None,
            auteur: None,
            source_url: "https://www.discogs.com/label/281".into(),
            fichier: None,
            douteux: false,
        };
        qobuz.inscrire("Les deux", entree(None));
        qobuz.inscrire("Qobuz seul", entree(None));
        web.inscrire("Les deux", entree(Some(wikipedia)));
        web.inscrire("Discogs seul", entree(Some(discogs)));
        web.inscrire("Sans attribution", entree(None));
        let label = |nom: &str| LabelDeBibliotheque {
            nom: nom.into(),
            album_count: 1,
            album_qobuz: None,
            release_mb: Some("rel".into()),
        };

        let d = element(&qobuz, &web, &label("Les deux"));
        assert_eq!(
            d["image_credit"]["source"], "wikipedia",
            "le web prime : {d}"
        );
        assert_eq!(
            d["image_credit"]["license_status"],
            "non_free_identification"
        );
        assert_eq!(d["image_credit"]["license"], "Fair use");
        let q = element(&qobuz, &web, &label("Qobuz seul"));
        assert!(q.get("image_url").is_some());
        assert_eq!(
            q["image_credit"],
            json!({"source": "qobuz", "provider": "Qobuz", "license_status": "unknown"})
        );
        let g = element(&qobuz, &web, &label("Discogs seul"));
        assert_eq!(g["image_credit"]["source"], "discogs");
        assert_eq!(g["image_credit"]["license_status"], "unknown");
        assert!(g["image_credit"].get("license").is_none());
        assert!(g["image_credit"].get("doubtful").is_none());
        let s = element(&qobuz, &web, &label("Sans attribution"));
        assert!(
            s.get("image_url").is_none(),
            "jamais un logo du web sans attribution : {s}"
        );

        let demandes = demandes_web(&[label("Les deux"), label("Discogs seul")]);
        assert_eq!(
            demandes.iter().map(|d| d.nom.as_str()).collect::<Vec<_>>(),
            ["Les deux", "Discogs seul"],
            "le web est demandé même quand Qobuz a un logo"
        );
        assert_eq!(demandes[0].album_du_catalogue.as_deref(), Some("rel"));
    }

    /// La liste des labels porte le MBID d'une release de la bibliothèque.
    #[tokio::test(flavor = "multi_thread")]
    async fn la_liste_des_labels_porte_un_mbid_de_release_7089() {
        let dossier = tune_core::test_scratch::scratch_dir("labels-7089-mbid");
        let chemin = dossier.path().join("tune.db");
        let (_app, state) = demarrer(chemin.to_str().unwrap());
        state
            .backend
            .execute_batch(
                "INSERT INTO albums (title, source, track_count, label, musicbrainz_release_id) VALUES
                   ('A', 'local', 0, 'Lbl 7089', ''),
                   ('B', 'local', 0, 'Lbl 7089', 'rel-b'),
                   ('C', 'local', 0, 'Autre 7089', NULL);",
            )
            .unwrap();
        let liste = AlbumRepo::with_backend(state.backend.clone())
            .labels_de_la_bibliotheque()
            .unwrap();
        let par = |n: &str| {
            liste
                .iter()
                .find(|l| l.nom == n)
                .unwrap()
                .release_mb
                .clone()
        };
        assert_eq!(par("Lbl 7089").as_deref(), Some("rel-b"));
        assert_eq!(par("Autre 7089"), None);
    }

    // -----------------------------------------------------------------------
    // #6782 — les artistes du même label
    // -----------------------------------------------------------------------

    fn artiste(state: &AppState, id: i64, nom: &str) {
        state
            .backend
            .execute(
                "INSERT INTO artists (id, name) VALUES (?, ?)",
                &[&id as &dyn ToSqlValue, &nom],
            )
            .unwrap();
    }

    fn album_de(state: &AppState, titre: &str, artiste: i64, label: &str) {
        state
            .backend
            .execute(
                "INSERT INTO albums (title, artist_id, source, track_count, label) \
                 VALUES (?, ?, 'local', 0, ?)",
                &[&titre as &dyn ToSqlValue, &artiste, &label],
            )
            .unwrap();
    }

    /// Sans compte Qobuz connecté (démarrage neuf) : la bibliothèque seule,
    /// ni `decouvrir` ni `qobuz_label_id` — et aucun catalogue construit.
    #[tokio::test(flavor = "multi_thread")]
    async fn sans_qobuz_la_page_du_label_ne_montre_que_la_bibliotheque() {
        let dossier = tune_core::test_scratch::scratch_dir("labels-6782-route");
        let chemin = dossier.path().join("tune.db");
        let (app, state) = demarrer(chemin.to_str().unwrap());
        artiste(&state, 1, "Keith Jarrett");
        artiste(&state, 2, "Jan Garbarek");
        album_de(&state, "Köln", 1, "ECM 6782");
        album_de(&state, "Facing You", 1, "ECM 6782");
        album_de(&state, "Officium", 2, "ECM 6782");
        album_de(&state, "Autre", 2, "Autre 6782");

        let reponse = app
            .clone()
            .oneshot(
                Request::get("/api/v1/library/labels/ECM%206782/artists")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(reponse.status().is_success(), "{}", reponse.status());
        let corps: Value = serde_json::from_slice(
            &axum::body::to_bytes(reponse.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(corps["name"], "ECM 6782");
        let noms: Vec<(&str, i64)> = corps["bibliotheque"]
            .as_array()
            .expect("toujours une liste")
            .iter()
            .map(|a| {
                (
                    a["name"].as_str().unwrap(),
                    a["album_count"].as_i64().unwrap(),
                )
            })
            .collect();
        assert_eq!(noms, vec![("Keith Jarrett", 2), ("Jan Garbarek", 1)]);
        assert!(
            corps.get("decouvrir").is_none(),
            "absente sans Qobuz : {corps}"
        );
        assert!(corps.get("qobuz_label_id").is_none());
        assert!(qobuz_connecte(&state).await.is_none(), "aucun catalogue");
    }

    struct FauxQobuz {
        rapprochements: AtomicUsize,
        lectures: Mutex<Vec<String>>,
    }

    impl FauxQobuz {
        fn nouveau() -> Arc<Self> {
            Arc::new(Self {
                rapprochements: AtomicUsize::new(0),
                lectures: Mutex::new(Vec::new()),
            })
        }
        fn pour_le_label(self: &Arc<Self>) -> Option<QobuzPourLeLabel> {
            Some(QobuzPourLeLabel {
                labels: self.clone(),
                catalogue: self.clone(),
            })
        }
    }

    #[async_trait::async_trait]
    impl CatalogueDeLabels for FauxQobuz {
        async fn trouver_label(
            &self,
            nom: &str,
            album: Option<&str>,
        ) -> Result<Option<LabelDuCatalogue>, String> {
            self.rapprochements.fetch_add(1, Ordering::SeqCst);
            Ok((album == Some("q-ecm")).then(|| LabelDuCatalogue {
                id: "9".into(),
                nom: nom.into(),
                image_url: None,
                attribution: None,
            }))
        }
        async fn telecharger_image(&self, _: &str) -> Result<Vec<u8>, String> {
            Ok(PNG.to_vec())
        }
    }

    fn sa(id: &str, artiste: &str, an: u32) -> StreamAlbum {
        StreamAlbum {
            ai_generated: None,
            release_type: None,
            id: id.into(),
            title: format!("T{id}"),
            artist: artiste.into(),
            artist_id: None,
            cover_path: None,
            year: Some(an),
            track_count: 1,
            quality: None,
            released_at: None,
        }
    }

    #[async_trait::async_trait]
    impl CatalogueDuLabel for FauxQobuz {
        async fn albums_du_label(&self, label_id: &str) -> Result<Vec<StreamAlbum>, String> {
            self.lectures.lock().unwrap().push(label_id.to_string());
            Ok(vec![
                sa("1", "Keith Jarrett", 1975),
                sa("2", "Jan Garbarek", 1994),
                sa("3", "Jan Garbarek", 2002),
                sa("4", "Steve Reich", 1978),
            ])
        }
    }

    fn ecm(album_qobuz: Option<&str>) -> LabelDeBibliotheque {
        LabelDeBibliotheque {
            nom: "ECM".into(),
            album_count: 3,
            album_qobuz: album_qobuz.map(String::from),
            release_mb: None,
        }
    }

    /// Le label rapproché par le lot L : son identifiant sert tel quel, sans
    /// nouveau rapprochement — même quand la passe des LOGOS voudrait le
    /// redemander (ici, une image en erreur depuis plus d'un jour) ; les
    /// artistes déjà présents sont retirés ; le catalogue n'est lu qu'une fois.
    #[tokio::test]
    async fn le_label_rapproche_donne_ses_artistes_a_decouvrir() {
        let dossier = tune_core::test_scratch::scratch_dir("labels-6782-connu");
        let logos = CacheLogosLabels::ouvrir(dossier.path());
        logos.inscrire(
            "ECM",
            EntreeLogo {
                statut: Statut::Erreur,
                qobuz_label_id: Some("9".into()),
                mb_label_id: None,
                attribution: None,
                image: None,
                verifie_le: maintenant() - tune_core::library::logos_labels::DELAI_ERREUR_S - 1,
                communaute: false,
                partage: false,
            },
        );
        let decouverte = CacheDecouverte::default();
        let faux = FauxQobuz::nouveau();
        let deja: HashSet<String> = ["keith jarrett".to_string()].into();
        for _ in 0..3 {
            let (id, liste) =
                section_decouvrir(&logos, &decouverte, faux.pour_le_label(), &ecm(None), &deja)
                    .await
                    .expect("une section");
            assert_eq!(id, "9");
            let noms: Vec<(&str, usize)> = liste
                .iter()
                .map(|a| (a.name.as_str(), a.album_count))
                .collect();
            assert_eq!(noms, vec![("Jan Garbarek", 2), ("Steve Reich", 1)]);
        }
        assert_eq!(faux.rapprochements.load(Ordering::SeqCst), 0);
        assert_eq!(*faux.lectures.lock().unwrap(), vec!["9"], "pas de rafale");
    }

    /// Un label que la liste n'a pas encore vu (page ouverte depuis une fiche
    /// album) : rapproché ici, une fois, par l'album Qobuz de la bibliothèque.
    /// Un label sans correspondant Qobuz : pas de section, et pas de nouveau
    /// rapprochement à l'ouverture suivante.
    #[tokio::test]
    async fn un_label_encore_inconnu_est_rapproche_une_seule_fois() {
        let dossier = tune_core::test_scratch::scratch_dir("labels-6782-inconnu");
        let logos = CacheLogosLabels::ouvrir(dossier.path());
        let decouverte = CacheDecouverte::default();
        let faux = FauxQobuz::nouveau();
        let deja = HashSet::new();
        let (id, liste) = section_decouvrir(
            &logos,
            &decouverte,
            faux.pour_le_label(),
            &ecm(Some("q-ecm")),
            &deja,
        )
        .await
        .expect("rapproché par l'album");
        assert_eq!((id.as_str(), liste.len()), ("9", 3));
        assert_eq!(faux.rapprochements.load(Ordering::SeqCst), 1);

        let sans = LabelDeBibliotheque {
            nom: "Sans Qobuz 6782".into(),
            album_count: 1,
            album_qobuz: None,
            release_mb: None,
        };
        for _ in 0..2 {
            assert!(
                section_decouvrir(&logos, &decouverte, faux.pour_le_label(), &sans, &deja)
                    .await
                    .is_none()
            );
        }
        assert_eq!(
            faux.rapprochements.load(Ordering::SeqCst),
            2,
            "l'absence est gardée : un seul rapprochement de plus"
        );
        assert_eq!(*faux.lectures.lock().unwrap(), vec!["9"]);

        // Sans Qobuz : rien, et aucun appel.
        assert!(
            section_decouvrir(&logos, &decouverte, None, &ecm(Some("q-ecm")), &deja)
                .await
                .is_none()
        );
        assert_eq!(faux.rapprochements.load(Ordering::SeqCst), 2);
    }

    // ── #7414 : le partage communautaire dans la passe du web ──────────────

    fn attribution_web() -> Attribution {
        Attribution {
            source: SourceDuLogo::Commons,
            statut_licence: StatutDeLicence::Libre,
            fournisseur: "Wikimedia Commons".into(),
            licence: Some("CC0".into()),
            licence_url: None,
            auteur: None,
            source_url: "https://commons.wikimedia.org/wiki/File:Verve.png".into(),
            fichier: None,
            douteux: false,
        }
    }

    /// Le web : il connaît tout label, avec un logo attribué, et compte les
    /// noms demandés.
    #[derive(Default)]
    struct WebAttribue(Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl CatalogueDeLabels for WebAttribue {
        async fn trouver_label(
            &self,
            nom: &str,
            _: Option<&str>,
        ) -> Result<Option<LabelDuCatalogue>, String> {
            self.0.lock().unwrap().push(nom.to_string());
            Ok(Some(LabelDuCatalogue {
                id: "mb-1".into(),
                nom: nom.into(),
                image_url: Some("https://upload.test/l.png".into()),
                attribution: Some(attribution_web()),
            }))
        }
        async fn telecharger_image(&self, _: &str) -> Result<Vec<u8>, String> {
            Ok(PNG.to_vec())
        }
    }

    /// Le partage : il connaît « ECM Records », peut être muet, accepte ou
    /// non les propositions, et garde tout ce qu'on lui envoie.
    #[derive(Default)]
    struct FauxPartage {
        consultations: Mutex<Vec<Vec<String>>>,
        propositions: Mutex<Vec<PropositionDeLogo>>,
        muet: bool,
        consentement: bool,
    }

    #[async_trait::async_trait]
    impl LogosPartages for FauxPartage {
        async fn consulter(
            &self,
            labels: &[LabelAConsulter],
        ) -> Result<Vec<tune_core::library::logos_labels::LogoPartage>, String> {
            self.consultations
                .lock()
                .unwrap()
                .push(labels.iter().map(|l| l.nom.clone()).collect());
            if self.muet {
                return Err("délai dépassé".into());
            }
            Ok(labels
                .iter()
                .filter(|l| l.nom == "ECM Records")
                .map(|l| tune_core::library::logos_labels::LogoPartage {
                    nom: l.nom.clone(),
                    image_url: "https://mozaiklabs.fr/api/v1/community/label-logos/1/ab".into(),
                    mb_label_id: None,
                    attribution: attribution_web(),
                })
                .collect())
        }
        async fn telecharger(&self, _: &str) -> Result<Vec<u8>, String> {
            Ok(PNG.to_vec())
        }
        fn peut_proposer(&self) -> bool {
            self.consentement
        }
        async fn proposer(&self, p: &PropositionDeLogo) -> ProposeAuPartage {
            self.propositions.lock().unwrap().push(p.clone());
            ProposeAuPartage::Traitee
        }
    }

    fn demandes_de(noms: &[&str]) -> Vec<Demande> {
        noms.iter()
            .map(|n| Demande {
                nom: (*n).into(),
                album_du_catalogue: None,
            })
            .collect()
    }

    /// Le partage est demandé AVANT le web, en UN appel pour tous les labels
    /// inconnus ; ce qu'il rend n'est pas cherché sur le web, et n'est
    /// jamais reproposé ; ce que le web trouve est proposé, sous son nom
    /// affiché, une seule fois.
    #[tokio::test(flavor = "multi_thread")]
    async fn le_partage_passe_avant_le_web_et_recoit_ce_que_le_web_trouve_7414() {
        let dossier = tune_core::test_scratch::scratch_dir("labels-7414-partage");
        let web = Arc::new(CacheLogosLabels::ouvrir_index(
            dossier.path(),
            FICHIER_INDEX_WEB,
        ));
        let qobuz = Arc::new(CacheLogosLabels::ouvrir(dossier.path()));
        qobuz.inscrire(
            "Verve",
            EntreeLogo {
                statut: Statut::Absent,
                qobuz_label_id: Some("q-7".into()),
                mb_label_id: None,
                attribution: None,
                image: None,
                verifie_le: maintenant(),
                communaute: false,
                partage: false,
            },
        );
        let catalogue = Arc::new(WebAttribue::default());
        let partage = Arc::new(FauxPartage {
            consentement: true,
            ..Default::default()
        });

        let trouves = lancer_passe_web(
            web.clone(),
            qobuz.clone(),
            catalogue.clone(),
            Some(partage.clone()),
            demandes_de(&["ECM Records", "Verve", "Warp"]),
            Duration::ZERO,
            toujours,
        )
        .expect("une passe")
        .await
        .unwrap();

        assert_eq!(trouves, 3);
        assert_eq!(
            *partage.consultations.lock().unwrap(),
            vec![vec![
                "ECM Records".to_string(),
                "Verve".into(),
                "Warp".into()
            ]],
            "UN appel au partage, avant le web, pour tous les labels inconnus"
        );
        assert_eq!(
            *catalogue.0.lock().unwrap(),
            vec!["Verve".to_string(), "Warp".into()],
            "le label rendu par le partage n'est pas cherché sur le web"
        );
        assert!(web.entree("ECM Records").unwrap().communaute);

        let propositions = partage.propositions.lock().unwrap().clone();
        let noms: Vec<&str> = propositions.iter().map(|p| p.nom.as_str()).collect();
        assert_eq!(
            noms,
            vec!["Verve", "Warp"],
            "jamais le logo reçu du partage"
        );
        assert_eq!(propositions[0].qobuz_label_id.as_deref(), Some("q-7"));
        assert_eq!(propositions[0].mb_label_id.as_deref(), Some("mb-1"));
        assert_eq!(propositions[0].octets, PNG);
        assert!(web.entree("Verve").unwrap().partage);

        // Une seconde ouverture : rien à demander, rien à reproposer.
        assert!(
            lancer_passe_web(
                web.clone(),
                qobuz,
                catalogue,
                Some(partage.clone()),
                demandes_de(&["ECM Records", "Verve", "Warp"]),
                Duration::ZERO,
                toujours,
            )
            .is_none()
        );
        assert_eq!(partage.propositions.lock().unwrap().len(), 2);

        // Et la liste dit d'où vient le logo.
        let label = |nom: &str| LabelDeBibliotheque {
            nom: nom.into(),
            album_count: 1,
            album_qobuz: None,
            release_mb: None,
        };
        let qobuz_vide = CacheLogosLabels::ouvrir(dossier.path());
        assert_eq!(
            element(&qobuz_vide, &web, &label("ECM Records"))["image_credit"]["via"],
            "community"
        );
        assert!(
            element(&qobuz_vide, &web, &label("Verve"))["image_credit"]
                .get("via")
                .is_none()
        );
    }

    /// Sans consentement communautaire, rien n'est proposé ; la lecture du
    /// partage, elle, a lieu.
    #[tokio::test(flavor = "multi_thread")]
    async fn sans_consentement_rien_n_est_propose_7414() {
        let dossier = tune_core::test_scratch::scratch_dir("labels-7414-sans-consentement");
        let web = Arc::new(CacheLogosLabels::ouvrir_index(
            dossier.path(),
            FICHIER_INDEX_WEB,
        ));
        let qobuz = Arc::new(CacheLogosLabels::ouvrir(dossier.path()));
        let partage = Arc::new(FauxPartage::default());

        lancer_passe_web(
            web.clone(),
            qobuz,
            Arc::new(WebAttribue::default()),
            Some(partage.clone()),
            demandes_de(&["Verve"]),
            Duration::ZERO,
            toujours,
        )
        .expect("une passe")
        .await
        .unwrap();

        assert_eq!(partage.consultations.lock().unwrap().len(), 1);
        assert!(partage.propositions.lock().unwrap().is_empty());
        assert!(!web.entree("Verve").unwrap().partage);
    }

    /// Un partage muet ne bloque rien : le web prend tous les labels.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_partage_muet_laisse_tout_au_web_7414() {
        let dossier = tune_core::test_scratch::scratch_dir("labels-7414-muet");
        let web = Arc::new(CacheLogosLabels::ouvrir_index(
            dossier.path(),
            FICHIER_INDEX_WEB,
        ));
        let qobuz = Arc::new(CacheLogosLabels::ouvrir(dossier.path()));
        let catalogue = Arc::new(WebAttribue::default());
        let partage = Arc::new(FauxPartage {
            muet: true,
            ..Default::default()
        });

        let trouves = lancer_passe_web(
            web.clone(),
            qobuz,
            catalogue.clone(),
            Some(partage),
            demandes_de(&["ECM Records", "Verve"]),
            Duration::ZERO,
            toujours,
        )
        .expect("une passe")
        .await
        .unwrap();

        assert_eq!(trouves, 2);
        assert_eq!(catalogue.0.lock().unwrap().len(), 2);
        assert!(!web.entree("ECM Records").unwrap().communaute);
    }
}

/// Écran « État du serveur » (01/11/2026) — la passe des logos lancée par
/// `POST /library/labels/logos/enrich`, suivie par
/// `GET /library/labels/logos/status` et au registre `background_tasks`.
#[cfg(test)]
mod passe_explicite_des_logos {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tokio::sync::Semaphore;
    use tower::ServiceExt;
    use tune_core::db::backend::ToSqlValue;
    use tune_core::library::logos_labels::LabelDuCatalogue;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n-logo-etat-du-serveur-";

    fn demarrer(chemin: &str) -> (axum::Router, AppState) {
        let state = AppState::new(chemin, 0, Default::default()).expect("démarrage");
        (crate::routes::router(state.clone()), state)
    }

    /// Un nom de label unique : l'index des logos est global au processus.
    fn nom(base: &str) -> String {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{base} {n}")
    }

    fn album(state: &AppState, titre: &str, label: &str) {
        state
            .backend
            .execute(
                "INSERT INTO albums (title, artist_id, source, track_count, year, label) \
                 VALUES (?, NULL, 'local', 0, 1970, ?)",
                &[&titre as &dyn ToSqlValue, &label],
            )
            .unwrap();
    }

    fn labels_de(state: &AppState) -> Vec<LabelDeBibliotheque> {
        AlbumRepo::with_backend(state.backend.clone())
            .labels_de_la_bibliotheque()
            .unwrap()
    }

    /// Un catalogue du web qui ne répond qu'un label par jeton : la passe
    /// reste en cours tant que l'essai ne la laisse pas avancer.
    struct Retenu(Semaphore);

    #[async_trait::async_trait]
    impl CatalogueDeLabels for Retenu {
        async fn trouver_label(
            &self,
            nom: &str,
            _: Option<&str>,
        ) -> Result<Option<LabelDuCatalogue>, String> {
            self.0.acquire().await.map_err(|e| e.to_string())?.forget();
            Ok(Some(LabelDuCatalogue {
                id: "mb-etat".into(),
                nom: nom.into(),
                image_url: Some("https://upload.test/etat.png".into()),
                attribution: Some(Attribution {
                    source: SourceDuLogo::Commons,
                    fournisseur: "Wikimedia Commons".into(),
                    statut_licence: StatutDeLicence::Libre,
                    licence: Some("CC0".into()),
                    licence_url: None,
                    auteur: None,
                    source_url: "https://commons.test/etat".into(),
                    fichier: None,
                    douteux: false,
                }),
            }))
        }
        async fn telecharger_image(&self, _: &str) -> Result<Vec<u8>, String> {
            Ok(PNG.to_vec())
        }
    }

    fn sources(web: Arc<Retenu>) -> SourcesDeLogos {
        SourcesDeLogos {
            web: Some(web as Arc<dyn CatalogueDeLabels>),
            qobuz: None,
            partage: None,
            pause: Duration::ZERO,
        }
    }

    async fn requete(app: &axum::Router, methode: &str, chemin: &str) -> (StatusCode, Value) {
        let reponse = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(methode)
                    .uri(chemin)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let statut = reponse.status();
        let corps = axum::body::to_bytes(reponse.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            statut,
            serde_json::from_slice(&corps).unwrap_or(Value::Null),
        )
    }

    async fn statut(app: &axum::Router) -> Value {
        let (code, corps) = requete(app, "GET", "/api/v1/library/labels/logos/status").await;
        assert_eq!(code, StatusCode::OK, "{corps}");
        corps
    }

    /// Attend (10 s au plus) que `condition` tienne sur le statut.
    async fn attendre(app: &axum::Router, condition: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..200 {
            let s = statut(app).await;
            if condition(&s) {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("condition jamais atteinte : {}", statut(app).await);
    }

    /// LE contrat de l'écran : la passe s'inscrit sous `label_logos` avec son
    /// total, avance label par label, refuse un second lancement (409 par
    /// la route, `DejaEnCours` par la fonction) sans rien relancer, puis
    /// range son bilan ; le statut compte les labels avec et sans logo.
    #[tokio::test(flavor = "multi_thread")]
    async fn la_passe_s_inscrit_avance_refuse_un_doublon_et_range_son_bilan() {
        let dossier = tune_core::test_scratch::scratch_dir("labels-etat-passe");
        let chemin = dossier.path().join("tune.db");
        let (app, state) = demarrer(chemin.to_str().unwrap());
        for i in 0..3 {
            album(&state, &format!("A{i}"), &nom(&format!("Etat {i}")));
        }

        let avant = statut(&app).await;
        assert_eq!(avant["labels_total"], 3, "{avant}");
        assert_eq!(avant["with_logo"], 0, "{avant}");
        assert_eq!(avant["without_logo"], 3, "{avant}");
        assert_eq!(avant["running"], false, "{avant}");
        assert!(avant["last_run"].is_null(), "{avant}");
        assert!(avant.get("progress").is_none(), "{avant}");

        let web = Arc::new(Retenu(Semaphore::new(0)));
        assert_eq!(
            lancer_passes_logos(&state, labels_de(&state), sources(web.clone())),
            LancementLogos::Lancee { a_traiter: 3 }
        );
        let tache = state
            .background_tasks
            .get(TACHE_LOGOS_LABELS)
            .expect("la passe est inscrite au registre sous `label_logos`");
        assert_eq!(tache.progress.as_ref().map(|p| p.total), Some(3));

        // Un second lancement pendant la passe : rien de plus.
        assert_eq!(
            lancer_passes_logos(&state, labels_de(&state), sources(web.clone())),
            LancementLogos::DejaEnCours
        );
        let (code, corps) = requete(&app, "POST", "/api/v1/library/labels/logos/enrich").await;
        assert_eq!(code, StatusCode::CONFLICT, "{corps}");
        assert_eq!(corps["status"], "already_running", "{corps}");

        let en_cours = statut(&app).await;
        assert_eq!(en_cours["running"], true, "{en_cours}");
        assert_eq!(en_cours["progress"]["total"], 3, "{en_cours}");

        // Un label résolu : l'avancement le dit.
        web.0.add_permits(1);
        let un = attendre(&app, |s| s["progress"]["processed"].as_u64() >= Some(1)).await;
        assert_eq!(un["running"], true, "{un}");
        assert_eq!(un["with_logo"], 1, "{un}");

        web.0.add_permits(2);
        let fin = attendre(&app, |s| s["running"] == false).await;
        assert!(fin.get("progress").is_none(), "{fin}");
        assert_eq!(fin["with_logo"], 3, "{fin}");
        assert_eq!(fin["without_logo"], 0, "{fin}");
        let bilan = &fin["last_run"];
        assert_eq!(bilan["to_process"], 3, "{fin}");
        assert_eq!(bilan["processed"], 3, "{fin}");
        assert_eq!(bilan["found"], 3, "{fin}");
        assert_eq!(bilan["with_logo"], 3, "{fin}");
        assert!(
            bilan["finished_at"].as_u64() >= bilan["started_at"].as_u64(),
            "{fin}"
        );
        assert!(state.background_tasks.get(TACHE_LOGOS_LABELS).is_none());
    }

    /// Rien à demander : rien n'est lancé, et le verrou est rendu (un
    /// lancement suivant n'est pas refusé « déjà en cours »).
    #[tokio::test(flavor = "multi_thread")]
    async fn rien_a_faire_ne_lance_rien_et_rend_le_verrou() {
        let dossier = tune_core::test_scratch::scratch_dir("labels-etat-rien");
        let chemin = dossier.path().join("tune.db");
        let (_app, state) = demarrer(chemin.to_str().unwrap());
        let web = Arc::new(Retenu(Semaphore::new(0)));
        for _ in 0..2 {
            assert_eq!(
                lancer_passes_logos(&state, Vec::new(), sources(web.clone())),
                LancementLogos::RienAFaire
            );
            assert!(state.background_tasks.get(TACHE_LOGOS_LABELS).is_none());
        }
        let sans = SourcesDeLogos {
            web: None,
            qobuz: None,
            partage: None,
            pause: Duration::ZERO,
        };
        assert_eq!(
            lancer_passes_logos(&state, labels_de(&state), sans),
            LancementLogos::SansSource
        );
        assert!(state.background_tasks.get(TACHE_LOGOS_LABELS).is_none());
    }

    /// Le refus « déjà en cours » de la ROUTE ne dépend d'aucune source : une
    /// passe inscrite suffit (celle d'une ouverture de l'onglet, par exemple).
    #[tokio::test(flavor = "multi_thread")]
    async fn la_route_refuse_un_second_lancement_en_409() {
        let dossier = tune_core::test_scratch::scratch_dir("labels-etat-409");
        let chemin = dossier.path().join("tune.db");
        let (app, state) = demarrer(chemin.to_str().unwrap());
        album(&state, "A", &nom("Etat 409"));
        let _passe = state
            .background_tasks
            .try_begin(TACHE_LOGOS_LABELS, "…", "enrichment")
            .unwrap();
        let (code, corps) = requete(&app, "POST", "/api/v1/library/labels/logos/enrich").await;
        assert_eq!(code, StatusCode::CONFLICT, "{corps}");
        assert_eq!(corps["status"], "already_running", "{corps}");
        // Et la liste des labels ne lance rien de plus non plus.
        let (code, _) = requete(&app, "GET", "/api/v1/library/labels").await;
        assert_eq!(code, StatusCode::OK);
        assert!(state.background_tasks.get(TACHE_LOGOS_LABELS).is_some());
    }

    /// Les nouvelles routes ne masquent pas la page d'un label nommé
    /// « logos » (`/labels/{nom}/artists`).
    #[tokio::test(flavor = "multi_thread")]
    async fn un_label_nomme_logos_garde_sa_page() {
        let dossier = tune_core::test_scratch::scratch_dir("labels-etat-nom-logos");
        let chemin = dossier.path().join("tune.db");
        let (app, _state) = demarrer(chemin.to_str().unwrap());
        let (code, corps) = requete(&app, "GET", "/api/v1/library/labels/logos/artists").await;
        assert_eq!(code, StatusCode::OK, "{corps}");
        assert_eq!(corps["name"], "logos", "{corps}");
    }
}
