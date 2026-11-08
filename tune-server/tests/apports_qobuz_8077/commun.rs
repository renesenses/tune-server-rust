//! #8077 — scénarios partagés par les épreuves SQLite (`apports_qobuz_8077`)
//! et PostgreSQL (`pg_apports_qobuz_8077`) : un album LOCAL complété depuis
//! Qobuz, par le VRAI routeur, contre une doublure axum de l'API Qobuz sur
//! `127.0.0.1` (`QobuzService::avec_bases_de_test`). Aucun appel réseau réel ;
//! jeton, identifiants, noms et adresses sont factices.
//!
//! Chaque scénario prend un `AppState` neuf (SQLite en mémoire, ou une base
//! PostgreSQL jetable) et n'attend aucun identifiant fixe.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::models::{Album, Artist, Track};
use tune_core::db::track_repo::TrackRepo;
use tune_core::streaming::StreamingService;
use tune_core::streaming::qobuz::{BasesQobuz, QobuzService};

pub type Etat = tune_server::state::AppState;

pub const UPC: &str = "0123456789012";
pub const ISRC_1: &str = "XXA0D2600001";
pub const ISRC_2: &str = "XXA0D2600002";

/// Ce que la doublure répond (par chemin), et ce qu'elle a reçu.
#[derive(Clone, Default)]
pub struct Doublure {
    reponses: Arc<HashMap<&'static str, Value>>,
    pub appels: Arc<Mutex<Vec<String>>>,
}

impl Doublure {
    pub fn nombre_d_appels(&self) -> usize {
        self.appels.lock().unwrap().len()
    }
}

async fn repondre(d: Doublure, chemin: &'static str, q: HashMap<String, String>) -> Json<Value> {
    d.appels.lock().unwrap().push(format!(
        "{chemin}?{}",
        q.get("query")
            .or(q.get("album_id"))
            .cloned()
            .unwrap_or_default()
    ));
    Json(d.reponses.get(chemin).cloned().unwrap_or(json!({})))
}

