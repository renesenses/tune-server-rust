//! Le contrat recuperé par l'extracteur doit porter sur la réponse réelle.
use super::{CARTE_WEB, CarteContrats, get_json, respecte_tous_les_contrats};
use tune_core::db::{
    album_repo::AlbumRepo,
    models::{Album, Track},
    track_repo::TrackRepo,
};

#[tokio::test]
async fn i1897_pistes_album_extraites_du_client_passent_par_le_routeur() {
    let etat = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let albums = AlbumRepo::with_backend(etat.backend.clone());
    let pistes = TrackRepo::with_backend(etat.backend.clone());
    let autre = albums.create(&Album::new("Autre album".into())).unwrap();
    let album = albums
        .create(&Album::new("Album des contrats recuperes".into()))
        .unwrap();
    let mut ids = Vec::new();
    for (numero, album_id, format) in [
        (1, autre, "flac"),
        (1, album, "flac"),
        (2, album, "mp3"),
        (3, album, "flac"),
    ] {
        let mut t = Track::new(format!("Piste {album_id}-{numero}"));
        t.album_id = Some(album_id);
        t.file_path = Some(format!("/fixture/{album_id}/{numero}.{format}"));
        t.format = Some(format.into());
        t.track_number = numero;
        let id = pistes.create(&t).unwrap();
        if album_id == album {
            ids.push(id);
        }
    }
    let app = tune_server::routes::router(etat);
    let carte: CarteContrats = serde_json::from_str(CARTE_WEB).unwrap();
    for (query, attendues) in [("", ids.clone()), ("?format=flac", vec![ids[0], ids[2]])] {
        let chemin = format!("/api/v1/library/albums/{album}/tracks{query}");
        let payload = get_json(&app, &chemin)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        respecte_tous_les_contrats(&carte, "GET", "/library/albums/{}/tracks", &payload)
            .unwrap_or_else(|e| panic!("{chemin}: {e}; payload={payload}"));
        let items = payload.as_array().unwrap();
        let recues: Vec<_> = items.iter().map(|t| t["id"].as_i64().unwrap()).collect();
        assert_eq!(
            recues, attendues,
            "la reponse doit contenir les pistes du bon album, dans l'ordre, avec le filtre demande"
        );
        for t in items {
            assert_eq!(t["album_id"], album, "aucune piste d'un autre album");
            assert!(
                !t["title"].as_str().unwrap().is_empty(),
                "titre persiste present"
            );
            if !query.is_empty() {
                assert_eq!(t["format"], "flac");
            }
        }
    }
}
