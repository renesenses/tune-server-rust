//! Témoins de « Parce que vous aimez <artiste> » (web#3118), sans réseau :
//! une base en mémoire, des voisins du cloud fournis à la main, un service
//! factice.

use super::*;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::streaming::traits::StreamTrack;

fn etat() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn sans_filtres() -> Filtres {
    Filtres {
        ecartees: Ecartees::default(),
        bannis_locaux: HashSet::new(),
        bannis_services: HashSet::new(),
        parental: None,
        explicites_locaux: HashSet::new(),
        pistes_bloquees: HashSet::new(),
        noms_bloques_par_id: HashSet::new(),
    }
}

fn poser_politique(db: &Arc<dyn DbBackend>, profil: i64, politique: Value) {
    let p: tune_core::db::politique_parentale::PolitiqueParentale =
        serde_json::from_value(politique).unwrap();
    tune_core::db::politique_parentale::poser(db, profil, 1, 0, &p).unwrap();
}

/// Un état dont l'enrichissement ne part jamais sur le réseau : un port
/// fermé, refus immédiat.
fn etat_hors_ligne() -> AppState {
    let state = etat();
    SettingsRepo::with_backend(state.backend.clone())
        .set("artist_enrichment_api", "http://127.0.0.1:9")
        .unwrap();
    state
}

async fn appel(state: &AppState, profil: i64, limit: Option<usize>) -> Value {
    let Ok(Json(v)) = because_you_like(
        State(state.clone()),
        ActiveProfile(profil),
        Query(Params { limit }),
    )
    .await
    else {
        panic!("la route doit répondre")
    };
    v
}

fn ecarter_artiste(db: &Arc<dyn DbBackend>, profil: i64, nom: &str) {
    let s: tune_core::db::suggestions_ecartees::SuggestionEcartee =
        serde_json::from_value(json!({"item_type": "artist", "item_id": nom})).unwrap();
    SuggestionsEcarteesRepo::with_backend(db.clone())
        .ecarter(profil, &s)
        .unwrap();
}

fn maintenant() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn ecouter(db: &Arc<dyn DbBackend>, artiste: &str, fois: usize, profil: Option<i64>, source: &str) {
    let quand = maintenant();
    for _ in 0..fois {
        db.execute(
            "INSERT INTO listen_history (title, artist_name, listened_at, profile_id, source) \
             VALUES ('T', ?1, ?2, ?3, ?4)",
            &[&artiste, &quand.as_str(), &profil, &source],
        )
        .unwrap();
    }
}

/// Un artiste de bibliothèque : `albums` albums de `par_album` titres.
fn artiste(db: &Arc<dyn DbBackend>, id: i64, nom: &str, genre: &str, albums: i64, par_album: i64) {
    db.execute(
        "INSERT INTO artists (id, name) VALUES (?1, ?2)",
        &[&id, &nom],
    )
    .unwrap();
    for a in 0..albums {
        let album_id = id * 100 + a;
        let titre_album = format!("{nom} album {a}");
        let pochette = format!("/covers/{album_id}.jpg");
        db.execute(
            "INSERT INTO albums (id, title, artist_id, cover_path) VALUES (?1, ?2, ?3, ?4)",
            &[&album_id, &titre_album.as_str(), &id, &pochette.as_str()],
        )
        .unwrap();
        for n in 0..par_album {
            let track_id = album_id * 100 + n;
            let titre = format!("{nom} {a}-{n}");
            db.execute(
                "INSERT INTO tracks (id, title, artist_id, album_id, genre, duration_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 200000)",
                &[&track_id, &titre.as_str(), &id, &album_id, &genre],
            )
            .unwrap();
        }
    }
}

fn bibliotheque(db: &Arc<dyn DbBackend>) {
    artiste(db, 1, "Miles Davis", "Jazz", 3, 8);
    artiste(db, 2, "John Coltrane", "Jazz", 2, 8);
    artiste(db, 3, "Bill Evans", "Jazz", 2, 8);
    artiste(db, 4, "Metallica", "Rock", 2, 10);
    artiste(db, 5, "Herbie Hancock", "Jazz", 2, 8);
}

