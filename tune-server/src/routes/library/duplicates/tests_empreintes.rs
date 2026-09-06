//! BIB-B2 (phase D) : couverture de l'empreinte et rattrapage à la demande.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::track_metadata_repo::TrackMetadataRepo;

use super::tests_contenu::{empreinte_texte, signal};

type Etat = crate::state::AppState;

fn serveur() -> (axum::Router, Etat) {
    let state = Etat::new(":memory:", 0, Default::default()).unwrap();
    let routeur = crate::routes::router(state.clone());
    (routeur, state)
}

fn piste(state: &Etat, id: i64, titre: &str, chemin: &str, format: &str, empreinte: Option<&str>) {
    state
        .backend
        .execute(
            "INSERT INTO tracks (id, title, artist_id, file_path, duration_ms, format, sample_rate, bit_depth) \
             VALUES (?, ?, 1, ?, 30000, ?, 44100, 16)",
            &[&id as &dyn ToSqlValue, &titre, &chemin, &format],
        )
        .unwrap();
    if let Some(e) = empreinte {
        state
            .backend
            .execute(
                "UPDATE tracks SET audio_fingerprint = ? WHERE id = ?",
                &[&e as &dyn ToSqlValue, &id],
            )
            .unwrap();
    }
}

async fn appel(app: axum::Router, methode: Method, chemin: &str) -> (StatusCode, Value) {
    let reponse = app
        .oneshot(
            Request::builder()
                .method(methode)
                .uri(chemin)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

/// Quatre pistes : une empreinte valide, une marque « silence ou indécodable »,
/// une candidate (analysée par ReplayGain, fichier réel du dépôt) et un DSD
/// hors champ. La couverture les compte chacune à sa place ; le rattrapage
/// forcé traite la candidate et la couverture le reflète.
#[tokio::test]
async fn la_couverture_compte_juste_et_le_rattrapage_force_traite_les_candidates() {
    let (app, state) = serveur();
    state
        .backend
        .execute("INSERT INTO artists (id, name) VALUES (1, 'Artiste')", &[])
        .unwrap();
    let son = signal(&[220.0, 330.0, 440.0], 30.0, 0.8);
    piste(
        &state,
        1,
        "Avec",
        "/m/avec.flac",
        "flac",
        Some(&empreinte_texte(&son)),
    );
    piste(
        &state,
        2,
        "Muette",
        "/m/muette.flac",
        "flac",
        Some("env100ms-v1:-"),
    );
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tune-core/tests/fixtures/test.flac"
    );
    piste(&state, 3, "Candidate", fixture, "flac", None);
    TrackMetadataRepo::with_backend(state.backend.clone())
        .set(3, "rg_analyzed", "1")
        .unwrap();
    piste(&state, 4, "Un DSD", "/m/dsd.dsf", "dsf", None);

    let (statut, avant) = appel(
        app.clone(),
        Method::GET,
        "/api/v1/library/duplicates/empreintes",
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{avant}");
    assert_eq!(avant["disponible"], true);
    assert_eq!(avant["pistes"], 4, "{avant}");
    assert_eq!(avant["avec_empreinte"], 1, "{avant}");
    assert_eq!(avant["marquees_silence_ou_indecodable"], 1, "{avant}");
    assert_eq!(avant["dsd_exclues"], 1, "{avant}");
    assert_eq!(avant["candidates"], 1, "{avant}");
    assert_eq!(avant["groupes_par_contenu"], 0, "{avant}");

    // `max=0` : rien n'est traité, rien ne bouge.
    let (statut, rien) = appel(
        app.clone(),
        Method::POST,
        "/api/v1/library/duplicates/empreintes?max=0",
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{rien}");
    assert_eq!(rien["lots"], 0);
    assert_eq!(rien["traitees"], 0);
    assert_eq!(rien["restantes"], 1);

    // Sans mode ReplayGain actif, le rattrapage forcé respecte la même garde
    // que le fond : rien n'est traité, et la réponse le dit.
    let (_, inactif) = appel(
        app.clone(),
        Method::POST,
        "/api/v1/library/duplicates/empreintes?max=2",
    )
    .await;
    assert_eq!(inactif["traitees"], 0, "{inactif}");
    assert_eq!(inactif["analyse_active"], false, "{inactif}");
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set(tune_core::audio::replaygain::MODE_KEY, "track")
        .unwrap();

    // Deux lots au plus : le premier traite la candidate, le second rend 0 et arrête.
    let (statut, fait) = appel(
        app.clone(),
        Method::POST,
        "/api/v1/library/duplicates/empreintes?max=2",
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{fait}");
    assert_eq!(fait["traitees"], 1, "{fait}");
    assert_eq!(fait["restantes"], 0, "{fait}");
    assert_eq!(fait["lots"], 2, "{fait}");

    let (_, apres) = appel(app, Method::GET, "/api/v1/library/duplicates/empreintes").await;
    assert_eq!(apres["avec_empreinte"], 2, "{apres}");
    assert_eq!(apres["candidates"], 0, "{apres}");
}
