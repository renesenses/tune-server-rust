//! Tune Circle, étape T4 (#5327) : écouter chez un contact, côté AUDITEUR.
//!
//! Un seul faux serveur joue les deux rôles que le greffon appelle :
//! mozaiklabs (`POST /api/v1/circle/contacts/{id}/listen`,
//! `GET …/library/albums/{album_id}/tracks`, site-mozaiklabs#237) et le pont
//! (`GET /stream/circle/{billet}`). Il note tout ce qu'il reçoit. L'hôte de
//! lecture est simulé : il note la file posée. La résolution des lignes par
//! l'orchestrateur est rejouée en appelant le fournisseur d'URL du greffon,
//! comme le fait `tune_core::orchestrator` (prouvé de bout en bout dans
//! `tune-core/src/poller/source_url_5327.rs`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tune_circle::ecoute::{
    EVENEMENT_FIN_DE_PARTAGE, EVENEMENT_PROPRIETAIRE_ETEINT, Ecoute, HoteLecture, Ligne,
};
use tune_circle::relais::Relais;
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::sqlite::SqliteDb;
use tune_core::event_bus::{EventBus, TuneEvent};
use tune_core::source_url::{FournisseurDUrl, MOTIF_ARRET_DE_LA_FILE, MOTIF_PISTE_REFUSEE};

const JETON: &str = "jeton-acces-SECRET-5327";
const ZONE: i64 = 3;

#[derive(Default)]
struct Faux {
    base: String,
    /// `POST …/listen` : réponse par `track_id` ; par défaut, un billet.
    reponses_listen: HashMap<String, (u16, Value)>,
    /// Le pont : réponse par piste (`billet-<track_id>`) ; par défaut 206.
    reponses_pont: HashMap<String, (u16, Value)>,
    /// `GET …/albums/{id}/tracks` : réponse ; par défaut l'album 10.
    reponse_album: Option<(u16, Value)>,
    /// (Authorization, corps) de chaque `POST …/listen`.
    listens: Vec<(Option<String>, Value)>,
    /// (billet, Range) de chaque appel au pont.
    sondes: Vec<(String, Option<String>)>,
    albums: Vec<String>,
}

type Etat = Arc<Mutex<Faux>>;

fn piste(id: i64, titre: &str) -> Value {
    json!({
        "id": id, "title": titre, "artist_name": "Miles Davis",
        "album_title": "Kind of Blue", "album_id": 10, "format": "flac",
        "sample_rate": 96000, "bit_depth": 24, "duration_ms": 300000 + id,
        "track_number": id, "disc_number": 1
    })
}

fn album_10() -> Value {
    json!({ "data": [
        piste(1, "So What"), piste(2, "Freddie Freeloader"),
        piste(3, "Blue in Green"), piste(4, "All Blues")
    ]})
}

fn id_texte(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        autre => autre.to_string(),
    }
}

async fn listen(
    State(e): State<Etat>,
    Path(_user_id): Path<String>,
    headers: HeaderMap,
    corps: Bytes,
) -> Response {
    let mut f = e.lock().unwrap();
    let corps: Value = serde_json::from_slice(&corps).unwrap_or(Value::Null);
    let id = id_texte(&corps["track_id"]);
    f.listens.push((
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(String::from),
        corps,
    ));
    let (statut, corps) = f.reponses_listen.get(&id).cloned().unwrap_or_else(|| {
        let n = id.parse::<i64>().unwrap_or(0);
        (
            200,
            json!({
                "stream_url": format!("{}/stream/circle/billet-{id}", f.base),
                "expires_at": "2026-09-28T12:05:00Z",
                "track": piste(n, "Titre du billet"),
            }),
        )
    });
    (StatusCode::from_u16(statut).unwrap(), Json(corps)).into_response()
}

async fn album(
    State(e): State<Etat>,
    Path((_user_id, album_id)): Path<(String, String)>,
) -> Response {
    let mut f = e.lock().unwrap();
    f.albums.push(album_id);
    let (statut, corps) = f.reponse_album.clone().unwrap_or((200, album_10()));
    (StatusCode::from_u16(statut).unwrap(), Json(corps)).into_response()
}

