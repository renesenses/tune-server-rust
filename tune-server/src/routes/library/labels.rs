//! `GET /library/labels` (#6736) — les labels de la bibliothèque, avec leur
//! logo Qobuz quand il est connu.
//!
//! La route n'attend jamais Qobuz : elle rend ce que l'index des logos sait
//! déjà ([`tune_core::library::logos_labels`]) et, si un compte Qobuz est
//! connecté, lance en arrière-plan une passe bornée sur les labels encore
//! inconnus. Le logo apparaît donc à une ouverture suivante de l'onglet.
//! Sans compte Qobuz (service absent, désactivé ou non connecté) : aucune
//! passe, aucun appel.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};
use tokio::sync::RwLock;
use tune_core::db::album_repo::{AlbumRepo, LabelDeBibliotheque};
use tune_core::library::logos_labels::{
    CacheLogosLabels, CatalogueDeLabels, Demande, LabelDuCatalogue, PASSE_MAX, PAUSE_ENTRE_LABELS,
};
use tune_core::streaming::StreamingService;
use tune_core::streaming::qobuz::QobuzService;
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

/// Le catalogue Qobuz à interroger, ou `None` sans compte Qobuz connecté —
/// et alors, aucune passe.
pub(crate) async fn catalogue_qobuz(state: &AppState) -> Option<Arc<dyn CatalogueDeLabels>> {
    let svc = state.services.lock().await.get_actif("qobuz").await?;
    drop(connecte(&svc).await.ok()?);
    Some(Arc::new(QobuzConnecte(svc)))
}

/// Une entrée de la liste : `image_url` et `qobuz_label_id` sont ABSENTS
/// quand rien n'est connu (jamais `null`, jamais une adresse qui rend 404).
pub(crate) fn element(cache: &CacheLogosLabels, label: &LabelDeBibliotheque) -> Value {
    let mut o = json!({ "name": label.nom, "album_count": label.album_count });
    if let Some(condensat) = cache.image(&label.nom) {
        o["image_url"] = json!(format!("/api/v1/library/artwork/{condensat}"));
    }
    if let Some(id) = cache.entree(&label.nom).and_then(|e| e.qobuz_label_id) {
        o["qobuz_label_id"] = json!(id);
    }
    o
}

/// Lance, si besoin et si aucune ne tourne, une passe bornée sur les labels
/// encore inconnus. Rend la tâche, pour les essais.
pub(crate) fn lancer_passe(
    cache: Arc<CacheLogosLabels>,
    catalogue: Arc<dyn CatalogueDeLabels>,
    labels: &[LabelDeBibliotheque],
    pause: Duration,
) -> Option<tokio::task::JoinHandle<usize>> {
    let demandes: Vec<Demande> = labels
        .iter()
        .map(|l| Demande {
            nom: l.nom.clone(),
            album_du_catalogue: l.album_qobuz.clone(),
        })
        .collect();
    let a_faire = cache.a_resoudre(&demandes, maintenant(), PASSE_MAX);
    if a_faire.is_empty() {
        return None;
    }
    Some(tokio::spawn(async move {
        let Some(_passe) = cache.reserver_passe() else {
            return 0;
        };
        cache
            .resoudre(&*catalogue, a_faire, maintenant, pause)
            .await
    }))
}

pub(super) async fn list_labels(State(state): State<AppState>) -> Json<Value> {
    let labels = AlbumRepo::with_backend(state.backend.clone())
        .labels_de_la_bibliotheque()
        .ou_defaut_journalise();
    let cache = cache_de(&super::artwork_cache_dir());
    let items: Vec<Value> = labels.iter().map(|l| element(&cache, l)).collect();
    if let Some(catalogue) = catalogue_qobuz(&state).await {
        lancer_passe(cache, catalogue, &labels, PAUSE_ENTRE_LABELS);
    }
    Json(Value::Array(items))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;
    use tune_core::db::backend::ToSqlValue;
    use tune_core::library::logos_labels::{EntreeLogo, Statut};

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
            }))
        }
        async fn telecharger_image(&self, _: &str) -> Result<Vec<u8>, String> {
            Ok(PNG.to_vec())
        }
    }

    /// La passe lancée par la liste : bornée à `PASSE_MAX`, l'album Qobuz de
    /// la bibliothèque transmis, et une seconde liste ne redemande rien.
    #[tokio::test(flavor = "multi_thread")]
    async fn la_passe_est_bornee_et_ne_redemande_pas_ce_qu_elle_sait() {
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

        let cache_dir = tune_core::test_scratch::scratch_dir("labels-6736-passe-cache");
        let cache = Arc::new(CacheLogosLabels::ouvrir(cache_dir.path()));
        let compteur = Arc::new(Compteur(AtomicUsize::new(0)));
        let trouves = lancer_passe(cache.clone(), compteur.clone(), &liste, Duration::ZERO)
            .expect("une passe")
            .await
            .unwrap();
        assert_eq!(trouves, 1);
        assert_eq!(
            compteur.0.load(Ordering::SeqCst),
            PASSE_MAX,
            "pas de rafale"
        );
        assert!(cache.image("Lbl 6736 passe 00").is_some());

        // La passe suivante ne reprend que les labels jamais vus.
        lancer_passe(cache.clone(), compteur.clone(), &liste, Duration::ZERO)
            .unwrap()
            .await
            .unwrap();
        assert_eq!(compteur.0.load(Ordering::SeqCst), 2 * PASSE_MAX);
        lancer_passe(cache.clone(), compteur.clone(), &liste, Duration::ZERO)
            .unwrap()
            .await
            .unwrap();
        assert_eq!(
            compteur.0.load(Ordering::SeqCst),
            20,
            "chaque label demandé une fois"
        );
        assert!(
            lancer_passe(cache, compteur.clone(), &liste, Duration::ZERO).is_none(),
            "tout est connu, absence comprise : plus aucun appel"
        );
    }
}
