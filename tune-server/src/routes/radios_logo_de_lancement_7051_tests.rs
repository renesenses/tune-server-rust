//! #7051 — Radio Bergeijk (fil forum 2443) : une station jouée depuis
//! l'annuaire, hors de « Mes radios », perdait son logo au premier titre.
//!
//! La fiche est celle relevée le 27/10 sur l'annuaire, anonymisée : logo
//! RELATIF en `/storage/…`, flux Ogg FLAC. Second chemin, plus bas : la même
//! station ajoutée depuis radio-browser, favicon vide.

use super::*;

/// La fiche réelle donne un logo relatif ; c'est la version absolue, dans le
/// dossier des logos de l'annuaire, que l'écoute en un clic transmet.
#[test]
fn la_fiche_au_logo_relatif_donne_un_logo_absolu_a_la_lecture() {
    let brut = vec![json!({
        "id": 9901,
        "name": "Radio Exemple",
        "stream_url": "https://stream.radio-exemple.nl/listen/radio_exemple/flac",
        "logo_url": "/storage/radio-logos/radio-metadata/S009901.webp",
        "quality": "flac",
        "country": "NL",
    })];
    let fiches = annuaire_depuis_json(&brut);
    assert_eq!(fiches.len(), 1);
    let logo = fiches[0].logo_url.as_deref().expect("la fiche a un logo");
    assert_eq!(
        logo,
        "https://mozaiklabs.fr/storage/radio-logos/radio-metadata/S009901.webp"
    );
    assert!(logo.starts_with(LOGOS_DE_L_ANNUAIRE));
}

/// La lecture partagée (« Mes radios » et écoute depuis l'annuaire) retient
/// le logo de lancement : sans cet appel, le sondeur ne trouve rien pour une
/// station hors de « Mes radios ». La fonction appelée est définie dans
/// tune-core : sa présence ici ne peut pas venir de sa définition.
#[test]
fn jouer_un_flux_retient_le_logo_de_lancement() {
    let source = include_str!("radios.rs");
    let debut = source
        .find("pub(crate) async fn jouer_un_flux(")
        .expect("jouer_un_flux existe");
    let corps = &source[debut..];
    let fin = corps.find("\n}\n").expect("fin de jouer_un_flux");
    let corps = &corps[..fin];
    let appel = corps
        .find("retenir_le_logo_de_lancement(")
        .expect("jouer_un_flux doit retenir le logo de lancement (#7051)");
    let lecture = corps
        .find("state.orchestrator.play(")
        .expect("jouer_un_flux lance la lecture");
    assert!(
        appel < lecture,
        "le logo doit être retenu AVANT le lancement : le sondeur peut passer aussitôt"
    );
}

/// La lecture générique d'une zone (`POST /zones/{id}/play` avec
/// `source: "radio"` et une pochette) retient aussi le logo de lancement,
/// avant de lancer la lecture.
#[test]
fn la_lecture_generique_d_une_radio_retient_le_logo_de_lancement() {
    let source = include_str!("playback.rs");
    let debut = source
        .find("async fn play(")
        .expect("le play générique existe");
    let corps = &source[debut..];
    let fin = corps.find("\n}\n").expect("fin du play générique");
    let corps = &corps[..fin];
    let appel = corps
        .find("retenir_le_logo_de_lancement(")
        .expect("le play générique doit retenir le logo d'une radio (#7051)");
    let demande = corps[..appel]
        .rfind("let orch_req = ")
        .expect("le logo est lu dans une demande de lecture déjà construite");
    let lecture = appel
        + corps[appel..]
            .find("lancer_la_lecture(&state, orch_req")
            .expect("le logo doit être retenu AVANT de lancer la lecture de la radio");
    // Même bloc : la demande dont on lit la pochette est celle qu'on lance,
    // aucune autre n'est construite entre la retenue et le lancement.
    assert!(
        !corps[appel..lecture].contains("let orch_req"),
        "le logo retenu doit être celui de la demande lancée juste après"
    );
    assert!(
        corps[demande..lecture].contains("orch_req.source.as_deref() == Some(\"radio\")")
            && corps[appel..lecture].contains("orch_req.cover_url.as_deref()"),
        "seule une radio retient son logo, celui de sa propre demande"
    );
}

