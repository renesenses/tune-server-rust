//! #7043 — l'adresse du flux et le logo d'une station de l'annuaire suivent leur
//! fiche sans popup ; la fréquence des relevés est bornée.

use super::*;

const U_ANCIEN: &str = "http://ancien.example/fip.aac";
const U_NOUVEAU: &str = "https://icecast.example/fip-hifi.aac";
const U_SECOND: &str = "https://icecast.example/fip-secours.aac";
const L_ANCIEN: &str = "https://mozaiklabs.fr/storage/radio-logos/01KV2K6H8392ZH6JA8NNK4904E.png";
const L_NEUF: &str =
    "https://mozaiklabs.fr/storage/radio-logos/radio-metadata/S000973-0123456789ab.webp";

fn fiche_fip() -> StationAnnuaire {
    StationAnnuaire {
        name: "FIP".into(),
        stream_url: U_NOUVEAU.into(),
        stream_urls: vec![U_NOUVEAU.into(), U_SECOND.into()],
        logo_url: Some(L_NEUF.into()),
        external_id: Some("S000973".into()),
        codec: Some("AAC".into()),
        bitrate: Some(192),
        ..Default::default()
    }
}

fn station(url: &str, logo: Option<&str>) -> RadioStation {
    RadioStation {
        id: None,
        name: "FIP".into(),
        url: url.into(),
        homepage: None,
        logo_url: logo.map(str::to_string),
        country: None,
        language: None,
        genre: None,
        codec: Some("mp3".into()),
        bitrate: Some(128),
        is_favorite: false,
        last_played: None,
        play_count: 0,
    }
}

fn depot() -> RadioRepo {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    RadioRepo::with_backend(state.backend.clone())
}

#[test]
fn une_station_de_l_annuaire_prend_le_nouveau_flux_et_le_nouveau_logo_sans_popup() {
    let repo = depot();
    let id = repo.create(&station(U_ANCIEN, Some(L_ANCIEN))).unwrap();
    repo.lier_a_l_annuaire(id, Some("S000973")).unwrap();

    let mut bilan = RattrapageLogos::default();
    rattraper_les_logos(&repo, &[fiche_fip()], &mut bilan);

    let st = repo.get(id).unwrap().unwrap();
    assert_eq!(st.url, U_NOUVEAU, "le flux doit suivre la fiche");
    assert_eq!(
        st.logo_url.as_deref(),
        Some(L_NEUF),
        "le logo WebP remplace l'ancien"
    );
    assert_eq!(st.codec.as_deref(), Some("aac"));
    assert_eq!(st.bitrate, Some(192));
    assert_eq!((bilan.updated, bilan.flux_updated), (1, 1));

    // Idempotent : une seconde passe ne touche plus rien.
    let mut encore = RattrapageLogos::default();
    rattraper_les_logos(&repo, &[fiche_fip()], &mut encore);
    assert_eq!((encore.updated, encore.flux_updated), (0, 0));
}

#[test]
fn une_station_saisie_a_la_main_ne_voit_jamais_son_flux_change() {
    // Pas de lien enregistré, et une adresse qui n'est celle d'aucune fiche.
    let repo = depot();
    let id = repo.create(&station(U_ANCIEN, None)).unwrap();
    rattraper_les_logos(&repo, &[fiche_fip()], &mut RattrapageLogos::default());
    let st = repo.get(id).unwrap().unwrap();
    assert_eq!(
        st.url, U_ANCIEN,
        "une station sans lien n'est jamais réécrite"
    );
    assert!(!repo.liens_annuaire().unwrap().contains_key(&id));
}

#[test]
fn une_adresse_encore_publiee_par_la_fiche_est_gardee() {
    let repo = depot();
    let id = repo.create(&station(U_SECOND, None)).unwrap();
    repo.lier_a_l_annuaire(id, Some("S000973")).unwrap();
    let mut bilan = RattrapageLogos::default();
    rattraper_les_logos(&repo, &[fiche_fip()], &mut bilan);
    assert_eq!(repo.get(id).unwrap().unwrap().url, U_SECOND);
    assert_eq!(bilan.flux_updated, 0);
}

#[test]
fn une_fiche_sortie_de_l_annuaire_laisse_la_station_a_son_adresse() {
    let repo = depot();
    let id = repo.create(&station(U_ANCIEN, None)).unwrap();
    repo.lier_a_l_annuaire(id, Some("S000973")).unwrap();
    // Le relevé ne contient plus la fiche (retirée).
    let autre = StationAnnuaire {
        external_id: Some("S999999".into()),
        ..fiche_fip()
    };
    rattraper_les_logos(&repo, &[autre], &mut RattrapageLogos::default());
    assert_eq!(repo.get(id).unwrap().unwrap().url, U_ANCIEN);
}

#[test]
fn le_releve_de_fond_est_toutes_les_6_h_et_l_ouverture_au_plus_une_fois_par_heure() {
    use super::super::radios_annuaire::{
        DELAI_A_L_OUVERTURE, PERIODE_DE_RELEVE, tentative_permise,
    };
    assert_eq!(PERIODE_DE_RELEVE.as_secs(), 6 * 3600);
    assert_eq!(DELAI_A_L_OUVERTURE.as_secs(), 3600);
    let h = DELAI_A_L_OUVERTURE;
    assert!(tentative_permise(0, 10_000, h), "jamais tenté : permis");
    assert!(
        !tentative_permise(10_000, 10_000 + 59 * 60, h),
        "59 min : refusé"
    );
    assert!(tentative_permise(10_000, 10_000 + 3600, h), "1 h : permis");
}