pub async fn doublure_qobuz(reponses: HashMap<&'static str, Value>) -> (String, Doublure) {
    let d = Doublure {
        reponses: Arc::new(reponses),
        appels: Arc::default(),
    };
    let mut app = Router::new();
    for chemin in ["/album/search", "/track/search", "/album/get"] {
        app = app.route(
            chemin,
            get(
                move |State(s): State<Doublure>, Query(q): Query<HashMap<String, String>>| {
                    repondre(s, chemin, q)
                },
            ),
        );
    }
    let app = app.with_state(d.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    (format!("http://{adresse}"), d)
}

/// Branche Qobuz (connecté ou non) sur la doublure.
pub async fn brancher_qobuz(state: &Etat, base: &str, connecte: bool) {
    let mut svc = QobuzService::avec_bases_de_test(
        "app-essai".into(),
        "secret-essai".into(),
        BasesQobuz {
            api: base.to_string(),
            proxy: base.to_string(),
            config_distante: base.to_string(),
        },
    );
    if connecte {
        assert!(svc.restore_tokens(&json!({
            "user_auth_token": "jeton-essai",
            "app_id": "app-essai",
        })));
    }
    state.services.lock().await.register(Box::new(svc));
}

pub async fn appel(
    state: &Etat,
    methode: &str,
    chemin: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let app = tune_server::routes::router(state.clone());
    let req = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("content-type", "application/json");
    let req = match corps {
        Some(c) => req.body(Body::from(c.to_string())).unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// L'album local : deux pistes. `marque` rend les noms uniques (bases
/// PostgreSQL partagées). Rend (album, piste 1, piste 2).
pub struct Bibliotheque {
    pub album: i64,
    pub piste_1: i64,
    pub piste_2: i64,
    /// Un vrai fichier « audio » : ses octets ne doivent jamais changer.
    pub fichier: std::path::PathBuf,
    _dossier: tempfile::TempDir,
}

pub struct Local<'a> {
    pub marque: &'a str,
    pub upc: Option<&'a str>,
    pub label: Option<&'a str>,
    pub year: Option<i32>,
    /// ISRC de la piste 2 (la 1 n'en a jamais).
    pub isrc_2: Option<&'a str>,
    /// Durée locale de la piste 2.
    pub duree_2_ms: i64,
}

impl Default for Local<'_> {
    fn default() -> Self {
        Self {
            marque: "q8077",
            upc: Some(UPC),
            label: Some("Label Local"),
            year: Some(2015),
            isrc_2: Some(ISRC_2),
            duree_2_ms: 300_000,
        }
    }
}

pub fn bibliotheque(state: &Etat, l: &Local<'_>) -> Bibliotheque {
    let b = state.backend.clone();
    let artiste = ArtistRepo::with_backend(b.clone())
        .create(&Artist::new(format!("{} Groupe Factice", l.marque)))
        .unwrap();
    let mut album = Album::new(format!("{} Disque Factice", l.marque));
    album.artist_id = Some(artiste);
    album.barcode = l.upc.map(str::to_string);
    album.label = l.label.map(str::to_string);
    album.year = l.year;
    album.track_count = Some(2);
    let id_album = AlbumRepo::with_backend(b.clone()).create(&album).unwrap();
    let dossier = tempfile::tempdir().unwrap();
    let fichier = dossier.path().join("01.flac");
    std::fs::write(&fichier, b"fLaC-octets-factices").unwrap();
    let mut ids = Vec::new();
    for (n, duree, isrc, compositeur) in [
        (1, 200_000, None, Some("Compositeur Local")),
        (2, l.duree_2_ms, l.isrc_2, None),
    ] {
        let mut p = Track::new(format!("Titre {n}"));
        p.album_id = Some(id_album);
        p.artist_id = Some(artiste);
        p.disc_number = 1;
        p.track_number = n;
        p.duration_ms = duree;
        p.isrc = isrc.map(str::to_string);
        p.composer = compositeur.map(str::to_string);
        p.file_path = Some(if n == 1 {
            fichier.to_string_lossy().into_owned()
        } else {
            format!("/{}/factice/0{n}.flac", l.marque)
        });
        ids.push(TrackRepo::with_backend(b.clone()).create(&p).unwrap());
    }
    // Un crédit LOCAL d'interprète sur la piste 1, lu dans les balises.
    b.execute(
        "INSERT INTO track_credits (track_id, artist_name, role, instrument, position, source) \
         VALUES (?, 'Pianiste Local', 'performer', 'piano', 0, 'balises')",
        &[&ids[0] as &dyn ToSqlValue],
    )
    .unwrap();
    Bibliotheque {
        album: id_album,
        piste_1: ids[0],
        piste_2: ids[1],
        fichier,
        _dossier: dossier,
    }
}

pub fn fiche_qobuz(marque: &str, duree_2_s: i64) -> Value {
    json!({
        "id": "alb-q1", "title": format!("{marque} Disque Factice"),
        "artist": {"id": 77, "name": format!("{marque} Groupe Factice")},
        "upc": UPC, "tracks_count": 2,
        "label": {"name": "Label Qobuz"},
        "release_date_original": "2015-03-02",
        "copyright": "℗ 2015 Phono Factice © 2015 Droits Factices",
        "genres_list": ["Jazz", "Jazz→Cool Jazz"],
        "goodies": [{"name": "Livret", "file_format_id": 21,
                     "url": "https://static.qobuz.com/goodies/42/000000001.pdf"}],
        "awards": [{"name": "Diapason d'or", "publication_name": "Diapason", "awarded_at": 1425254400}],
        "hires": true, "maximum_bit_depth": 24, "maximum_sampling_rate": 96.0,
        "image": {"large": "https://static.qobuz.com/images/covers/aa/bb/alb_600.jpg"},
        "tracks": {"total": 2, "items": [
            {"id": 901, "media_number": 1, "track_number": 1, "duration": 201,
             "isrc": ISRC_1, "title": "Titre 1",
             "performers": "Pianiste Qobuz, Piano - Producteur Qobuz, Producer",
             "composer": {"name": "Compositeur Qobuz"}},
            {"id": 902, "media_number": 1, "track_number": 2, "duration": duree_2_s,
             "isrc": ISRC_2, "title": "Titre 2",
             "performers": "Bassiste Qobuz, Double Bass"}
        ]}
    })
}

pub fn reponses_par_upc(marque: &str) -> HashMap<&'static str, Value> {
    HashMap::from([
        (
            "/album/search",
            json!({"albums": {"items": [
                {"id": "alb-voisin", "upc": "0999999999999", "title": "Autre", "artist": {"name": "Autre"}},
                {"id": "alb-q1", "upc": UPC, "title": format!("{marque} Disque Factice"),
                 "artist": {"id": 77, "name": format!("{marque} Groupe Factice")}, "tracks_count": 2},
            ]}}),
        ),
        ("/album/get", fiche_qobuz(marque, 300)),
    ])
}