// ---------------------------------------------------------------------------
// Second chemin : la station ajoutée par l'adresse de son flux
// ---------------------------------------------------------------------------
//
// Formes relevées le 27/10 (anonymisées) : la fiche de l'annuaire au logo
// RELATIF, et, sur radio-browser, la variante FLAC au MÊME flux avec une
// favicon VIDE. Ajoutée ainsi, la station recevait la favicon calculée par
// Tune, que le relais de pochettes refuse (fond gris), et que le rattrapage
// des logos ne remplaçait jamais.

const FLUX_FLAC: &str = "https://stream.radio-exemple.nl/listen/radio_exemple/flac";
const LOGO_DE_LA_FICHE: &str =
    "https://mozaiklabs.fr/storage/radio-logos/radio-metadata/S009901.webp";

fn fiche_reelle() -> Vec<StationAnnuaire> {
    annuaire_depuis_json(&[json!({
        "id": 9901,
        "name": "Radio Exemple",
        "stream_url": FLUX_FLAC,
        "logo_url": "/storage/radio-logos/radio-metadata/S009901.webp",
        "quality": "flac",
        "country": "NL",
    })])
}

/// La station telle que radio-browser la publie : variante FLAC, favicon vide.
fn station_radio_browser() -> Value {
    json!({
        "name": "Radio Exemple FLAC",
        "url": FLUX_FLAC,
        "codec": "OGG",
        "favicon": "",
    })
}

fn hote(logo: &str) -> String {
    reqwest::Url::parse(logo)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default()
}

/// Le constat : la favicon calculée passe par le relais, qui refuse son hôte ;
/// le logo de la fiche, lui, est admis. D'où le fond gris.
#[test]
fn la_favicon_calculee_est_refusee_par_le_relais_et_le_logo_de_la_fiche_admis() {
    use tune_core::library::artwork_proxy::hote_autorise;
    let favicon = favicon_from_url(FLUX_FLAC).expect("favicon calculée");
    assert!(favicon.starts_with(FAVICON_CALCULEE));
    assert!(!hote_autorise(&hote(&favicon), &[]));
    assert!(hote_autorise(&hote(LOGO_DE_LA_FICHE), &[]));
}

/// Ajoutée depuis radio-browser (favicon vide, envoyée absente ou vide), la
/// station prend le logo de la fiche au même flux, pas la favicon calculée.
#[test]
fn la_station_ajoutee_depuis_radio_browser_prend_le_logo_de_la_fiche() {
    let annuaire = fiche_reelle();
    let rb = station_radio_browser();
    let url = rb["url"].as_str().unwrap();
    let favicon = rb["favicon"].as_str().map(str::to_string);
    for saisi in [None, favicon] {
        let logo = logo_a_la_creation(&annuaire, url, saisi.clone(), || favicon_from_url(url));
        assert_eq!(
            logo.as_deref(),
            Some(LOGO_DE_LA_FICHE),
            "logo saisi {saisi:?} : une station ajoutée par l'adresse de son flux reçoit \
             la favicon calculée au lieu du logo de sa fiche (fond gris, #7051)"
        );
    }
}

