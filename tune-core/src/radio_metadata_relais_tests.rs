//! Essais du relais radio-metadata (#6516). Aucun ne touche au réseau : le
//! relais est un serveur monté dans l'essai.

use super::*;
use serde_json::json;

async fn faux_relais(statut: u16, corps: Value) -> String {
    let app = axum::Router::new().route(
        "/now-playing",
        axum::routing::get(move || {
            let corps = corps.clone();
            async move {
                (
                    axum::http::StatusCode::from_u16(statut).unwrap(),
                    axum::Json(corps),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port local");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    format!("http://127.0.0.1:{port}/now-playing")
}

#[test]
fn la_cadence_vaut_5_s_par_defaut_et_reste_entre_5_et_10() {
    assert_eq!(periode_depuis(None), Duration::from_secs(5));
    assert_eq!(periode_depuis(Some("")), Duration::from_secs(5));
    assert_eq!(periode_depuis(Some("abc")), Duration::from_secs(5));
    assert_eq!(periode_depuis(Some("3")), Duration::from_secs(5));
    assert_eq!(periode_depuis(Some(" 7 ")), Duration::from_secs(7));
    assert_eq!(periode_depuis(Some("10")), Duration::from_secs(10));
    assert_eq!(periode_depuis(Some("30")), Duration::from_secs(10));
}

#[test]
fn une_station_se_retrouve_par_son_flux_puis_par_son_nom() {
    let reg = Registre::depuis(&[StationSuivie {
        flux: vec![
            "https://icecast.radiofrance.fr/fip-hifi.aac".into(),
            "http://radio-metadata.fr:8000/fiproon".into(),
        ],
        nom: "FIP".into(),
        cle: "fip".into(),
        external_id: None,
    }]);
    // Le schéma, la casse et le `/` final ne séparent pas deux écritures.
    assert_eq!(
        reg.cle_pour("Ma FIP", "HTTP://icecast.radiofrance.fr/fip-hifi.aac/")
            .as_deref(),
        Some("fip")
    );
    assert_eq!(
        reg.cle_pour("Autre", "https://radio-metadata.fr:8000/fiproon")
            .as_deref(),
        Some("fip")
    );
    // Adresse inconnue : le nom replié prend le relais.
    assert_eq!(
        reg.cle_pour(" fip ", "https://ailleurs.example/x")
            .as_deref(),
        Some("fip")
    );
    assert_eq!(
        reg.cle_pour("France Culture", "https://ailleurs.example/x"),
        None
    );
}

#[test]
fn un_nom_porte_par_deux_cles_ne_rapproche_rien() {
    let reg = Registre::depuis(&[
        StationSuivie {
            flux: vec!["https://a.example/1".into()],
            nom: "Jazz Radio".into(),
            cle: "jazzradio".into(),
            external_id: None,
        },
        StationSuivie {
            flux: vec!["https://b.example/2".into()],
            nom: "Jazz-Radio".into(),
            cle: "jazzradio_fr".into(),
            external_id: None,
        },
    ]);
    assert_eq!(
        reg.cle_pour("Jazz Radio", "https://inconnue.example/"),
        None
    );
    // L'adresse, elle, reste sans ambiguïté.
    assert_eq!(
        reg.cle_pour("Jazz Radio", "https://b.example/2").as_deref(),
        Some("jazzradio_fr")
    );
}

#[test]
fn le_nom_replie_ignore_accents_casse_et_ponctuation() {
    assert_eq!(nom_replie("FIP: Nouveautés"), nom_replie("FIP Nouveautes"));
    assert_eq!(nom_replie("FIP: Sacré Français!"), "fipsacrefrancais");
}

#[test]
fn l_entree_du_relais_donne_titre_artiste_et_pochette() {
    let corps = json!({"active": true, "items": {"fip": {
        "mode": "song", "title": "Sha-la-la", "artist": "Al Green",
        "cover_url": "https://www.radiofrance.fr/pikapi/images/x/800x800"
    }}});
    let m = lire_l_entree(&corps, "fip").expect("le relais a un titre");
    assert_eq!(m.title, "Sha-la-la");
    assert_eq!(m.artist.as_deref(), Some("Al Green"));
    assert_eq!(
        m.cover_url.as_deref(),
        Some("https://www.radiofrance.fr/pikapi/images/x/800x800")
    );
    assert!(
        lire_l_entree(&corps, "fipjazz").is_none(),
        "station absente"
    );
}

#[test]
fn une_emission_n_affiche_pas_deux_fois_son_nom_et_les_erreurs_sont_tues() {
    let corps = json!({"items": {
        "triplej": {"mode": "program", "title": "ABC Triple J", "artist": "ABC Triple J", "cover_url": null},
        "casse": {"mode": "ERROR", "title": "x"},
        "vide": {"mode": "Empty", "title": "y"},
        "sans_titre": {"mode": "song", "title": "  ", "artist": "z"}
    }});
    let m = lire_l_entree(&corps, "triplej").unwrap();
    assert_eq!(m.title, "ABC Triple J");
    assert_eq!(m.artist, None);
    assert_eq!(m.cover_url, None);
    for cle in ["casse", "vide", "sans_titre"] {
        assert!(lire_l_entree(&corps, cle).is_none(), "{cle} doit être tu");
    }
    assert!(lire_l_entree(&json!({"active": false, "items": {}}), "fip").is_none());
}

#[tokio::test]
async fn le_relais_rend_le_titre_et_un_relais_inactif_se_met_en_pause() {
    let base = faux_relais(
        200,
        json!({"active": true, "items": {"fip": {"mode": "song", "title": "T", "artist": "A"}}}),
    )
    .await;
    let m = titre_par_le_relais(&base, "fip")
        .await
        .expect("titre du relais");
    assert_eq!((m.title.as_str(), m.artist.as_deref()), ("T", Some("A")));
    assert!(!relais_en_pause(&base));

    let inactif = faux_relais(503, json!({"active": false, "error": "relais_inactif"})).await;
    assert!(titre_par_le_relais(&inactif, "fip").await.is_none());
    assert!(
        relais_en_pause(&inactif),
        "un relais inactif ne doit pas être réinterrogé toutes les 5 s"
    );
    // La pause d'un relais ne touche pas l'autre.
    assert!(!relais_en_pause(&base));
}

#[test]
fn seule_une_zone_servie_par_le_relais_passe_a_sa_cadence() {
    let zone = 6_516_001;
    assert!(!zone_a_relire(zone, Duration::ZERO), "zone jamais servie");
    noter_zone(zone, true);
    assert!(zone_a_relire(zone, Duration::ZERO));
    assert!(!zone_a_relire(zone, Duration::from_secs(5)), "trop tôt");
    noter_zone(zone, false);
    assert!(
        !zone_a_relire(zone, Duration::ZERO),
        "zone rendue au sondeur"
    );
}

#[test]
fn une_station_liee_se_retrouve_par_son_id_avant_l_adresse_et_le_nom() {
    let reg = Registre::depuis(&[StationSuivie {
        flux: vec!["https://icecast.radiofrance.fr/fip-hifi.aac".into()],
        nom: "FIP".into(),
        cle: "fip".into(),
        external_id: Some("S000973".into()),
    }]);
    // L'annuaire a changé l'adresse, et la station porte un autre nom : le
    // lien suffit.
    assert_eq!(
        reg.cle_pour_lien(Some("S000973"), "Ma radio", "https://nouveau.example/fip")
            .as_deref(),
        Some("fip")
    );
    // Liée à une fiche que le registre ne suit pas : jamais par le NOM…
    assert_eq!(
        reg.cle_pour_lien(Some("S999999"), "FIP", "https://nouveau.example/fip"),
        None
    );
    // …mais l'adresse exacte reste admise.
    assert_eq!(
        reg.cle_pour_lien(
            Some("S999999"),
            "Autre",
            "https://icecast.radiofrance.fr/fip-hifi.aac"
        )
        .as_deref(),
        Some("fip")
    );
    // Sans lien : adresse, puis nom, comme avant.
    assert_eq!(
        reg.cle_pour_lien(None, "FIP", "https://ailleurs.example/x")
            .as_deref(),
        Some("fip")
    );
}
