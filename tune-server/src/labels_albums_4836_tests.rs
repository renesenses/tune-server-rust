//! #4836 (suite) — le label des pistes atteint l'onglet Labels SANS scan
//! manuel.
//!
//! v0.9.164 lisait le label des fichiers dans `tracks.label` et savait le
//! remonter sur `albums.label` — celui que lisent l'onglet Labels (le client
//! regroupe `GET /library/albums` par `album.label`) et `search_labels` —, mais
//! seulement en fin de scan MANUEL. Mesuré le 25/09 sur le .18 : 2 641 pistes
//! étiquetées, 0 album étiqueté. Ces témoins passent par les vraies routes
//! HTTP et par le vrai redémarrage (`AppState::new` rejoue le schéma).

use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::track_repo::TrackRepo;

type Etat = crate::state::AppState;

/// Un démarrage du serveur sur la base `chemin` : schéma, passes rejouées à
/// chaque lancement, puis le routeur complet.
fn demarrer(chemin: &str) -> (axum::Router, Etat) {
    let state = Etat::new(chemin, 0, Default::default()).expect("démarrage");
    (crate::routes::router(state.clone()), state)
}

async fn get_json(app: &axum::Router, uri: &str) -> Value {
    let reponse = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(
        reponse.status().is_success(),
        "{uri} : {}",
        reponse.status()
    );
    let corps = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&corps).unwrap()
}

/// `titre d'album → label` tel que le rend `GET /library/albums`, la route
/// que l'onglet Labels regroupe.
async fn labels_de_l_onglet(app: &axum::Router) -> std::collections::BTreeMap<String, Value> {
    let json = get_json(app, "/api/v1/library/albums?limit=100&offset=0").await;
    let items = json["items"]
        .as_array()
        .or_else(|| json.as_array())
        .unwrap_or_else(|| panic!("liste d'albums attendue : {json}"));
    items
        .iter()
        .map(|a| {
            (
                a["title"].as_str().unwrap_or_default().to_string(),
                a["label"].clone(),
            )
        })
        .collect()
}

/// Les labels que rend la recherche (`search_labels`).
async fn labels_de_la_recherche(app: &axum::Router, q: &str) -> Vec<String> {
    let json = get_json(app, &format!("/api/v1/library/search?q={q}")).await;
    json["labels"]
        .as_array()
        .unwrap_or_else(|| panic!("`labels` absent de la recherche : {json}"))
        .iter()
        .filter_map(|l| l["name"].as_str().map(String::from))
        .collect()
}

fn album(state: &Etat, titre: &str, label: Option<&str>, pistes: &[Option<&str>]) {
    state
        .backend
        .execute(
            "INSERT INTO albums (title, artist_id, source, track_count, year, label) \
             VALUES (?, 1, 'local', ?, 1970, ?)",
            &[
                &titre as &dyn ToSqlValue,
                &(pistes.len() as i64),
                &label.map(String::from),
            ],
        )
        .unwrap();
    let album_id = state.backend.last_insert_rowid();
    for (i, l) in pistes.iter().enumerate() {
        let chemin = format!("/musique/{titre}/{i}.flac");
        let titre_piste = format!("{titre} {i}");
        state
            .backend
            .execute(
                "INSERT INTO tracks (title, album_id, artist_id, track_number, file_path, \
                 duration_ms, source, label) VALUES (?, ?, 1, ?, ?, 30000, 'local', ?)",
                &[
                    &titre_piste as &dyn ToSqlValue,
                    &album_id,
                    &(i as i64 + 1),
                    &chemin,
                    &l.map(String::from),
                ],
            )
            .unwrap();
    }
}

