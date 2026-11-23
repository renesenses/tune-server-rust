//! #8452 — la cascade d'appels et le point d'entrée du sondeur, face à un
//! FAUX iTunes local. **Aucun test n'appelle iTunes.**

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::Query;
use serde_json::{Value, json};

use super::pretraitement::preparer;
use super::*;
use crate::db::backend::DbBackend;
use crate::db::sqlite::SqliteDb;

type Journal = Arc<Mutex<Vec<HashMap<String, String>>>>;

/// Les pages d'artiste du faux music.apple.com : `/artist/<nom>` rend la
/// balise `og:image` donnée, ou une page sans elle.
fn page_artiste(og: Option<&str>) -> String {
    match og {
        Some(og) => format!(
            r#"<html><head><meta property="og:title" content="x"><meta property="og:image" content="{og}"></head></html>"#
        ),
        None => "<html><head></head></html>".into(),
    }
}

/// Un faux iTunes : `reponse(chemin, paramètres)` rend la liste `results`.
/// Chaque appel est noté, paramètres compris. `/artist/<nom>` sert la page
/// d'artiste dont `pages` donne la balise `og:image`.
async fn faux_itunes(
    reponse: impl Fn(&str, &HashMap<String, String>) -> Vec<Value> + Send + Sync + 'static,
) -> (String, Journal) {
    let journal: Journal = Arc::default();
    let reponse = Arc::new(reponse);
    let route = |chemin: &'static str| {
        let (journal, reponse) = (journal.clone(), reponse.clone());
        axum::routing::get(move |Query(q): Query<HashMap<String, String>>| {
            let (journal, reponse) = (journal.clone(), reponse.clone());
            async move {
                let mut q = q;
                q.insert("_chemin".into(), chemin.into());
                journal.lock().unwrap().push(q.clone());
                let results = reponse(chemin, &q);
                axum::Json(json!({ "resultCount": results.len(), "results": results }))
            }
        })
    };
    let pages_journal = journal.clone();
    let app = axum::Router::new()
        .route("/search", route("/search"))
        .route("/lookup", route("/lookup"))
        .route(
            "/artist/{nom}",
            axum::routing::get(
                move |axum::extract::Path(nom): axum::extract::Path<String>,
                      entetes: axum::http::HeaderMap| {
                    let journal = pages_journal.clone();
                    async move {
                        let mut q = HashMap::new();
                        q.insert("_chemin".to_string(), "/artist".to_string());
                        q.insert("nom".to_string(), nom.clone());
                        q.insert(
                            "agent".to_string(),
                            entetes
                                .get("user-agent")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or_default()
                                .to_string(),
                        );
                        journal.lock().unwrap().push(q);
                        let og = match nom.as_str() {
                            "generique" => Some("https://music.apple.example/assets/meta/apple-music.png"),
                            "sans" => None,
                            _ => Some("https://is1.mzstatic.example/photo/1200x630cw.png"),
                        };
                        axum::response::Html(page_artiste(og))
                    }
                },
            ),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, journal)
}

fn source(base: &str) -> Source<'static> {
    Source {
        base: Box::leak(base.to_string().into_boxed_str()),
        limiter: false,
    }
}

fn morceau(artiste: &str, titre: &str, album: &str, image: &str, date: &str, cid: i64) -> Value {
    json!({
        "artistName": artiste, "trackName": titre, "collectionName": album,
        "artworkUrl100": format!("https://is1.mzstatic.example/{image}/100x100bb.jpg"),
        "releaseDate": format!("{date}T08:00:00Z"), "trackCount": 11,
        "collectionId": cid, "artistId": 777,
    })
}

