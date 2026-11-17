//! #8935 — TIDAL « Pour vous » et titres recommandés d'une playlist, lus dans
//! des réponses TIDAL simulées (forme de l'API v1, contenu fictif). Les
//! témoins de bout en bout passent par un faux TIDAL sur la boucle locale,
//! armé d'un jeton d'essai : aucun appel à TIDAL.

use super::*;
use serde_json::json;

/// Une page `pages/for_you` réduite à ce que le code lit : une rangée par
/// nature, une rangée mêlée, et ce qui doit être écarté (vidéos, promotions,
/// module sans titre).
fn page_pour_vous() -> serde_json::Value {
    json!({
        "id": "for_you",
        "title": "For You",
        "rows": [
            {"modules": [{
                "id": "m-albums", "type": "ALBUM_LIST", "title": "Suggested new albums for you",
                "description": "Based on your listening",
                "pagedList": {"items": [
                    {"id": 111, "title": "Album Un", "artist": {"id": 7, "name": "Artiste A"},
                     "cover": "aaaa-bbbb", "explicit": false},
                    {"id": 112, "title": "Album Explicite", "artist": {"id": 8, "name": "Artiste B"},
                     "explicit": true},
                    {"title": "sans identifiant"}
                ]}
            }]},
            {"modules": [{
                "id": "m-mixes", "type": "MIX_LIST", "title": "Your mixes",
                "pagedList": {"items": [
                    {"id": "00aa11bb", "title": "My Mix 1", "subTitle": "A, B",
                     "images": {"MEDIUM": {"url": "https://images.tidal.invalid/m1/640.jpg"}}}
                ]}
            }]},
            {"modules": [{
                "id": "m-pl", "type": "PLAYLIST_LIST", "title": "Recommended playlists",
                "pagedList": {"items": [
                    {"uuid": "1f2e3d4c-0000-1111-2222-333344445555", "title": "Jazz Focus",
                     "numberOfTracks": 40, "squareImage": "cccc-dddd", "creator": {"name": "TIDAL"}},
                    {"uuid": "", "title": "sans uuid"}
                ]}
            }]},
            {"modules": [{
                "id": "m-tracks", "type": "TRACK_LIST", "title": "Tracks for you",
                "pagedList": {"items": [
                    {"id": 900001, "title": "Piste Une", "duration": 200,
                     "artist": {"id": 7, "name": "Artiste A"}, "album": {"id": 111, "title": "Album Un"},
                     "explicit": true}
                ]}
            }]},
            {"modules": [{
                "id": "m-artistes", "type": "ARTIST_LIST", "title": "Artists you may like",
                "pagedList": {"items": [{"id": 55, "name": "Artiste C", "picture": "eeee-ffff"}]}
            }]},
            {"modules": [{
                "id": "m-mele", "type": "MIXED_TYPES_LIST", "title": "Because you listened",
                "pagedList": {"items": [
                    {"type": "ALBUM", "item": {"id": 113, "title": "Album Trois", "artist": {"name": "D"}}},
                    {"type": "VIDEO", "item": {"id": 4242, "title": "Une vidéo"}},
                    {"type": "MIX", "item": {"id": "00cc", "title": "Daily Discovery"}},
                    {"type": "TRACK", "item": {"id": 900002, "title": "Piste Deux", "artist": {"name": "E"}}}
                ]}
            }]},
            {"modules": [{
                "id": "m-videos", "type": "VIDEO_LIST", "title": "Videos for you",
                "pagedList": {"items": [{"id": 5, "title": "Vidéo"}]}
            }]},
            {"modules": [{
                "type": "FEATURED_PROMOTIONS", "title": "",
                "items": [{"header": "Promo", "artifactId": "x"}]
            }]},
            {"modules": [{
                "type": "ALBUM_LIST", "title": "",
                "pagedList": {"items": [{"id": 114, "title": "Rangée sans titre"}]}
            }]}
        ]
    })
}

