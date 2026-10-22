//! #6516 — l'annuaire radio-metadata.fr complet dans Tune.
//!
//! Le serveur relève l'annuaire v2, le garde en cache local (rafraîchi avec
//! `If-None-Match`, servi hors ligne), l'expose à l'interface et rapproche
//! « Mes radios » de leurs fiches sans doublon. Aucun essai ne touche au
//! réseau : l'annuaire est un serveur monté ici, qui sert un extrait réel du
//! 19/10/2026 (FIP, FIP: Jazz, FIP: Nouveautés, Radio Paradise: Main Mix) plus
//! deux fiches non publiées (FIP Latino refusée, FIP Tout nouveau retirée).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::radio_repo::{RadioRepo, RadioStation};
use tune_server::routes::radios_annuaire::{self, Releve};

const EXTRAIT: &str = include_str!("fixtures/annuaire_v2_extrait_20261019.json");
const ETAG: &str = "W/\"annuaire-6516\"";

fn etat() -> tune_server::state::AppState {
    tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap()
}

/// Un faux annuaire : 200 + `ETag`, ou 304 quand on lui rend son `ETag`.
/// Compte les requêtes et celles qui portaient `If-None-Match`.
async fn faux_annuaire() -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let total = Arc::new(AtomicUsize::new(0));
    let conditionnelles = Arc::new(AtomicUsize::new(0));
    let (t, c) = (total.clone(), conditionnelles.clone());
    let app = axum::Router::new().route(
        "/api/v2/radios",
        axum::routing::get(move |headers: HeaderMap| {
            let (t, c) = (t.clone(), c.clone());
            async move {
                t.fetch_add(1, Ordering::SeqCst);
                if headers.get("if-none-match").and_then(|v| v.to_str().ok()) == Some(ETAG) {
                    c.fetch_add(1, Ordering::SeqCst);
                    return axum::response::Response::builder()
                        .status(304)
                        .body(Body::empty())
                        .unwrap();
                }
                axum::response::Response::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .header("etag", ETAG)
                    .body(Body::from(EXTRAIT))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (
        format!("http://127.0.0.1:{port}/api/v2/radios"),
        total,
        conditionnelles,
    )
}

/// Une adresse où rien n'écoute : le réseau est « coupé ».
async fn adresse_morte() -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    format!("http://127.0.0.1:{port}/api/v2/radios")
}

async fn requete(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let mut r = Request::builder().method(methode).uri(chemin);
    let body = match corps {
        Some(c) => {
            r = r.header("content-type", "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let rep = app.clone().oneshot(r.body(body).unwrap()).await.unwrap();
    let status = rep.status();
    let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

async fn etat_avec_annuaire() -> (
    tune_server::state::AppState,
    axum::Router,
    tempfile::TempDir,
) {
    let dossier = tempfile::tempdir().unwrap();
    let (url, _, _) = faux_annuaire().await;
    let state = etat();
    let r = radios_annuaire::rafraichir_depuis(&state, &url, &dossier.path().join("annuaire.json"))
        .await;
    assert_eq!(r, Releve::Nouveau(4));
    let app = tune_server::routes::router(state.clone());
    (state, app, dossier)
}

#[tokio::test]
async fn l_annuaire_se_releve_puis_se_revalide_par_etag_puis_sert_le_cache_hors_ligne() {
    let dossier = tempfile::tempdir().unwrap();
    let cache = dossier.path().join("radios").join("annuaire-v2.json");
    let (url, total, conditionnelles) = faux_annuaire().await;

    let state = etat();
    assert_eq!(
        radios_annuaire::rafraichir_depuis(&state, &url, &cache).await,
        Releve::Nouveau(4),
        "4 fiches publiées sur 6 : la refusée et la retirée sont écartées"
    );
    assert!(
        cache.exists(),
        "le relevé doit être écrit dans le cache local"
    );
    assert_eq!(state.annuaire_radios.read().unwrap().len(), 4);

    // Second relevé : `If-None-Match`, 304, rien n'est retéléchargé.
    assert_eq!(
        radios_annuaire::rafraichir_depuis(&state, &url, &cache).await,
        Releve::NonModifie
    );
    assert_eq!(total.load(Ordering::SeqCst), 2);
    assert_eq!(conditionnelles.load(Ordering::SeqCst), 1);

    // Un serveur qui redémarre HORS LIGNE sert l'annuaire du cache.
    let hors_ligne = etat();
    assert_eq!(
        radios_annuaire::rafraichir_depuis(&hors_ligne, &adresse_morte().await, &cache).await,
        Releve::Injoignable
    );
    assert_eq!(hors_ligne.annuaire_radios.read().unwrap().len(), 4);
    assert_eq!(hors_ligne.annuaire_etat.read().unwrap().origine, "cache");

    // Et le cache garde l'`ETag` : le premier relevé en ligne est conditionnel.
    assert_eq!(
        radios_annuaire::rafraichir_depuis(&hors_ligne, &url, &cache).await,
        Releve::NonModifie
    );
}

#[tokio::test]
async fn l_interface_recoit_l_annuaire_filtre_avec_la_mention_et_mes_radios() {
    let (_state, app, _d) = etat_avec_annuaire().await;
    let (s, corps) = requete(&app, "GET", "/api/v1/radios/directory?country=FR", None).await;
    assert_eq!(s, StatusCode::OK, "{corps}");
    assert_eq!(corps["total"], 3, "{corps}");
    assert_eq!(corps["origine"], "reseau");
    assert_eq!(corps["attribution"]["name"], "radio-metadata.fr");
    assert_eq!(
        corps["attribution"]["support_url"],
        "https://ko-fi.com/aleceiffel"
    );
    let items = corps["items"].as_array().unwrap();
    let fip = items
        .iter()
        .find(|i| i["external_id"] == "S000973")
        .unwrap();
    assert!(
        fip["logo_url"]
            .as_str()
            .unwrap()
            .starts_with("https://mozaiklabs.fr/storage/"),
        "logo relatif résolu : {fip}"
    );
    // FIP est dans le catalogue livré : la fiche le dit.
    assert!(fip["my_radio_id"].is_i64(), "{fip}");
    // FIP: Nouveautés est semée SANS le `?id=radiofrance` de l'annuaire : le
    // nom la rapproche quand même.
    let nouv = items
        .iter()
        .find(|i| i["external_id"] == "S000981")
        .unwrap();
    assert!(nouv["my_radio_id"].is_i64(), "{nouv}");
}

#[tokio::test]
async fn mes_radios_portent_leur_fiche_et_le_relais_connait_leur_cle() {
    let (_state, app, _d) = etat_avec_annuaire().await;
    let (_, radios) = requete(&app, "GET", "/api/v1/radios", None).await;
    let fip = radios
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["stream_url"] == "https://icecast.radiofrance.fr/fip-hifi.aac")
        .expect("FIP est semée");
    assert_eq!(fip["annuaire"]["radio_metadata_key"], "fip", "{fip}");
    assert_eq!(fip["annuaire"]["external_id"], "S000973");
    // Le registre du relais de titres est rempli au relevé.
    assert_eq!(
        tune_core::radio_metadata_relais::cle_pour(
            "FIP",
            "https://icecast.radiofrance.fr/fip-hifi.aac"
        )
        .as_deref(),
        Some("fip")
    );
}

#[tokio::test]
async fn ajouter_depuis_l_annuaire_ne_cree_jamais_de_doublon() {
    let (state, app, _d) = etat_avec_annuaire().await;
    let repo = RadioRepo::with_backend(state.backend.clone());
    let avant = repo.list().unwrap().len();

    // FIP est déjà dans « Mes radios » : rendue telle quelle, rien n'est créé.
    let (s, fip) = requete(&app, "POST", "/api/v1/radios/directory/S000973/add", None).await;
    assert_eq!(s, StatusCode::OK, "{fip}");
    assert_eq!(repo.list().unwrap().len(), avant);

    // On retire FIP: Jazz de « Mes radios », puis on l'ajoute depuis l'annuaire.
    let jazz = repo
        .list()
        .unwrap()
        .into_iter()
        .find(|r| r.url == "https://icecast.radiofrance.fr/fipjazz-hifi.aac")
        .expect("FIP Jazz est semée");
    repo.delete(jazz.id.unwrap()).unwrap();
    let (s, cree) = requete(
        &app,
        "POST",
        "/api/v1/radios/directory/S000978/add",
        Some(json!({"favorite": true})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{cree}");
    assert_eq!(cree["favorite"], true);
    assert!(
        cree["logo_url"]
            .as_str()
            .unwrap()
            .starts_with("https://mozaiklabs.fr/")
    );
    assert_eq!(cree["annuaire"]["radio_metadata_key"], "fipjazz");
    assert_eq!(repo.list().unwrap().len(), avant);

    // Un second clic ne crée rien.
    let (s, encore) = requete(&app, "POST", "/api/v1/radios/directory/S000978/add", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(encore["id"], cree["id"]);
    assert_eq!(repo.list().unwrap().len(), avant);

    let (s, _) = requete(&app, "POST", "/api/v1/radios/directory/S999001/add", None).await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "une fiche refusée ne s'ajoute pas"
    );
}

#[tokio::test]
async fn une_station_refusee_quitte_l_annuaire_mais_reste_dans_mes_radios() {
    let (state, app, _d) = etat_avec_annuaire().await;
    let repo = RadioRepo::with_backend(state.backend.clone());
    repo.create(&RadioStation {
        id: None,
        name: "FIP Latino".into(),
        url: "https://icecast.radiofrance.fr/fiplatino-hifi.aac".into(),
        homepage: None,
        logo_url: None,
        country: Some("France".into()),
        language: None,
        genre: None,
        codec: None,
        bitrate: None,
        is_favorite: true,
        last_played: None,
        play_count: 0,
    })
    .unwrap();

    let (_, annuaire) = requete(&app, "GET", "/api/v1/radios/directory?q=latino", None).await;
    assert_eq!(annuaire["total"], 0, "{annuaire}");

    let (_, radios) = requete(&app, "GET", "/api/v1/radios", None).await;
    let latino = radios
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "FIP Latino")
        .expect("la station de l'utilisateur doit rester dans « Mes radios »");
    assert!(latino.get("annuaire").is_none(), "{latino}");
    assert_eq!(latino["favorite"], true);
}

// ---------------------------------------------------------------------------
// Le lien `external_id` (SQLite 126 / PG 090, décision de Bertrand du 19/10)
// ---------------------------------------------------------------------------

use tune_server::routes::radios::StationAnnuaire;

fn fiches_publiees(state: &tune_server::state::AppState) -> Vec<StationAnnuaire> {
    state.annuaire_radios.read().unwrap().clone()
}

fn id_de(repo: &RadioRepo, url: &str) -> i64 {
    repo.list()
        .unwrap()
        .into_iter()
        .find(|r| r.url == url)
        .and_then(|r| r.id)
        .expect("station semée")
}

const FIP: &str = "https://icecast.radiofrance.fr/fip-hifi.aac";

#[tokio::test]
async fn ajouter_depuis_l_annuaire_enregistre_le_lien_des_la_creation() {
    let (state, app, _d) = etat_avec_annuaire().await;
    let repo = RadioRepo::with_backend(state.backend.clone());
    let jazz = id_de(&repo, "https://icecast.radiofrance.fr/fipjazz-hifi.aac");
    repo.delete(jazz).unwrap();
    let (s, cree) = requete(&app, "POST", "/api/v1/radios/directory/S000978/add", None).await;
    assert_eq!(s, StatusCode::CREATED);
    let id = cree["id"].as_i64().unwrap();
    assert_eq!(
        repo.liens_annuaire().unwrap().get(&id).map(String::as_str),
        Some("S000978")
    );
}

#[tokio::test]
async fn le_lien_survit_au_changement_d_adresse_et_propose_la_mise_a_jour_sans_rien_changer() {
    let (state, app, _d) = etat_avec_annuaire().await;
    let repo = RadioRepo::with_backend(state.backend.clone());
    // Rapprochement de départ : FIP (semée) est liée par son adresse.
    radios_annuaire::lier_les_stations(&repo, &fiches_publiees(&state));
    let fip = id_de(&repo, FIP);
    assert_eq!(
        repo.liens_annuaire().unwrap().get(&fip).map(String::as_str),
        Some("S000973")
    );

    // L'annuaire change TOUTES les adresses de FIP.
    let neuve = "https://nouveau-flux.example/fip.aac";
    let mut fiches = fiches_publiees(&state);
    let f = fiches
        .iter_mut()
        .find(|f| f.external_id.as_deref() == Some("S000973"))
        .unwrap();
    f.stream_url = neuve.into();
    f.stream_urls = vec![neuve.into()];
    radios_annuaire::publier(&state, fiches);

    let (_, radios) = requete(&app, "GET", "/api/v1/radios", None).await;
    let st = radios
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == fip)
        .unwrap();
    assert_eq!(st["annuaire"]["external_id"], "S000973", "{st}");
    assert_eq!(st["annuaire"]["rapproche_par"], "id");
    assert_eq!(st["annuaire"]["flux_change"], true);
    assert_eq!(st["annuaire"]["stream_url_annuaire"], neuve);
    // Rien n'a changé tout seul.
    assert_eq!(st["stream_url"], FIP);
    assert_eq!(repo.get(fip).unwrap().unwrap().url, FIP);

    // L'annuaire continue de la reconnaître : pas de doublon proposé.
    let (_, ann) = requete(&app, "GET", "/api/v1/radios/directory?q=fip", None).await;
    let item = ann["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["external_id"] == "S000973")
        .unwrap();
    assert_eq!(item["my_radio_id"], fip);

    // Le geste de l'utilisateur : « Mettre à jour l'adresse du flux ».
    let (s, maj) = requete(
        &app,
        "POST",
        &format!("/api/v1/radios/directory/update-stream/{fip}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{maj}");
    assert_eq!(maj["stream_url"], neuve);
    assert_eq!(maj["annuaire"]["flux_change"], false);
    assert_eq!(repo.get(fip).unwrap().unwrap().url, neuve);
}

#[tokio::test]
async fn un_nom_ambigu_ne_lie_rien_et_le_rapprochement_est_idempotent() {
    let (state, _app, _d) = etat_avec_annuaire().await;
    let repo = RadioRepo::with_backend(state.backend.clone());
    let mut fiches = fiches_publiees(&state);
    for (ext, url) in [
        ("S990001", "https://a.example/jazz"),
        ("S990002", "https://b.example/jazz"),
    ] {
        fiches.push(StationAnnuaire {
            name: "Jazz Radio".into(),
            stream_url: url.into(),
            stream_urls: vec![url.into()],
            external_id: Some(ext.into()),
            ..Default::default()
        });
    }
    let locale = repo
        .create(&RadioStation {
            id: None,
            name: "Jazz Radio".into(),
            url: "https://c.example/jazz".into(),
            homepage: None,
            logo_url: None,
            country: None,
            language: None,
            genre: None,
            codec: None,
            bitrate: None,
            is_favorite: false,
            last_played: None,
            play_count: 0,
        })
        .unwrap();
    let premiers = radios_annuaire::lier_les_stations(&repo, &fiches);
    assert!(premiers > 0, "les stations semées se lient");
    assert!(
        !repo.liens_annuaire().unwrap().contains_key(&locale),
        "un nom porté par deux fiches ne lie rien"
    );
    // Rejouer ne pose rien de plus, et ne réécrit pas un lien existant.
    let fip = id_de(&repo, FIP);
    repo.lier_a_l_annuaire(fip, Some("S999999")).unwrap();
    assert_eq!(radios_annuaire::lier_les_stations(&repo, &fiches), 0);
    assert_eq!(
        repo.liens_annuaire().unwrap().get(&fip).map(String::as_str),
        Some("S999999")
    );
}
