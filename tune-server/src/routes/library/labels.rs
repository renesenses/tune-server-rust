//! `GET /library/labels` (#6736) — les labels de la bibliothèque, avec leur
//! logo Qobuz quand il est connu.
//!
//! La route n'attend jamais Qobuz : elle rend ce que l'index des logos sait
//! déjà ([`tune_core::library::logos_labels`]) et, si un compte Qobuz est
//! connecté, lance en arrière-plan une passe bornée sur les labels encore
//! inconnus. Le logo apparaît donc à une ouverture suivante de l'onglet.
//! Sans compte Qobuz (service absent, désactivé ou non connecté) : aucune
//! passe, aucun appel.

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
    CacheLogosLabels, CatalogueDeLabels, Demande, LabelDuCatalogue, PASSE_MAX, PAUSE_ENTRE_LABELS,
};
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