#[test]
fn la_page_pour_vous_rend_ses_rangees_par_nature_8935() {
    let (rangees, paresseux) = TidalService::rangees_de_la_page(&page_pour_vous());
    assert!(paresseux.is_empty());
    let titres: Vec<&str> = rangees.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(
        titres,
        [
            "Suggested new albums for you",
            "Your mixes",
            "Recommended playlists",
            "Tracks for you",
            "Artists you may like",
            "Because you listened",
        ],
        "les rangées lisibles, dans l'ordre de TIDAL ; vidéos, promotions et \
         module sans titre écartés"
    );
    let albums = &rangees[0];
    assert_eq!(albums.id, "m-albums");
    assert_eq!(albums.subtitle.as_deref(), Some("Based on your listening"));
    assert_eq!(
        albums.albums.len(),
        2,
        "l'album sans identifiant est écarté"
    );
    assert_eq!(
        albums.albums[1].explicit,
        Some(true),
        "le marquage explicite suit, pour le contrôle parental"
    );
    assert_eq!(
        rangees[1].playlists[0].id, "mix:00aa11bb",
        "un mix se lit par mix:"
    );
    assert_eq!(
        rangees[2].playlists.len(),
        1,
        "la playlist sans uuid est écartée"
    );
    assert_eq!(
        rangees[2].playlists[0].id,
        "1f2e3d4c-0000-1111-2222-333344445555"
    );
    assert_eq!(rangees[3].tracks[0].id, "900001");
    assert!(rangees[3].tracks[0].explicit);
    assert_eq!(rangees[4].artists[0].name, "Artiste C");

    let mele = &rangees[5];
    assert_eq!(mele.albums.len(), 1);
    assert_eq!(mele.playlists[0].id, "mix:00cc");
    assert_eq!(
        mele.tracks
            .iter()
            .map(|t| t.id.as_str())
            .collect::<Vec<_>>(),
        ["900002"],
        "la vidéo n'est pas une piste"
    );
}

#[test]
fn une_page_refusee_ne_rend_rien_sans_paniquer_8935() {
    let (r, p) = TidalService::rangees_de_la_page(&json!({"status": 404, "subStatus": 2001}));
    assert!(r.is_empty() && p.is_empty());
    let (r, _) = TidalService::rangees_de_la_page(&json!({}));
    assert!(r.is_empty());
}

#[test]
fn seul_un_chemin_de_page_relatif_est_relu_8935() {
    assert_eq!(
        TidalService::chemin_de_module("pages/data/0a1b-2c3d?limit=20").as_deref(),
        Some("/pages/data/0a1b-2c3d?limit=20")
    );
    assert_eq!(
        TidalService::chemin_de_module("/v1/pages/data/abc").as_deref(),
        Some("/pages/data/abc")
    );
    for refuse in [
        "https://ailleurs.invalid/pages/data/x",
        "pages/../users/1/favorites",
        "users/1/favorites",
        "pages/data/x y",
        "pages/data/x#y",
    ] {
        assert_eq!(TidalService::chemin_de_module(refuse), None, "{refuse}");
    }
}

#[test]
fn les_recommandations_deballent_leurs_enveloppes_8935() {
    let data = json!({
        "limit": 20, "offset": 20, "totalNumberOfItems": 97,
        "items": [
            {"type": "track", "item": {"id": 31, "title": "Reco Un", "artist": {"name": "X"}, "explicit": true}},
            {"type": "video", "item": {"id": 32, "title": "Vidéo"}},
            {"type": "track", "item": {"title": "sans identifiant"}},
            {"id": 33, "title": "Reco nue", "artist": {"name": "Y"}}
        ]
    });
    let r = TidalService::titres_recommandes(&data, 20);
    assert_eq!(
        r.tracks
            .iter()
            .map(|t| t.title.as_str())
            .collect::<Vec<_>>(),
        ["Reco Un", "Reco nue"]
    );
    assert!(r.tracks[0].explicit);
    assert_eq!((r.total, r.offset), (97, 20));
}

#[test]
fn un_identifiant_de_playlist_malforme_est_refuse_8935() {
    assert!(TidalService::identifiant_de_playlist_sain(
        "1f2e3d4c-0000-1111-2222-333344445555"
    ));
    for refuse in ["", "../users/1", "a/b", "a?b=c", "a b"] {
        assert!(
            !TidalService::identifiant_de_playlist_sain(refuse),
            "{refuse}"
        );
    }
}

/// Ce que le faux TIDAL sert, et le journal des chemins demandés (avec leur
/// requête).
#[derive(Clone, Default)]
struct Scenario {
    pour_vous: Option<serde_json::Value>,
    accueil: Option<serde_json::Value>,
    module: Option<serde_json::Value>,
    recommandations: Option<serde_json::Value>,
}