async fn pont(State(e): State<Etat>, Path(billet): Path<String>, headers: HeaderMap) -> Response {
    let mut f = e.lock().unwrap();
    f.sondes.push((
        billet.clone(),
        headers
            .get(header::RANGE)
            .and_then(|v| v.to_str().ok())
            .map(String::from),
    ));
    let (statut, corps) = f
        .reponses_pont
        .get(&billet)
        .cloned()
        .unwrap_or((206, Value::Null));
    if statut < 300 {
        return (
            StatusCode::from_u16(statut).unwrap(),
            [("content-type", "audio/flac")],
            b"f".to_vec(),
        )
            .into_response();
    }
    (StatusCode::from_u16(statut).unwrap(), Json(corps)).into_response()
}

async fn demarrer() -> (String, Etat) {
    let etat: Etat = Arc::new(Mutex::new(Faux::default()));
    let app = Router::new()
        .route("/api/v1/circle/contacts/{user_id}/listen", post(listen))
        .route(
            "/api/v1/circle/contacts/{user_id}/library/albums/{album_id}/tracks",
            get(album),
        )
        .route("/stream/circle/{billet}", get(pont))
        .with_state(etat.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", ecoute.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.unwrap();
    });
    etat.lock().unwrap().base = base.clone();
    (base, etat)
}

/// L'hôte de lecture simulé : note chaque file posée.
#[derive(Default)]
struct Hote {
    files: Mutex<Vec<(i64, Vec<Ligne>)>>,
    refus: Option<String>,
}

#[async_trait]
impl HoteLecture for Hote {
    async fn jouer_file(&self, zone_id: i64, lignes: Vec<Ligne>) -> Result<(), String> {
        self.files.lock().unwrap().push((zone_id, lignes));
        match &self.refus {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }
}

/// Réglages d'un serveur dont l'interrupteur d'écoute est OUVERT : les tests
/// du chemin d'écoute.
fn reglages(base: &str, connecte: bool) -> Arc<dyn DbBackend> {
    let backend = reglages_ferme(base, connecte);
    SettingsRepo::with_backend(backend.clone())
        .set(tune_core::cloud::CLE_ECOUTE_DE_CERCLE, "true")
        .unwrap();
    backend
}

/// Réglages par défaut : interrupteur d'écoute absent, donc fermé.
fn reglages_ferme(base: &str, connecte: bool) -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let s = SettingsRepo::with_backend(backend.clone());
    s.set("mozaik_base_url", base).unwrap();
    if connecte {
        s.set("mozaik_access_token", JETON).unwrap();
    }
    backend
}

struct Greffon {
    app: Router,
    ecoute: Arc<Ecoute>,
    hote: Arc<Hote>,
}

fn greffon_avec(base: &str, connecte: bool, bus: Option<EventBus>, hote: Hote) -> Greffon {
    let hote = Arc::new(hote);
    let ecoute = Arc::new(Ecoute::new(
        Arc::new(Relais::new(reglages(base, connecte))),
        bus,
        None,
        Some(hote.clone() as Arc<dyn HoteLecture>),
    ));
    Greffon {
        app: tune_circle::ecoute::router(ecoute.clone()),
        ecoute,
        hote,
    }
}

fn greffon(base: &str, connecte: bool, bus: Option<EventBus>) -> Greffon {
    greffon_avec(base, connecte, bus, Hote::default())
}

struct Rendu {
    statut: StatusCode,
    octets: Vec<u8>,
}

impl Rendu {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.octets).unwrap_or(Value::Null)
    }
}

async fn appel(app: &Router, chemin: &str, corps: Value) -> Rendu {
    use tower::ServiceExt;
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(chemin)
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(corps.to_string()))
        .unwrap();
    let r = app.clone().oneshot(req).await.unwrap();
    let statut = r.status();
    let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    Rendu { statut, octets }
}

fn une_piste(track: &str) -> Value {
    json!({ "track_id": track, "zone_id": ZONE })
}

fn un_album(ids: Value) -> Value {
    json!({ "album_id": 10, "track_ids": ids, "zone_id": ZONE })
}

