//! #7500 : une station du portail (`mozaiklabs:ID`) reçoit un numéro
//! radio-metadata (`S…`). « Mes radios » l'avait liée sous l'ancienne clé : elle
//! retrouve sa fiche, et son lien prend le nouveau numéro. Vraie base en
//! mémoire, annuaire synthétique, aucun appel réseau ni son.
use super::*;
use axum::{body::Body, http::Request};
use tower::ServiceExt;

const FLUX: &str = "https://console.example/live.mp3";

/// La fiche 1644 telle que la v2 la sert : sans numéro (avant), avec (après).
fn fiche_1644(external_id: Option<&str>) -> Value {
    json!({"id": 1644, "external_id": external_id, "source": if external_id.is_some() { "radio-metadata" } else { "mozaiklabs" },
        "name": "Station de la console", "status": "active", "stream_url": FLUX,
        "logo_url": "/storage/radio-logos/console.webp",
        "streams": [{"id": 1, "url": FLUX, "format": "MP3", "bitrate": 128, "is_primary": true, "is_published": true}]})
}

fn station_locale(url: &str) -> RadioStation {
    RadioStation {
        id: None,
        name: "Station de la console".into(),
        url: url.into(),
        homepage: None,
        logo_url: None,
        country: None,
        language: None,
        genre: None,
        codec: None,
        bitrate: None,
        is_favorite: true,
        last_played: None,
        play_count: 0,
    }
}

#[test]
fn la_fiche_numerotee_garde_son_ancienne_cle_7500() {
    let avant = annuaire_depuis_json(&[fiche_1644(None)]);
    assert_eq!(avant[0].external_id.as_deref(), Some("mozaiklabs:1644"));
    assert_eq!(avant[0].cle_locale.as_deref(), Some("mozaiklabs:1644"));

    let apres = annuaire_depuis_json(&[fiche_1644(Some("S004321"))]);
    assert_eq!(apres[0].external_id.as_deref(), Some("S004321"));
    assert!(apres[0].porte_la_cle("S004321"));
    assert!(apres[0].porte_la_cle("mozaiklabs:1644"));
    assert!(!apres[0].porte_la_cle("mozaiklabs:1645"));

    // Le lien enregistré sous l'ancienne clé retrouve la fiche, par son lien
    // (logo prêté), même quand l'adresse locale a changé.
    let index = IndexAnnuaire::new(&apres);
    assert_eq!(
        index.lien(
            Some("mozaiklabs:1644"),
            "Autre nom",
            "https://ailleurs.example/x"
        ),
        Some((0, Lien::Id))
    );
    assert_eq!(index.lien(Some("S004321"), "", ""), Some((0, Lien::Id)));
}

#[test]
fn contre_epreuve_une_autre_ancienne_cle_ne_rapproche_rien_7500() {
    let apres = annuaire_depuis_json(&[fiche_1644(Some("S004321"))]);
    let index = IndexAnnuaire::new(&apres);
    assert_eq!(
        index.lien(Some("mozaiklabs:1645"), "Station de la console", FLUX),
        None
    );
}

#[test]
fn le_lien_sous_l_ancienne_cle_prend_le_nouveau_numero_7500() {
    let etat = AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = RadioRepo::with_backend(etat.backend.clone());
    let id = repo.create(&station_locale(FLUX)).unwrap();
    repo.lier_a_l_annuaire(id, Some("mozaiklabs:1644")).unwrap();
    // une station liée à une fiche qui n'a pas changé de clé : jamais réécrite
    let autre = repo
        .create(&station_locale("https://autre.example/s"))
        .unwrap();
    repo.lier_a_l_annuaire(autre, Some("S000973")).unwrap();

    let apres = annuaire_depuis_json(&[fiche_1644(Some("S004321"))]);
    assert_eq!(lier_les_stations(&repo, &apres), 1);
    assert_eq!(repo.lien_annuaire(id).unwrap().as_deref(), Some("S004321"));
    assert_eq!(
        repo.lien_annuaire(autre).unwrap().as_deref(),
        Some("S000973")
    );
    // rejoué : rien de plus
    assert_eq!(lier_les_stations(&repo, &apres), 0);
}

#[tokio::test]
async fn ajouter_la_fiche_numerotee_ne_cree_pas_de_doublon_7500() {
    let etat = AppState::new(":memory:", 0, Default::default()).unwrap();
    *etat.annuaire_radios.write().unwrap() = annuaire_depuis_json(&[fiche_1644(Some("S004321"))]);
    let repo = RadioRepo::with_backend(etat.backend.clone());
    // l'adresse locale n'est plus celle de la fiche : seul le lien la rattache
    let id = repo
        .create(&station_locale("https://console.example/ancien.mp3"))
        .unwrap();
    repo.lier_a_l_annuaire(id, Some("mozaiklabs:1644")).unwrap();
    let avant = repo.list().unwrap().len(); // la base neuve a ses radios de départ

    let reponse = crate::routes::router(etat)
        .oneshot(
            Request::post("/api/v1/radios/directory/S004321/add")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reponse.status(), StatusCode::OK, "la station existe déjà");
    assert_eq!(repo.list().unwrap().len(), avant, "aucun doublon");
    assert_eq!(repo.lien_annuaire(id).unwrap().as_deref(), Some("S004321"));
}

#[tokio::test]
async fn l_ancienne_cle_ouvre_encore_la_fiche_7500() {
    let etat = AppState::new(":memory:", 0, Default::default()).unwrap();
    *etat.annuaire_radios.write().unwrap() = annuaire_depuis_json(&[fiche_1644(Some("S004321"))]);
    let reponse = crate::routes::router(etat.clone())
        .oneshot(
            Request::post("/api/v1/radios/directory/mozaiklabs:1644/add")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reponse.status(), StatusCode::CREATED);
    let repo = RadioRepo::with_backend(etat.backend.clone());
    let id = repo
        .list()
        .unwrap()
        .into_iter()
        .find(|st| st.name == "Station de la console")
        .and_then(|st| st.id)
        .unwrap();
    assert_eq!(
        repo.lien_annuaire(id).unwrap().as_deref(),
        Some("mozaiklabs:1644")
    );
    // le relevé suivant lui donne le numéro
    let fiches = etat.annuaire_radios.read().unwrap().clone();
    assert_eq!(lier_les_stations(&repo, &fiches), 1);
    assert_eq!(repo.lien_annuaire(id).unwrap().as_deref(), Some("S004321"));
}
