//! #7466 — `GET /radios/search` et `GET /radios/directory` par critères, par
//! la route HTTP. L'annuaire est déposé à la main dans `state.annuaire_radios`
//! (aucun réseau), comme dans `radios_recherche_distinction.rs`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::radio_repo::{RadioRepo, RadioStation};
use tune_server::routes::radios::{FluxAnnuaire, StationAnnuaire};

fn app_et_etat() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn get(app: &axum::Router, chemin: &str) -> (StatusCode, Value) {
    let reponse = app
        .clone()
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn semer(
    state: &tune_server::state::AppState,
    nom: &str,
    url: &str,
    pays: &str,
    genre: &str,
    codec: &str,
    debit: i32,
    favori: bool,
) {
    let repo = RadioRepo::with_backend(state.backend.clone());
    let id = repo
        .create(&RadioStation {
            id: None,
            name: nom.into(),
            url: url.into(),
            homepage: None,
            logo_url: None,
            country: Some(pays.into()),
            language: None,
            genre: Some(genre.into()),
            codec: Some(codec.into()),
            bitrate: Some(debit),
            is_favorite: false,
            last_played: None,
            play_count: 0,
        })
        .unwrap();
    if favori {
        repo.set_favorite(id, true).unwrap();
    }
}

fn fiche(
    nom: &str,
    url: &str,
    pays: &str,
    genre: &str,
    codec: &str,
    debit: i32,
    frequence: i32,
) -> StationAnnuaire {
    StationAnnuaire {
        name: nom.into(),
        stream_url: url.into(),
        country: Some(pays.into()),
        genre: Some(genre.into()),
        codec: Some(codec.into()),
        bitrate: Some(debit),
        flux: vec![FluxAnnuaire {
            url: url.into(),
            codec: Some(codec.into()),
            bitrate: Some(debit),
            sample_rate: Some(frequence),
            primary: true,
            ..Default::default()
        }],
        external_id: Some(format!("T-{nom}")),
        ..Default::default()
    }
}

/// Deux stations à moi (préfixe « Critx »), deux fiches d'annuaire absentes
/// de « Mes radios » (préfixe « Annux »).
fn monter() -> (axum::Router, tune_server::state::AppState) {
    let (app, state) = app_et_etat();
    semer(
        &state,
        "Critx Jazz",
        "https://t.invalid/critx-jazz.flac",
        "France",
        "Jazz",
        "FLAC",
        1411,
        true,
    );
    semer(
        &state,
        "Critx Rock",
        "https://t.invalid/critx-rock.mp3",
        "Royaume-Uni",
        "Rock",
        "MP3",
        128,
        false,
    );
    let mut haute = fiche(
        "Annux Hires",
        "https://t.invalid/annux-hires.flac",
        "DE",
        "classical",
        "FLAC",
        2304,
        96_000,
    );
    haute.note_centiemes = Some(480);
    haute.nombre_de_notes = 3;
    haute.radio_metadata_key = Some("annuxhires".into());
    let basse = fiche(
        "Annux News",
        "https://t.invalid/annux-news.mp3",
        "US",
        "news",
        "MP3",
        64,
        44_100,
    );
    *state.annuaire_radios.write().unwrap() = vec![haute, basse];
    (app, state)
}

fn noms<'a>(corps: &'a Value, champ: &str) -> Vec<&'a str> {
    corps[champ]
        .as_array()
        .unwrap_or_else(|| panic!("`{champ}` absent : {corps}"))
        .iter()
        .filter_map(|s| s["name"].as_str())
        .filter(|n| n.starts_with("Critx") || n.starts_with("Annux"))
        .collect()
}

#[tokio::test]
async fn sans_critere_la_recherche_est_celle_d_avant() {
    let (app, _state) = monter();
    // `q` reste exigé quand rien d'autre n'est passé.
    let (status, _) = get(&app, "/api/v1/radios/search").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, corps) = get(&app, "/api/v1/radios/search?q=Critx").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(noms(&corps, "items"), ["Critx Jazz", "Critx Rock"]);
}

#[tokio::test]
async fn chaque_critere_sur_mes_radios_avec_sa_contre_epreuve() {
    let (app, _state) = monter();
    for (criteres, attendu) in [
        ("codec=flac", vec!["Critx Jazz"]),
        ("codec=mp3", vec!["Critx Rock"]),
        ("genre=jazz", vec!["Critx Jazz"]),
        ("genre=classical,rock", vec!["Critx Rock"]),
        ("country=FR", vec!["Critx Jazz"]),
        ("country=GB", vec!["Critx Rock"]),
        ("bitrate_min=320", vec!["Critx Jazz"]),
        ("lossless=true", vec!["Critx Jazz"]),
        ("favorites_only=true", vec!["Critx Jazz"]),
        ("country=FR&codec=mp3", vec![]),
    ] {
        let (status, corps) = get(&app, &format!("/api/v1/radios/search?q=Critx&{criteres}")).await;
        assert_eq!(
            noms(&corps, "items"),
            attendu,
            "critères {criteres:?} : {corps}"
        );
        if attendu.is_empty() {
            assert_eq!(corps["statut"], "aucun_resultat", "{criteres:?} : {corps}");
            assert_eq!(status, StatusCode::OK);
        }
    }
}

#[tokio::test]
async fn criteres_seuls_sans_texte_parcourent_tout_et_l_annuaire() {
    let (app, _state) = monter();
    let (status, corps) = get(&app, "/api/v1/radios/search?lossless=true").await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    let items = noms(&corps, "items");
    assert!(
        items.contains(&"Critx Jazz") && !items.contains(&"Critx Rock"),
        "{corps}"
    );
    assert_eq!(noms(&corps, "annuaire"), ["Annux Hires"]);

    for (critere, attendu) in [
        ("hires=true", vec!["Annux Hires"]),
        ("rated_min=4.5", vec!["Annux Hires"]),
        ("rated_min=4.9", vec![]),
        ("metadata_plus=true", vec!["Annux Hires"]),
        ("country=US", vec!["Annux News"]),
        ("genre=news", vec!["Annux News"]),
        ("codec=mp3&bitrate_min=128", vec![]),
    ] {
        let (_, corps) = get(&app, &format!("/api/v1/radios/search?{critere}")).await;
        assert_eq!(
            noms(&corps, "annuaire"),
            attendu,
            "critère {critere:?} : {corps}"
        );
    }
}

#[tokio::test]
async fn l_annuaire_se_filtre_par_criteres() {
    // Une ouverture de l'annuaire peut lancer un relevé en tâche de fond, qui
    // remplacerait les fiches déposées ici : chaque requête a donc son propre
    // serveur, et ne lit que le relevé d'avant ce lancement.
    let (app, _state) = monter();
    let (_, tout) = get(&app, "/api/v1/radios/directory").await;
    assert_eq!(tout["total"], 2, "{tout}");
    for (critere, attendu) in [
        ("hires=true", vec!["Annux Hires"]),
        ("codec=MP3", vec!["Annux News"]),
        ("lossless=true&country=US", vec![]),
        ("metadata_plus=true", vec!["Annux Hires"]),
        ("rated_min=4", vec!["Annux Hires"]),
        ("genre=news,classical", vec!["Annux Hires", "Annux News"]),
    ] {
        let (app, _state) = monter();
        let (status, corps) = get(&app, &format!("/api/v1/radios/directory?{critere}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            noms(&corps, "items"),
            attendu,
            "critère {critere:?} : {corps}"
        );
        assert_eq!(corps["total"], attendu.len(), "{critere:?}");
    }
}