fn credits(state: &Etat, track: i64, source: &str) -> Vec<(String, String)> {
    state
        .backend
        .query_many(
            "SELECT artist_name, role FROM track_credits WHERE track_id = ? AND source = ? ORDER BY position",
            &[&track as &dyn ToSqlValue, &source as &dyn ToSqlValue],
        )
        .unwrap()
        .into_iter()
        .map(|r| (r[0].as_string().unwrap(), r[1].as_string().unwrap()))
        .collect()
}

fn lignes_apports(state: &Etat, album: i64) -> i64 {
    state
        .backend
        .query_one(
            "SELECT COUNT(*) FROM apports_service WHERE album_id = ?",
            &[&album as &dyn ToSqlValue],
        )
        .unwrap()
        .and_then(|r| r[0].as_i64())
        .unwrap_or(-1)
}

// ── Scénarios ────────────────────────────────────────────────────────────

/// Rapprochement par UPC : tout ce qui manque est complété, rien de local
/// n'est écrasé, chaque donnée dit sa source, rien n'est écrit dans le
/// fichier. Puis le retrait efface TOUT, et n'est pas défait.
pub async fn upc_complete_sans_ecraser_puis_retrait(state: Etat, marque: &str) {
    let (base, d) = doublure_qobuz(reponses_par_upc(marque)).await;
    brancher_qobuz(&state, &base, true).await;
    let l = Local {
        marque,
        ..Default::default()
    };
    let bib = bibliotheque(&state, &l);
    let octets_avant = std::fs::read(&bib.fichier).unwrap();
    let mtime_avant = std::fs::metadata(&bib.fichier).unwrap().modified().unwrap();

    let (status, v) = appel(
        &state,
        "GET",
        &format!("/api/v1/library/albums/{}/apports-qobuz", bib.album),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["statut"], "applique", "{v}");
    assert_eq!(v["methode"], "upc", "{v}");
    assert_eq!(v["service_album_id"], "alb-q1", "{v}");
    let champs = &v["champs"];
    // Présents en local : jamais repris.
    assert!(champs.get("label").is_none(), "label local écrasé : {v}");
    assert!(champs.get("upc").is_none(), "UPC local écrasé : {v}");
    // Manquants en local : repris, avec la source.
    for (champ, attendu) in [
        ("date_de_sortie", json!("2015-03-02")),
        ("phonogramme", json!("2015 Phono Factice")),
        ("copyright", json!("2015 Droits Factices")),
        ("genres", json!(["Jazz", "Cool Jazz"])),
        ("hires", json!({"bit_depth": 24, "sample_rate": 96000})),
    ] {
        assert_eq!(champs[champ]["valeur"], attendu, "{champ} : {v}");
        assert_eq!(champs[champ]["source"], "qobuz", "{champ} : {v}");
    }
    assert_eq!(
        champs["livrets"]["valeur"][0]["url"], "https://static.qobuz.com/goodies/42/000000001.pdf",
        "{v}"
    );
    assert_eq!(
        champs["recompenses"]["valeur"][0]["nom"], "Diapason d'or",
        "{v}"
    );
    assert_eq!(
        champs["pochette_hd"]["valeur"]["url"],
        "https://static.qobuz.com/images/covers/aa/bb/alb_max.jpg",
        "{v}"
    );
    for c in champs.as_object().unwrap().values() {
        assert_eq!(c["source"], "qobuz", "source inexacte : {v}");
    }

    // L'album local n'a pas bougé.
    let album = AlbumRepo::with_backend(state.backend.clone())
        .get(bib.album)
        .unwrap()
        .unwrap();
    assert_eq!(album.label.as_deref(), Some("Label Local"));
    assert_eq!(album.release_date, None);
    assert_eq!(album.genre, None);

    // La fiche album et les pistes exposent les apports.
    let (_, fiche) = appel(
        &state,
        "GET",
        &format!("/api/v1/library/albums/{}", bib.album),
        None,
    )
    .await;
    assert_eq!(fiche["label"], "Label Local", "{fiche}");
    assert_eq!(
        fiche["apports_qobuz"]["champs"]["date_de_sortie"]["source"], "qobuz",
        "{fiche}"
    );
    let (_, pistes) = appel(
        &state,
        "GET",
        &format!("/api/v1/library/albums/{}/tracks", bib.album),
        None,
    )
    .await;
    let piste = |id: i64| {
        pistes
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id)
            .cloned()
            .unwrap()
    };
    assert_eq!(
        piste(bib.piste_1)["apports_qobuz"]["isrc"],
        json!({"valeur": ISRC_1, "source": "qobuz"}),
        "{pistes}"
    );
    assert!(
        piste(bib.piste_2).get("apports_qobuz").is_none(),
        "ISRC local écrasé : {pistes}"
    );

    // Crédits : seuls les rôles absents en local, avec la source.
    assert_eq!(
        credits(&state, bib.piste_1, "qobuz"),
        vec![("Producteur Qobuz".to_string(), "producer".to_string())],
        "piste 1 : interprète et compositeur locaux, seul le producteur manque"
    );
    assert_eq!(
        credits(&state, bib.piste_2, "qobuz"),
        vec![("Bassiste Qobuz".to_string(), "performer".to_string())]
    );
    assert_eq!(credits(&state, bib.piste_1, "balises").len(), 1);
    let (_, c1) = appel(
        &state,
        "GET",
        &format!("/api/v1/library/tracks/{}/credits", bib.piste_1),
        None,
    )
    .await;
    let qobuz: Vec<&Value> = c1
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["source"] == "qobuz")
        .collect();
    assert_eq!(qobuz.len(), 1, "{c1}");
    assert_eq!(qobuz[0]["artist_name"], "Producteur Qobuz", "{c1}");

    // Rien dans les fichiers audio.
    assert_eq!(std::fs::read(&bib.fichier).unwrap(), octets_avant);
    assert_eq!(
        std::fs::metadata(&bib.fichier).unwrap().modified().unwrap(),
        mtime_avant
    );

    // Rouverte : servie depuis la base, Qobuz n'est pas rappelé.
    let n = d.nombre_d_appels();
    let (_, encore) = appel(
        &state,
        "GET",
        &format!("/api/v1/library/albums/{}/apports-qobuz", bib.album),
        None,
    )
    .await;
    assert_eq!(encore["statut"], "applique");
    assert_eq!(d.nombre_d_appels(), n, "Qobuz rappelé");

    // Retrait : complet.
    let (status, r) = appel(
        &state,
        "DELETE",
        &format!("/api/v1/library/albums/{}/apports-qobuz", bib.album),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{r}");
    assert_eq!(r["statut"], "retire", "{r}");
    assert_eq!(
        lignes_apports(&state, bib.album),
        0,
        "des apports restent après le retrait"
    );
    assert!(
        credits(&state, bib.piste_1, "qobuz").is_empty(),
        "crédits Qobuz restants"
    );
    assert!(
        credits(&state, bib.piste_2, "qobuz").is_empty(),
        "crédits Qobuz restants"
    );
    assert_eq!(
        credits(&state, bib.piste_1, "balises").len(),
        1,
        "le retrait a touché un crédit local"
    );
    let (_, apres) = appel(
        &state,
        "GET",
        &format!(
            "/api/v1/library/albums/{}/apports-qobuz?rafraichir=true",
            bib.album
        ),
        None,
    )
    .await;
    assert_eq!(apres["statut"], "retire", "{apres}");
    assert_eq!(apres["champs"], json!({}), "{apres}");
    let (_, pistes) = appel(
        &state,
        "GET",
        &format!("/api/v1/library/albums/{}/tracks", bib.album),
        None,
    )
    .await;
    assert!(
        pistes
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p.get("apports_qobuz").is_none()),
        "{pistes}"
    );
    let album = AlbumRepo::with_backend(state.backend.clone())
        .get(bib.album)
        .unwrap()
        .unwrap();
    assert_eq!(album.label.as_deref(), Some("Label Local"));
}