#[tokio::test]
async fn boutique_de_la_station_puis_etats_unis_et_pochette_agrandie() {
    let (base, journal) = faux_itunes(|chemin, q| {
        if chemin == "/search" && q.get("country").map(String::as_str) == Some("US") {
            vec![morceau(
                "Roy Haynes",
                "How Deep Is the Ocean",
                "Out of the Afternoon",
                "ooa",
                "1962-01-01",
                5,
            )]
        } else {
            Vec::new()
        }
    })
    .await;
    let prep = preparer("Roy Haynes - How Deep Is the Ocean", "FIP Jazz");
    let url = chercher(source(&base), &prep, "FR", "FIP Jazz", false).await.map(|t| t.url);
    assert_eq!(
        url.as_deref(),
        Some("https://is1.mzstatic.example/ooa/600x600bb.jpg")
    );
    let appels = journal.lock().unwrap().clone();
    let recherches: Vec<_> = appels
        .iter()
        .filter(|q| q["_chemin"] == "/search")
        .collect();
    assert_eq!(
        recherches[0]["country"], "FR",
        "la boutique de la station d'abord"
    );
    assert_eq!(recherches[1]["country"], "US");
    assert_eq!(recherches[0]["entity"], "song");
    assert_eq!(recherches[0]["limit"], "50");
    assert_eq!(recherches[0]["term"], "Roy Haynes How Deep Is the Ocean");
}

#[tokio::test]
async fn la_discographie_classe_les_pochettes() {
    // Un artistId propre à cette épreuve : le cache des discographies est
    // partagé par les épreuves du processus, par artiste et par boutique.
    let artiste_8452 = |mut v: Value| {
        v["artistId"] = json!(84520);
        v
    };
    let (base, journal) = faux_itunes(move |chemin, _| match chemin {
        "/search" => vec![
            artiste_8452(morceau("Weather Report", "Black Market", "This Is Jazz, Vol. 10", "jazz", "1973-01-01", 70)),
            artiste_8452(morceau("Weather Report", "Black Market", "Black Market", "bm", "1976-03-01", 71)),
        ],
        _ => vec![json!({"wrapperType": "collection", "collectionId": 71, "releaseDate": "1976-03-01T08:00:00Z"})],
    })
    .await;
    let prep = preparer("Weather Report - Black Market", "Jazz Radio 8452");
    let url = chercher(source(&base), &prep, "US", "Jazz Radio 8452", false).await.map(|t| t.url);
    assert_eq!(
        url.as_deref(),
        Some("https://is1.mzstatic.example/bm/600x600bb.jpg")
    );
    let appels = journal.lock().unwrap().clone();
    assert!(
        appels
            .iter()
            .any(|q| q["_chemin"] == "/lookup" && q["id"] == "84520" && q["entity"] == "album"),
        "{appels:?}"
    );
}

#[tokio::test]
async fn ce_qui_n_est_pas_un_morceau_ne_coute_aucun_appel() {
    let (base, journal) = faux_itunes(|_, _| Vec::new()).await;
    let prep = preparer("STOP ADBREAK 1", "Radio 8452");
    assert_eq!(chercher(source(&base), &prep, "FR", "Radio 8452", true).await, None);
    assert!(journal.lock().unwrap().is_empty());
}

#[tokio::test]
async fn sans_concordance_pas_de_pochette() {
    let (base, _) = faux_itunes(|_, _| {
        vec![morceau(
            "Autre Artiste",
            "Autre Titre",
            "Album",
            "x",
            "2000-01-01",
            1,
        )]
    })
    .await;
    let prep = preparer("Noname - Diddy Bop", "Radio 8452 bis");
    assert_eq!(chercher(source(&base), &prep, "US", "Radio 8452 bis", false).await, None);
}

fn db() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    // Cover+ part éteint : les épreuves l'allument, sauf celle qui garde
    // l'extinction.
    regler_globalement(&db, true).unwrap();
    db
}