fn voisins(paires: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
    paires
        .iter()
        .map(|(g, v)| (normaliser(g), v.iter().map(|s| s.to_string()).collect()))
        .collect()
}

async fn composer_pour(
    db: &Arc<dyn DbBackend>,
    jour: &str,
    graines: &[(String, i64)],
    v: HashMap<String, Vec<String>>,
    services: Vec<Box<dyn SourceRadio>>,
) -> Vec<Value> {
    composer(Entrees {
        db,
        profil: 1,
        jour,
        graines,
        voisins: v,
        services,
        filtres: Arc::new(sans_filtres()),
    })
    .await
    .playlists
}

fn ids(p: &Value) -> Vec<String> {
    p["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            t["track_id"]
                .as_i64()
                .map(|i| i.to_string())
                .or_else(|| t["source_id"].as_str().map(str::to_owned))
                .unwrap()
        })
        .collect()
}

fn artistes_de(p: &Value) -> HashSet<String> {
    p["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["artist_name"].as_str().map(str::to_owned))
        .collect()
}

/// Des blocs FIXES de trois jours : trois jours de suite donnent la même
/// période, le quatrième en ouvre une autre.
#[test]
fn la_periode_dure_trois_jours() {
    let d = |s: &str| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
    let texte = |x: chrono::NaiveDate| x.format("%Y-%m-%d").to_string();
    let mut debut = d("2026-11-01");
    while periode(debut) != texte(debut) {
        debut = debut.succ_opt().unwrap();
    }
    let p = periode(debut);
    for k in 0..PERIODE_JOURS {
        assert_eq!(periode(debut + chrono::Duration::days(k)), p, "jour {k}");
    }
    let suivant = debut + chrono::Duration::days(PERIODE_JOURS);
    assert_eq!(
        periode(suivant),
        texte(suivant),
        "le 4e jour ouvre une période"
    );
    let veille = debut - chrono::Duration::days(1);
    assert_eq!(
        periode(veille),
        texte(debut - chrono::Duration::days(PERIODE_JOURS))
    );
}

#[test]
fn les_graines_suivent_le_profil_sans_radio_ni_compilation() {
    let state = etat();
    let db = &state.backend;
    ecouter(db, "Miles Davis", 5, None, "local");
    ecouter(db, "John Coltrane", 3, Some(1), "qobuz");
    ecouter(db, "Metallica", 10, Some(2), "local");
    ecouter(db, "Various Artists", 20, None, "local");
    ecouter(db, "France Inter", 30, None, "radio");

    let g = graines(db, 1, &sans_filtres(), 5);
    assert_eq!(
        g,
        vec![
            ("Miles Davis".to_string(), 5),
            ("John Coltrane".to_string(), 3)
        ],
        "profil 2, compilation et radio écartés"
    );
}

#[test]
fn une_graine_ecartee_par_pas_pour_moi_ne_revient_pas() {
    let state = etat();
    let db = &state.backend;
    ecouter(db, "Miles Davis", 5, None, "local");
    ecouter(db, "John Coltrane", 3, None, "local");
    let s: tune_core::db::suggestions_ecartees::SuggestionEcartee =
        serde_json::from_value(json!({"item_type": "artist", "item_id": "Miles Davis"})).unwrap();
    let mut f = sans_filtres();
    f.ecartees.ajouter(&s);
    let g = graines(db, 1, &f, 5);
    assert_eq!(g, vec![("John Coltrane".to_string(), 3)]);
}

#[tokio::test]
async fn vingt_titres_de_la_bibliotheque_autour_de_l_artiste() {
    let state = etat();
    let db = &state.backend;
    bibliotheque(db);
    let graines = vec![("Miles Davis".to_string(), 12)];
    let v = voisins(&[(
        "Miles Davis",
        &["John Coltrane", "Absent Artist", "Metallica"],
    )]);

    let p = composer_pour(db, "2026-11-16", &graines, v, Vec::new()).await;
    assert_eq!(p.len(), 1, "{p:?}");
    let pl = &p[0];
    assert_eq!(pl["track_count"], json!(TITRES_PAR_PLAYLIST), "{pl}");
    assert_eq!(pl["artist"], json!("Miles Davis"));
    assert_eq!(
        pl["tracks"][0]["artist_name"],
        json!("Miles Davis"),
        "la playlist s'ouvre sur l'artiste"
    );
    let arts = artistes_de(pl);
    assert!(
        arts.contains("John Coltrane"),
        "voisin du cloud présent : {arts:?}"
    );
    assert!(
        arts.contains("Metallica"),
        "voisin du cloud présent, même hors genre : {arts:?}"
    );
    assert!(
        arts.contains("Bill Evans") || arts.contains("Herbie Hancock"),
        "voisins de même genre : {arts:?}"
    );
    assert!(!arts.contains("Absent Artist"));
    assert_eq!(pl["sources"], json!(["local"]));
    let t = &pl["tracks"][0];
    assert!(t["album_id"].as_i64().is_some(), "{t}");
    assert!(
        t["cover_path"]
            .as_str()
            .is_some_and(|c| c.starts_with("/covers/")),
        "{t}"
    );
    let covers = pl["covers"].as_array().unwrap();
    assert!(!covers.is_empty() && covers.len() <= 4, "{covers:?}");
    // Aucun doublon.
    let tous = ids(pl);
    let uniques: HashSet<&String> = tous.iter().collect();
    assert_eq!(uniques.len(), tous.len());
}

#[tokio::test]
async fn la_meme_periode_donne_la_meme_playlist() {
    let state = etat();
    let db = &state.backend;
    bibliotheque(db);
    let graines = vec![("Miles Davis".to_string(), 12)];
    let v = || voisins(&[("Miles Davis", &["John Coltrane"])]);

    let a = composer_pour(db, "2026-11-16", &graines, v(), Vec::new()).await;
    let b = composer_pour(db, "2026-11-16", &graines, v(), Vec::new()).await;
    assert_eq!(
        ids(&a[0]),
        ids(&b[0]),
        "deux rafraîchissements, la même liste"
    );
    let c = composer_pour(db, "2026-11-19", &graines, v(), Vec::new()).await;
    assert_ne!(
        ids(&a[0]),
        ids(&c[0]),
        "la période suivante, une autre liste"
    );
}

#[tokio::test]
async fn un_titre_ne_sert_qu_une_playlist() {
    let state = etat();
    let db = &state.backend;
    bibliotheque(db);
    let graines = vec![
        ("Miles Davis".to_string(), 12),
        ("John Coltrane".to_string(), 8),
    ];
    let p = composer_pour(db, "2026-11-16", &graines, HashMap::new(), Vec::new()).await;
    assert_eq!(p.len(), 2, "{p:?}");
    let a: HashSet<String> = ids(&p[0]).into_iter().collect();
    let b: HashSet<String> = ids(&p[1]).into_iter().collect();
    assert!(
        a.is_disjoint(&b),
        "{:?}",
        a.intersection(&b).collect::<Vec<_>>()
    );
}

/// Un service factice : « Ghost » y a vingt titres, et rien d'autre.
struct ServiceFactice;

#[async_trait::async_trait]
impl SourceRadio for ServiceFactice {
    fn nom(&self) -> String {
        "factice".into()
    }
    async fn artistes_similaires(&self, _artiste: &str, _max: usize) -> Vec<String> {
        Vec::new()
    }
    async fn titres_de(&self, artiste: &str, max: usize) -> Vec<Candidat> {
        if artiste != "Ghost" {
            return Vec::new();
        }
        (0..20)
            .take(max)
            .map(|n| Candidat::Service {
                source: "factice".into(),
                piste: serde_json::from_value::<StreamTrack>(json!({
                    "source_id": format!("g{n}"),
                    "title": format!("Ghost {n}"),
                    "artist_name": "Ghost",
                    "duration_ms": 180000,
                    "explicit": false,
                }))
                .unwrap(),
            })
            .collect()
    }
}

#[tokio::test]
async fn les_services_completent_une_petite_bibliotheque() {
    let state = etat();
    let db = &state.backend;
    artiste(db, 1, "Miles Davis", "Jazz", 1, 5);
    let graines = vec![("Miles Davis".to_string(), 12)];
    let v = || voisins(&[("Miles Davis", &["Ghost"])]);

    let seule = composer_pour(db, "2026-11-16", &graines, v(), Vec::new()).await;
    assert!(
        seule.is_empty(),
        "5 titres : sous le minimum, pas de carte : {seule:?}"
    );

    let avec = composer_pour(
        db,
        "2026-11-16",
        &graines,
        v(),
        vec![Box::new(ServiceFactice)],
    )
    .await;
    assert_eq!(avec.len(), 1, "{avec:?}");
    let pl = &avec[0];
    assert_eq!(pl["track_count"], json!(TITRES_PAR_PLAYLIST), "{pl}");
    assert_eq!(pl["sources"].as_array().unwrap().len(), 2, "{pl}");
    assert!(artistes_de(pl).contains("Ghost"));
}

#[tokio::test]
async fn sans_historique_rien_n_est_propose() {
    let state = etat();
    bibliotheque(&state.backend);
    let Ok(r) = because_you_like(
        State(state.clone()),
        ActiveProfile(1),
        Query(Params { limit: None }),
    )
    .await
    else {
        panic!("la route doit répondre")
    };
    assert_eq!(r.0["playlists"], json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn la_route_garde_sa_reponse_pour_la_periode() {
    let state = etat_hors_ligne();
    let db = &state.backend;
    bibliotheque(db);
    ecouter(db, "Miles Davis", 6, None, "local");
    ecouter(db, "John Coltrane", 4, None, "local");

    let a = appel(&state, 1, None).await;
    assert_eq!(a["playlists"].as_array().unwrap().len(), 2, "{a}");

    // De nouvelles écoutes ne rebattent pas les cartes de la période.
    ecouter(db, "Metallica", 50, None, "local");
    let b = appel(&state, 1, None).await;
    assert_eq!(a, b, "même période, même réponse");
    assert_eq!(a["refresh_days"], json!(PERIODE_JOURS));
    assert_eq!(a["playlists"][0]["track_count"], json!(TITRES_PAR_PLAYLIST));

    let un = appel(&state, 1, Some(1)).await;
    assert_eq!(un["playlists"].as_array().unwrap().len(), 1);
    assert_eq!(un["playlists"][0], a["playlists"][0]);

    // Un autre profil a son propre calcul (verrou et cache par profil) : ses
    // écoutes à lui, plus les anciennes sans profil.
    ecouter(db, "Herbie Hancock", 200, Some(2), "local");
    let autre = appel(&state, 2, None).await;
    assert_eq!(
        autre["playlists"][0]["artist"],
        json!("Herbie Hancock"),
        "{autre}"
    );
}

/// Le cache vit en mémoire : rien dans la table des réglages, donc rien dans
/// `GET /system/config`, qui la publie. Les clés d'une version antérieure
/// sont retirées.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_cache_ne_sort_par_aucune_route_generique() {
    let state = etat_hors_ligne();
    let db = &state.backend;
    bibliotheque(db);
    ecouter(db, "Miles Davis", 6, None, "local");
    SettingsRepo::with_backend(db.clone())
        .set("home_because_you_like_p1", r#"{"v":2,"playlists":[]}"#)
        .unwrap();

    let a = appel(&state, 1, None).await;
    assert_eq!(a["playlists"].as_array().unwrap().len(), 1, "{a}");

    let restantes = db
        .query_one(
            "SELECT COUNT(*) FROM settings WHERE key LIKE 'home_because_you_like%'",
            &[],
        )
        .unwrap()
        .and_then(|r| r[0].as_i64())
        .unwrap_or(-1);
    assert_eq!(restantes, 0, "aucune clé de ce cache dans les réglages");

    use tower::ServiceExt;
    let reponse = crate::routes::router(state.clone())
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/v1/system/config")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(reponse.status().is_success(), "{}", reponse.status());
    let corps = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let texte = String::from_utf8_lossy(&corps);
    assert!(!texte.contains("because_you_like"), "{texte}");
    assert!(
        !texte.contains("Miles Davis"),
        "les playlists ne sortent pas par la configuration"
    );
}

/// « Pas pour moi » et les titres bannis s'appliquent à la lecture, même
/// quand la réponse vient du cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn les_decisions_du_profil_s_appliquent_au_cache() {
    let state = etat_hors_ligne();
    let db = &state.backend;
    bibliotheque(db);
    ecouter(db, "Miles Davis", 6, None, "local");
    ecouter(db, "John Coltrane", 4, None, "local");

    let a = appel(&state, 1, None).await;
    assert_eq!(a["playlists"].as_array().unwrap().len(), 2, "{a}");
    let miles = &a["playlists"][0];
    assert_eq!(miles["artist"], json!("Miles Davis"));
    let banni = miles["tracks"][0]["track_id"].as_i64().unwrap();

    ecarter_artiste(db, 1, "John Coltrane");
    tune_core::db::hidden_repo::HiddenRepo::with_backend(db.clone())
        .ban_track(1, banni)
        .unwrap();

    let b = appel(&state, 1, None).await;
    let listes = b["playlists"].as_array().unwrap();
    assert!(
        listes.iter().all(|p| p["artist"] != json!("John Coltrane")),
        "la playlist de l'artiste écarté disparaît : {b}"
    );
    for p in listes {
        assert!(!artistes_de(p).contains("John Coltrane"), "{p}");
        assert!(
            !ids(p).contains(&banni.to_string()),
            "titre banni servi : {p}"
        );
        assert_eq!(
            p["track_count"],
            json!(p["tracks"].as_array().unwrap().len())
        );
    }
}

/// Profil supervisé : ses seules écoutes désignent les artistes, et la
/// politique (artiste bloqué, contenus explicites masqués) retire graines et
/// titres.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_profil_supervise_suit_sa_politique() {
    let state = etat_hors_ligne();
    let db = &state.backend;
    bibliotheque(db);
    db.execute("UPDATE tracks SET explicit = 1 WHERE artist_id = 5", &[])
        .unwrap();
    let politique: tune_core::db::politique_parentale::PolitiqueParentale =
        serde_json::from_value(json!({
            "artistes_bloques": [{"nom": "Bill Evans"}, {"nom": "John Coltrane"}],
            "masquer_explicite": true,
        }))
        .unwrap();
    tune_core::db::politique_parentale::poser(db, 3, 1, 0, &politique).unwrap();
    // Des écoutes sans profil : elles comptent pour les autres, pas pour lui.
    ecouter(db, "Metallica", 50, None, "local");
    ecouter(db, "John Coltrane", 40, Some(3), "local");
    ecouter(db, "Miles Davis", 6, Some(3), "local");

    let f = charger_filtres(db, 3, &[]);
    assert_eq!(
        graines(db, 3, &f, 5),
        vec![("Miles Davis".to_string(), 6)],
        "écoutes strictement à lui, artiste bloqué retiré"
    );

    let r = appel(&state, 3, None).await;
    let listes = r["playlists"].as_array().unwrap();
    assert_eq!(listes.len(), 1, "{r}");
    let arts = artistes_de(&listes[0]);
    assert!(
        !arts.contains("Bill Evans") && !arts.contains("John Coltrane"),
        "{arts:?}"
    );
    assert!(
        !arts.contains("Herbie Hancock"),
        "titres explicites masqués : {arts:?}"
    );
}

#[test]
fn la_duree_de_vie_suit_la_qualite() {
    let t0 = Instant::now();
    let e = |expire| EntreeCache {
        periode: "2026-11-16".into(),
        playlists: Arc::new(Vec::new()),
        expire,
        supervise: false,
    };
    assert!(e(None).valide("2026-11-16", t0 + Duration::from_secs(86_400), false));
    assert!(
        !e(None).valide("2026-11-19", t0, false),
        "période suivante : recalcul"
    );
    assert!(
        !e(None).valide("2026-11-16", t0, true),
        "politique posée depuis : recalcul"
    );
    let degradee = e(Some(t0 + DUREE_DEGRADE));
    assert!(degradee.valide("2026-11-16", t0 + Duration::from_secs(60), false));
    assert!(!degradee.valide(
        "2026-11-16",
        t0 + DUREE_DEGRADE + Duration::from_secs(1),
        false
    ));
    assert!(DUREE_VIDE < DUREE_DEGRADE);
}

/// Artiste bloqué par son seul identifiant, et artiste d'ALBUM bloqué : ni
/// graine, ni titres — même ceux d'un autre interprète sur son album.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_artiste_bloque_par_id_ou_comme_artiste_d_album_ne_passe_pas() {
    let state = etat_hors_ligne();
    let db = &state.backend;
    bibliotheque(db);
    // « Sideman » joue sur un album de Bill Evans.
    db.execute("INSERT INTO artists (id, name) VALUES (6, 'Sideman')", &[])
        .unwrap();
    db.execute(
        "INSERT INTO albums (id, title, artist_id, cover_path) \
         VALUES (700, 'Evans sessions', 3, '/covers/700.jpg')",
        &[],
    )
    .unwrap();
    for n in 0..8i64 {
        let id = 70_000 + n;
        let titre = format!("Sideman {n}");
        db.execute(
            "INSERT INTO tracks (id, title, artist_id, album_id, genre, duration_ms) \
             VALUES (?1, ?2, 6, 700, 'Jazz', 200000)",
            &[&id, &titre.as_str()],
        )
        .unwrap();
    }
    poser_politique(
        db,
        3,
        json!({"artistes_bloques": [{"id": 2}, {"nom": "Bill Evans"}]}),
    );
    ecouter(db, "John Coltrane", 40, Some(3), "local");
    ecouter(db, "Miles Davis", 6, Some(3), "local");

    let f = charger_filtres(db, 3, &[]);
    assert_eq!(
        graines(db, 3, &f, 5),
        vec![("Miles Davis".to_string(), 6)],
        "bloqué par identifiant seul : pas une graine"
    );
    let r = appel(&state, 3, None).await;
    let listes = r["playlists"].as_array().unwrap();
    assert_eq!(listes.len(), 1, "{r}");
    let arts = artistes_de(&listes[0]);
    assert!(
        !arts.contains("John Coltrane"),
        "bloqué par identifiant : {arts:?}"
    );
    assert!(!arts.contains("Bill Evans"), "{arts:?}");
    assert!(
        !arts.contains("Sideman"),
        "titres d'un album de l'artiste bloqué : {arts:?}"
    );
}

/// Une politique posée après le calcul invalide le cache : les graines du
/// profil deviennent strictes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn une_politique_posee_apres_le_calcul_le_refait() {
    let state = etat_hors_ligne();
    let db = &state.backend;
    bibliotheque(db);
    ecouter(db, "Metallica", 50, None, "local");
    ecouter(db, "Miles Davis", 6, Some(4), "local");
    let artistes = |r: &Value| -> Vec<Value> {
        r["playlists"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["artist"].clone())
            .collect()
    };

    let avant = appel(&state, 4, None).await;
    assert!(artistes(&avant).contains(&json!("Metallica")), "{avant}");

    poser_politique(db, 4, json!({}));
    let apres = appel(&state, 4, None).await;
    assert_eq!(artistes(&apres), vec![json!("Miles Davis")], "{apres}");
}
