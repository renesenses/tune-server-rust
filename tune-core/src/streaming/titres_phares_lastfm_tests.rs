//! #7107 — titres phares : popularité Last.fm, tri à quatre niveaux, repli.
//! Last.fm est un faux serveur local (axum), le service un faux « chercher »,
//! l'historique une base en mémoire. Chaque faux Last.fm a une URL unique,
//! même si le système réutilise un port : les caches globaux restent isolés.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};

use super::*;
use crate::db::backend::DbBackend;
use crate::db::history_repo::{HistoryRepo, ListenRecord};
use crate::db::migrations;
use crate::db::sqlite::SqliteDb;
use crate::streaming::titres_phares_ecoutes::{
    CHAMP_LOCAL, CHAMP_TOTAL, classer, enrichir_et_classer,
};

const KIWANUKA: &str = "Michael Kiwanuka";

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

/// Un faux Last.fm : rend `corps` (statut `statut`) et compte les appels.
async fn faux_lastfm(statut: u16, corps: Value) -> (String, Arc<AtomicUsize>) {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    faux_lastfm_sur(ecoute, statut, corps).await
}

async fn faux_lastfm_sur(
    ecoute: tokio::net::TcpListener,
    statut: u16,
    corps: Value,
) -> (String, Arc<AtomicUsize>) {
    use axum::Router;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::routing::get;

    type Etat = (Arc<AtomicUsize>, u16, Value);
    async fn repondre(State((n, statut, corps)): State<Etat>) -> (StatusCode, axum::Json<Value>) {
        n.fetch_add(1, Ordering::SeqCst);
        (StatusCode::from_u16(statut).unwrap(), axum::Json(corps))
    }
    let appels = Arc::new(AtomicUsize::new(0));
    // Un port éphémère peut être réattribué après la fin d'un test, alors que
    // TOPS et RESOLUS conservent encore les entrées de son URL de base.
    static NEXT_FAKE: AtomicUsize = AtomicUsize::new(1);
    let chemin = format!("/lastfm-{}", NEXT_FAKE.fetch_add(1, Ordering::Relaxed));
    let app =
        Router::new()
            .route(&chemin, get(repondre))
            .with_state((appels.clone(), statut, corps));
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.ok();
    });
    (format!("http://{adresse}{chemin}"), appels)
}

#[test]
fn deux_faux_lastfm_sur_le_meme_port_ne_partagent_pas_un_echec() {
    // Garder le port réservé force sa réutilisation, sans dépendre du choix
    // aléatoire du système. Chaque exécution détruit son serveur et ses sockets,
    // comme deux tests Tokio successifs, mais les caches globaux restent vivants.
    let ecoute = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    ecoute.set_nonblocking(true).unwrap();
    for passage in 1..=2 {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let ecoute = tokio::net::TcpListener::from_std(ecoute.try_clone().unwrap()).unwrap();
            let (url, appels) = faux_lastfm_sur(ecoute, 500, json!({})).await;
            let client = crate::http::client::builder().no_proxy().build().unwrap();
            let mut items = fudge();
            let avant = items.clone();
            for _ in 0..2 {
                completer(
                    &client,
                    &cfg(&url, "cle", 8),
                    "qobuz",
                    &mut items,
                    catalogue(vec![], Arc::default()),
                )
                .await;
                assert_eq!(items, avant);
            }
            assert_eq!(
                appels.load(Ordering::SeqCst),
                1,
                "passage {passage} : le faux serveur doit recevoir son premier appel malgré le port réutilisé, puis mémoriser son propre échec"
            );
        });
    }
}

fn top(titres: &[&str]) -> Value {
    json!({"toptracks": {"track": titres.iter().map(|t| json!({"name": t})).collect::<Vec<_>>()}})
}

fn piste(id: &str, titre: &str, artiste: &str, isrc: Option<&str>) -> StreamTrack {
    serde_json::from_value(json!({
        "source_id": id, "title": titre, "artist_name": artiste,
        "album_title": null, "album_id": null, "duration_ms": 240_000,
        "cover_path": null, "track_number": null, "disc_number": null,
        "explicit": false, "quality": null, "isrc": isrc,
    }))
    .unwrap()
}

fn json_de(p: &StreamTrack) -> Value {
    json!(p)
}

/// Les pistes de FUDGE, dans l'ordre de l'album, comme Qobuz les rend.
fn fudge() -> Vec<Value> {
    [
        ("f-1", "Summer Clothes"),
        ("f-2", "California"),
        ("f-3", "Just Another Moment"),
        ("f-4", "Stars"),
    ]
    .iter()
    .map(|(id, t)| json_de(&piste(id, t, KIWANUKA, None)))
    .collect()
}

fn ordre(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .map(|v| v["title"].as_str().unwrap().to_string())
        .collect()
}

fn ecouter(b: &Arc<dyn DbBackend>, id: &str, titre: &str, fois: usize) {
    for _ in 0..fois {
        HistoryRepo::with_backend(b.clone())
            .record(&ListenRecord {
                title: titre.into(),
                artist_name: Some(KIWANUKA.into()),
                source: "qobuz".into(),
                source_id: Some(id.into()),
                duration_ms: 240_000,
                ..Default::default()
            })
            .unwrap();
    }
}