async fn attendre(db: &Arc<dyn DbBackend>, t: &TitreEnCours<'_>, src: Source<'static>) -> Decision {
    for _ in 0..100 {
        let d = pochette_du_titre_par(db, t, src);
        if d != Decision::EnCours {
            return d;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Decision::EnCours
}

#[tokio::test]
async fn le_sondeur_n_attend_pas_et_le_cache_sert_la_suite() {
    let (base, journal) = faux_itunes(|chemin, _| match chemin {
        "/search" => vec![morceau(
            "Air",
            "La femme d'argent",
            "Moon Safari",
            "moon",
            "1998-01-16",
            9,
        )],
        _ => Vec::new(),
    })
    .await;
    let db = db();
    let t = TitreEnCours {
        station_id: None,
        station_nom: "Radio Cover+ 8452 A",
        station_pays: Some("France"),
        artiste: Some("Air"),
        titre: "La femme d'argent",
        pochette_station: None,
    };
    let src = source(&base);
    // Premier passage : la recherche part en tâche de fond, le logo en attendant.
    assert_eq!(pochette_du_titre_par(&db, &t, src), Decision::EnCours);
    assert_eq!(
        attendre(&db, &t, src).await,
        Decision::Pochette(Trouvaille {
            url: "https://is1.mzstatic.example/moon/600x600bb.jpg".into(),
            origine: Origine::Pochette,
        })
    );
    let n = journal.lock().unwrap().len();
    // Une chaîne déjà vue n'est jamais recherchée deux fois.
    let _ = pochette_du_titre_par(&db, &t, src);
    assert_eq!(journal.lock().unwrap().len(), n);
    // Le pays en clair de la station donne sa boutique.
    assert_eq!(journal.lock().unwrap()[0]["country"], "FR");
}

#[tokio::test]
async fn un_echec_est_retenu_lui_aussi() {
    let (base, journal) = faux_itunes(|_, _| Vec::new()).await;
    let db = db();
    let t = TitreEnCours {
        station_id: None,
        station_nom: "Radio Cover+ 8452 B",
        station_pays: Some("US"),
        artiste: Some("Inconnu 8452"),
        titre: "Titre introuvable",
        pochette_station: None,
    };
    let src = source(&base);
    assert_eq!(attendre(&db, &t, src).await, Decision::Introuvable);
    let n = journal.lock().unwrap().len();
    assert_eq!(pochette_du_titre_par(&db, &t, src), Decision::Introuvable);
    assert_eq!(journal.lock().unwrap().len(), n);
}

#[tokio::test]
async fn station_qui_donne_sa_pochette_ou_station_coupee_aucune_recherche() {
    let (base, journal) = faux_itunes(|_, _| Vec::new()).await;
    let db = db();
    let src = source(&base);
    let avec_pochette = TitreEnCours {
        station_id: None,
        station_nom: "Radio Cover+ 8452 C",
        station_pays: None,
        artiste: Some("Air"),
        titre: "Sexy Boy",
        pochette_station: Some("https://station.example/pochette.jpg"),
    };
    assert_eq!(
        pochette_du_titre_par(&db, &avec_pochette, src),
        Decision::Rien
    );

    regler_la_station(&db, 8452, false).unwrap();
    assert!(!actif_pour_la_station(&db, Some(8452)));
    let coupee = TitreEnCours {
        station_id: Some(8452),
        pochette_station: None,
        ..avec_pochette.clone()
    };
    assert_eq!(pochette_du_titre_par(&db, &coupee, src), Decision::Rien);
    regler_la_station(&db, 8452, true).unwrap();
    assert!(actif_pour_la_station(&db, Some(8452)));

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(journal.lock().unwrap().is_empty(), "aucun appel attendu");
}

#[test]
fn boutique_et_chaine() {
    assert_eq!(boutique_de_la_station(Some("fr")), "FR");
    assert_eq!(boutique_de_la_station(Some("Royaume-Uni")), "GB");
    assert_eq!(boutique_de_la_station(Some("Atlantide")), "US");
    assert_eq!(boutique_de_la_station(None), "US");
    assert_eq!(chaine_du_titre(Some("Air"), "Sexy Boy"), "Air - Sexy Boy");
    assert_eq!(chaine_du_titre(None, "Air - Sexy Boy"), "Air - Sexy Boy");
}

// ---------------------------------------------------------------------------
// § 3.2 : la cascade, et § 3.6 : le repli « photo de l'artiste »
// ---------------------------------------------------------------------------

type Base = Arc<std::sync::OnceLock<String>>;

/// Un artiste de `entity=musicArtist`, sa page sur le faux music.apple.com.
fn artiste(base: &Base, nom: &str, page: &str) -> Value {
    json!({
        "wrapperType": "artist", "artistName": nom, "artistId": 4242,
        "artistLinkUrl": format!("{}/artist/{page}", base.get().expect("base posée")),
    })
}

fn recherches(journal: &Journal) -> Vec<HashMap<String, String>> {
    journal.lock().unwrap().clone()
}

#[tokio::test]
async fn la_requete_du_remixeur_ne_part_que_dans_la_boutique_de_la_station() {
    let (base, journal) = faux_itunes(|_, _| Vec::new()).await;
    let prep = preparer(
        "Lisa Leblanc - Pourquoi faire aujourd'hui (Poirier remix)",
        "Radio 8452 remix",
    );
    assert_eq!(
        chercher(source(&base), &prep, "CA", "Radio 8452 remix", false).await,
        None
    );
    let vus: Vec<(String, String)> = recherches(&journal)
        .iter()
        .map(|q| (q["country"].clone(), q["term"].clone()))
        .collect();
    assert_eq!(
        vus,
        vec![
            ("CA".into(), "Lisa Leblanc Pourquoi faire aujourd'hui poirier".into()),
            ("CA".into(), "Lisa Leblanc Pourquoi faire aujourd'hui".into()),
            ("US".into(), "Lisa Leblanc Pourquoi faire aujourd'hui".into()),
        ]
    );
}

#[tokio::test]
async fn une_station_americaine_n_interroge_qu_une_boutique() {
    let (base, journal) = faux_itunes(|_, _| Vec::new()).await;
    let prep = preparer("Noname - Diddy Bop", "KJZZ 8452");
    let _ = chercher(source(&base), &prep, "US", "KJZZ 8452", false).await;
    assert_eq!(recherches(&journal).len(), 1);
}

#[tokio::test]
async fn sans_pochette_la_photo_de_l_artiste_au_nom_exact() {
    let cellule: Base = Arc::default();
    let c = cellule.clone();
    let (base, journal) = faux_itunes(move |_, q| {
        if q.get("entity").map(String::as_str) == Some("musicArtist") {
            // « Tank And The Bangas » d'abord : jamais de préfixe.
            vec![
                artiste(&c, "Tank And The Bangas", "bangas"),
                artiste(&c, "Tank", "tank"),
            ]
        } else {
            Vec::new()
        }
    })
    .await;
    cellule.set(base.clone()).unwrap();
    let prep = preparer("Tank - When We Get On", "Radio 8452 photo");
    let t = chercher(source(&base), &prep, "US", "Radio 8452 photo", true)
        .await
        .expect("la photo de Tank");
    assert_eq!(
        t,
        Trouvaille {
            url: "https://is1.mzstatic.example/photo/600x600cc.jpg".into(),
            origine: Origine::PhotoArtiste,
        }
    );
    let pages: Vec<_> = recherches(&journal)
        .into_iter()
        .filter(|q| q["_chemin"] == "/artist")
        .collect();
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0]["nom"], "tank", "jamais la page de « Tank And The Bangas »");
    assert!(pages[0]["agent"].contains("Safari"), "{:?}", pages[0]);
    let artistes: Vec<_> = recherches(&journal)
        .into_iter()
        .filter(|q| q.get("entity").map(String::as_str) == Some("musicArtist"))
        .collect();
    assert_eq!(artistes[0]["term"], "Tank");
    assert_eq!(artistes[0]["country"], "US");
}

