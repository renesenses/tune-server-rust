//! #8676 — les deux routes de « Remplacer le chemin », sur une base de
//! fichier et de vrais fichiers : l'aperçu ne touche à rien, l'application
//! exige le nombre annoncé, réécrit pistes et `music_dirs` sans changer un
//! identifiant, et refuse quand aucun fichier n'est retrouvé.
use super::*;
use axum::body::to_bytes;
use tune_core::db::settings_repo::SettingsRepo;

async fn corps(r: Response) -> (StatusCode, serde_json::Value) {
    let statut = r.status();
    let octets = to_bytes(r.into_body(), usize::MAX).await.unwrap();
    (statut, serde_json::from_slice(&octets).unwrap())
}

fn demande(ancien: &str, nouveau: &str, confirm: Option<u64>) -> Json<CorpsDeplacement> {
    Json(CorpsDeplacement {
        old_prefix: ancien.into(),
        new_prefix: nouveau.into(),
        confirm_tracks: confirm,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::await_holding_lock)]
async fn remplacer_le_chemin_par_les_routes_8676() {
    let _seul = crate::routes::system::scan::serialiser_les_scans_de_test_sans_bloquer().await;
    crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;

    let base = tune_core::test_scratch::scratch_dir("deplacer-racine-8676");
    // L'ancienne racine n'existe pas sur cette machine ; la nouvelle porte les
    // fichiers.
    let ancienne = "/machine-d-origine-8676/Musique".to_string();
    let nouvelle = base.path().join("Musique");
    std::fs::create_dir_all(nouvelle.join("Album")).unwrap();
    for f in ["01.flac", "02.flac"] {
        std::fs::write(nouvelle.join("Album").join(f), b"x").unwrap();
    }
    let nouvelle = nouvelle.to_string_lossy().into_owned();

    let etat = AppState::new(
        &base.path().join("tune-8676.db").to_string_lossy(),
        0,
        Default::default(),
    )
    .expect("AppState sur base de fichier");
    let db = etat.backend.clone();
    db.execute("INSERT INTO artists (id, name) VALUES (1, 'A')", &[])
        .unwrap();
    db.execute(
        "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Album', 1)",
        &[],
    )
    .unwrap();
    for (id, f) in [(1, "01.flac"), (2, "02.flac"), (3, "03.flac")] {
        db.execute(
            &format!(
                "INSERT INTO tracks (id, title, artist_id, album_id, track_number, file_path) \
                 VALUES ({id}, 'T{id}', 1, 1, {id}, '{ancienne}/Album/{f}')"
            ),
            &[],
        )
        .unwrap();
    }
    SettingsRepo::with_backend(db.clone())
        .set("music_dirs", &serde_json::to_string(&[&ancienne]).unwrap())
        .unwrap();

    // L'aperçu : trois pistes, deux retrouvées, une manquante.
    let (s, a) = corps(
        apercu(
            RequireAdmin,
            State(etat.clone()),
            demande(&ancienne, &nouvelle, None),
        )
        .await,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{a}");
    assert_eq!(
        (
            a["tracks"].as_u64(),
            a["found"].as_u64(),
            a["missing"].as_u64()
        ),
        (Some(3), Some(2), Some(1)),
        "{a}"
    );
    assert_eq!(a["can_apply"], true, "{a}");
    assert!(
        a["missing_examples"][0]["new"]
            .as_str()
            .unwrap()
            .ends_with("03.flac"),
        "{a}"
    );

    // Préfixe invalide : 400.
    let (s, a) = corps(
        apercu(
            RequireAdmin,
            State(etat.clone()),
            demande("/", &nouvelle, None),
        )
        .await,
    )
    .await;
    assert_eq!(
        (s, a["refusal"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_prefix"))
    );

    // Aucun fichier retrouvé : l'application est refusée, rien ne change.
    let (s, a) = corps(
        appliquer(
            RequireAdmin,
            State(etat.clone()),
            demande(&ancienne, "/nulle-part-8676", Some(3)),
        )
        .await,
    )
    .await;
    assert_eq!(
        (s, a["refusal"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("none_found")),
        "{a}"
    );

    // Confirmation absente ou périmée : 409, rien ne change.
    let (s, a) = corps(
        appliquer(
            RequireAdmin,
            State(etat.clone()),
            demande(&ancienne, &nouvelle, Some(2)),
        )
        .await,
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{a}");
    assert_eq!(a["confirm_tracks_required"], 3);
    let chemin = |id: i64| {
        db.query_one(
            &format!("SELECT file_path FROM tracks WHERE id = {id}"),
            &[],
        )
        .unwrap()
        .unwrap()[0]
            .as_string()
            .unwrap()
    };
    assert!(chemin(1).starts_with(&ancienne));

    // Application.
    let (s, a) = corps(
        appliquer(
            RequireAdmin,
            State(etat.clone()),
            demande(&ancienne, &nouvelle, Some(3)),
        )
        .await,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{a}");
    assert_eq!(a["tracks_updated"], 3, "{a}");
    assert_eq!(a["dirs"], serde_json::json!([nouvelle]), "{a}");
    assert_eq!(a["reverse"]["new_prefix"], ancienne.as_str());
    for (id, f) in [(1, "01.flac"), (2, "02.flac"), (3, "03.flac")] {
        assert_eq!(
            chemin(id),
            format!("{nouvelle}/Album/{f}"),
            "#8676 : la piste {id} garde son identifiant"
        );
    }
    // Le droit de scanner est rendu.
    crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;
}
