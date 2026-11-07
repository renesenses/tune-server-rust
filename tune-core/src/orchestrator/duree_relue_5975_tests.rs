//! #5975 — une piste dont la base ignore la durée (0) : la conversion WAV de
//! la zone navigateur partait sans longueur, chunkée, et la sonde
//! `Range: bytes=0-1` de Safari recevait un 200 qu'il refuse. La durée est
//! relue dans le fichier, pour que la longueur annoncée soit connue.
use super::duree_de_la_piste_ms;

fn fixture(nom: &str) -> String {
    format!("{}/tests/fixtures/{nom}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn une_duree_inconnue_en_base_est_relue_dans_le_fichier() {
    let chemin = fixture("test.flac");
    let attendue = crate::metadata::probe_duration_ms(std::path::Path::new(&chemin))
        .expect("la fixture a une durée lisible") as i64;
    assert!(attendue > 0);
    assert_eq!(
        duree_de_la_piste_ms(0, &chemin, false),
        attendue,
        "#5975 : une durée nulle en base doit être relue dans le fichier, sans quoi \
         la conversion part sans longueur et Safari refuse sa sonde"
    );
}

#[test]
fn une_duree_connue_en_base_n_est_pas_relue() {
    assert_eq!(
        duree_de_la_piste_ms(123_456, &fixture("test.flac"), false),
        123_456
    );
}

#[test]
fn une_tranche_cue_garde_sa_duree_meme_nulle() {
    // Le fichier est l'image entière : sa durée n'est pas celle de la piste.
    assert_eq!(duree_de_la_piste_ms(0, &fixture("test.flac"), true), 0);
}

#[test]
fn un_fichier_illisible_garde_la_duree_de_la_base() {
    assert_eq!(duree_de_la_piste_ms(0, "/nulle/part/absent.flac", false), 0);
}