/// Témoins 1, 3 et 4 : une base déjà scannée par la v0.9.164 (pistes
/// étiquetées, albums sans label) voit ses labels au simple REDÉMARRAGE, dans
/// l'onglet Labels comme dans la recherche ; un label posé à la main n'est
/// jamais écrasé ; un second démarrage ne change rien.
#[tokio::test(flavor = "multi_thread")]
async fn i4836_le_demarrage_remonte_le_label_des_pistes_jusqu_a_l_onglet_labels() {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("tune.db");
    let chemin = chemin.to_str().unwrap();

    // ── La base telle que la v0.9.164 l'a laissée ──
    {
        let (_, state) = demarrer(chemin);
        state
            .backend
            .execute(
                "INSERT INTO artists (id, name) VALUES (1, 'Keith Jarrett')",
                &[],
            )
            .unwrap();
        // Vote majoritaire : ECM ×2 contre Columbia ×1 ; la piste vide ne vote pas.
        album(
            &state,
            "Koln Concert",
            None,
            &[Some("ECM"), Some("ECM"), Some("Columbia"), None],
        );
        // Chaîne vide = trou, comme `genre`.
        album(&state, "Somewhere Before", Some(""), &[Some("Atlantic")]);
        // Posé à la main : intouchable.
        album(
            &state,
            "Facing You",
            Some("Label Maison"),
            &[Some("ECM"), Some("ECM")],
        );
        // Aucune piste étiquetée : reste sans label.
        album(&state, "Bremen", None, &[None, None]);
    }

    // ── Redémarrage ──
    let attendu = |onglet: &std::collections::BTreeMap<String, Value>| {
        assert_eq!(
            onglet["Koln Concert"].as_str(),
            Some("ECM"),
            "#4836 — après un démarrage, l'album porte le label le plus \
             fréquent de ses pistes ; sinon l'onglet Labels le range sous \
             « Sans label » : {onglet:?}"
        );
        assert_eq!(
            onglet["Somewhere Before"].as_str(),
            Some("Atlantic"),
            "un label d'album vide est un trou à combler : {onglet:?}"
        );
        assert_eq!(
            onglet["Facing You"].as_str(),
            Some("Label Maison"),
            "un label d'album posé à la main n'est jamais écrasé : {onglet:?}"
        );
        assert!(
            onglet["Bremen"].as_str().map_or(true, str::is_empty),
            "sans piste étiquetée, rien à inventer : {onglet:?}"
        );
    };
    let premier = {
        let (app, _state) = demarrer(chemin);
        let onglet = labels_de_l_onglet(&app).await;
        attendu(&onglet);
        let trouves = labels_de_la_recherche(&app, "ecm").await;
        assert!(
            trouves.iter().any(|l| l == "ECM"),
            "#4836 — `search_labels` lit `albums.label` : ECM doit y paraître \
             après le démarrage, obtenu {trouves:?}"
        );
        onglet
    };

    // ── Second redémarrage : idempotence ──
    let (app, state) = demarrer(chemin);
    let second = labels_de_l_onglet(&app).await;
    assert_eq!(
        premier, second,
        "deux démarrages, deux résultats différents"
    );
    assert_eq!(
        AlbumRepo::with_backend(state.backend.clone())
            .combler_les_labels_depuis_les_pistes()
            .unwrap(),
        0,
        "la passe rejouée sur une base déjà comblée ne doit toucher aucune ligne"
    );
}

/// Témoin 2 : un fichier AJOUTÉ puis MODIFIÉ, vu par le surveillant de
/// fichiers (le « scan automatique »). La remontée précédait l'insertion : le
/// premier fichier d'un album neuf n'y portait jamais son label, et un fichier
/// ré-étiqueté (supprimé puis réinséré) sortait du vote.
#[tokio::test(flavor = "multi_thread")]
async fn i4836_le_surveillant_remonte_le_label_du_fichier_qu_il_range() {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("tune.db");
    let (app, state) = demarrer(chemin.to_str().unwrap());
    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let artist_repo = ArtistRepo::with_backend(state.backend.clone());

    let fichier =
        |chemin: &str, album: &str, label: Option<&str>| tune_core::scanner::walker::ScannedFile {
            path: chemin.to_string(),
            metadata: Some(tune_core::metadata::TrackMetadata {
                title: Some(format!("{album} — piste")),
                artist: Some("Bill Evans".into()),
                album_artist: Some("Bill Evans".into()),
                album: Some(album.into()),
                label: label.map(String::from),
                ..Default::default()
            }),
            unsupported: None,
            audio_hash: None,
            file_size: 1234,
            mtime: 1,
        };
    // Le chemin du surveillant : `ChangeType::Added`, puis `Modified`
    // (suppression de l'ancienne ligne, relecture, réinsertion).
    let ranger = |sf: &tune_core::scanner::walker::ScannedFile, modifie: bool| {
        if modifie {
            track_repo.delete_by_path(&sf.path).ok();
        }
        let (track, album_id) =
            crate::auto_scan::build_track_from_metadata(sf, &artist_repo, &album_repo)
                .expect("piste construite");
        assert!(crate::auto_scan::ranger_la_piste_du_surveillant(
            &track_repo,
            &album_repo,
            &track,
            album_id,
        ));
    };

    // Ajout : le premier fichier d'un album neuf, étiqueté.
    ranger(
        &fichier(
            "/musique/sunday/1.flac",
            "Sunday at the Village Vanguard",
            Some("Riverside"),
        ),
        false,
    );
    // Modification : un fichier sans label, ré-étiqueté ensuite.
    let sans = fichier("/musique/portrait/1.flac", "Portrait in Jazz", None);
    ranger(&sans, false);
    ranger(
        &fichier(
            "/musique/portrait/1.flac",
            "Portrait in Jazz",
            Some("Riverside Records"),
        ),
        true,
    );

    let onglet = labels_de_l_onglet(&app).await;
    assert_eq!(
        onglet["Sunday at the Village Vanguard"].as_str(),
        Some("Riverside"),
        "#4836 — un fichier AJOUTÉ par le surveillant doit donner son label à \
         son album : {onglet:?}"
    );
    assert_eq!(
        onglet["Portrait in Jazz"].as_str(),
        Some("Riverside Records"),
        "#4836 — un fichier MODIFIÉ par le surveillant doit donner son label à \
         son album : {onglet:?}"
    );
}
