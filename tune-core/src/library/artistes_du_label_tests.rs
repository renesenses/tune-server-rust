//! Témoins de #6782 — artistes du même label. Aucun appel réseau : le
//! catalogue est simulé, ou servi par un faux Qobuz local.
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::db::sqlite::SqliteDb;

fn album(id: &str, titre: &str, artiste: &str, artiste_id: Option<&str>, an: u32) -> StreamAlbum {
    StreamAlbum {
        ai_generated: None,
        release_type: None,
        id: id.into(),
        title: titre.into(),
        artist: artiste.into(),
        artist_id: artiste_id.map(String::from),
        cover_path: Some(format!("https://static.qobuz.test/{id}.jpg")),
        year: Some(an),
        track_count: 8,
        quality: None,
        released_at: None,
    }
}

fn catalogue_ecm() -> Vec<StreamAlbum> {
    vec![
        album("1", "Köln Concert", "Keith Jarrett", Some("10"), 1975),
        album("2", "Officium", "Jan Garbarek", Some("20"), 1994),
        album("3", "Facing You", "Keith Jarrett", Some("10"), 1972),
        album("4", "Dis", "Jan Garbarek", Some("20"), 1976),
        album("5", "Rarum", "Jan Garbarek", Some("20"), 2002),
        album("6", "Music for 18 Musicians", "Steve Reich", None, 1978),
        album("7", "ECM Anthology", "Various Artists", None, 2019),
        album("8", "Lontano", "Tomasz Stańko", Some("40"), 2006),
    ]
}

#[test]
fn le_catalogue_se_regroupe_par_artiste_sans_la_bibliotheque_ni_les_collectifs() {
    // « Tomasz Stanko » est déjà dans la bibliothèque, sans accent.
    let deja: HashSet<String> = ["tomasz stanko".to_string()].into();
    let rendu = regrouper(&catalogue_ecm(), &deja);
    let noms: Vec<&str> = rendu.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(
        noms,
        vec!["Jan Garbarek", "Keith Jarrett", "Steve Reich"],
        "trié par nombre d'albums puis par nom ; ni Various Artists ni un \
         artiste déjà présent (accents près)"
    );
    let garbarek = &rendu[0];
    assert_eq!(garbarek.album_count, 3);
    assert_eq!(garbarek.qobuz_artist_id.as_deref(), Some("20"));
    assert_eq!(
        garbarek.album_phare.title, "Rarum",
        "l'album phare est le plus récent"
    );
    assert_eq!(
        garbarek.album_phare.cover_url.as_deref(),
        Some("https://static.qobuz.test/5.jpg")
    );
    assert_eq!(rendu[1].album_count, 2);
    assert_eq!(rendu[1].album_phare.title, "Köln Concert");
    assert_eq!(rendu[2].qobuz_artist_id, None);
}

#[test]
fn deux_graphies_du_meme_artiste_ne_font_qu_un() {
    let albums = vec![
        album("1", "A", "Bill Évans", None, 1961),
        album("2", "B", "bill  evans", Some("7"), 1962),
        album("3", "C", "Bill Evans Trio", None, 1963),
    ];
    let rendu = regrouper(&albums, &HashSet::new());
    assert_eq!(rendu.len(), 2, "{rendu:?}");
    assert_eq!(rendu[0].name, "Bill Évans", "le nom du premier album");
    assert_eq!(rendu[0].album_count, 2);
    assert_eq!(rendu[0].qobuz_artist_id.as_deref(), Some("7"));
}

struct Compteur {
    appels: AtomicUsize,
    echoue: bool,
}

#[async_trait::async_trait]
impl CatalogueDuLabel for Compteur {
    async fn albums_du_label(&self, _: &str) -> Result<Vec<StreamAlbum>, String> {
        self.appels.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        if self.echoue {
            Err("HTTP 500".into())
        } else {
            Ok(catalogue_ecm())
        }
    }
}

/// Dix ouvertures simultanées de la même page : UNE lecture du catalogue.
/// Puis le cache sert jusqu'à sa durée, et une lecture est refaite après.
#[tokio::test(flavor = "multi_thread")]
async fn le_catalogue_d_un_label_n_est_lu_qu_une_fois_par_jour() {
    let cache = Arc::new(CacheDecouverte::default());
    let cat = Arc::new(Compteur {
        appels: AtomicUsize::new(0),
        echoue: false,
    });
    let mut taches = Vec::new();
    for _ in 0..10 {
        let (cache, cat) = (cache.clone(), cat.clone());
        taches.push(tokio::spawn(async move {
            cache.albums(&*cat, "9", || 1_000).await.map(|a| a.len())
        }));
    }
    for t in taches {
        assert_eq!(t.await.unwrap(), Ok(8));
    }
    assert_eq!(cat.appels.load(Ordering::SeqCst), 1, "pas de rafale");

    let presque = 1_000 + DUREE_SUCCES_S - 1;
    cache.albums(&*cat, "9", || presque).await.unwrap();
    assert_eq!(cat.appels.load(Ordering::SeqCst), 1, "encore frais");
    cache
        .albums(&*cat, "9", || 1_000 + DUREE_SUCCES_S)
        .await
        .unwrap();
    assert_eq!(cat.appels.load(Ordering::SeqCst), 2, "relu après un jour");

    // Un autre label est une autre entrée.
    cache.albums(&*cat, "10", || 1_000).await.unwrap();
    assert_eq!(cat.appels.load(Ordering::SeqCst), 3);
}

