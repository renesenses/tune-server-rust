//! #7822 — les mixes personnels du compte TIDAL (My Daily Discovery, My Mix
//! 1 à 8, My New Arrivals…), lus dans des réponses TIDAL enregistrées.
//!
//! Les réponses sont dans `tests/fixtures/tidal_mixes_7822/` (voir son
//! `LISEZMOI.md` : forme de l'API v1, contenu fictif). Les témoins de bout en
//! bout passent par un faux TIDAL sur la boucle locale, armé d'un jeton
//! d'essai : aucun appel à TIDAL.

use super::*;

const PAGE_MES_MIXES: &str =
    include_str!("../../tests/fixtures/tidal_mixes_7822/page_mes_mixes.json");
const PAGE_MES_MIXES_VIDE: &str =
    include_str!("../../tests/fixtures/tidal_mixes_7822/page_mes_mixes_vide.json");
const PAGE_ACCUEIL_AVEC_MIXES: &str =
    include_str!("../../tests/fixtures/tidal_mixes_7822/page_accueil_avec_mixes.json");
const PAGE_MIX: &str = include_str!("../../tests/fixtures/tidal_mixes_7822/page_mix.json");
const MIX_ITEMS: &str = include_str!("../../tests/fixtures/tidal_mixes_7822/mix_items.json");

const DAILY_DISCOVERY: &str = "mix:0011aa22bb33cc44dd55ee66ff7788";

fn json_de(texte: &str) -> serde_json::Value {
    serde_json::from_str(texte).expect("réponse enregistrée lisible")
}

#[test]
fn la_page_des_mixes_rend_chaque_mix_comme_une_playlist_7822() {
    let mixes = TidalService::mixes_de_la_page(&json_de(PAGE_MES_MIXES));
    let noms: Vec<&str> = mixes.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(
        noms,
        ["My Daily Discovery", "My Mix 1", "My New Arrivals"],
        "les mixes du compte doivent tous ressortir, dans l'ordre de TIDAL"
    );
    let discovery = &mixes[0];
    assert_eq!(discovery.id, DAILY_DISCOVERY);
    assert_eq!(
        discovery.description.as_deref(),
        Some("GoGo Penguin, Nils Frahm, Hania Rani")
    );
    assert_eq!(
        discovery.cover_path.as_deref(),
        Some("https://images.tidal.invalid/discovery/1500.jpg"),
        "la plus grande image est préférée"
    );
    assert_eq!(discovery.editable, Some(false), "un mix ne se modifie pas");
    assert_eq!(
        mixes[1].cover_path.as_deref(),
        Some("https://images.tidal.invalid/mix1/640.jpg"),
        "à défaut de LARGE, MEDIUM"
    );
    assert_eq!(
        mixes[2].cover_path, None,
        "sans image, pas d'adresse fabriquée"
    );
    assert_eq!(
        mixes[2].description, None,
        "un sous-titre vide n'en est pas un"
    );
}

#[test]
fn un_compte_sans_mix_rend_une_liste_vide_7822() {
    assert!(TidalService::mixes_de_la_page(&json_de(PAGE_MES_MIXES_VIDE)).is_empty());
    assert!(TidalService::mixes_de_la_page(&serde_json::json!({})).is_empty());
    assert!(
        TidalService::mixes_de_la_page(&serde_json::json!({"status": 404, "subStatus": 2001}))
            .is_empty(),
        "un refus de TIDAL en JSON ne panique pas"
    );
}

#[test]
fn la_page_d_accueil_rend_ses_mixes_sans_les_albums_ni_doublon_7822() {
    let mixes = TidalService::mixes_de_la_page(&json_de(PAGE_ACCUEIL_AVEC_MIXES));
    assert_eq!(
        mixes.len(),
        1,
        "un mix, pas l'album voisin ni le doublon : {mixes:?}"
    );
    assert_eq!(mixes[0].name, "My Mix 2");
    assert_eq!(mixes[0].id, "mix:0044dd55ee66ff778899aabbcc2233");
}