#[tokio::test]
async fn tank_ne_vaut_pas_tank_and_the_bangas() {
    let cellule: Base = Arc::default();
    let c = cellule.clone();
    let (base, journal) = faux_itunes(move |_, q| {
        if q.get("entity").map(String::as_str) == Some("musicArtist") {
            vec![artiste(&c, "Tank And The Bangas", "bangas")]
        } else {
            Vec::new()
        }
    })
    .await;
    cellule.set(base.clone()).unwrap();
    let prep = preparer("Tank - Maybe I Deserve", "Radio 8452 tank");
    assert_eq!(
        // Une autre boutique que l'épreuve précédente : le cache des photos
        // est par nom ET par boutique, partagé entre les épreuves.
        chercher(source(&base), &prep, "GB", "Radio 8452 tank", true).await,
        None
    );
    assert!(
        recherches(&journal).iter().all(|q| q["_chemin"] != "/artist"),
        "aucune page lue"
    );
}

#[tokio::test]
async fn le_logo_generique_d_apple_n_est_pas_une_photo_et_le_flux_iheart_prend_le_relais() {
    let cellule: Base = Arc::default();
    let c = cellule.clone();
    let (base, _) = faux_itunes(move |_, q| {
        if q.get("entity").map(String::as_str) == Some("musicArtist") {
            vec![artiste(&c, "Lizzo", "generique")]
        } else {
            Vec::new()
        }
    })
    .await;
    cellule.set(base.clone()).unwrap();
    let prep = preparer(
        r#"Lizzo - text="About Damn Time" song_spot="M" amgArtworkURL="https://i.iheart.example/a.jpg""#,
        "KIIS 8452",
    );
    assert_eq!(
        chercher(source(&base), &prep, "US", "KIIS 8452", true).await,
        Some(Trouvaille {
            url: "https://i.iheart.example/a.jpg".into(),
            origine: Origine::PochetteDuFlux,
        })
    );
}

