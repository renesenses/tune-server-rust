//! Vague bibliotheque du contrat web (#1897) : jamais une liste vide comme preuve.
use super::{CARTE_WEB, CarteContrats, get_json, respecte_tous_les_contrats};
use serde_json::Value;
use tune_core::db::{
    album_repo::AlbumRepo,
    artist_repo::ArtistRepo,
    models::{Album, Artist, Track},
    track_repo::TrackRepo,
};

struct Bibliotheque {
    app: axum::Router,
    artiste: i64,
    albums: [i64; 2],
    pistes: [i64; 2],
}

fn bibliotheque() -> Bibliotheque {
    let etat = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("bibliotheque de contrat isolee");
    let artistes = ArtistRepo::with_backend(etat.backend.clone());
    let albums = AlbumRepo::with_backend(etat.backend.clone());
    let pistes = TrackRepo::with_backend(etat.backend.clone());
    // Un autre artiste/piste empeche un handler qui renvoie toujours la
    // premiere ligne, ou ignore le filtre d'artiste, de satisfaire le temoin.
    let autre = artistes
        .create(&Artist::new("Autre artiste".into()))
        .unwrap();
    let artiste = artistes
        .create(&Artist::new("Artiste du contrat".into()))
        .unwrap();
    let mut ids_albums = Vec::new();
    let mut ids_pistes = Vec::new();
    for (index, artiste_id) in [autre, artiste, artiste].into_iter().enumerate() {
        let mut album = Album::new(format!("Album du contrat {index}"));
        album.artist_id = Some(artiste_id);
        album.source = "local".into();
        let album_id = albums.create(&album).unwrap();
        let mut piste = Track::new(format!("Piste du contrat {index}"));
        piste.artist_id = Some(artiste_id);
        piste.album_id = Some(album_id);
        piste.file_path = Some(format!("/fixture-contracts/album-{index}/track.flac"));
        piste.track_number = 1;
        piste.duration_ms = 60_000;
        let piste_id = pistes.create(&piste).unwrap();
        if index > 0 {
            ids_albums.push(album_id);
            ids_pistes.push(piste_id);
        }
    }
    Bibliotheque {
        app: tune_server::routes::router(etat),
        artiste,
        albums: ids_albums.try_into().unwrap(),
        pistes: ids_pistes.try_into().unwrap(),
    }
}

async fn reponse(b: &Bibliotheque, contrat: &str, chemin: &str) -> Value {
    let carte: CarteContrats = serde_json::from_str(CARTE_WEB).unwrap();
    let payload = get_json(&b.app, chemin)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    respecte_tous_les_contrats(&carte, "GET", contrat, &payload)
        .unwrap_or_else(|e| panic!("{chemin}: {e}; payload={payload}"));
    payload
}

fn exige_identites(payload: &Value, attendues: &[i64], route: &str) {
    let mut recues: Vec<i64> = payload
        .as_array()
        .expect("liste deja validee")
        .iter()
        .map(|item| item["id"].as_i64().expect("identite persistante"))
        .collect();
    recues.sort_unstable();
    let mut attendues = attendues.to_vec();
    attendues.sort_unstable();
    assert_eq!(
        recues, attendues,
        "{route}: les identites doivent provenir du perimetre demande"
    );
}

#[tokio::test]
async fn i1897_fiches_bibliotheque_respectent_la_carte_et_l_identite_demandee() {
    let b = bibliotheque();
    for (route, id, champ, nom) in [
        (
            "/library/artists/{}",
            b.artiste,
            "name",
            "Artiste du contrat",
        ),
        (
            "/library/albums/{}",
            b.albums[1],
            "title",
            "Album du contrat 2",
        ),
        (
            "/library/tracks/{}",
            b.pistes[1],
            "title",
            "Piste du contrat 2",
        ),
    ] {
        let chemin = format!("/api/v1{}", route.replace("{}", &id.to_string()));
        let payload = reponse(&b, route, &chemin).await;
        assert_eq!(
            payload["id"], id,
            "{route}: la fiche doit garder l'identite demandee"
        );
        assert_eq!(
            payload[champ], nom,
            "{route}: la fiche doit porter le nom persiste"
        );
    }
}

#[tokio::test]
async fn i1897_listes_artiste_respectent_la_carte_sur_chaque_element_persiste() {
    let b = bibliotheque();
    for (suffixe, ids) in [("albums", b.albums), ("tracks", b.pistes)] {
        let route = format!("/library/artists/{{}}/{suffixe}");
        let chemin = format!("/api/v1/library/artists/{}/{suffixe}", b.artiste);
        let payload = reponse(&b, &route, &chemin).await;
        exige_identites(&payload, &ids, &route);
        for (index, id) in ids.into_iter().enumerate() {
            let item = payload
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["id"] == id)
                .unwrap();
            let prefixe = if suffixe == "albums" {
                "Album"
            } else {
                "Piste"
            };
            assert_eq!(
                item["title"],
                format!("{prefixe} du contrat {}", index + 1),
                "{route}: chaque element doit garder son propre titre"
            );
        }
    }
}

#[tokio::test]
async fn i1897_albums_recents_respectent_la_carte_et_la_limite_sans_preuve_vide() {
    let b = bibliotheque();
    let payload = reponse(
        &b,
        "/library/albums/recent",
        "/api/v1/library/albums/recent?limit=2",
    )
    .await;
    // Les trois albums ont ete crees pendant le test : ne pas supposer un
    // ordre entre horodatages egaux. On prouve la limite et chaque fiche.
    let liste = payload.as_array().unwrap();
    assert_eq!(
        liste.len(),
        2,
        "albums recents: la limite demandee doit etre appliquee"
    );
    assert_ne!(
        liste[0]["id"], liste[1]["id"],
        "albums recents: pas deux fois la meme fiche"
    );
    for item in liste {
        let id = item["id"].as_i64().expect("identite d'album persistee");
        let fiche = reponse(
            &b,
            "/library/albums/{}",
            &format!("/api/v1/library/albums/{id}"),
        )
        .await;
        assert_eq!(
            item["title"], fiche["title"],
            "albums recents: titre conforme a la fiche persistante"
        );
    }
}
