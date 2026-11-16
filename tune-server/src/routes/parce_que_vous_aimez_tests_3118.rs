//! Témoins de « Parce que vous aimez <artiste> » (web#3118), sans réseau :
//! une base en mémoire, des voisins du cloud fournis à la main, un service
//! factice.

use super::*;
use tune_core::streaming::traits::StreamTrack;

fn etat() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn sql(db: &Arc<dyn DbBackend>, requete: &str) {
    db.execute(requete, &[]).expect(requete);
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
    let ecartees = Ecartees::default();
    composer(Entrees {
        db,
        profil: 1,
        jour,
        graines,
        voisins: v,
        services,
        ecartees: &ecartees,
    })
    .await
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

#[test]
fn les_graines_suivent_le_profil_sans_radio_ni_compilation() {
    let state = etat();
    let db = &state.backend;
    ecouter(db, "Miles Davis", 5, None, "local");
    ecouter(db, "John Coltrane", 3, Some(1), "qobuz");
    ecouter(db, "Metallica", 10, Some(2), "local");
    ecouter(db, "Various Artists", 20, None, "local");
    ecouter(db, "France Inter", 30, None, "radio");

    let g = graines(db, 1, &Ecartees::default(), 5);
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
    let mut e = Ecartees::default();
    let s: tune_core::db::suggestions_ecartees::SuggestionEcartee =
        serde_json::from_value(json!({"item_type": "artist", "item_id": "Miles Davis"})).unwrap();
    e.ajouter(&s);
    let g = graines(db, 1, &e, 5);
    assert_eq!(g, vec![("John Coltrane".to_string(), 3)]);
}

#[tokio::test]
async fn cinquante_titres_de_la_bibliotheque_autour_de_l_artiste() {
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
async fn le_meme_jour_donne_la_meme_playlist() {
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
    let c = composer_pour(db, "2026-11-17", &graines, v(), Vec::new()).await;
    assert_ne!(ids(&a[0]), ids(&c[0]), "le lendemain, une autre liste");
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
    assert!(pl["track_count"].as_u64().unwrap() >= 20, "{pl}");
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
async fn la_route_garde_sa_reponse_pour_la_journee() {
    let state = etat();
    let db = &state.backend;
    bibliotheque(db);
    // Aucun appel réel au cloud : un port fermé, refus immédiat.
    SettingsRepo::with_backend(db.clone())
        .set("artist_enrichment_api", "http://127.0.0.1:9")
        .unwrap();
    ecouter(db, "Miles Davis", 6, None, "local");
    ecouter(db, "John Coltrane", 4, None, "local");

    let st = &state;
    let appel = |limit| async move {
        let Ok(Json(v)) =
            because_you_like(State(st.clone()), ActiveProfile(1), Query(Params { limit })).await
        else {
            panic!("la route doit répondre")
        };
        v
    };
    let a = appel(None).await;
    assert_eq!(a["playlists"].as_array().unwrap().len(), 2, "{a}");
    assert!(
        lire_cache(db, 1, a["date"].as_str().unwrap()).is_some(),
        "le cache est écrit"
    );

    // De nouvelles écoutes ne rebattent pas les cartes du jour.
    ecouter(db, "Metallica", 50, None, "local");
    let b = appel(None).await;
    assert_eq!(a, b, "même journée, même réponse");

    let un = appel(Some(1)).await;
    assert_eq!(un["playlists"].as_array().unwrap().len(), 1);
    assert_eq!(un["playlists"][0], a["playlists"][0]);
}