fn listens(etat: &Etat) -> Vec<String> {
    etat.lock()
        .unwrap()
        .listens
        .iter()
        .map(|(_, c)| id_texte(&c["track_id"]))
        .collect()
}

fn file_posee(g: &Greffon) -> Vec<Ligne> {
    let files = g.hote.files.lock().unwrap();
    assert_eq!(files.len(), 1, "une seule file posee");
    assert_eq!(files[0].0, ZONE, "la file est posee sur la zone demandee");
    files[0].1.clone()
}

fn aucune_url(lignes: &[Ligne]) {
    for l in lignes {
        assert!(
            !l.reference.contains("http") && !l.reference.contains("billet"),
            "une URL est entree dans la file : {}",
            l.reference
        );
    }
}

// 1. Une piste ------------------------------------------------------------------

/// `track_id` seul : un billet, une ligne `{user}:{track}` avec les
/// métadonnées du billet ; la première résolution sert CE billet, sans en
/// redemander un.
#[tokio::test]
async fn une_piste_pose_une_ligne_de_reference_et_sert_son_billet_une_fois() {
    let (base, etat) = demarrer().await;
    let g = greffon(&base, true, None);

    let r = appel(&g.app, "/contacts/7/listen", une_piste("42")).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), json!({ "ok": true }));

    let lignes = file_posee(&g);
    assert_eq!(lignes.len(), 1);
    assert_eq!(lignes[0].reference, "7:42");
    assert_eq!(lignes[0].titre, "Titre du billet");
    assert_eq!(lignes[0].media_format.as_deref(), Some("flac"));
    aucune_url(&lignes);
    {
        let f = etat.lock().unwrap();
        assert_eq!(
            f.listens[0].0.as_deref(),
            Some(format!("Bearer {JETON}").as_str())
        );
        assert_eq!(f.listens[0].1, json!({ "track_id": "42" }));
        assert_eq!(f.sondes[0].1.as_deref(), Some("bytes=0-0"));
    }

    // L'orchestrateur résout la ligne : le billet déjà obtenu, pas un second.
    let u = g.ecoute.url(ZONE, "7:42").await.unwrap();
    assert!(u.url.ends_with("/stream/circle/billet-42"));
    assert_eq!(u.media_format.as_deref(), Some("flac"));
    assert_eq!(
        listens(&etat),
        vec!["42"],
        "un seul billet pour cette piste"
    );
    // Une nouvelle résolution (retour arrière, reprise) : un billet neuf.
    g.ecoute.url(ZONE, "7:42").await.unwrap();
    assert_eq!(listens(&etat), vec!["42", "42"]);
}

// 2. L'album --------------------------------------------------------------------

/// `{ album_id, track_ids }` : les métadonnées viennent de l'album, seules les
/// pistes demandées sont gardées, dans l'ORDRE demandé ; puis trois pistes
/// qui s'enchaînent, UN billet par piste, demandé quand elle va jouer.
#[tokio::test]
async fn un_album_pose_les_pistes_demandees_et_demande_un_billet_par_piste() {
    let (base, etat) = demarrer().await;
    let g = greffon(&base, true, None);

    let r = appel(&g.app, "/contacts/7/listen", un_album(json!([3, "1", 2]))).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), json!({ "ok": true, "queued": 3 }));
    assert_eq!(etat.lock().unwrap().albums, vec!["10"]);

    let lignes = file_posee(&g);
    assert_eq!(
        lignes
            .iter()
            .map(|l| l.reference.as_str())
            .collect::<Vec<_>>(),
        vec!["7:3", "7:1", "7:2"]
    );
    assert_eq!(
        lignes.iter().map(|l| l.titre.as_str()).collect::<Vec<_>>(),
        vec!["Blue in Green", "So What", "Freddie Freeloader"],
        "les metadonnees viennent de l'album"
    );
    aucune_url(&lignes);
    // Au départ, le billet de la première piste SEULEMENT.
    assert_eq!(listens(&etat), vec!["3"]);

    for (i, l) in lignes.iter().enumerate() {
        let u = g.ecoute.url(ZONE, &l.reference).await.unwrap();
        let (_, track) = l.reference.split_once(':').unwrap();
        assert!(u.url.ends_with(&format!("billet-{track}")), "{}", u.url);
        assert_eq!(
            listens(&etat).len(),
            if i == 0 { 1 } else { i + 1 },
            "un billet par piste, au moment de la jouer"
        );
    }
    assert_eq!(listens(&etat), vec!["3", "1", "2"]);
}