/// Un « chercher » qui sert un catalogue fixe et compte ses appels.
fn catalogue(
    pistes: Vec<StreamTrack>,
    appels: Arc<AtomicUsize>,
) -> impl FnMut(String) -> std::future::Ready<Vec<StreamTrack>> {
    move |requete: String| {
        appels.fetch_add(1, Ordering::SeqCst);
        let r = requete.to_lowercase();
        std::future::ready(
            pistes
                .iter()
                .filter(|p| {
                    r.contains(
                        &p.title
                            .to_lowercase()
                            .split(" (")
                            .next()
                            .unwrap()
                            .to_string(),
                    )
                })
                .cloned()
                .collect(),
        )
    }
}

fn cfg<'a>(base: &'a str, cle: &'a str, max: usize) -> Reglages<'a> {
    Reglages {
        base,
        cle,
        pause: Duration::ZERO,
        recherches_max: max,
    }
}

fn client() -> reqwest::Client {
    crate::http::client::shared().clone()
}

#[test]
fn le_titre_est_normalise_sans_ses_mentions_de_version() {
    assert_eq!(
        cle_titre("Love & Hate"),
        cle_titre("Love & Hate (Live at Abbey Road)")
    );
    assert_eq!(
        cle_titre("Cold Little Heart"),
        cle_titre("Cold Little Heart - 2016 Remaster")
    );
    assert_eq!(
        cle_titre("Black Man in a White World"),
        cle_titre("black man in a white world!")
    );
    assert!(est_version_alternative("Home Again (Live)"));
    assert!(est_version_alternative("Home Again - Remastered 2020"));
    assert!(!est_version_alternative("Love (Is Not Enough)"));
    assert_eq!(
        cle_titre("Love (Is Not Enough)"),
        "loveisnotenough",
        "une parenthèse qui n'est pas une version reste dans le titre"
    );
}

#[test]
fn le_tri_a_quatre_niveaux() {
    let mut items = vec![
        json!({"title": "service-1"}),
        json!({"title": "lastfm-2", "lastfm_rank": 2}),
        json!({"title": "local-1", "local_play_count": 1, "lastfm_rank": 9}),
        json!({"title": "lastfm-1", "lastfm_rank": 1}),
        json!({"title": "communaute-5", CHAMP_TOTAL: 5}),
        json!({"title": "local-3", "local_play_count": 3}),
        json!({"title": "communaute-50", CHAMP_TOTAL: 50}),
        json!({"title": "service-2"}),
        json!({"title": "communaute-5-local", CHAMP_TOTAL: 5, "local_play_count": 2}),
    ];
    classer(&mut items);
    assert_eq!(
        ordre(&items),
        [
            "communaute-50",
            "communaute-5-local",
            "communaute-5",
            "local-3",
            "local-1",
            "lastfm-1",
            "lastfm-2",
            "service-1",
            "service-2"
        ]
    );
}

