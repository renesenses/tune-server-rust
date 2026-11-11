//! #8110 — les flux de repli que la route retient au lancement : les autres
//! flux autorisés de la fiche, du meilleur au moins bon, sans le flux joué.
//! Fiche synthétique, aucun réseau, aucun son.
use super::*;
use crate::routes::radios::annuaire_depuis_json;

const UK: &str = "https://radio.example/8110/uk.m3u8";
const WW_320: &str = "https://radio.example/8110/ww320.m3u8";
const WW_96: &str = "https://radio.example/8110/ww96.m3u8";
const FR: &str = "https://radio.example/8110/fr.aac";

/// BBC Radio 6 Music, en substance : un principal réservé au Royaume-Uni,
/// deux flux mondiaux (96 et 320 kbit/s), un flux réservé à la France.
fn document() -> Value {
    json!({
        "external_id": "S8110", "name": "Station 8110", "country": "GB",
        "stream_url": UK,
        "streams": [
            {"url": UK, "format": "AAC", "bitrate": 320, "is_hls": true,
             "is_primary": true, "is_published": true, "restricted_to_countries": ["GB"]},
            {"url": WW_96, "format": "AAC", "bitrate": 96, "is_hls": true,
             "is_published": true, "restricted_to_countries": []},
            {"url": WW_320, "format": "AAC", "bitrate": 320, "is_hls": true,
             "is_published": true, "restricted_to_countries": []},
            {"url": FR, "format": "AAC", "bitrate": 192,
             "is_published": true, "restricted_to_countries": ["FR"]}
        ]
    })
}

fn state() -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    *state.annuaire_radios.write().unwrap() = annuaire_depuis_json(&[document()]);
    state
}

fn set_country(state: &AppState, country: &str) {
    SettingsRepo::with_backend(state.backend.clone())
        .set(COUNTRY_SETTING, country)
        .unwrap();
}

#[test]
fn les_replis_sont_les_autres_flux_autorises_du_meilleur_au_moins_bon() {
    let state = state();
    set_country(&state, "FR");
    // Depuis la France, le principal `uk` choisi par l'auditeur (#7658) :
    // ses replis sont les flux autorisés en France, 320 avant 192 avant 96.
    assert_eq!(
        replis_autorises(&state, UK),
        vec![WW_320.to_string(), FR.to_string(), WW_96.to_string()]
    );
    // Le flux joué n'est jamais son propre repli.
    assert_eq!(
        replis_autorises(&state, WW_320),
        vec![FR.to_string(), WW_96.to_string()]
    );
    // Au Royaume-Uni, le flux réservé à la France est écarté.
    set_country(&state, "GB");
    assert_eq!(
        replis_autorises(&state, UK),
        vec![WW_320.to_string(), WW_96.to_string()]
    );
}

#[test]
fn pays_inconnu_les_replis_sont_les_flux_libres() {
    let state = state();
    // Pays d'écoute non renseigné : le flux restreint choisi reste le sien
    // (#7658), ses replis sont les seuls flux sans restriction.
    assert_eq!(
        replis_autorises(&state, UK),
        vec![WW_320.to_string(), WW_96.to_string()]
    );
}

#[test]
fn aucun_repli_hors_fiche_ou_sans_autre_flux() {
    let state = state();
    assert!(replis_autorises(&state, "https://ailleurs.example/live.mp3").is_empty());

    let seule = json!({
        "external_id": "S8110b", "name": "Seule", "country": "GB",
        "stream_url": "https://radio.example/8110/seule.m3u8",
        "streams": [
            {"url": "https://radio.example/8110/seule.m3u8", "format": "AAC", "bitrate": 128,
             "is_hls": true, "is_primary": true, "is_published": true,
             "restricted_to_countries": ["GB"]}
        ]
    });
    *state.annuaire_radios.write().unwrap() = annuaire_depuis_json(&[seule]);
    set_country(&state, "GB");
    assert!(replis_autorises(&state, "https://radio.example/8110/seule.m3u8").is_empty());
}
