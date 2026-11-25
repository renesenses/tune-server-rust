//! #9488 — l'ordre d'essai des autres flux d'une station : après le flux
//! lancé (le préféré de l'auditeur s'il en a choisi un), le même codec à la
//! qualité la plus proche, puis le reste. Fiche synthétique, aucun réseau.
use super::*;
use crate::routes::radios::annuaire_depuis_json;

const AAC_256: &str = "https://radio.example/9488/aac256";
const MP3_320: &str = "https://radio.example/9488/mp3320";
const AAC_128: &str = "https://radio.example/9488/aac128";
const AAC_192: &str = "https://radio.example/9488/aac192";
const MP3_128: &str = "https://radio.example/9488/mp3128";

fn state() -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let document = json!({
        "external_id": "S9488", "name": "Station 9488", "country": "FR",
        "stream_url": AAC_256,
        "streams": [
            {"url": AAC_256, "format": "AAC", "bitrate": 256, "is_primary": true,
             "is_published": true, "restricted_to_countries": []},
            {"url": MP3_320, "format": "MP3", "bitrate": 320,
             "is_published": true, "restricted_to_countries": []},
            {"url": AAC_128, "format": "AAC", "bitrate": 128,
             "is_published": true, "restricted_to_countries": []},
            {"url": AAC_192, "format": "AAC", "bitrate": 192,
             "is_published": true, "restricted_to_countries": []},
            {"url": MP3_128, "format": "MP3", "bitrate": 128,
             "is_published": true, "restricted_to_countries": []}
        ]
    });
    *state.annuaire_radios.write().unwrap() = annuaire_depuis_json(&[document]);
    state
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|u| u.to_string()).collect()
}

#[test]
fn apres_le_flux_par_defaut_le_meme_codec_a_la_qualite_la_plus_proche_puis_le_reste() {
    let state = state();
    // Le 256 AAC par défaut en panne : 192 puis 128 AAC, avant le MP3 320
    // pourtant mieux classé dans l'absolu.
    assert_eq!(
        replis_autorises(&state, AAC_256),
        s(&[AAC_192, AAC_128, MP3_320, MP3_128])
    );
}

#[test]
fn le_flux_prefere_part_en_premier_et_ses_replis_suivent_son_codec() {
    let state = state();
    // L'auditeur a choisi le 128 AAC : c'est lui que la lecture lance…
    assert_eq!(stream_for_url(&state, AAC_128).unwrap(), AAC_128);
    // …puis, s'il échoue, le plus proche du même codec d'abord.
    assert_eq!(
        replis_autorises(&state, AAC_128),
        s(&[AAC_192, AAC_256, MP3_320, MP3_128])
    );
    // Préféré en MP3 : l'autre MP3 avant les AAC.
    assert_eq!(
        replis_autorises(&state, MP3_320),
        s(&[MP3_128, AAC_256, AAC_192, AAC_128])
    );
}

#[test]
fn sans_flux_joue_connu_l_ordre_reste_celui_du_rang() {
    let fiches = annuaire_depuis_json(&[json!({
        "external_id": "S9488b", "name": "B", "country": "FR", "stream_url": AAC_256,
        "streams": [
            {"url": AAC_128, "format": "AAC", "bitrate": 128, "is_published": true,
             "restricted_to_countries": []},
            {"url": MP3_320, "format": "MP3", "bitrate": 320, "is_published": true,
             "restricted_to_countries": []}
        ]
    })]);
    let mut flux = fiches[0].flux.clone();
    ordonner_les_replis(None, &mut flux);
    let urls: Vec<String> = flux.into_iter().map(|f| f.url).collect();
    assert_eq!(urls, s(&[MP3_320, AAC_128]));
}