#[tokio::test]
async fn une_collaboration_retombe_sur_le_premier_nom() {
    let cellule: Base = Arc::default();
    let c = cellule.clone();
    let (base, journal) = faux_itunes(move |_, q| {
        if q.get("entity").map(String::as_str) == Some("musicArtist")
            && q.get("term").map(String::as_str) == Some("MOGWAI")
        {
            vec![artiste(&c, "Mogwai", "mogwai")]
        } else {
            Vec::new()
        }
    })
    .await;
    cellule.set(base.clone()).unwrap();
    let prep = preparer("MOGWAI [+] IGGY POP - Punk Rock", "Radio 8452 collab");
    let t = chercher(source(&base), &prep, "US", "Radio 8452 collab", true).await;
    assert_eq!(t.map(|t| t.origine), Some(Origine::PhotoArtiste));
    let termes: Vec<String> = recherches(&journal)
        .iter()
        .filter(|q| q.get("entity").map(String::as_str) == Some("musicArtist"))
        .map(|q| q["term"].clone())
        .collect();
    assert_eq!(termes, vec!["MOGWAI [+] IGGY POP", "MOGWAI"]);
}

#[tokio::test]
async fn interrupteur_coupe_et_chaine_sans_separateur_pas_de_photo() {
    let cellule: Base = Arc::default();
    let c = cellule.clone();
    let (base, journal) = faux_itunes(move |_, q| {
        if q.get("entity").map(String::as_str) == Some("musicArtist") {
            vec![artiste(&c, "Air", "air")]
        } else {
            Vec::new()
        }
    })
    .await;
    cellule.set(base.clone()).unwrap();
    // Interrupteur coupé (Apple a changé sa page) : aucun appel « artiste ».
    let prep = preparer("Air - Sexy Boy", "Radio 8452 coupe");
    assert_eq!(
        chercher(source(&base), &prep, "US", "Radio 8452 coupe", false).await,
        None
    );
    // Sans séparateur : on ne sait pas qui est l'artiste.
    let prep = preparer("Air Sexy Boy", "Radio 8452 coupe");
    assert!(prep.hypotheses.is_empty());
    assert_eq!(
        chercher(source(&base), &prep, "US", "Radio 8452 coupe", true).await,
        None
    );
    assert!(
        recherches(&journal)
            .iter()
            .all(|q| q.get("entity").map(String::as_str) != Some("musicArtist")),
        "{:?}",
        recherches(&journal)
    );

    // Et l'interrupteur se lit dans les réglages, actif par défaut.
    let db = db();
    assert!(photo_artiste_active(&db));
    regler_photo_artiste(&db, false).unwrap();
    assert!(!photo_artiste_active(&db));
}

#[tokio::test]
async fn sans_separateur_seule_l_egalite_exacte_compte() {
    let (base, _) = faux_itunes(|_, q| {
        if q.get("entity").map(String::as_str) == Some("song") {
            vec![
                morceau("Air", "Sexy Boy (Remix)", "Remixes", "remix", "2000-01-01", 1),
                morceau("Air", "Sexy Boy", "Moon Safari", "moon", "1998-01-16", 2),
                morceau("Airbag", "Sexy Boy Blues", "Autre", "autre", "1990-01-01", 3),
            ]
        } else {
            Vec::new()
        }
    })
    .await;
    let prep = preparer("Air Sexy Boy", "Radio 8452 sans sep");
    let t = chercher(source(&base), &prep, "US", "Radio 8452 sans sep", false).await;
    assert_eq!(
        t.map(|t| t.url).as_deref(),
        Some("https://is1.mzstatic.example/moon/600x600bb.jpg"),
        "le remix n'est pas voulu : le titre de référence est la chaîne diffusée"
    );
}