/// Une lecture en échec est gardée une heure : la page rouverte ne relance
/// pas Qobuz à chaque fois.
#[tokio::test]
async fn un_echec_est_garde_une_heure() {
    let cache = CacheDecouverte::default();
    let cat = Compteur {
        appels: AtomicUsize::new(0),
        echoue: true,
    };
    assert!(cache.albums(&cat, "9", || 5_000).await.is_err());
    assert!(cache.albums(&cat, "9", || 5_000 + 60).await.is_err());
    assert_eq!(cat.appels.load(Ordering::SeqCst), 1);
    assert!(
        cache
            .albums(&cat, "9", || 5_000 + DUREE_ECHEC_S)
            .await
            .is_err()
    );
    assert_eq!(cat.appels.load(Ordering::SeqCst), 2);
}

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

fn artiste(db: &Arc<dyn DbBackend>, id: i64, nom: &str, portrait: Option<&str>) {
    db.execute(
        "INSERT INTO artists (id, name, image_path) VALUES (?, ?, ?)",
        &[&id as &dyn ToSqlValue, &nom, &portrait.map(String::from)],
    )
    .unwrap();
}

fn album_local(db: &Arc<dyn DbBackend>, titre: &str, artiste: i64, label: &str) -> i64 {
    db.execute(
        "INSERT INTO albums (title, artist_id, source, track_count, label) \
         VALUES (?, ?, 'local', 0, ?)",
        &[&titre as &dyn ToSqlValue, &artiste, &label],
    )
    .unwrap();
    db.query_many("SELECT MAX(id) FROM albums", &[]).unwrap()[0][0]
        .as_i64()
        .unwrap()
}

#[test]
fn la_bibliotheque_rend_les_artistes_du_label_par_nombre_d_albums() {
    let db = base();
    artiste(&db, 1, "Keith Jarrett", Some("/art/kj.jpg"));
    artiste(&db, 2, "Jan Garbarek", None);
    artiste(&db, 3, "Miles Davis", Some("/art/md.jpg"));
    artiste(&db, 4, "Various Artists", None);
    album_local(&db, "Köln", 1, "ECM");
    album_local(&db, "Facing You", 1, " ECM ");
    album_local(&db, "Officium", 2, "ECM");
    album_local(&db, "Kind of Blue", 3, "Columbia");
    album_local(&db, "Anthologie", 4, "ECM");
    let cache = album_local(&db, "Caché", 2, "ECM");
    db.execute(
        "INSERT INTO hidden_items (item_type, item_id) VALUES ('album', ?)",
        &[&cache as &dyn ToSqlValue],
    )
    .unwrap();

    let rendu = artistes_de_la_bibliotheque(&db, " ECM").unwrap();
    assert_eq!(
        rendu,
        vec![
            ArtisteDuLabel {
                id: 1,
                name: "Keith Jarrett".into(),
                album_count: 2,
                image_url: Some("/api/v1/library/artists/1/image".into()),
            },
            ArtisteDuLabel {
                id: 2,
                name: "Jan Garbarek".into(),
                album_count: 1,
                image_url: None,
            },
        ],
        "espaces de bord retirés, album masqué exclu, ni Columbia ni \
         Various Artists, aucun portrait inventé"
    );
    assert!(
        artistes_de_la_bibliotheque(&db, "Inconnu")
            .unwrap()
            .is_empty()
    );

    let cles = cles_de_la_bibliotheque(&db).unwrap();
    assert!(cles.contains("keith jarrett") && cles.contains("miles davis"));
}

/// Le vrai client Qobuz contre un faux Qobuz local : `label/get` paginé,
/// `extra=albums`, et la précommande écartée — la lecture partagée avec la
/// page d'un label.
#[tokio::test]
async fn qobuz_lit_le_catalogue_du_label_page_par_page() {
    use axum::extract::Query;
    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::json;
    use std::collections::HashMap as Carte;

    let journal = Arc::new(Mutex::new(Vec::<String>::new()));
    let j = journal.clone();
    let app = Router::new().route(
        "/label/get",
        get(move |Query(q): Query<Carte<String, String>>| {
            j.lock().unwrap().push(format!(
                "{} {} {}",
                q["label_id"],
                q.get("extra").cloned().unwrap_or_default(),
                q["offset"]
            ));
            async move {
                let offset: usize = q["offset"].parse().unwrap();
                let items: Vec<_> = (offset..(offset + 50).min(60))
                    .map(|i| {
                        json!({
                            "id": format!("a{i}"), "title": format!("T{i}"),
                            "artist": {"id": i % 3, "name": format!("Artiste {}", i % 3)},
                            "tracks_count": 1,
                            // La toute dernière est une précommande.
                            "released_at": if i == 59 { 4_000_000_000u64 } else { 0 },
                        })
                    })
                    .collect();
                Json(json!({"id": q["label_id"], "albums": {"total": 60, "items": items}}))
            }
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    let svc = QobuzService::avec_base_forcee(format!("http://{adresse}"));
    let albums = CatalogueDuLabel::albums_du_label(&svc, "9").await.unwrap();
    assert_eq!(albums.len(), 59, "60 albums, moins la précommande");
    assert_eq!(*journal.lock().unwrap(), vec!["9 albums 0", "9 albums 50"]);
    let rendu = regrouper(&albums, &HashSet::new());
    assert_eq!(rendu.len(), 3);
    assert_eq!(rendu.iter().map(|a| a.album_count).sum::<usize>(), 59);
}