/// Sans code : par artiste + titre. Deux éditions identiques chez Qobuz :
/// douteux, marqué `a_confirmer`, RIEN n'est appliqué. Confirmé : appliqué.
pub async fn titre_douteux_non_applique_puis_confirme(state: Etat, marque: &str) {
    let mut reponses = HashMap::from([("/album/get", fiche_qobuz(marque, 300))]);
    reponses.insert(
        "/album/search",
        json!({"albums": {"items": [
            {"id": "alb-q1", "title": format!("{marque} Disque Factice"),
             "artist": {"name": format!("{marque} Groupe Factice")}, "tracks_count": 2},
            {"id": "alb-q2", "title": format!("{marque} Disque Factice"),
             "artist": {"name": format!("{marque} Groupe Factice")}, "tracks_count": 2},
        ]}}),
    );
    let (base, _d) = doublure_qobuz(reponses).await;
    brancher_qobuz(&state, &base, true).await;
    let bib = bibliotheque(
        &state,
        &Local {
            marque,
            upc: None,
            isrc_2: None,
            ..Default::default()
        },
    );

    let (_, v) = appel(
        &state,
        "GET",
        &format!("/api/v1/library/albums/{}/apports-qobuz", bib.album),
        None,
    )
    .await;
    assert_eq!(v["statut"], "a_confirmer", "{v}");
    assert_eq!(v["motif"], "plusieurs_editions", "{v}");
    assert_eq!(
        v["champs"],
        json!({}),
        "rapprochement douteux appliqué : {v}"
    );
    assert_eq!(
        lignes_apports(&state, bib.album),
        0,
        "rapprochement douteux appliqué"
    );
    assert!(
        credits(&state, bib.piste_2, "qobuz").is_empty(),
        "crédits d'un rapprochement douteux"
    );

    let (status, c) = appel(
        &state,
        "POST",
        &format!(
            "/api/v1/library/albums/{}/apports-qobuz/confirmer",
            bib.album
        ),
        Some(json!({"service_album_id": "alb-q1"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{c}");
    assert_eq!(c["statut"], "applique", "{c}");
    assert_eq!(c["methode"], "confirme", "{c}");
    assert_eq!(c["champs"]["upc"]["valeur"], "123456789012", "{c}");
}

/// Par titre, un seul album, mais une piste dont la durée diffère et sans
/// ISRC : douteux, rien n'est appliqué.
pub async fn titre_pistes_differentes_non_applique(state: Etat, marque: &str) {
    let mut reponses = reponses_par_upc(marque);
    reponses.insert("/album/get", fiche_qobuz(marque, 420));
    let (base, _d) = doublure_qobuz(reponses).await;
    brancher_qobuz(&state, &base, true).await;
    let bib = bibliotheque(
        &state,
        &Local {
            marque,
            upc: None,
            isrc_2: None,
            ..Default::default()
        },
    );

    let (_, v) = appel(
        &state,
        "GET",
        &format!("/api/v1/library/albums/{}/apports-qobuz", bib.album),
        None,
    )
    .await;
    assert_eq!(v["methode"], "titre", "{v}");
    assert_eq!(v["statut"], "a_confirmer", "{v}");
    assert_eq!(v["motif"], "pistes_differentes", "{v}");
    assert_eq!(v["pistes_rapprochees"], 1, "{v}");
    assert_eq!(
        lignes_apports(&state, bib.album),
        0,
        "rapprochement douteux appliqué"
    );
}

/// Par titre, sûr : un seul album, même nombre de pistes, toutes les pistes
/// rapprochées par place et durée.
pub async fn titre_sur_applique(state: Etat, marque: &str) {
    let (base, _d) = doublure_qobuz(reponses_par_upc(marque)).await;
    brancher_qobuz(&state, &base, true).await;
    let bib = bibliotheque(
        &state,
        &Local {
            marque,
            upc: None,
            isrc_2: None,
            ..Default::default()
        },
    );

    let (_, v) = appel(
        &state,
        "GET",
        &format!("/api/v1/library/albums/{}/apports-qobuz", bib.album),
        None,
    )
    .await;
    assert_eq!(v["methode"], "titre", "{v}");
    assert_eq!(v["statut"], "applique", "{v}");
    assert_eq!(v["pistes_rapprochees"], 2, "{v}");
    assert_eq!(v["champs"]["upc"]["source"], "qobuz", "{v}");
}

/// Sans compte Qobuz connecté : aucun appel, rien de rangé.
pub async fn sans_compte_aucun_appel(state: Etat, marque: &str) {
    let (base, d) = doublure_qobuz(reponses_par_upc(marque)).await;
    brancher_qobuz(&state, &base, false).await;
    let bib = bibliotheque(
        &state,
        &Local {
            marque,
            ..Default::default()
        },
    );

    let (status, v) = appel(
        &state,
        "GET",
        &format!("/api/v1/library/albums/{}/apports-qobuz", bib.album),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(v["statut"].is_null(), "{v}");
    assert_eq!(v["service_connecte"], false, "{v}");
    let _ = appel(
        &state,
        "GET",
        &format!("/api/v1/library/albums/{}", bib.album),
        None,
    )
    .await;
    assert_eq!(
        d.nombre_d_appels(),
        0,
        "Qobuz interrogé sans compte : {:?}",
        d.appels.lock().unwrap()
    );
}
