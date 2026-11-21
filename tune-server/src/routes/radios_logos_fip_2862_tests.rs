//! Fil forum 2862 (Lio, rc12) : « FIP Monde » et « FIP Nouveautés » restaient
//! sans logo. Le semis (`seed_default_radios`) leur donnait l'adresse nue
//! `…-hifi.aac`, que leur fiche de l'annuaire ne publie pas (elle publie
//! `…-hifi.aac?id=radiofrance`) : le rattrapage des logos ne les rapprochait
//! jamais (de même FIP Reggae et FIP Sacré français). Le semis et la
//! migration 157 sont corrigés.
//!
//! L'annuaire est celui que servait mozaiklabs.fr le 20/11/2026 pour les
//! treize webradios FIP (`tests/fixtures/annuaire_v2_fip_20261120.json`).
//! Aucun accès réseau.

use super::*;

fn annuaire_fip() -> Vec<StationAnnuaire> {
    let brut: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/annuaire_v2_fip_20261120.json"
    ))
    .expect("fixture lisible");
    let fiches = annuaire_depuis_json(brut["radios"].as_array().expect("radios"));
    assert_eq!(fiches.len(), 13, "les treize fiches FIP de l'annuaire");
    fiches
}

fn stations_fip(repo: &RadioRepo) -> Vec<RadioStation> {
    repo.list()
        .unwrap()
        .into_iter()
        .filter(|s| s.name.to_lowercase().starts_with("fip"))
        .collect()
}

#[test]
fn chaque_radio_fip_semee_recoit_le_logo_de_sa_fiche() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = RadioRepo::with_backend(state.backend.clone());
    let annuaire = annuaire_fip();

    let semees = stations_fip(&repo);
    // Les douze FIP du semis (33), plus « Fip Cultes » du semis de l'annuaire.
    assert!(semees.len() >= 12, "FIP semées : {semees:?}");

    let mut bilan = RattrapageLogos::default();
    rattraper_les_logos(&repo, &annuaire, &mut bilan);

    let liens = repo.liens_annuaire().unwrap();
    for st in stations_fip(&repo) {
        let logo = st.logo_url.as_deref().unwrap_or_default();
        assert!(
            logo.starts_with(LOGOS_DE_L_ANNUAIRE),
            "« {} » ({}) est restée sans logo de l'annuaire : {:?}",
            st.name,
            st.url,
            st.logo_url
        );
        // Le logo est bien celui de SA fiche, retrouvée par le lien posé.
        let ext = liens
            .get(&st.id.unwrap())
            .unwrap_or_else(|| panic!("« {} » n'est liée à aucune fiche", st.name));
        let fiche = annuaire
            .iter()
            .find(|f| f.external_id.as_deref() == Some(ext.as_str()))
            .unwrap();
        assert_eq!(st.logo_url, fiche.logo_url, "« {} »", st.name);
    }
}

#[test]
fn fip_monde_et_nouveautes_portent_leur_propre_logo() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = RadioRepo::with_backend(state.backend.clone());
    rattraper_les_logos(&repo, &annuaire_fip(), &mut RattrapageLogos::default());

    let logo_de = |nom: &str| {
        stations_fip(&repo)
            .into_iter()
            .find(|s| s.name == nom)
            .and_then(|s| s.logo_url)
            .unwrap_or_else(|| panic!("« {nom} » sans logo"))
    };
    assert!(logo_de("FIP Monde").contains("/S000980"));
    assert!(logo_de("FIP Nouveautés").contains("/S000981"));
    assert!(logo_de("FIP Reggae").contains("/S000983"));
    assert!(logo_de("FIP Sacré français").contains("/S000985"));
}