type Journal = Arc<std::sync::Mutex<Vec<String>>>;

async fn faux_tidal(scenario: Scenario) -> (TidalService, Journal) {
    use axum::extract::{Path as Chemin, RawQuery, State};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;

    type Etat = (Arc<Scenario>, Journal);

    fn servir(corps: Option<serde_json::Value>) -> axum::response::Response {
        match corps {
            Some(c) => axum::Json(c).into_response(),
            None => (
                StatusCode::NOT_FOUND,
                axum::Json(json!({"status": 404, "subStatus": 2001, "userMessage": "not found"})),
            )
                .into_response(),
        }
    }

    fn noter(journal: &Journal, chemin: String, requete: Option<String>) {
        // `countryCode` est ajouté à toutes les requêtes : on le retire pour
        // lire le reste.
        let reste: Vec<String> = requete
            .unwrap_or_default()
            .split('&')
            .filter(|p| !p.is_empty() && !p.starts_with("countryCode="))
            .map(String::from)
            .collect();
        journal
            .lock()
            .unwrap()
            .push(format!("{chemin}?{}", reste.join("&")));
    }

    async fn page(
        State((scenario, journal)): State<Etat>,
        Chemin(nom): Chemin<String>,
        RawQuery(q): RawQuery,
    ) -> axum::response::Response {
        noter(&journal, format!("/pages/{nom}"), q);
        servir(match nom.as_str() {
            "for_you" => scenario.pour_vous.clone(),
            "home" => scenario.accueil.clone(),
            _ => None,
        })
    }

    async fn donnees(
        State((scenario, journal)): State<Etat>,
        Chemin(id): Chemin<String>,
        RawQuery(q): RawQuery,
    ) -> axum::response::Response {
        noter(&journal, format!("/pages/data/{id}"), q);
        servir(scenario.module.clone())
    }

    async fn recommandations(
        State((scenario, journal)): State<Etat>,
        Chemin(id): Chemin<String>,
        RawQuery(q): RawQuery,
    ) -> axum::response::Response {
        noter(
            &journal,
            format!("/playlists/{id}/recommendations/items"),
            q,
        );
        servir(scenario.recommandations.clone())
    }

    let journal: Journal = Arc::default();
    let app = axum::Router::new()
        .route("/pages/{nom}", get(page))
        .route("/pages/data/{id}", get(donnees))
        .route(
            "/playlists/{id}/recommendations/items",
            get(recommandations),
        )
        .with_state((Arc::new(scenario), journal.clone()));
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let adresse = ecoute.local_addr().expect("adresse locale");
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    (
        TidalService::pour_un_faux_tidal(format!("http://{adresse}")),
        journal,
    )
}

#[tokio::test]
async fn la_page_pour_vous_se_lit_puis_se_sert_du_cache_8935() {
    let (svc, journal) = faux_tidal(Scenario {
        pour_vous: Some(page_pour_vous()),
        ..Default::default()
    })
    .await;
    let page = svc.get_for_you().await.expect("page pour vous");
    assert_eq!(page.source, "for_you");
    assert_eq!(page.rows.len(), 6);
    let encore = svc.get_for_you().await.expect("page en cache");
    assert_eq!(encore.rows.len(), 6);
    assert_eq!(
        journal.lock().unwrap().as_slice(),
        ["/pages/for_you?deviceType=BROWSER"],
        "une seule lecture : la seconde vient du cache court"
    );
}

#[tokio::test]
async fn sans_page_pour_vous_l_accueil_sert_et_le_dit_8935() {
    let (svc, journal) = faux_tidal(Scenario {
        pour_vous: None,
        accueil: Some(page_pour_vous()),
        ..Default::default()
    })
    .await;
    let page = svc.get_for_you().await.expect("repli sur l'accueil");
    assert_eq!(page.source, "home");
    assert_eq!(page.rows.len(), 6);
    assert_eq!(
        journal.lock().unwrap().as_slice(),
        [
            "/pages/for_you?deviceType=BROWSER",
            "/pages/home?deviceType=BROWSER"
        ]
    );
}

