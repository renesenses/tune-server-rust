//! web#2795 — la surcharge locale d'une station de l'annuaire : posée par
//! « Modifier », respectée par le relevé et la lecture, effacée par « Revenir
//! à la fiche de l'annuaire ». Aucun accès réseau ni sortie audio : les
//! modifications ne touchent que le nom et le logo (une adresse SAISIE serait
//! sondée), l'adresse surchargée est posée en base.

use super::*;
use crate::routes::radios::RattrapageLogos;
use crate::routes::radios::rattraper_les_logos;
use axum::{body::Body, http::Request};
use tower::ServiceExt;

const EXT: &str = "S002795";
const FLUX_FICHE: &str = "https://icecast.example/fip-hifi.aac";
const FLUX_SECOURS: &str = "https://icecast.example/fip-secours.aac";
const FLUX_A_MOI: &str = "https://mon-relais.example/fip.mp3";
const LOGO_FICHE: &str =
    "https://mozaiklabs.fr/storage/radio-logos/radio-metadata/S002795-0123456789ab.webp";
const LOGO_NEUF: &str =
    "https://mozaiklabs.fr/storage/radio-logos/radio-metadata/S002795-ba9876543210.webp";
const LOGO_A_MOI: &str = "https://images.example/mon-logo-fip.png";

fn fiche() -> StationAnnuaire {
    StationAnnuaire {
        name: "FIP".into(),
        stream_url: FLUX_FICHE.into(),
        stream_urls: vec![FLUX_FICHE.into(), FLUX_SECOURS.into()],
        logo_url: Some(LOGO_FICHE.into()),
        external_id: Some(EXT.into()),
        codec: Some("AAC".into()),
        bitrate: Some(192),
        ..Default::default()
    }
}