/// Une piste que le cloud refuse (404, 409) pendant l'album est SAUTÉE : le
/// refus porte la marque « piste refusée », que la boucle d'avancement
/// reconnaît.
#[tokio::test]
async fn une_piste_refusee_en_cours_d_album_est_une_piste_a_sauter() {
    let (base, etat) = demarrer().await;
    {
        let mut f = etat.lock().unwrap();
        f.reponses_listen
            .insert("1".into(), (404, json!({ "error": "not_found" })));
        f.reponses_listen
            .insert("2".into(), (409, json!({ "error": "owner_unavailable" })));
    }
    let g = greffon(&base, true, None);
    let r = appel(&g.app, "/contacts/7/listen", un_album(json!([3, 1, 2]))).await;
    assert_eq!(r.statut, StatusCode::OK);

    for (reference, code) in [("7:1", "not_found"), ("7:2", "owner_unavailable")] {
        let e = g.ecoute.url(ZONE, reference).await.unwrap_err();
        let message = e.en_message();
        assert!(message.contains(MOTIF_PISTE_REFUSEE), "{message}");
        assert!(message.contains(code), "{message}");
        assert!(!message.contains(MOTIF_ARRET_DE_LA_FILE));
    }
}

/// Le serveur du contact s'éteint pendant l'album : la résolution demande
/// l'ARRÊT de la file, et `circle.owner_offline { zone_id, user_id }` part.
#[tokio::test]
async fn owner_offline_en_cours_d_album_arrete_la_file_et_le_dit() {
    let (base, etat) = demarrer().await;
    let bus = EventBus::new();
    let mut rx = bus.subscribe();
    let g = greffon(&base, true, Some(bus));
    let r = appel(&g.app, "/contacts/7/listen", un_album(json!([3, 1, 2]))).await;
    assert_eq!(r.statut, StatusCode::OK);

    etat.lock()
        .unwrap()
        .reponses_pont
        .insert("billet-1".into(), (503, json!({ "code": "owner_offline" })));
    let e = g.ecoute.url(ZONE, "7:1").await.unwrap_err();
    assert!(e.en_message().contains(MOTIF_ARRET_DE_LA_FILE), "{e:?}");
    let evt = evenement_suivant(&mut rx, EVENEMENT_PROPRIETAIRE_ETEINT)
        .await
        .expect("circle.owner_offline n'est pas parti");
    assert_eq!(evt.data, json!({ "zone_id": ZONE, "user_id": "7" }));
}

#[tokio::test]
async fn l_album_ignore_les_pistes_qui_n_en_sont_pas_et_refuse_s_il_n_en_reste_aucune() {
    let (base, etat) = demarrer().await;
    let g = greffon(&base, true, None);
    let r = appel(&g.app, "/contacts/7/listen", un_album(json!([4, 99]))).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(
        file_posee(&g)
            .iter()
            .map(|l| l.reference.as_str())
            .collect::<Vec<_>>(),
        vec!["7:4"]
    );

    let g2 = greffon(&base, true, None);
    let r = appel(&g2.app, "/contacts/7/listen", un_album(json!([98, 99]))).await;
    assert_eq!(r.statut, StatusCode::NOT_FOUND);
    assert_eq!(r.json(), json!({ "code": "circle.not_found" }));
    assert!(g2.hote.files.lock().unwrap().is_empty());
    assert_eq!(listens(&etat), vec!["4"], "aucun billet pour un album vide");
}