#[tokio::test]
async fn un_module_paresseux_est_relu_a_son_chemin_8935() {
    let pour_vous = json!({"rows": [{"modules": [{
        "id": "m-lazy", "type": "ALBUM_LIST", "title": "New for you",
        "pagedList": {"items": [], "dataApiPath": "pages/data/0a1b2c?limit=20"}
    }, {
        "id": "m-ailleurs", "type": "ALBUM_LIST", "title": "Ailleurs",
        "pagedList": {"items": [], "dataApiPath": "https://ailleurs.invalid/pages/data/x"}
    }]}]});
    let (svc, journal) = faux_tidal(Scenario {
        pour_vous: Some(pour_vous),
        module: Some(
            json!({"items": [{"id": 501, "title": "Paresseux", "artist": {"name": "Z"}}]}),
        ),
        ..Default::default()
    })
    .await;
    let page = svc.get_for_you().await.expect("page");
    assert_eq!(
        page.rows
            .iter()
            .map(|r| (r.title.as_str(), r.albums.len()))
            .collect::<Vec<_>>(),
        [("New for you", 1)],
        "le module paresseux est rempli ; celui au chemin étranger écarté"
    );
    assert_eq!(
        journal.lock().unwrap().as_slice(),
        [
            "/pages/for_you?deviceType=BROWSER",
            "/pages/data/0a1b2c?limit=20&deviceType=BROWSER"
        ],
        "le chemin étranger n'est jamais demandé"
    );
}

#[tokio::test]
async fn la_deconnexion_oublie_la_page_pour_vous_8935() {
    let (mut svc, _journal) = faux_tidal(Scenario {
        pour_vous: Some(page_pour_vous()),
        ..Default::default()
    })
    .await;
    svc.get_for_you().await.expect("page");
    assert!(svc.pour_vous_depuis_le_cache().is_some());
    svc.logout().await.expect("déconnexion");
    assert!(
        svc.pour_vous_depuis_le_cache().is_none(),
        "la page d'un compte déconnecté ne se sert plus"
    );
}

#[tokio::test]
async fn les_recommandations_d_une_playlist_passent_leur_page_a_tidal_8935() {
    let (svc, journal) = faux_tidal(Scenario {
        recommandations: Some(json!({
            "limit": 5, "offset": 10, "totalNumberOfItems": 95,
            "items": [{"type": "track", "item": {"id": 31, "title": "Reco", "artist": {"name": "X"}}}]
        })),
        ..Default::default()
    })
    .await;
    let r = svc
        .get_playlist_recommendations("1f2e3d4c-0000-1111-2222-333344445555", 5, 10)
        .await
        .expect("recommandations");
    assert_eq!(r.tracks.len(), 1);
    assert_eq!((r.total, r.offset), (95, 10));
    // Une limite extravagante est ramenée à la borne de l'API.
    svc.get_playlist_recommendations("1f2e3d4c-0000-1111-2222-333344445555", 5000, 0)
        .await
        .expect("recommandations");
    assert_eq!(
        journal.lock().unwrap().as_slice(),
        [
            "/playlists/1f2e3d4c-0000-1111-2222-333344445555/recommendations/items?limit=5&offset=10",
            "/playlists/1f2e3d4c-0000-1111-2222-333344445555/recommendations/items?limit=100&offset=0",
        ]
    );
}

#[tokio::test]
async fn un_mix_ou_un_identifiant_malforme_ne_partent_pas_chez_tidal_8935() {
    let (svc, journal) = faux_tidal(Scenario::default()).await;
    let r = svc
        .get_playlist_recommendations("mix:00aa11bb", 20, 0)
        .await
        .expect("un mix n'a pas de recommandations, sans erreur");
    assert!(r.tracks.is_empty());
    let e = svc
        .get_playlist_recommendations("../users/1/favorites", 20, 0)
        .await
        .expect_err("identifiant malformé");
    assert!(matches!(e, TuneError::NotFound(_)), "{e:?}");
    assert!(journal.lock().unwrap().is_empty(), "aucune requête");
}

#[tokio::test]
async fn une_playlist_inconnue_de_tidal_rend_introuvable_8935() {
    let (svc, _journal) = faux_tidal(Scenario::default()).await;
    let e = svc
        .get_playlist_recommendations("1f2e3d4c-0000-1111-2222-333344445555", 20, 0)
        .await
        .expect_err("404 de TIDAL");
    assert!(matches!(e, TuneError::NotFound(_)), "{e:?}");
}
