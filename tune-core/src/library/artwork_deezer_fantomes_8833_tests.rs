//! #8833 — réponse réelle de https://api.deezer.com/search/artist?q=Jean-Jacques%20Goldman
//! archivée le 16/11/2026. Aucun appel réseau pendant les tests.
use super::deezer_pick_image_url;
use serde_json::{Value, json};

#[test]
fn la_fiche_fantome_de_goldman_n_empeche_plus_son_portrait_8833() {
    let data: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/deezer_goldman_20261116_8833.json"
    ))
    .unwrap();
    let vraie = data["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == 1120)
        .unwrap();
    let attendue = vraie["picture_xl"].as_str().unwrap();
    assert!(!attendue.contains("/images/artist//"));
    assert_eq!(
        deezer_pick_image_url(&data, "Jean-Jacques Goldman"),
        Some(attendue),
        "#8833 : la fiche fantôme sans image est comptée comme un homonyme réel"
    );
}

#[test]
fn les_images_absentes_vides_ou_par_defaut_a_zero_fan_sont_ignorees_8833() {
    for image in [
        Value::Null,
        json!(""),
        json!("https://cdn-images.dzcdn.net/images/artist//1000x1000.jpg"),
    ] {
        let data = json!({"data": [
            {"name":"Artiste témoin", "nb_fan":100, "picture_xl":"https://example.invalid/vraie.jpg"},
            {"name":"Artiste témoin", "nb_fan":0, "picture_xl":image}
        ]});
        assert_eq!(
            deezer_pick_image_url(&data, "Artiste témoin"),
            Some("https://example.invalid/vraie.jpg"),
            "#8833 : une fiche sans image à zéro fan bloque le portrait"
        );
    }
}

#[test]
fn deux_fiches_avec_image_restent_des_homonymes_8833() {
    let data = json!({"data": [
        {"name":"Banc", "nb_fan":100000, "picture_xl":"https://example.invalid/a.jpg"},
        {"name":"Banc", "nb_fan":0, "picture_xl":"https://example.invalid/b.jpg"}
    ]});
    assert!(deezer_pick_image_url(&data, "Banc").is_none());
}

#[test]
fn un_nombre_de_fans_inconnu_ou_non_nul_ne_prouve_pas_un_fantome_8833() {
    for fans in [Value::Null, json!(19)] {
        let data = json!({"data": [
            {"name":"Banc", "nb_fan":100000, "picture_xl":"https://example.invalid/a.jpg"},
            {"name":"Banc", "nb_fan":fans, "picture_xl":""}
        ]});
        assert!(deezer_pick_image_url(&data, "Banc").is_none());
    }
}

#[test]
fn une_fiche_fantome_seule_ne_fournit_aucune_image_8833() {
    let data = json!({"data": [{"name":"Banc", "nb_fan":0, "picture_xl":""}]});
    assert!(deezer_pick_image_url(&data, "Banc").is_none());
}
