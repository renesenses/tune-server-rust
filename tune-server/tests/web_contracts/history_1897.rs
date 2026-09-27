//! Contrats d'historique exercés sur des écoutes persistées (#1897).
use super::{CARTE_WEB, CarteContrats, get_json, respecte_tous_les_contrats};

#[tokio::test]
async fn i1897_historique_non_vide_respecte_les_contrats_du_web() {
    let etat = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("historique de contrat isolé");
    for _ in 0..2 {
        etat.backend
            .execute(
                "INSERT INTO listen_history (title, artist_name, album_title, source, duration_ms, cover_url) \
                 VALUES ('Piste témoin', 'Artiste témoin', 'Album témoin', 'local', 60000, 'https://example.invalid/cover.jpg')",
                &[],
            )
            .expect("écoute persistée");
    }
    // Une radio ne doit pas faire passer le témoin d'une liste vide de pistes.
    etat.backend
        .execute(
            "INSERT INTO listen_history (title, source, duration_ms) VALUES ('Radio témoin', 'radio', 60000)",
            &[],
        )
        .expect("radio persistée");

    let app = tune_server::routes::router(etat);
    let carte: CarteContrats = serde_json::from_str(CARTE_WEB).expect("carte web");
    let tops = get_json(&app, "/api/v1/library/history/top-tracks?limit=10")
        .await
        .expect("route top-tracks réelle");
    respecte_tous_les_contrats(&carte, "GET", "/library/history/top-tracks", &tops)
        .unwrap_or_else(|erreur| panic!("{erreur}; payload={tops}"));
    let pistes = tops.as_array().expect("liste de pistes");
    assert_eq!(
        pistes.len(),
        1,
        "la radio ne doit pas figurer parmi les pistes"
    );
    assert_eq!(pistes[0]["title"], "Piste témoin");
    assert_eq!(pistes[0]["plays"], 2);

    let dashboard = get_json(&app, "/api/v1/library/history/dashboard?period=all")
        .await
        .expect("route dashboard réelle");
    respecte_tous_les_contrats(&carte, "GET", "/library/history/dashboard", &dashboard)
        .unwrap_or_else(|erreur| panic!("{erreur}; payload={dashboard}"));
    assert_eq!(dashboard["top_tracks"][0]["title"], "Piste témoin");
    assert_eq!(dashboard["top_tracks"][0]["plays"], 2);
}
