//! Témoins synthétiques du contrat v2 : aucune fiche réelle restreinte n'a
//! été remise à cette tentative. Aucun accès réseau ni sortie audio.
use super::*;
use crate::routes::{radios::annuaire_depuis_json, radios_annuaire::station_depuis_fiche};
use axum::{body::Body, http::Request};
use tower::ServiceExt;

fn document() -> Value {
    json!({
        "external_id": "S7199", "name": "Station témoin", "country": "GB",
        "stream_url": "https://radio.example/gb.m3u8",
        "v1_stream_url": "https://radio.example/legacy",
        "streams": [
            {"url": "https://radio.example/gb.m3u8", "format": "AAC", "bitrate": 320, "is_hls": true,
             "is_primary": true, "is_published": true, "restricted_to_countries": ["GB"]},
            {"url": "https://radio.example/world.mp3", "format": "MP3", "bitrate": 128,
             "is_published": true, "restricted_to_countries": null},
            {"url": "https://radio.example/fr.aac", "format": "AAC", "bitrate": 192,
             "is_published": true, "restricted_to_countries": ["fr", "BE"]},
            {"url": "https://radio.example/hidden.flac", "format": "FLAC", "bitrate": 1000,
             "is_published": false, "restricted_to_countries": []}
        ]
    })
}
fn fiche(value: Value) -> StationAnnuaire {
    annuaire_depuis_json(&[value]).remove(0)
}
fn state(value: Value) -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    *state.annuaire_radios.write().unwrap() = vec![fiche(value)];
    state
}
fn set_country(state: &AppState, country: &str) {
    SettingsRepo::with_backend(state.backend.clone())
        .set(COUNTRY_SETTING, country)
        .unwrap();
}

#[test]
fn country_filter_rejects_the_foreign_primary_7199() {
    let f = fiche(document());
    assert_eq!(
        select_stream(&f, Some(" FR "), None).unwrap().url,
        "https://radio.example/fr.aac",
        "le flux principal réservé au Royaume-Uni doit être écarté en France"
    );
    assert_eq!(
        select_stream(&f, Some("gb"), None).unwrap().url,
        f.stream_url
    );
    assert_eq!(
        select_stream(&f, Some("US"), None).unwrap().url,
        "https://radio.example/world.mp3"
    );
    assert_eq!(f.flux_proposes()[0].restricted_to_countries, ["GB"]);
}

#[test]
fn unknown_country_uses_only_unrestricted_streams_7199() {
    let f = fiche(document());
    for country in [None, Some(""), Some("auto"), Some("ZZ"), Some("France")] {
        assert_eq!(
            select_stream(&f, country, None).unwrap().url,
            "https://radio.example/world.mp3"
        );
    }
    let mut value = document();
    value["streams"].as_array_mut().unwrap().truncate(1);
    let f = fiche(value);
    assert!(
        select_stream(&f, None, None).is_none(),
        "un alias v1 ne doit pas contourner une restriction v2"
    );
    assert!(select_stream(&f, Some("FR"), None).is_none());
}

#[test]
fn allowed_preference_is_kept_and_forbidden_preference_falls_back_7199() {
    let f = fiche(document());
    let world = "https://radio.example/world.mp3";
    assert_eq!(
        select_stream(&f, Some("FR"), Some(world)).unwrap().url,
        world
    );
    assert_eq!(
        select_stream(&f, Some("FR"), Some(&f.stream_url))
            .unwrap()
            .url,
        "https://radio.example/fr.aac"
    );
    assert_eq!(
        select_stream(&f, Some("FR"), Some("https://radio.example/hidden.flac"))
            .unwrap()
            .url,
        "https://radio.example/fr.aac"
    );
}