#[test]
fn les_pistes_d_un_mix_sont_rendues_7822() {
    let page = json_de(PAGE_MIX);
    let pistes = TidalService::pistes_du_mix(&page);
    let titres: Vec<(&str, &str)> = pistes
        .iter()
        .map(|p| (p.title.as_str(), p.artist.as_str()))
        .collect();
    assert_eq!(
        titres,
        [("Hopopono", "GoGo Penguin"), ("Says", "Nils Frahm")],
        "les pistes du module TRACK_LIST doivent être rendues"
    );
    assert_eq!(pistes[0].id, "900000001");
    assert_eq!(pistes[0].duration_ms, 280_000);
    assert_eq!(pistes[1].quality.as_ref().map(|q| q.bit_depth), Some(24));

    let entete = TidalService::entete_du_mix(&page).expect("en-tête du mix");
    assert_eq!(entete.id, DAILY_DISCOVERY);
    assert_eq!(entete.name, "My Daily Discovery");
}

#[test]
fn le_repli_des_items_ecarte_les_videos_7822() {
    let pistes = TidalService::pistes_du_mix(&json_de(MIX_ITEMS));
    assert_eq!(pistes.len(), 1, "la vidéo ne se lit pas comme une piste");
    assert_eq!(pistes[0].title, "Eden");
}

#[test]
fn la_liste_des_mixes_ne_se_sert_qu_au_meme_compte_et_peu_de_temps_7822() {
    let fraiche = MixesCache {
        compte: Some(42),
        mixes: Vec::new(),
        fetched_at: Instant::now(),
    };
    assert!(fraiche.frais_pour(Some(42)));
    assert!(
        !fraiche.frais_pour(Some(43)),
        "un autre compte relit ses mixes"
    );
    let Some(il_y_a_longtemps) = Instant::now().checked_sub(TTL_MIXES + Duration::from_secs(1))
    else {
        return; // horloge trop jeune pour reculer : rien à prouver ici
    };
    let perimee = MixesCache {
        compte: Some(42),
        mixes: Vec::new(),
        fetched_at: il_y_a_longtemps,
    };
    assert!(!perimee.frais_pour(Some(42)), "un mix est éphémère");
    assert!(
        TTL_MIXES <= Duration::from_secs(4 * 3600),
        "cache court : quelques heures au plus"
    );
}

/// Ce que le faux TIDAL sert sur chaque page, et le journal des chemins
/// demandés (avec leur `mixId`).
#[derive(Clone, Default)]
struct Scenario {
    mes_mixes: Option<&'static str>,
    accueil: Option<&'static str>,
    mix: Option<&'static str>,
    items: Option<&'static str>,
}