/// Le cas Kiwanuka : Qobuz ne rend que FUDGE, mais Last.fm connaît les
/// titres des autres albums.
#[tokio::test]
async fn kiwanuka_les_titres_des_autres_albums_remontent_apres_les_ecoutes() {
    let (url, _) = faux_lastfm(
        200,
        top(&[
            "Cold Little Heart",
            "Love & Hate",
            "Stars",
            "Summer Clothes",
        ]),
    )
    .await;
    let b = base();
    ecouter(&b, "f-1", "Summer Clothes", 3);
    ecouter(&b, "f-2", "California", 1);
    let mut items = fudge();
    let recherches = Arc::new(AtomicUsize::new(0));
    let service = vec![
        // Le live passe en premier : il ne doit pas être choisi.
        piste("c-live", "Cold Little Heart (Live)", KIWANUKA, None),
        piste("c-1", "Cold Little Heart", KIWANUKA, None),
        piste("l-1", "Love & Hate", KIWANUKA, None),
        piste("x-1", "Love & Hate", "Quelqu'un d'autre", None),
    ];
    completer(
        &client(),
        &cfg(&url, "cle-de-test", 8),
        "qobuz",
        &mut items,
        catalogue(service, recherches.clone()),
    )
    .await;
    enrichir_et_classer(&b, &client(), "qobuz", &mut items).await;

    assert_eq!(
        ordre(&items),
        [
            "Summer Clothes",      // 3 écoutes de ce serveur
            "California",          // 1 écoute
            "Cold Little Heart",   // Last.fm 1, ajoutée par la recherche
            "Love & Hate",         // Last.fm 2, ajoutée
            "Stars",               // Last.fm 3, déjà dans FUDGE
            "Just Another Moment", // l'ordre du service, en dernier
        ]
    );
    let ids: Vec<&str> = items
        .iter()
        .map(|v| v["source_id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&"c-1") && !ids.contains(&"c-live"),
        "la version d'origine : {ids:?}"
    );
    assert!(
        ids.contains(&"l-1") && !ids.contains(&"x-1"),
        "le bon interprète : {ids:?}"
    );
    assert_eq!(items[0][CHAMP_LOCAL], json!(3));
    assert_eq!(items[2][CHAMP_RANG], json!(1));
    assert_eq!(
        recherches.load(Ordering::SeqCst),
        2,
        "deux titres absents, deux recherches"
    );
}

#[tokio::test]
async fn sans_cle_la_liste_ne_change_pas_et_rien_n_est_appele() {
    let (url, appels) = faux_lastfm(200, top(&["Cold Little Heart"])).await;
    let mut items = fudge();
    let avant = items.clone();
    let recherches = Arc::new(AtomicUsize::new(0));
    completer(
        &client(),
        &cfg(&url, "  ", 8),
        "qobuz",
        &mut items,
        catalogue(vec![], recherches.clone()),
    )
    .await;
    assert_eq!(items, avant);
    assert_eq!(appels.load(Ordering::SeqCst), 0, "pas de clé, pas d'appel");
    assert_eq!(recherches.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn si_lastfm_echoue_la_liste_ne_change_pas_et_l_echec_est_memorise() {
    for (statut, corps) in [
        (500, json!({})),
        (200, json!({"error": 10, "message": "Invalid API key"})),
    ] {
        let (url, appels) = faux_lastfm(statut, corps).await;
        let mut items = fudge();
        let avant = items.clone();
        for _ in 0..2 {
            completer(
                &client(),
                &cfg(&url, "cle", 8),
                "qobuz",
                &mut items,
                catalogue(vec![], Arc::default()),
            )
            .await;
            assert_eq!(items, avant, "statut {statut} : repli sans erreur");
        }
        assert_eq!(
            appels.load(Ordering::SeqCst),
            1,
            "l'échec est mémorisé, pas de rafale"
        );
    }
}

#[tokio::test]
async fn lastfm_injoignable_est_un_repli() {
    let mut items = fudge();
    let avant = items.clone();
    completer(
        &client(),
        &cfg("http://127.0.0.1:1/", "cle", 8),
        "qobuz",
        &mut items,
        catalogue(vec![], Arc::default()),
    )
    .await;
    assert_eq!(items, avant);
}

#[tokio::test]
async fn le_succes_est_mis_en_cache() {
    let (url, appels) = faux_lastfm(200, top(&["Stars"])).await;
    for _ in 0..3 {
        let mut items = fudge();
        completer(
            &client(),
            &cfg(&url, "cle", 8),
            "qobuz",
            &mut items,
            catalogue(vec![], Arc::default()),
        )
        .await;
        assert_eq!(items[3][CHAMP_RANG], json!(1));
    }
    assert_eq!(appels.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn les_recherches_sont_bornees() {
    let titres: Vec<String> = (0..15).map(|i| format!("Titre inconnu {i}")).collect();
    let refs: Vec<&str> = titres.iter().map(String::as_str).collect();
    let (url, _) = faux_lastfm(200, top(&refs)).await;
    let mut items = fudge();
    let recherches = Arc::new(AtomicUsize::new(0));
    completer(
        &client(),
        &cfg(&url, "cle", 3),
        "qobuz",
        &mut items,
        catalogue(vec![], recherches.clone()),
    )
    .await;
    assert_eq!(recherches.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn la_version_live_s_efface_devant_l_originale_et_reste_seule_sinon() {
    let (url, _) = faux_lastfm(200, top(&["Home Again", "Cold Little Heart"])).await;
    let mut items = vec![
        json_de(&piste("h-live", "Home Again (Live)", KIWANUKA, None)),
        json_de(&piste("h-1", "Home Again", KIWANUKA, None)),
        json_de(&piste("c-live", "Cold Little Heart - Live", KIWANUKA, None)),
    ];
    completer(
        &client(),
        &cfg(&url, "cle", 0),
        "qobuz",
        &mut items,
        catalogue(vec![], Arc::default()),
    )
    .await;
    let ids: Vec<&str> = items
        .iter()
        .map(|v| v["source_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        ["h-1", "c-live"],
        "Home Again : l'originale seule ; Cold Little Heart n'a que le live"
    );
    assert_eq!(items[0][CHAMP_RANG], json!(1));
    assert_eq!(
        items[1][CHAMP_RANG],
        json!(2),
        "à défaut d'originale, le live porte le rang"
    );
}

#[tokio::test]
async fn une_piste_deja_presente_par_son_isrc_n_est_pas_ajoutee_deux_fois() {
    let (url, _) = faux_lastfm(200, top(&["Cold Little Heart"])).await;
    let mut items = vec![json_de(&piste(
        "a-1",
        "Autre titre",
        KIWANUKA,
        Some("GBUM71600001"),
    ))];
    // Même enregistrement sous un autre identifiant : l'ISRC le dit.
    let service = vec![piste(
        "a-2",
        "Cold Little Heart",
        KIWANUKA,
        Some("GBUM71600001"),
    )];
    completer(
        &client(),
        &cfg(&url, "cle", 8),
        "qobuz",
        &mut items,
        catalogue(service, Arc::default()),
    )
    .await;
    assert_eq!(items.len(), 1, "{items:?}");
}