#[tokio::test]
async fn une_demande_d_album_mal_formee_est_refusee_sans_appel() {
    let (base, etat) = demarrer().await;
    let g = greffon(&base, true, None);
    for corps in [
        json!({ "album_id": 10, "track_ids": [], "zone_id": ZONE }),
        json!({ "album_id": 10, "track_ids": ["1/../2"], "zone_id": ZONE }),
        json!({ "album_id": 10, "track_ids": "1", "zone_id": ZONE }),
        json!({ "album_id": 10, "track_ids": [-1], "zone_id": ZONE }),
        json!({ "track_ids": [1], "zone_id": ZONE }),
        json!({ "album_id": "10/../11", "track_ids": [1], "zone_id": ZONE }),
    ] {
        let r = appel(&g.app, "/contacts/7/listen", corps.clone()).await;
        assert_eq!(r.statut, StatusCode::UNPROCESSABLE_ENTITY, "{corps}");
        assert_eq!(r.json()["code"], "circle.invalid_tracks");
    }
    let f = etat.lock().unwrap();
    assert!(f.albums.is_empty() && f.listens.is_empty());
}

#[tokio::test]
async fn le_refus_du_cloud_sur_l_album_est_relaye_tel_quel() {
    let (base, etat) = demarrer().await;
    etat.lock().unwrap().reponse_album = Some((404, json!({ "error": "not_found" })));
    let g = greffon(&base, true, None);
    let r = appel(&g.app, "/contacts/7/listen", un_album(json!([1]))).await;
    assert_eq!(r.statut, StatusCode::NOT_FOUND);
    assert_eq!(r.json(), json!({ "error": "not_found" }));
    assert!(listens(&etat).is_empty());
}

// 3. Les refus au départ, relayés tels quels ------------------------------------

#[tokio::test]
async fn les_refus_du_cloud_sont_relayes_tels_quels_et_rien_ne_joue() {
    for (statut, corps) in [
        (
            402,
            json!({ "error": "premium_required", "who": "listener" }),
        ),
        (404, json!({ "error": "not_found" })),
        (409, json!({ "error": "owner_unavailable" })),
        (429, json!({ "message": "Too Many Attempts." })),
    ] {
        let (base, etat) = demarrer().await;
        etat.lock()
            .unwrap()
            .reponses_listen
            .insert("42".into(), (statut, corps.clone()));
        let g = greffon(&base, true, None);

        let r = appel(&g.app, "/contacts/7/listen", une_piste("42")).await;
        assert_eq!(r.statut.as_u16(), statut);
        assert_eq!(r.json(), corps, "{statut}");
        assert!(
            etat.lock().unwrap().sondes.is_empty(),
            "{statut} : pont sonde"
        );
        assert!(
            g.hote.files.lock().unwrap().is_empty(),
            "{statut} : file posee"
        );
    }
}