async fn faux_tidal(scenario: Scenario) -> (TidalService, Arc<std::sync::Mutex<Vec<String>>>) {
    use axum::extract::{Path as Chemin, Query, State};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;

    type Etat = (Arc<Scenario>, Arc<std::sync::Mutex<Vec<String>>>);

    fn servir(corps: Option<&'static str>) -> axum::response::Response {
        match corps {
            Some(c) => {
                ([(axum::http::header::CONTENT_TYPE, "application/json")], c).into_response()
            }
            None => (
                StatusCode::NOT_FOUND,
                axum::Json(serde_json::json!({"status": 404, "subStatus": 2001})),
            )
                .into_response(),
        }
    }

    async fn page(
        State((scenario, journal)): State<Etat>,
        Chemin(nom): Chemin<String>,
        Query(q): Query<HashMap<String, String>>,
    ) -> axum::response::Response {
        let mix_id = q.get("mixId").cloned().unwrap_or_default();
        journal
            .lock()
            .unwrap()
            .push(format!("/pages/{nom}?{mix_id}"));
        servir(match nom.as_str() {
            "my_collection_my_mixes" => scenario.mes_mixes,
            "home" => scenario.accueil,
            "mix" => scenario.mix,
            _ => None,
        })
    }

    async fn items(
        State((scenario, journal)): State<Etat>,
        Chemin(id): Chemin<String>,
    ) -> axum::response::Response {
        journal.lock().unwrap().push(format!("/mixes/{id}/items"));
        servir(scenario.items)
    }

    let journal: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let app = axum::Router::new()
        .route("/pages/{nom}", get(page))
        .route("/mixes/{id}/items", get(items))
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
async fn la_route_des_mixes_lit_la_page_du_compte_puis_sert_le_cache_7822() {
    let (svc, journal) = faux_tidal(Scenario {
        mes_mixes: Some(PAGE_MES_MIXES),
        ..Default::default()
    })
    .await;
    let mixes = svc.get_user_mixes().await.expect("mixes");
    assert_eq!(mixes.len(), 3);
    let encore = svc.get_user_mixes().await.expect("mixes en cache");
    assert_eq!(encore.len(), 3);
    assert_eq!(
        journal.lock().unwrap().as_slice(),
        ["/pages/my_collection_my_mixes?"],
        "la seconde lecture vient du cache court"
    );
}

#[tokio::test]
async fn un_compte_sans_mix_rend_une_liste_vide_sans_erreur_7822() {
    let (svc, _journal) = faux_tidal(Scenario {
        mes_mixes: Some(PAGE_MES_MIXES_VIDE),
        accueil: Some(r#"{"rows": []}"#),
        ..Default::default()
    })
    .await;
    let mixes = svc
        .get_user_mixes()
        .await
        .expect("un compte sans mix n'est pas une erreur");
    assert!(mixes.is_empty());
}

#[tokio::test]
async fn sans_page_des_mixes_les_rangees_de_l_accueil_servent_7822() {
    let (svc, journal) = faux_tidal(Scenario {
        mes_mixes: None,
        accueil: Some(PAGE_ACCUEIL_AVEC_MIXES),
        ..Default::default()
    })
    .await;
    let mixes = svc.get_user_mixes().await.expect("repli sur l'accueil");
    assert_eq!(mixes.len(), 1);
    assert_eq!(mixes[0].name, "My Mix 2");
    assert_eq!(journal.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn un_mix_se_lit_comme_une_playlist_7822() {
    let (svc, journal) = faux_tidal(Scenario {
        mes_mixes: Some(PAGE_MES_MIXES),
        mix: Some(PAGE_MIX),
        ..Default::default()
    })
    .await;
    // Le chemin des routes de lecture et d'ajout à la file
    // (`streaming_playlist_id`) : `get_playlist_tracks`.
    let pistes = svc
        .get_playlist_tracks(DAILY_DISCOVERY)
        .await
        .expect("pistes du mix");
    assert_eq!(
        pistes.len(),
        2,
        "le mix doit rendre ses pistes : {pistes:?}"
    );
    assert_eq!(pistes[0].title, "Hopopono");
    assert_eq!(
        journal.lock().unwrap().as_slice(),
        ["/pages/mix?0011aa22bb33cc44dd55ee66ff7788"],
        "un mix ne passe pas par /playlists/{{uuid}}/tracks"
    );

    let fiche = svc
        .get_playlist(DAILY_DISCOVERY)
        .await
        .expect("fiche du mix");
    assert_eq!(fiche.name, "My Daily Discovery");
}

#[tokio::test]
async fn sans_page_de_mix_les_items_servent_7822() {
    let (svc, journal) = faux_tidal(Scenario {
        mix: Some(r#"{"rows": []}"#),
        items: Some(MIX_ITEMS),
        ..Default::default()
    })
    .await;
    let pistes = svc
        .get_playlist_tracks(DAILY_DISCOVERY)
        .await
        .expect("pistes du mix");
    assert_eq!(pistes.len(), 1);
    assert_eq!(journal.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn la_deconnexion_oublie_les_mixes_du_compte_7822() {
    let (mut svc, journal) = faux_tidal(Scenario {
        mes_mixes: Some(PAGE_MES_MIXES),
        ..Default::default()
    })
    .await;
    svc.get_user_mixes().await.expect("mixes");
    assert!(svc.mixes_depuis_le_cache().is_some());
    svc.logout().await.expect("déconnexion");
    assert!(
        svc.mixes_depuis_le_cache().is_none(),
        "les mixes d'un compte déconnecté ne se servent plus"
    );
    assert_eq!(journal.lock().unwrap().len(), 1);
}
