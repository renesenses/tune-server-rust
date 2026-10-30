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
    Attribution, CacheLogosLabels, CatalogueDeLabels, Demande, LabelDuCatalogue, PASSE_MAX,
    PAUSE_ENTRE_LABELS, SourceDuLogo, Statut, StatutDeLicence,
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
        let mut trouves = 0;
        for (i, tranche) in a_faire.chunks(PASSE_MAX).enumerate() {
            if !continuer() {
                break;
            }
            if i > 0 && !pause.is_zero() {
                tokio::time::sleep(pause).await;
            }
            trouves += cache
                .resoudre(&*catalogue, tranche.to_vec(), maintenant, pause)
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
    }))
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
    if let Some(catalogue) = catalogue_web(&state) {
        lancer_passe(
            web,
            catalogue,
            demandes_web(&labels),
            PAUSE_ENTRE_LABELS,
            enrichissement_actif,
        );
    }
    if let Some(catalogue) = catalogue_qobuz(&state).await {
        lancer_passe(
            cache,
            catalogue,
            demandes_qobuz(&labels),
            PAUSE_ENTRE_LABELS,
            toujours,
        );
    }
    Json(Value::Array(items))
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
}