/// Un serveur dont l'annuaire porte la fiche, une station de « Mes radios »
/// liée à elle et une station saisie à la main.
fn serveur() -> (AppState, i64, i64) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    *state.annuaire_radios.write().unwrap() = vec![fiche()];
    let repo = RadioRepo::with_backend(state.backend.clone());
    let liee = repo.create(&station_depuis_fiche(&fiche())).unwrap();
    repo.lier_a_l_annuaire(liee, Some(EXT)).unwrap();
    let main = repo
        .create(&RadioStation {
            name: "Radio de quartier".into(),
            url: "https://quartier.example/live.mp3".into(),
            logo_url: None,
            ..station_depuis_fiche(&fiche())
        })
        .unwrap();
    (state, liee, main)
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    path: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(methode).uri(path);
    let body = match corps {
        Some(c) => {
            req = req.header("content-type", "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let response = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let statut = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn dans_la_liste(liste: &Value, id: i64) -> Value {
    liste
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == id)
        .cloned()
        .unwrap()
}

#[tokio::test]
async fn modifier_une_station_de_l_annuaire_la_surcharge_chez_soi_et_le_dit() {
    let (state, liee, main) = serveur();
    let app = crate::routes::router(state.clone());

    let (_, liste) = appel(&app, "GET", "/api/v1/radios", None).await;
    assert_eq!(dans_la_liste(&liste, liee)["surcharge_locale"], false);

    // Le favori ne touche pas ce que la fiche fournit : pas de surcharge.
    let (s, _) = appel(
        &app,
        "PUT",
        &format!("/api/v1/radios/{liee}"),
        Some(json!({"favorite": true})),
    )
    .await;
    assert!(s.is_success(), "{s}");
    let (_, liste) = appel(&app, "GET", "/api/v1/radios", None).await;
    assert_eq!(
        dans_la_liste(&liste, liee)["surcharge_locale"],
        false,
        "un favori n'est pas une surcharge"
    );

    // L'éditeur renvoie le nom et le logo : le nom change.
    let (s, corps) = appel(
        &app,
        "PUT",
        &format!("/api/v1/radios/{liee}"),
        Some(json!({"name": "FIP (chez moi)", "logo_url": LOGO_A_MOI})),
    )
    .await;
    assert!(s.is_success(), "{s}: {corps}");
    assert_eq!(
        corps["surcharge_locale"], true,
        "la réponse porte le repère : {corps}"
    );
    let (_, liste) = appel(&app, "GET", "/api/v1/radios", None).await;
    let st = dans_la_liste(&liste, liee);
    assert_eq!(st["surcharge_locale"], true, "{st}");
    assert_eq!(st["name"], "FIP (chez moi)");
    assert_eq!(
        st["annuaire"]["external_id"], EXT,
        "la station reste liée à sa fiche"
    );

    // La fiche de l'annuaire n'est jamais modifiée.
    let f = state.annuaire_radios.read().unwrap()[0].clone();
    assert_eq!(
        (f.name.as_str(), f.logo_url.as_deref()),
        ("FIP", Some(LOGO_FICHE))
    );

    // Une station saisie à la main n'a pas de fiche à surcharger.
    let (s, corps) = appel(
        &app,
        "PUT",
        &format!("/api/v1/radios/{main}"),
        Some(json!({"name": "Radio du quartier"})),
    )
    .await;
    assert!(s.is_success(), "{s}");
    assert_eq!(corps["surcharge_locale"], false, "{corps}");
}

#[test]
fn le_releve_de_l_annuaire_n_ecrase_pas_la_surcharge() {
    let (state, liee, _) = serveur();
    let repo = RadioRepo::with_backend(state.backend.clone());
    // La station surchargée : son nom, son flux hors de la fiche, et un logo
    // de l'ANNUAIRE (l'ancien) — celui que le relevé remplacerait.
    let mut st = repo.get(liee).unwrap().unwrap();
    let avant = st.clone();
    st.name = "FIP (chez moi)".into();
    st.url = FLUX_A_MOI.into();
    repo.update(&st).unwrap();
    surcharge::noter_la_surcharge(&repo, &avant, &st);

    // L'annuaire publie un nouveau logo pour la fiche.
    let neuve = StationAnnuaire {
        logo_url: Some(LOGO_NEUF.into()),
        ..fiche()
    };
    let mut bilan = RattrapageLogos::default();
    rattraper_les_logos(&repo, &[neuve.clone()], &mut bilan);

    let apres = repo.get(liee).unwrap().unwrap();
    assert_eq!(
        apres.url, FLUX_A_MOI,
        "le relevé a repris le flux surchargé"
    );
    assert_eq!(apres.name, "FIP (chez moi)");
    assert_eq!(
        apres.logo_url.as_deref(),
        Some(LOGO_FICHE),
        "le relevé a touché le logo d'une station surchargée"
    );
    assert_eq!((bilan.updated, bilan.flux_updated), (0, 0));

    // Témoin : sans surcharge, le même relevé fait suivre la fiche.
    repo.surcharger_localement(liee, None).unwrap();
    rattraper_les_logos(&repo, &[neuve], &mut RattrapageLogos::default());
    let suit = repo.get(liee).unwrap().unwrap();
    assert_eq!(suit.url, FLUX_FICHE);
    assert_eq!(suit.logo_url.as_deref(), Some(LOGO_NEUF));
}

#[test]
fn une_station_surchargee_joue_son_flux_pas_celui_de_la_fiche() {
    let (state, liee, _) = serveur();
    let repo = RadioRepo::with_backend(state.backend.clone());
    let mut st = repo.get(liee).unwrap().unwrap();
    st.url = FLUX_A_MOI.into();
    repo.update(&st).unwrap();

    // Sans surcharge : la lecture suit la fiche (comportement de #7199).
    assert_eq!(
        crate::routes::radios::geofencing::stream_for_saved(&state, &st).unwrap(),
        FLUX_FICHE
    );
    repo.surcharger_localement(liee, Some("2026-11-11T10:00:00Z"))
        .unwrap();
    assert_eq!(
        crate::routes::radios::geofencing::stream_for_saved(&state, &st).unwrap(),
        FLUX_A_MOI,
        "la station surchargée doit jouer l'adresse choisie chez soi"
    );
}

#[tokio::test]
async fn revenir_a_la_fiche_efface_la_surcharge() {
    let (state, liee, main) = serveur();
    let repo = RadioRepo::with_backend(state.backend.clone());
    let mut st = repo.get(liee).unwrap().unwrap();
    st.name = "FIP (chez moi)".into();
    st.url = FLUX_A_MOI.into();
    st.logo_url = Some(LOGO_A_MOI.into());
    repo.update(&st).unwrap();
    repo.surcharger_localement(liee, Some("2026-11-11T10:00:00Z"))
        .unwrap();
    let app = crate::routes::router(state.clone());

    let (s, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/radios/directory/revert/{liee}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{corps}");
    assert_eq!(corps["surcharge_locale"], false);
    assert_eq!(corps["name"], "FIP");
    assert_eq!(corps["annuaire"]["external_id"], EXT);

    let apres = repo.get(liee).unwrap().unwrap();
    assert_eq!(apres.name, "FIP");
    assert_eq!(apres.url, FLUX_FICHE);
    assert_eq!(apres.logo_url.as_deref(), Some(LOGO_FICHE));
    assert!(repo.surcharges_locales().unwrap().is_empty());

    // Une station saisie à la main n'a pas de fiche où revenir.
    let (s, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/radios/directory/revert/{main}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(corps["error"], "radio_sans_fiche_liee");
    let (s, _) = appel(&app, "POST", "/api/v1/radios/directory/revert/999999", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[test]
fn un_flux_publie_par_la_fiche_est_garde_au_retour() {
    let mut st = station_depuis_fiche(&fiche());
    st.url = FLUX_SECOURS.into();
    st.name = "autre".into();
    surcharge::reprendre_la_fiche(&fiche(), &mut st);
    assert_eq!((st.name.as_str(), st.url.as_str()), ("FIP", FLUX_SECOURS));
}