#[tokio::test]
async fn cloud_en_panne_503_cloud_unavailable() {
    let (base, etat) = demarrer().await;
    etat.lock()
        .unwrap()
        .reponses_listen
        .insert("42".into(), (500, json!({})));
    let g = greffon(&base, true, None);
    let r = appel(&g.app, "/contacts/7/listen", une_piste("42")).await;
    assert_eq!(r.statut, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(r.json()["code"], "circle.cloud_unavailable");
    assert!(g.hote.files.lock().unwrap().is_empty());
}

#[tokio::test]
async fn sans_session_412_et_rien_ne_part() {
    let (base, etat) = demarrer().await;
    let g = greffon(&base, false, None);
    let r = appel(&g.app, "/contacts/7/listen", une_piste("42")).await;
    assert_eq!(r.statut, StatusCode::PRECONDITION_FAILED);
    assert_eq!(
        r.json(),
        json!({ "connected": false, "code": "circle.not_connected" })
    );
    assert!(listens(&etat).is_empty());
    assert!(g.hote.files.lock().unwrap().is_empty());
}

/// Le pont dit « serveur du contact éteint » au départ : 503
/// `circle.owner_offline`, et la zone n'est pas touchée.
#[tokio::test]
async fn serveur_du_contact_eteint_503_owner_offline() {
    let (base, etat) = demarrer().await;
    etat.lock().unwrap().reponses_pont.insert(
        "billet-42".into(),
        (503, json!({ "code": "owner_offline" })),
    );
    let g = greffon(&base, true, None);
    let r = appel(&g.app, "/contacts/7/listen", une_piste("42")).await;
    assert_eq!(r.statut, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        r.json(),
        json!({ "connected": true, "code": "circle.owner_offline" })
    );
    assert!(g.hote.files.lock().unwrap().is_empty());
}

#[tokio::test]
async fn billet_refuse_par_le_pont_404() {
    let (base, etat) = demarrer().await;
    etat.lock()
        .unwrap()
        .reponses_pont
        .insert("billet-42".into(), (404, json!({ "code": "not_found" })));
    let g = greffon(&base, true, None);
    let r = appel(&g.app, "/contacts/7/listen", une_piste("42")).await;
    assert_eq!(r.statut, StatusCode::NOT_FOUND);
    assert_eq!(r.json(), json!({ "code": "circle.not_found" }));
    assert!(g.hote.files.lock().unwrap().is_empty());
}

#[tokio::test]
async fn un_zone_id_ou_un_user_id_invalide_est_refuse_sans_appel() {
    let (base, etat) = demarrer().await;
    let g = greffon(&base, true, None);
    for zone in [json!(null), json!("3/../1"), json!("salon"), json!(1.5)] {
        let r = appel(
            &g.app,
            "/contacts/7/listen",
            json!({ "track_id": "42", "zone_id": zone }),
        )
        .await;
        assert_eq!(r.statut, StatusCode::UNPROCESSABLE_ENTITY, "{zone}");
        assert_eq!(r.json()["code"], "circle.invalid_zone");
    }
    let r = appel(&g.app, "/contacts/alice/listen", une_piste("42")).await;
    assert_eq!(r.statut, StatusCode::NOT_FOUND);
    assert!(listens(&etat).is_empty());
}

#[tokio::test]
async fn un_billet_sans_adresse_http_ne_joue_pas() {
    let (base, etat) = demarrer().await;
    etat.lock().unwrap().reponses_listen.insert(
        "42".into(),
        (
            200,
            json!({ "stream_url": "file:///etc/passwd", "track": {} }),
        ),
    );
    let g = greffon(&base, true, None);
    let r = appel(&g.app, "/contacts/7/listen", une_piste("42")).await;
    assert_eq!(r.statut, StatusCode::BAD_GATEWAY);
    assert!(etat.lock().unwrap().sondes.is_empty());
    assert!(g.hote.files.lock().unwrap().is_empty());
}

/// La zone refuse de jouer : 409 `circle.playback_failed`, et le billet
/// préparé n'est pas gardé.
#[tokio::test]
async fn le_refus_de_la_zone_est_rendu_et_le_billet_prepare_oublie() {
    let (base, etat) = demarrer().await;
    let g = greffon_avec(
        &base,
        true,
        None,
        Hote {
            refus: Some("zone inconnue".into()),
            ..Default::default()
        },
    );
    let r = appel(&g.app, "/contacts/7/listen", une_piste("42")).await;
    assert_eq!(r.statut, StatusCode::CONFLICT);
    assert_eq!(r.json()["code"], "circle.playback_failed");
    // Le billet préparé a été jeté : une résolution en demande un neuf.
    g.ecoute.url(ZONE, "7:42").await.unwrap();
    assert_eq!(listens(&etat), vec!["42", "42"]);
}

#[tokio::test]
async fn une_reference_qui_n_en_est_pas_une_est_une_piste_refusee_sans_appel() {
    let (base, etat) = demarrer().await;
    let g = greffon(&base, true, None);
    for mauvaise in ["7", "7:../1", "7:1/2", "http://x/y", ""] {
        let e = g.ecoute.url(ZONE, mauvaise).await.unwrap_err();
        assert!(e.en_message().contains(MOTIF_PISTE_REFUSEE), "{mauvaise}");
    }
    assert!(listens(&etat).is_empty());
}

// 4. Fin de partage en cours de lecture -----------------------------------------

async fn evenement_suivant(
    rx: &mut tokio::sync::broadcast::Receiver<TuneEvent>,
    nom: &str,
) -> Option<TuneEvent> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match rx.recv().await {
                Ok(e) if e.event_type == nom => return e,
                Ok(_) => continue,
                Err(_) => std::future::pending::<()>().await,
            }
        }
    })
    .await
    .ok()
}

