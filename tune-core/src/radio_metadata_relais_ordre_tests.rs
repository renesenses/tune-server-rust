//! #6516 — radio-metadata.fr passe AVANT le code propre au diffuseur.
//!
//! Aucun essai ne touche au réseau : le relais et le `livemeta` de Radio
//! France sont des serveurs montés ici. La station est inscrite au registre du
//! processus par une adresse de flux LOCALE et un nom unique, pour qu'aucun
//! autre essai du binaire ne la rencontre.

use super::*;
use crate::radio_metadata_relais::{StationSuivie, ajouter_les_cles_pour_un_essai};
use serde_json::json;

async fn servir(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port local");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    format!("http://127.0.0.1:{port}")
}

/// Faux `livemeta/pull` : chaque canal rend un morceau qui le nomme.
async fn faux_livemeta() -> String {
    servir(
        axum::Router::new().fallback(|uri: axum::http::Uri| async move {
            let canal = uri.path().trim_start_matches('/').to_string();
            axum::Json(json!({
                "levels": [{ "position": 0, "items": ["pas"] }],
                "steps": { "pas": {
                    "title": format!("Morceau du canal {canal}"),
                    "authors": "Interprète Radio France",
                }},
            }))
        }),
    )
    .await
}

async fn faux_relais(statut: u16, corps: serde_json::Value) -> String {
    let base = servir(axum::Router::new().route(
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
    ))
    .await;
    format!("{base}/now-playing")
}

/// Une station FIP servie par un flux local, inscrite au registre sous `cle`.
fn station_fip_locale(livemeta: &str, cle: &str) -> String {
    let url = format!("{livemeta}/essai-{cle}/radiofrance/fip-hifi.aac");
    ajouter_les_cles_pour_un_essai(&[StationSuivie {
        flux: vec![url.clone()],
        nom: format!("Station d'essai {cle}"),
        cle: cle.into(),
        external_id: None,
    }]);
    url
}

#[tokio::test]
async fn le_relais_passe_avant_radio_france_pour_une_station_suivie() {
    let livemeta = faux_livemeta().await;
    let relais = faux_relais(
        200,
        json!({"active": true, "items": {"essai_6516_a": {
            "mode": "song", "title": "Titre du relais", "artist": "Artiste du relais",
            "cover_url": "https://exemple.invalid/pochette.jpg"
        }}}),
    )
    .await;
    let url = station_fip_locale(&livemeta, "essai_6516_a");
    let voies = VoiesRadioFrance {
        pull: &livemeta,
        live: &livemeta,
    };

    let (meta, par_le_relais) =
        titre_relais_puis_diffuseur(&relais, voies, None, "FIP", &url).await;
    let meta = meta.expect("un titre");
    assert_eq!(
        meta.title, "Titre du relais",
        "radio-metadata.fr doit passer avant le code Radio France"
    );
    assert_eq!(meta.artist.as_deref(), Some("Artiste du relais"));
    assert_eq!(
        meta.cover_url.as_deref(),
        Some("https://exemple.invalid/pochette.jpg")
    );
    assert!(par_le_relais);
}

#[tokio::test]
async fn un_relais_inactif_rend_la_main_au_diffuseur() {
    let livemeta = faux_livemeta().await;
    let relais = faux_relais(503, json!({"active": false, "error": "relais_inactif"})).await;
    let url = station_fip_locale(&livemeta, "essai_6516_b");
    let voies = VoiesRadioFrance {
        pull: &livemeta,
        live: &livemeta,
    };

    let (meta, par_le_relais) =
        titre_relais_puis_diffuseur(&relais, voies, None, "FIP", &url).await;
    let meta = meta.expect("le repli Radio France doit répondre");
    assert!(
        meta.title.starts_with("Morceau du canal"),
        "repli attendu sur livemeta, obtenu « {} »",
        meta.title
    );
    assert!(!par_le_relais);
}

#[tokio::test]
async fn une_station_non_suivie_ne_touche_pas_le_relais() {
    let livemeta = faux_livemeta().await;
    // Un relais qui rendrait un titre pour n'importe quelle clé : s'il était
    // interrogé, son titre gagnerait.
    let relais = faux_relais(
        200,
        json!({"active": true, "items": {"fip": {"mode": "song", "title": "Ne doit pas paraître"}}}),
    )
    .await;
    let url = format!("{livemeta}/essai-non-suivie/radiofrance/fip-hifi.aac");
    let voies = VoiesRadioFrance {
        pull: &livemeta,
        live: &livemeta,
    };
    let (meta, par_le_relais) =
        titre_relais_puis_diffuseur(&relais, voies, None, "Station non suivie 6516", &url).await;
    assert!(
        meta.expect("titre du diffuseur")
            .title
            .starts_with("Morceau du canal")
    );
    assert!(!par_le_relais);
}

#[tokio::test]
async fn une_station_liee_garde_son_titre_apres_un_changement_d_adresse() {
    // #6516 — l'annuaire a changé l'adresse du flux ; la station, liée à sa
    // fiche par `external_id`, garde l'ancienne et un autre nom. Le relais la
    // retrouve par le lien.
    let livemeta = faux_livemeta().await;
    let relais = faux_relais(
        200,
        json!({"active": true, "items": {"essai_6516_lien": {
            "mode": "song", "title": "Titre par le lien", "artist": "A"
        }}}),
    )
    .await;
    ajouter_les_cles_pour_un_essai(&[StationSuivie {
        flux: vec![format!(
            "{livemeta}/essai-nouvelle-adresse/radiofrance/fip-hifi.aac"
        )],
        nom: "Fiche d'essai 6516 lien".into(),
        cle: "essai_6516_lien".into(),
        external_id: Some("S6516LIEN".into()),
    }]);
    let ancienne = format!("{livemeta}/essai-ancienne-adresse/radiofrance/fip-hifi.aac");
    let voies = VoiesRadioFrance {
        pull: &livemeta,
        live: &livemeta,
    };
    let (meta, par_le_relais) = titre_relais_puis_diffuseur(
        &relais,
        voies,
        Some("S6516LIEN"),
        "Ma station renommée",
        &ancienne,
    )
    .await;
    assert_eq!(
        meta.expect("un titre").title,
        "Titre par le lien",
        "le relais doit retrouver la station par son external_id"
    );
    assert!(par_le_relais);
}