/// Le reste ne bouge pas : un logo saisi est gardé ; sans fiche au même flux,
/// le repli d'avant (favicon calculée) ; un nom pareil ne prête rien.
#[test]
fn a_la_creation_le_logo_saisi_et_le_repli_restent() {
    let annuaire = fiche_reelle();
    let choisi = "https://exemple.invalid/mon-logo.png";
    assert_eq!(
        logo_a_la_creation(&annuaire, FLUX_FLAC, Some(choisi.into()), || None).as_deref(),
        Some(choisi)
    );
    let ailleurs = "https://autre.radio-exemple.nl/listen/radio_exemple/mobiel";
    assert_eq!(
        logo_a_la_creation(&annuaire, ailleurs, None, || favicon_from_url(ailleurs)),
        favicon_from_url(ailleurs)
    );
    let repli = || favicon_from_url(FLUX_FLAC);
    assert_eq!(
        logo_a_la_creation(&[], FLUX_FLAC, Some(String::new()), repli),
        Some(String::new()),
        "un logo vide saisi, sans fiche, reste vide comme avant"
    );
}

/// Les deux portes de création passent par `logo_a_la_creation`. La fonction
/// est définie dans ce fichier : on compte l'appel dans le CORPS de chaque
/// route, jamais la définition.
#[test]
fn les_routes_de_creation_prennent_le_logo_de_la_fiche() {
    let source = include_str!("radios.rs");
    for route in ["async fn create_radio(", "async fn add_from_web("] {
        let debut = source.find(route).expect("la route existe");
        let corps = &source[debut..];
        let corps = &corps[..corps.find("\n}\n").expect("fin de la route")];
        assert!(
            corps.contains("logo_a_la_creation("),
            "{route} doit prendre le logo de la fiche au même flux (#7051)"
        );
    }
}

/// La station déjà créée avec la favicon calculée (avant ce correctif) : le
/// rattrapage des logos lui pose le logo de la fiche au même flux, sur une
/// vraie base, et ce logo passe le relais.
#[test]
fn la_favicon_calculee_cede_au_logo_de_la_fiche_au_rattrapage() {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = RadioRepo::with_backend(state.backend.clone());
    let rb = station_radio_browser();
    let station = RadioStation {
        id: None,
        name: rb["name"].as_str().unwrap().into(),
        url: FLUX_FLAC.into(),
        homepage: None,
        logo_url: favicon_from_url(FLUX_FLAC),
        country: None,
        language: None,
        genre: None,
        codec: Some("ogg".into()),
        bitrate: None,
        is_favorite: true,
        last_played: None,
        play_count: 0,
    };
    let id = repo.create(&station).unwrap();
    let annuaire = fiche_reelle();

    assert_eq!(
        logo_a_poser(&annuaire, &repo.get(id).unwrap().unwrap(), None),
        DecisionLogo::Poser(Some(LOGO_DE_LA_FICHE.into())),
        "la favicon calculée est tenue pour un logo choisi : le logo de la fiche \
         au même flux ne la remplace jamais (fond gris, #7051)"
    );

    let mut bilan = RattrapageLogos::default();
    rattraper_les_logos(&repo, &annuaire, &mut bilan);
    let logo = repo.get(id).unwrap().unwrap().logo_url.unwrap_or_default();
    assert_eq!(logo, LOGO_DE_LA_FICHE);
    assert_eq!(bilan.updated, 1);
    let admis = tune_core::library::artwork_proxy::hote_autorise(&hote(&logo), &[]);
    assert!(admis, "le logo posé doit passer le relais de pochettes");
}

/// Sans fiche au même flux, la favicon calculée reste : il n'y a rien de
/// mieux, et le nom seul ne prête jamais de logo.
#[test]
fn sans_fiche_au_meme_flux_la_favicon_calculee_reste() {
    let annuaire = fiche_reelle();
    let ailleurs = "https://autre.radio-exemple.nl/listen/radio_exemple/mobiel";
    let st = RadioStation {
        id: Some(1),
        name: "Radio Exemple".into(),
        url: ailleurs.into(),
        homepage: None,
        logo_url: favicon_from_url(ailleurs),
        country: None,
        language: None,
        genre: None,
        codec: None,
        bitrate: None,
        is_favorite: true,
        last_played: None,
        play_count: 0,
    };
    assert_eq!(logo_a_poser(&annuaire, &st, None), DecisionLogo::Garder);
}