fn erreur_de_lecture(zone: i64) -> TuneEvent {
    TuneEvent {
        event_type: "zone.playback_error".into(),
        data: json!({ "zone_id": zone, "error": "decode", "fatal": true }),
    }
}

/// La zone qui joue le flux d'un contact tombe en erreur, et le pont refuse
/// désormais le billet : `circle.stream_revoked { zone_id }`.
#[tokio::test]
async fn un_billet_refuse_en_cours_de_lecture_emet_circle_stream_revoked() {
    let (base, etat) = demarrer().await;
    let bus = EventBus::new();
    let mut rx = bus.subscribe();
    let g = greffon(&base, true, Some(bus));
    let r = appel(&g.app, "/contacts/7/listen", une_piste("42")).await;
    assert_eq!(r.statut, StatusCode::OK);
    g.ecoute.url(ZONE, "7:42").await.unwrap();

    etat.lock()
        .unwrap()
        .reponses_pont
        .insert("billet-42".into(), (404, json!({ "code": "not_found" })));
    g.ecoute.sur_evenement(&erreur_de_lecture(ZONE)).await;

    let e = evenement_suivant(&mut rx, EVENEMENT_FIN_DE_PARTAGE)
        .await
        .expect("circle.stream_revoked n'est pas parti");
    assert_eq!(e.data, json!({ "zone_id": ZONE }));
}

/// Pont toujours ouvert : ce n'est PAS une fin de partage. Zone sans flux de
/// contact : rien n'est même sondé.
#[tokio::test]
async fn une_autre_erreur_n_est_pas_une_fin_de_partage() {
    let (base, etat) = demarrer().await;
    let bus = EventBus::new();
    let mut rx = bus.subscribe();
    let g = greffon(&base, true, Some(bus));
    let r = appel(&g.app, "/contacts/7/listen", une_piste("42")).await;
    assert_eq!(r.statut, StatusCode::OK);
    g.ecoute.url(ZONE, "7:42").await.unwrap();
    let sondes_avant = etat.lock().unwrap().sondes.len();

    g.ecoute.sur_evenement(&erreur_de_lecture(5)).await;
    g.ecoute.sur_evenement(&erreur_de_lecture(ZONE)).await;
    assert!(
        evenement_suivant(&mut rx, EVENEMENT_FIN_DE_PARTAGE)
            .await
            .is_none()
    );
    assert_eq!(etat.lock().unwrap().sondes.len(), sondes_avant + 1);
}

// Interrupteur fermé (décision produit du 10/10) ---------------------------------

/// Interrupteur local absent (le défaut) : `POST …/listen` rend 404
/// `circle.listen_disabled`, AUCUN appel au cloud ni au pont, aucune file
/// posée ; une ligne déjà en file ne se résout plus et arrête la file.
#[tokio::test]
async fn interrupteur_ferme_aucune_ecoute_de_contact() {
    let (base, etat) = demarrer().await;
    let hote = Arc::new(Hote::default());
    let ecoute = Arc::new(Ecoute::new(
        Arc::new(Relais::new(reglages_ferme(&base, true))),
        None,
        None,
        Some(hote.clone() as Arc<dyn HoteLecture>),
    ));
    let app = tune_circle::ecoute::router(ecoute.clone());

    for corps in [une_piste("42"), un_album(json!([1, 2]))] {
        let r = appel(&app, "/contacts/7/listen", corps).await;
        assert_eq!(r.statut, StatusCode::NOT_FOUND);
        assert_eq!(r.json()["code"], "circle.listen_disabled");
    }
    let e = ecoute.url(ZONE, "7:42").await.unwrap_err();
    assert!(e.en_message().contains(MOTIF_ARRET_DE_LA_FILE), "{e:?}");

    assert!(hote.files.lock().unwrap().is_empty(), "aucune file posee");
    let f = etat.lock().unwrap();
    assert!(f.listens.is_empty(), "le cloud a ete appele");
    assert!(f.albums.is_empty(), "le cloud a ete appele");
    assert!(f.sondes.is_empty(), "le pont a ete appele");
}
