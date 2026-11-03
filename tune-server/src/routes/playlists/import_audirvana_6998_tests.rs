//! #6998 — schéma constaté dans la capture Audirvana du fil 2449.
//! Fichier synthétique mixte ; aucun compte ni appel Qobuz, aucun son.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::playlist_repo::{EntryContent, PlaylistRepo};
use tune_core::db::track_repo::TrackRepo;

struct Banc {
    _dossier: tempfile::TempDir,
    etat: crate::state::AppState,
    local: i64,
    chemin: String,
}
impl Banc {
    fn nouveau() -> Self {
        let dossier = tempfile::tempdir().unwrap();
        let etat = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
        let chemin = dossier
            .path()
            .join("locale.flac")
            .to_string_lossy()
            .into_owned();
        let mut piste = tune_core::db::models::Track::new("III. Allegro non troppo".into());
        piste.file_path = Some(chemin.clone());
        let local = TrackRepo::with_backend(etat.backend.clone())
            .create(&piste)
            .unwrap();
        Self {
            _dossier: dossier,
            etat,
            local,
            chemin,
        }
    }
    fn m3u(&self, chemin_local: &str) -> String {
        format!(
            "#EXTM3U\n#EXTINF:124,Cello Suite No. 1\nqobuz://track/1017009\n#EXTINF:60,III. Allegro non troppo\n{chemin_local}\n#EXTINF:357,III. Allegro non troppo\nqobuz://track/21577553\n#EXTINF:124,Cello Suite No. 1\nqobuz://track/1017009\n#EXTINF:1,III. Allegro non troppo\nqobuz://track/invalide\n"
        )
    }
    fn verifier(&self, id: i64) {
        let lignes = PlaylistRepo::with_backend(self.etat.backend.clone())
            .get_entries(id)
            .unwrap();
        assert_eq!(
            lignes.len(),
            3,
            "#6998 : les deux titres Qobuz doivent rester avec le local, dans leur ordre, sans doublon"
        );
        let contenus: Vec<_> = lignes.into_iter().map(|l| l.content).collect();
        let EntryContent::Service(a) = &contenus[0] else {
            panic!("#6998 : premier titre Qobuz perdu : {contenus:?}")
        };
        assert_eq!((&*a.source, &*a.source_id), ("qobuz", "1017009"));
        assert_eq!(a.title, "Cello Suite No. 1");
        assert_eq!(contenus[1], EntryContent::Local(self.local));
        let EntryContent::Service(b) = &contenus[2] else {
            panic!("#6998 : homonyme Qobuz remplacé par le local : {contenus:?}")
        };
        assert_eq!((&*b.source, &*b.source_id), ("qobuz", "21577553"));
        assert_eq!(b.title, "III. Allegro non troppo");
    }
}

#[tokio::test]
async fn le_scan_garde_les_liens_qobuz_audirvana_et_leur_ordre_6998() {
    let b = Banc::nouveau();
    let chemin = b._dossier.path().join("Audirvana.m3u");
    std::fs::write(&chemin, b.m3u("locale.flac")).unwrap();
    let dirs = [b._dossier.path().to_string_lossy().into_owned()];
    let bilan = tune_core::library::playlist_scan::import_local_playlists(&b.etat.backend, &dirs);
    assert_eq!(bilan.playlists_created, 1);
    assert_eq!(
        bilan.tracks_added, 3,
        "#6998 : l'import au scan ne doit plus perdre les liens Qobuz"
    );
    let repo = PlaylistRepo::with_backend(b.etat.backend.clone());
    let id = repo.list(1, 10, 0).unwrap()[0].id.unwrap();
    b.verifier(id);
    let second = tune_core::library::playlist_scan::import_local_playlists(&b.etat.backend, &dirs);
    assert_eq!(
        second.playlists_created, 0,
        "un nouveau scan ne crée pas une seconde playlist"
    );
    b.verifier(id);
}

#[tokio::test]
async fn le_multipart_m3u_garde_qobuz_et_signale_un_lien_invalide_6998() {
    let b = Banc::nouveau();
    let body = format!(
        "--audirvana6998\r\nContent-Disposition: form-data; name=\"file\"; filename=\"Audirvana.m3u\"\r\nContent-Type: audio/x-mpegurl\r\n\r\n{}\r\n--audirvana6998--\r\n",
        b.m3u(&b.chemin)
    );
    let app = crate::routes::router(b.etat.clone());
    let reponse = app
        .oneshot(
            Request::post("/api/v1/playlists/import/m3u")
                .header(
                    "Content-Type",
                    "multipart/form-data; boundary=audirvana6998",
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reponse.status(), StatusCode::CREATED);
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&octets).unwrap();
    assert_eq!(json["total_entries"], 5, "{json}");
    assert_eq!(
        json["matched"], 4,
        "#6998 : les titres Qobuz se résolvent par identité de service : {json}"
    );
    assert_eq!(json["imported"], 3, "{json}");
    assert_eq!(json["duplicates_skipped"], 1, "{json}");
    assert_eq!(
        json["not_found"], 1,
        "un lien Qobuz invalide ne doit pas devenir un titre local homonyme : {json}"
    );
    assert_eq!(json["lookup_errors"], 0);
    let id = json["id"].as_i64().unwrap();
    b.verifier(id);
    let lignes = PlaylistRepo::with_backend(b.etat.backend.clone())
        .get_entries(id)
        .unwrap();
    assert_eq!(lignes[0].service().unwrap().duration_ms, Some(124_000));
    assert_eq!(lignes[2].service().unwrap().duration_ms, Some(357_000));
}