/// § 3.6, point 1 : sur une station « TITRE - ARTISTE » (Oüi FM), la photo
/// cherchée est celle de la 2e partie, une fois l'ordre appris.
#[tokio::test]
async fn la_photo_suit_l_ordre_qui_reussit_sur_la_station() {
    let station = "Oüi FM 8452 Alternatif";
    let cellule: Base = Arc::default();
    let c = cellule.clone();
    let (base, journal) = faux_itunes(move |_, q| {
        match (q.get("entity").map(String::as_str), q.get("term").map(String::as_str)) {
            (Some("song"), Some("GOOD TIMES BAD TIMES LED ZEPPELIN")) => vec![morceau(
                "Led Zeppelin",
                "Good Times Bad Times",
                "Led Zeppelin",
                "lz",
                "1969-01-12",
                1,
            )],
            (Some("musicArtist"), Some("PIXIES")) => vec![artiste(&c, "Pixies", "pixies")],
            _ => Vec::new(),
        }
    })
    .await;
    cellule.set(base.clone()).unwrap();
    let prep = preparer("GOOD TIMES BAD TIMES - LED ZEPPELIN", station);
    let t = chercher(source(&base), &prep, "FR", station, true).await;
    assert_eq!(t.map(|t| t.origine), Some(Origine::Pochette));
    let prep = preparer("WHERE IS MY MIND - PIXIES", station);
    assert_eq!(artiste_pour_la_photo(station, &prep).as_deref(), Some("PIXIES"));
    let t = chercher(source(&base), &prep, "FR", station, true).await;
    assert_eq!(t.map(|t| t.origine), Some(Origine::PhotoArtiste));
    assert!(
        recherches(&journal)
            .iter()
            .any(|q| q.get("term").map(String::as_str) == Some("PIXIES")),
    );
    // Une autre station, sans ordre appris : la 1re partie.
    assert_eq!(
        artiste_pour_la_photo("Autre 8452", &prep).as_deref(),
        Some("WHERE IS MY MIND")
    );
}

#[tokio::test]
async fn livre_eteint_rien_n_est_cherche() {
    let (base, journal) = faux_itunes(|_, _| Vec::new()).await;
    let db = db();
    regler_globalement(&db, false).unwrap();
    assert!(!actif_globalement(&db));
    let t = TitreEnCours {
        station_id: None,
        station_nom: "Radio Cover+ 8452 eteint",
        station_pays: Some("FR"),
        artiste: Some("Air"),
        titre: "Kelly Watch the Stars",
        pochette_station: None,
    };
    assert_eq!(pochette_du_titre_par(&db, &t, source(&base)), Decision::Rien);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(journal.lock().unwrap().is_empty());
    // Une base neuve : éteint.
    let neuve = SqliteDb::open_in_memory().unwrap();
    neuve.init_schema().unwrap();
    crate::db::migrations::run_migrations(&neuve).unwrap();
    let neuve: Arc<dyn DbBackend> = Arc::new(neuve);
    assert!(!actif_globalement(&neuve));
}

/// Le débit (§ 3.1) : les appels passent par le limiteur iTunes partagé, un
/// appel toutes les 3 s au moins, soit 20 par minute au plus.
#[tokio::test]
async fn les_appels_passent_par_le_limiteur_partage() {
    let (limiteur, cle) = crate::library::images_rapides::Hote::Itunes.limiteur();
    assert!(limiteur.intervalle_courant(cle) >= Duration::from_secs(3));
    let (base, journal) = faux_itunes(|_, _| Vec::new()).await;
    let src = Source {
        base: Box::leak(base.into_boxed_str()),
        limiter: true,
    };
    let debut = std::time::Instant::now();
    let _ = appeler(src, "/search", &[("term", "a")]).await;
    let _ = appeler(src, "/search", &[("term", "b")]).await;
    assert_eq!(journal.lock().unwrap().len(), 2);
    assert!(
        debut.elapsed() >= Duration::from_millis(2900),
        "deux appels en {:?} : le limiteur n'a pas été traversé",
        debut.elapsed()
    );
}