#[test]
fn empty_unpublished_or_malformed_v2_never_reopens_legacy_7199() {
    let mut value = document();
    value["streams"] = json!([]);
    assert!(select_stream(&fiche(value.clone()), None, None).is_none());
    value["streams"] = json!([{"url": "https://radio.example/gb.m3u8", "is_published": false}]);
    assert!(select_stream(&fiche(value.clone()), None, None).is_none());
    for restriction in [json!("GB"), json!([42]), json!([""])] {
        value["streams"] = json!([{"url": "https://radio.example/gb.m3u8", "restricted_to_countries": restriction}]);
        assert!(select_stream(&fiche(value.clone()), Some("FR"), None).is_none());
    }
    value.as_object_mut().unwrap().remove("streams");
    assert_eq!(
        select_stream(&fiche(value), None, None).unwrap().url,
        "https://radio.example/gb.m3u8"
    );
}

#[test]
fn restrictions_absent_null_and_empty_are_unrestricted_7199() {
    for restriction in [None, Some(Value::Null), Some(json!([]))] {
        let mut value = document();
        value["streams"].as_array_mut().unwrap().truncate(1);
        value["streams"][0]
            .as_object_mut()
            .unwrap()
            .remove("restricted_to_countries");
        if let Some(r) = restriction {
            value["streams"][0]["restricted_to_countries"] = r;
        }
        assert!(select_stream(&fiche(value), None, None).is_some());
    }
}

#[test]
fn saved_radio_reads_current_country_and_stable_link_7199() {
    let state = state(document());
    let repo = RadioRepo::with_backend(state.backend.clone());
    let f = state.annuaire_radios.read().unwrap()[0].clone();
    let mut radio = station_depuis_fiche(&f);
    radio.id = Some(repo.create(&radio).unwrap());
    repo.lier_a_l_annuaire(radio.id.unwrap(), Some("S7199"))
        .unwrap();
    set_country(&state, "FR");
    assert_eq!(
        stream_for_saved(&state, &radio).unwrap(),
        "https://radio.example/fr.aac"
    );
    set_country(&state, "GB");
    assert_eq!(stream_for_saved(&state, &radio).unwrap(), f.stream_url);
    radio.url = "https://radio.example/obsolete".into();
    assert_eq!(stream_for_saved(&state, &radio).unwrap(), f.stream_url);
    assert_eq!(
        repo.get(radio.id.unwrap()).unwrap().unwrap().url,
        f.stream_url,
        "la préférence enregistrée n'est pas réécrite par une écoute"
    );
    radio.id = None;
    assert_eq!(
        stream_for_saved(&state, &radio).unwrap(),
        radio.url,
        "une saisie personnelle sans fiche reste intacte"
    );
    radio.url = f.stream_url;
    set_country(&state, "FR");
    assert_eq!(
        stream_for_saved(&state, &radio).unwrap(),
        "https://radio.example/fr.aac"
    );
}

async fn request(app: &axum::Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn http_country_setting_and_addition_select_the_allowed_stream_7199() {
    let state = state(document());
    let app = crate::routes::router(state.clone());
    let (status, body) = request(
        &app,
        "PATCH",
        "/api/v1/system/config",
        json!({"radios_country": " fr "}),
    )
    .await;
    assert!(status.is_success(), "{status}: {body}");
    let (status, body) = request(&app, "GET", "/api/v1/system/config", Value::Null).await;
    assert!(status.is_success());
    assert_eq!(body[COUNTRY_SETTING], "FR");
    let (status, body) = request(
        &app,
        "POST",
        "/api/v1/radios/directory/S7199/add",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["stream_url"], "https://radio.example/fr.aac");
    assert_eq!(body["codec"], "aac");
    assert_eq!(body["bitrate"], 192);
    for invalid in [json!("France"), json!(true), json!("ZZ")] {
        let (status, _) = request(
            &app,
            "PATCH",
            "/api/v1/system/config",
            json!({"radios_country": invalid}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn http_play_refuses_before_any_output_or_play_count_7199() {
    let mut value = document();
    value["streams"].as_array_mut().unwrap().truncate(1);
    let state = state(value);
    set_country(&state, "FR");
    let repo = RadioRepo::with_backend(state.backend.clone());
    let id = repo
        .create(&station_depuis_fiche(
            &state.annuaire_radios.read().unwrap()[0],
        ))
        .unwrap();
    repo.lier_a_l_annuaire(id, Some("S7199")).unwrap();
    let app = crate::routes::router(state.clone());
    for (method, path) in [
        (
            "POST",
            "/api/v1/radios/directory/S7199/play/99999".to_string(),
        ),
        ("POST", format!("/api/v1/radios/{id}/play/99999")),
        ("GET", format!("/api/v1/radios/{id}/audio.wav")),
    ] {
        let (status, body) = request(&app, method, &path, json!({})).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{path}: {body}");
        assert_eq!(body["error_code"], "radio_country_unavailable");
        assert_eq!(body["output_sent"], false);
        assert!(body["error"].as_str().unwrap().contains("Aucun flux"));
    }
    assert_eq!(repo.get(id).unwrap().unwrap().play_count, 0);
    set_country(&state, "");
    let (_, body) = request(
        &app,
        "POST",
        "/api/v1/radios/directory/S7199/play/99999",
        json!({}),
    )
    .await;
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("pays d’écoute n’est pas renseigné")
    );
}

#[tokio::test]
async fn generic_zone_play_rejects_a_foreign_radio_stream_7199() {
    let mut value = document();
    value["streams"].as_array_mut().unwrap().truncate(1);
    let state = state(value);
    set_country(&state, "FR");
    let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .create("Zone témoin sans son", Some("browser"), None)
        .unwrap();
    let app = crate::routes::router(state);
    let (_, body) = request(&app, "POST", &format!("/api/v1/zones/{zone}/play"), json!({
        "source": "radio", "source_id": "https://radio.example/gb.m3u8", "title": "Station témoin"
    })).await;
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|e| e.contains("Aucun flux") && e.contains("FR")),
        "{body}"
    );
}

#[test]
fn equal_streams_keep_directory_order_and_lossless_wins_fallback_7199() {
    let mut value = document();
    value["streams"][1]["bitrate"] = json!(192);
    assert_eq!(
        select_stream(&fiche(value.clone()), Some("FR"), None)
            .unwrap()
            .url,
        "https://radio.example/world.mp3"
    );
    value["streams"][2]["format"] = json!("FLAC");
    value["streams"][2]["bitrate"] = Value::Null;
    assert_eq!(
        select_stream(&fiche(value), Some("FR"), None).unwrap().url,
        "https://radio.example/fr.aac"
    );
}

#[tokio::test]
async fn unavailable_radio_can_still_be_saved_as_favorite_7199() {
    let mut value = document();
    value["streams"].as_array_mut().unwrap().truncate(1);
    let state = state(value);
    let app = crate::routes::router(state);
    let (status, body) = request(
        &app,
        "POST",
        "/api/v1/radios/directory/S7199/add",
        json!({"favorite": true}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["favorite"], true);
    let id = body["id"].as_i64().unwrap();
    let (status, body) = request(
        &app,
        "POST",
        &format!("/api/v1/radios/{id}/play/99999"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

// ── #7587 — le flux choisi d'une fiche hors « Mes radios » et la garde #7199 ──

/// `POST` sans en-tête `Content-Type`, avec ou sans corps : ce qu'envoie un
/// client qui ne choisit rien.
async fn request_raw(app: &axum::Router, path: &str, body: &'static str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Station réservée au Royaume-Uni, écoute depuis la France : quel que soit
/// le corps — `{}`, `null`, rien, un `stream_url` vide ou le flux interdit
/// choisi sur la fiche —, la lecture rend le refus structuré de #7199, sans
/// sortie ni comptage d'écoute de la station liée.
#[tokio::test]
async fn http_directory_play_with_or_without_chosen_stream_keeps_the_country_refusal_7587() {
    let mut value = document();
    value["streams"].as_array_mut().unwrap().truncate(1);
    let state = state(value);
    set_country(&state, "FR");
    let repo = RadioRepo::with_backend(state.backend.clone());
    let id = repo
        .create(&station_depuis_fiche(
            &state.annuaire_radios.read().unwrap()[0],
        ))
        .unwrap();
    repo.lier_a_l_annuaire(id, Some("S7199")).unwrap();
    let app = crate::routes::router(state.clone());
    let path = "/api/v1/radios/directory/S7199/play/99999";
    let mut reponses = Vec::new();
    for body in [
        json!({}),
        Value::Null,
        json!({"stream_url": null}),
        json!({"stream_url": ""}),
        json!({"stream_url": "https://radio.example/gb.m3u8"}),
    ] {
        reponses.push((body.to_string(), request(&app, "POST", path, body).await));
    }
    reponses.push(("<sans corps>".into(), request_raw(&app, path, "").await));
    for (body, (status, reponse)) in reponses {
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body}: {reponse}"
        );
        assert_eq!(
            reponse["error_code"], "radio_country_unavailable",
            "#7587 : le corps {body} doit atteindre la garde de pays #7199, \
             pas être rejeté avant elle ({reponse})"
        );
        assert_eq!(reponse["output_sent"], false, "{body}: {reponse}");
    }
    assert_eq!(repo.get(id).unwrap().unwrap().play_count, 0);
}

/// Les flux de la fiche témoin servis en local : la lecture n'atteint pas le
/// réseau. Rend l'application et une zone navigateur.
async fn local_directory_and_zone() -> (axum::Router, i64, String) {
    let audio: &'static [u8] = include_bytes!("../../../tune-core/tests/fixtures/test.mp3");
    let serveur = axum::Router::new()
        .fallback(move || async move { ([("content-type", "audio/mpeg")], audio) });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = format!(
        "http://127.0.0.1:{}/",
        listener.local_addr().unwrap().port()
    );
    tokio::spawn(async move {
        axum::serve(listener, serveur).await.ok();
    });
    let texte = document()
        .to_string()
        .replace("https://radio.example/", &local);
    let state = state(serde_json::from_str(&texte).unwrap());
    set_country(&state, "FR");
    let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .create("Onglet 7587", Some("browser"), None)
        .unwrap();
    (crate::routes::router(state), zone, local)
}

/// Écoute depuis la France : le flux choisi interdit (réservé au Royaume-Uni)
/// cède la place au meilleur flux autorisé ; le flux choisi autorisé joue.
#[tokio::test]
async fn http_directory_play_chosen_forbidden_stream_falls_back_to_allowed_one_7587() {
    let (app, zone, local) = local_directory_and_zone().await;
    let path = format!("/api/v1/radios/directory/S7199/play/{zone}");
    for (choisi, attendu) in [("gb.m3u8", "fr.aac"), ("world.mp3", "world.mp3")] {
        let (status, body) = request(
            &app,
            "POST",
            &path,
            json!({"stream_url": format!("{local}{choisi}")}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{choisi}: {body}");
        assert_eq!(
            body["state"]["now_playing"]["source_id"],
            format!("{local}{attendu}"),
            "#7587 : choisi {choisi}, c'est {attendu} qui doit jouer ({body})"
        );
    }
}

/// La fiche BBC Radio 3 de l'annuaire, réduite à ses trois flux : le
/// principal 320 kbit/s « ww » (autorisé partout sauf au Royaume-Uni), le
/// « nonuk » 96 kbit/s sans restriction, le « uk » 320 kbit/s.
fn document_bbc_7658() -> Value {
    json!({
        "external_id": "S7658", "name": "Station BBC témoin", "country": "GB",
        "stream_url": "https://radio.example/ww/audio=320000.m3u8",
        "streams": [
            {"url": "https://radio.example/ww/audio=320000.m3u8", "format": "AAC", "bitrate": 320,
             "is_hls": true, "is_primary": true, "is_published": true,
             "restricted_to_countries": ["FR", "BE", "US", "DE"]},
            {"url": "https://radio.example/nonuk/master.m3u8", "format": "AAC", "bitrate": 96,
             "is_hls": true, "is_published": true, "restricted_to_countries": null},
            {"url": "https://radio.example/uk/master.m3u8", "format": "AAC", "bitrate": 320,
             "is_hls": true, "is_published": true, "restricted_to_countries": ["GB", "GG", "IM", "JE"]}
        ]
    })
}

#[test]
fn pays_inconnu_le_flux_choisi_est_respecte_7658() {
    let f = fiche(document_bbc_7658());
    let principal = "https://radio.example/ww/audio=320000.m3u8";
    assert_eq!(
        select_stream(&f, None, Some(principal)).unwrap().url,
        principal,
        "le flux choisi par l'auditeur ne doit pas céder au 96 kbit/s"
    );
    // Contre-épreuve : sans choix explicite et sans pays, la règle de #7199
    // reste entière (seul le flux sans restriction est proposé).
    assert_eq!(
        select_stream(&f, None, None).unwrap().url,
        "https://radio.example/nonuk/master.m3u8"
    );
    // Un pays connu qui interdit le flux choisi l'écarte toujours.
    assert_eq!(
        select_stream(&f, Some("GB"), Some(principal)).unwrap().url,
        "https://radio.example/uk/master.m3u8"
    );
}

#[test]
fn pays_inconnu_sans_flux_libre_le_repli_enregistre_reste_refuse_7199_7658() {
    // Contrat de #7199 : sans flux libre, l'adresse enregistrée est le repli
    // de l'ajout, pas un choix ; pays inconnu, la lecture reste refusée.
    let mut value = document();
    value["streams"].as_array_mut().unwrap().truncate(1);
    let f = fiche(value);
    assert!(select_stream(&f, None, Some("https://radio.example/gb.m3u8")).is_none());
    assert_eq!(
        select_stream(&f, Some("GB"), Some("https://radio.example/gb.m3u8"))
            .unwrap()
            .url,
        "https://radio.example/gb.m3u8"
    );
}

#[tokio::test]
async fn pays_inconnu_deux_chemins_de_lecture_gardent_le_choix_7658() {
    let state = state(document_bbc_7658());
    let principal = "https://radio.example/ww/audio=320000.m3u8";
    // Chemin 1 : « Mes radios » (lecture d'une radio enregistrée, liée à sa fiche).
    let repo = RadioRepo::with_backend(state.backend.clone());
    let f = state.annuaire_radios.read().unwrap()[0].clone();
    let mut radio = station_depuis_fiche(&f);
    radio.url = principal.into();
    radio.id = Some(repo.create(&radio).unwrap());
    repo.lier_a_l_annuaire(radio.id.unwrap(), Some("S7658"))
        .unwrap();
    assert_eq!(stream_for_saved(&state, &radio).unwrap(), principal);
    // Chemin 2 : commande générique de zone (favori, historique, widget) qui porte l'URL.
    assert_eq!(stream_for_url(&state, principal).unwrap(), principal);
    // Contre-épreuve : le flux 96 kbit/s choisi reste lui aussi respecté.
    let nonuk = "https://radio.example/nonuk/master.m3u8";
    assert_eq!(stream_for_url(&state, nonuk).unwrap(), nonuk);
    // Et un pays renseigné qui l'interdit reprend la main.
    set_country(&state, "GB");
    assert_eq!(
        stream_for_url(&state, principal).unwrap(),
        "https://radio.example/uk/master.m3u8"
    );
}

/// #7515 — BBC Radio 6 Music telle que le testeur l'a vue depuis la France :
/// le principal 320 kbit/s réservé au Royaume-Uni, puis un 96 kbit/s et un
/// 320 kbit/s qui jouent hors du Royaume-Uni, dans cet ordre de fiche.
fn document_bbc_6music_7515() -> Value {
    json!({
        "external_id": "S7515", "name": "BBC Radio 6 Music témoin", "country": "GB",
        "stream_url": "https://radio.example/ww/6music-320.m3u8",
        "streams": [
            {"url": "https://radio.example/ww/6music-320.m3u8", "format": "AAC", "bitrate": 320,
             "is_hls": true, "is_primary": true, "is_published": true,
             "restricted_to_countries": ["GB"]},
            {"url": "https://radio.example/ms6/6music-96.m3u8", "format": "AAC", "bitrate": 96,
             "is_hls": true, "is_published": true, "restricted_to_countries": null},
            {"url": "https://radio.example/ms6/6music-320.m3u8", "format": "AAC", "bitrate": 320,
             "is_hls": true, "is_published": true, "restricted_to_countries": null}
        ]
    })
}

const BBC6_PRINCIPAL: &str = "https://radio.example/ww/6music-320.m3u8";
const BBC6_96: &str = "https://radio.example/ms6/6music-96.m3u8";
const BBC6_320: &str = "https://radio.example/ms6/6music-320.m3u8";

#[test]
fn les_flux_proposes_sont_ranges_par_qualite_decroissante_7515() {
    let f = fiche(document_bbc_6music_7515());
    let urls: Vec<String> = f.flux_proposes().into_iter().map(|f| f.url).collect();
    assert_eq!(
        urls,
        [BBC6_PRINCIPAL, BBC6_320, BBC6_96],
        "#7515 : les flux doivent venir du meilleur au moins bon (le principal \
         ne départage que deux flux de même débit), pas dans l'ordre de la fiche"
    );
    // Sans perte avant tout débit, le débit avant le principal, puis la
    // fréquence d'échantillonnage.
    let f = fiche(json!({
        "external_id": "Q7515", "name": "Qualité", "stream_url": "https://q.example/a.mp3",
        "streams": [
            {"url": "https://q.example/a.mp3", "format": "MP3", "bitrate": 128, "is_primary": true,
             "is_published": true},
            {"url": "https://q.example/b.aac", "format": "AAC", "bitrate": 320, "sample_rate": 44100,
             "is_published": true},
            {"url": "https://q.example/d.aac", "format": "AAC", "bitrate": 320, "sample_rate": 48000,
             "is_published": true},
            {"url": "https://q.example/c.flac", "format": "FLAC", "bitrate": 0, "is_published": true}
        ]
    }));
    let urls: Vec<String> = f.flux_proposes().into_iter().map(|f| f.url).collect();
    assert_eq!(
        urls,
        [
            "https://q.example/c.flac",
            "https://q.example/d.aac",
            "https://q.example/b.aac",
            "https://q.example/a.mp3"
        ]
    );
}

#[test]
fn le_flux_par_defaut_est_le_meilleur_autorise_pas_le_principal_7515() {
    // Un principal moins bon qu'un autre flux autorisé ne passe plus devant.
    let f = fiche(json!({
        "external_id": "P7515", "name": "Principal faible", "stream_url": "https://p.example/128.mp3",
        "streams": [
            {"url": "https://p.example/128.mp3", "format": "MP3", "bitrate": 128, "is_primary": true,
             "is_published": true},
            {"url": "https://p.example/320.aac", "format": "AAC", "bitrate": 320, "is_published": true}
        ]
    }));
    for country in [None, Some("FR")] {
        assert_eq!(
            select_stream(&f, country, None).unwrap().url,
            "https://p.example/320.aac",
            "#7515 ({country:?}) : sans choix, le meilleur flux autorisé est joué, pas le principal"
        );
    }
    // Un choix explicite autorisé reste le sien.
    assert_eq!(
        select_stream(&f, Some("FR"), Some("https://p.example/128.mp3"))
            .unwrap()
            .url,
        "https://p.example/128.mp3"
    );
    // BBC Radio 6 Music : depuis la France comme pays inconnu, le principal
    // réservé au Royaume-Uni n'est jamais le flux par défaut. Garde de
    // non-régression seulement : sans choix, ce principal était déjà écarté
    // avant #7515 ; le témoin du correctif est la fiche « Principal faible ».
    let f = fiche(document_bbc_6music_7515());
    for country in [None, Some("FR")] {
        assert_eq!(select_stream(&f, country, None).unwrap().url, BBC6_320);
    }
    assert_eq!(
        select_stream(&f, Some("FR"), Some(BBC6_PRINCIPAL))
            .unwrap()
            .url,
        BBC6_320,
        "le principal choisi mais interdit en France cède au meilleur flux autorisé"
    );
    assert_eq!(
        select_stream(&f, Some("GB"), None).unwrap().url,
        BBC6_PRINCIPAL
    );
}

fn flux_de_l_annuaire(corps: &Value) -> Vec<(String, Value, Value)> {
    corps["items"][0]["streams"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["url"].as_str().unwrap().to_string(),
                s["par_defaut"].clone(),
                s["autorise"].clone(),
            )
        })
        .collect()
}

#[test]
fn l_annuaire_marque_le_flux_par_defaut_jouable_dans_le_pays_7515() {
    let state = state(document_bbc_6music_7515());
    let f = state.annuaire_radios.read().unwrap()[0].clone();
    let corps = || json!({"items": [{"external_id": "S7515", "streams": f.flux_proposes()}]});

    // Pays d'écoute inconnu : le principal restreint n'est pas proposé par
    // défaut, et le serveur ne prétend pas savoir s'il joue.
    let mut c = corps();
    marquer_le_flux_par_defaut(&state, &mut c);
    assert_eq!(
        flux_de_l_annuaire(&c),
        [
            (BBC6_PRINCIPAL.to_string(), json!(false), Value::Null),
            (BBC6_320.to_string(), json!(true), json!(true)),
            (BBC6_96.to_string(), json!(false), json!(true)),
        ],
        "#7515 : le flux par défaut doit être jouable ({c})"
    );

    // Depuis la France : le principal est dit interdit.
    set_country(&state, "FR");
    let mut c = corps();
    marquer_le_flux_par_defaut(&state, &mut c);
    assert_eq!(
        flux_de_l_annuaire(&c),
        [
            (BBC6_PRINCIPAL.to_string(), json!(false), json!(false)),
            (BBC6_320.to_string(), json!(true), json!(true)),
            (BBC6_96.to_string(), json!(false), json!(true)),
        ]
    );

    // Depuis le Royaume-Uni : le principal redevient le flux par défaut.
    set_country(&state, "GB");
    let mut c = corps();
    marquer_le_flux_par_defaut(&state, &mut c);
    assert_eq!(
        flux_de_l_annuaire(&c)[0],
        (BBC6_PRINCIPAL.to_string(), json!(true), json!(true))
    );
}

#[test]
fn le_debit_annonce_suit_le_flux_jouable_7515() {
    // « Mes radios » a gardé le principal réservé au Royaume-Uni : depuis la
    // France, l'écran annonce le flux réellement joué (320 kbit/s autorisé).
    let state = state(document_bbc_6music_7515());
    set_country(&state, "FR");
    let mut station = json!({"stream_url": BBC6_PRINCIPAL, "bitrate": 320, "codec": "aac"});
    annoncer_le_debit_ecoute(&state, &mut station);
    assert_eq!(station["flux_ecoute"]["url"], BBC6_320, "{station}");
    assert_eq!(station["flux_ecoute"]["raison"], "restreint_au_pays");
    assert_eq!(station["bitrate"], 320);
}

#[test]
fn autorise_et_choix_du_flux_suivent_la_meme_regle_de_pays_7515() {
    let aucun: [&str; 0] = [];
    assert_eq!(permis_dans_le_pays(&aucun, None), Some(true));
    assert_eq!(permis_dans_le_pays(&["GB"], Some("gb")), Some(true));
    assert_eq!(permis_dans_le_pays(&["GB"], Some("FR")), Some(false));
    assert_eq!(permis_dans_le_pays(&["GB"], None), None);
    assert_eq!(
        permis_dans_le_pays(&["invalid"], None),
        Some(false),
        "#7515 : un flux marqué invalid n'est jamais annoncé jouable, même pays inconnu"
    );
}
