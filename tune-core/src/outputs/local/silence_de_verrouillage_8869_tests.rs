//! #8869 — témoins du silence de verrouillage : format inchangé → rien ;
//! changement → la durée réglée ; DoP → trames de repos 0x69 sans rupture
//! d'alternance ; DSD natif → motif 0x69 ; réglage 0 → rien.

use super::*;
use crate::audio::dsd_to_dop::DsdToDoP;
use crate::audio::silence_de_verrouillage::MemoireDuFormat;
use crate::outputs::traits::{AudioSpec, ProfondeurPcm};

struct Enregistreur<'v>(&'v mut Vec<f32>);

impl PuitsDEchantillons for Enregistreur<'_> {
    fn ecrire(&mut self, mots: &[f32]) -> bool {
        self.0.extend_from_slice(mots);
        true
    }
}

struct EnregistreurNatif<'v>(&'v mut Vec<u8>);

impl PuitsNatif for EnregistreurNatif<'_> {
    fn ecrire(&mut self, bloc: BlocPcm<'_>) -> bool {
        self.0.extend_from_slice(bloc.octets());
        true
    }
}

const CADENCE: u32 = 48_000;

/// Joue `mots` (stéréo, `cadence`) sur une sortie neuve à chaque appel, qui
/// partage `memoire` ; rend ce que le puits réel a reçu.
fn jouer_flottant(
    memoire: &MemoireDuFormat,
    reglage_ms: u32,
    cadence: u32,
    mots: &[f32],
) -> Vec<f32> {
    let mut recu = Vec::new();
    {
        let mut puits = PuitsFlottantVerrouille::nouveau(
            Box::new(Enregistreur(&mut recu)),
            FormatOuvert::new(cadence, 2),
            SilenceDeVerrouillage::new(reglage_ms, memoire.clone()),
            None,
        );
        // Deux blocs : seul le premier est précédé.
        let moitie = mots.len() / 2;
        assert!(puits.ecrire(&mots[..moitie]));
        assert!(puits.ecrire(&mots[moitie..]));
    }
    recu
}

fn musique(trames: usize) -> Vec<f32> {
    (0..trames * 2)
        .map(|i| 0.25 + (i % 7) as f32 * 0.01)
        .collect()
}

/// Dans une MÊME ouverture (mémoire non oubliée), le même format ne reçoit
/// rien de plus.
#[test]
fn format_inchange_aucun_silence() {
    let memoire = MemoireDuFormat::default();
    let piste = musique(1_000);
    let premiere = jouer_flottant(&memoire, 250, CADENCE, &piste);
    assert_eq!(
        premiere.len(),
        piste.len() + 12_000 * 2,
        "premier flux : 250 ms"
    );
    let seconde = jouer_flottant(&memoire, 250, CADENCE, &piste);
    assert_eq!(seconde, piste, "même format : pas un échantillon de plus");
}

#[test]
fn changement_de_cadence_silence_de_la_duree_reglee() {
    let memoire = MemoireDuFormat::default();
    let piste = musique(1_000);
    jouer_flottant(&memoire, 500, CADENCE, &piste);
    let recu = jouer_flottant(&memoire, 500, 96_000, &piste);
    let silence = 48_000 * 2; // 500 ms à 96 kHz, stéréo
    assert_eq!(recu.len(), silence + piste.len());
    assert!(recu[..silence].iter().all(|&s| s == 0.0), "PCM : des zéros");
    assert_eq!(&recu[silence..], &piste[..], "la piste suit, intacte");
}

#[test]
fn reglage_nul_rien_meme_au_changement() {
    let memoire = MemoireDuFormat::default();
    let piste = musique(500);
    assert_eq!(jouer_flottant(&memoire, 0, CADENCE, &piste), piste);
    assert_eq!(jouer_flottant(&memoire, 0, 96_000, &piste), piste);
}

/// Des trames DoP réelles (stéréo, 24 bits petit-boutien).
fn dop_octets(trames: usize) -> Vec<u8> {
    let mut e = DsdToDoP::new(2, false);
    e.feed(&vec![0xA5; trames * 4])
}

/// Vérifie que chaque trame porte un marqueur DoP, identique sur les deux
/// canaux, en alternance stricte ; et que les `silence` premières ont la
/// charge utile de repos 0x6969.
fn verifier_le_porteur(mots_24: &[i32], silence: usize) {
    let mut precedent = None;
    for (i, t) in mots_24.chunks_exact(2).enumerate() {
        let m = (t[0] >> 16) as u8;
        assert!(
            (m == 0x05 || m == 0xFA) && (t[1] >> 16) as u8 == m,
            "trame {i} sans marqueur DoP : {t:06x?}"
        );
        assert_ne!(precedent, Some(m), "trame {i} : alternance rompue");
        precedent = Some(m);
        if i < silence {
            assert_eq!(t[0] & 0xFFFF, 0x6969, "trame {i} : repos 0x69 attendu");
            assert_eq!(t[1] & 0xFFFF, 0x6969, "trame {i} : repos 0x69 attendu");
        }
    }
}

#[test]
fn dop_flottant_trames_de_repos_0x69_sans_rupture_de_marqueur() {
    let piste = pcm_bytes_to_f32(&dop_octets(400), 24);
    // Les deux parités du nombre de trames de silence : 100 ms et 50 ms à
    // 176,4 kHz.
    for (ms, trames) in [(100, 17_640), (50, 8_820), (1, 176)] {
        let memoire = MemoireDuFormat::default();
        let recu = jouer_flottant(&memoire, ms, 176_400, &piste);
        assert_eq!(recu.len(), trames * 2 + piste.len());
        let mots: Vec<i32> = recu
            .iter()
            .map(|&s| ((s * 8_388_608.0) as i32) & 0x00FF_FFFF)
            .collect();
        verifier_le_porteur(&mots, trames);
    }
}

#[test]
fn natif_pcm_des_octets_nuls_au_format_du_bloc() {
    let spec = AudioSpec::nouvelle(44_100, ProfondeurPcm::Entier24, 2).unwrap();
    let piste: Vec<u8> = (0..600).map(|i| (i % 251) as u8 + 1).collect();
    let memoire = MemoireDuFormat::default();
    let mut recu = Vec::new();
    {
        let mut puits = PuitsNatifVerrouille::nouveau(
            Box::new(EnregistreurNatif(&mut recu)),
            SilenceDeVerrouillage::new(100, memoire.clone()),
        );
        assert!(puits.ecrire(spec.bloc(&piste)));
    }
    let silence = 4_410 * 6;
    assert_eq!(recu.len(), silence + piste.len());
    assert!(recu[..silence].iter().all(|&o| o == 0));
    assert_eq!(&recu[silence..], &piste[..]);

    // Même format ensuite : rien.
    let mut encore = Vec::new();
    {
        let mut puits = PuitsNatifVerrouille::nouveau(
            Box::new(EnregistreurNatif(&mut encore)),
            SilenceDeVerrouillage::new(100, memoire.clone()),
        );
        assert!(puits.ecrire(spec.bloc(&piste)));
    }
    assert_eq!(encore, piste);
}

#[test]
fn natif_dop_trames_de_repos_0x69() {
    let spec = AudioSpec::nouvelle(176_400, ProfondeurPcm::Entier24, 2).unwrap();
    let piste = dop_octets(400);
    let memoire = MemoireDuFormat::default();
    // Un flux PCM à la MÊME cadence avant : seul le mode change.
    memoire.retenir(crate::audio::silence_de_verrouillage::SignatureDuFormat {
        cadence: 176_400,
        canaux: 2,
        mode: crate::audio::silence_de_verrouillage::ModeDuFlux::Pcm,
    });
    let mut recu = Vec::new();
    {
        let mut puits = PuitsNatifVerrouille::nouveau(
            Box::new(EnregistreurNatif(&mut recu)),
            SilenceDeVerrouillage::new(20, memoire.clone()),
        );
        assert!(puits.ecrire(spec.bloc(&piste)));
    }
    let trames = 3_528;
    assert_eq!(recu.len(), trames * 6 + piste.len());
    let mots: Vec<i32> = recu
        .chunks_exact(3)
        .map(|o| i32::from(o[0]) | i32::from(o[1]) << 8 | i32::from(o[2]) << 16)
        .collect();
    verifier_le_porteur(&mots, trames);
}

#[test]
fn dsd_natif_motif_0x69_de_la_duree_reglee() {
    let memoire = MemoireDuFormat::default();
    let anneau = AnneauDsd::new(1 << 20);
    // DSD64 stéréo, 100 ms : 352 800 / 8 / 10 = 35 280 octets par canal.
    let v = SilenceDeVerrouillage::new(100, memoire.clone());
    let pousse = pousser_dans_l_anneau_dsd(&v, &anneau, 2_822_400, 2, 8, || false);
    assert_eq!(pousse, 35_280 * 2 - (35_280 * 2) % 8);
    let mut lu = vec![0u8; anneau.disponible()];
    anneau.tirer(&mut lu);
    assert!(
        lu.iter().all(|&o| o == 0x69),
        "repos DSD 0x69, jamais des zéros"
    );

    // Même format : rien. Réglage nul : rien.
    assert_eq!(
        pousser_dans_l_anneau_dsd(&v, &anneau, 2_822_400, 2, 8, || false),
        0
    );
    let nul = SilenceDeVerrouillage::new(0, MemoireDuFormat::default());
    assert_eq!(
        pousser_dans_l_anneau_dsd(&nul, &anneau, 2_822_400, 2, 8, || false),
        0
    );
}

#[test]
fn le_plafond_du_chemin_partage_borne_le_silence() {
    let memoire = MemoireDuFormat::default();
    let mut recu = Vec::new();
    {
        let mut puits = PuitsFlottantVerrouille::nouveau(
            Box::new(Enregistreur(&mut recu)),
            FormatOuvert::new(CADENCE, 2),
            SilenceDeVerrouillage::new(2_000, memoire),
            Some(1_000),
        );
        assert!(puits.ecrire(&musique(10)));
    }
    assert_eq!(recu.len(), 1_000 * 2 + 20);
}

/// Gapless : une piste enchaînée sur le périphérique resté ouvert passe par le
/// MÊME puits ; seul le premier bloc de l'ouverture est précédé.
#[test]
fn enchainement_sans_fermeture_aucun_silence_entre_les_pistes() {
    let memoire = MemoireDuFormat::default();
    let piste = musique(400);
    let mut recu = Vec::new();
    {
        let mut puits = PuitsFlottantVerrouille::nouveau(
            Box::new(Enregistreur(&mut recu)),
            FormatOuvert::new(CADENCE, 2),
            SilenceDeVerrouillage::new(100, memoire),
            None,
        );
        for _ in 0..3 {
            assert!(puits.ecrire(&piste));
        }
    }
    assert_eq!(recu.len(), 4_800 * 2 + 3 * piste.len(), "un seul silence");
}

/// Rouverte après fermeture (arrêt, pause qui rend le périphérique, piste non
/// enchaînée, changement de zone) : silence même au MÊME format.
#[test]
fn reouverture_au_meme_format_silence_regle() {
    let sortie = LocalOutput::new("DAC test 8869".to_string());
    sortie.set_silence_de_verrouillage_ms(100);
    let piste = musique(400);
    for ouverture in 0..2 {
        let mut recu = Vec::new();
        {
            let mut puits = PuitsFlottantVerrouille::nouveau(
                Box::new(Enregistreur(&mut recu)),
                FormatOuvert::new(CADENCE, 2),
                sortie.silence_pour_une_ouverture(),
                None,
            );
            assert!(puits.ecrire(&piste));
        }
        assert_eq!(
            recu.len(),
            4_800 * 2 + piste.len(),
            "ouverture {ouverture} : le DAC se reverrouille, silence attendu"
        );
    }
    // Réglage nul : rien, même à l'ouverture.
    sortie.set_silence_de_verrouillage_ms(0);
    let mut recu = Vec::new();
    {
        let mut puits = PuitsFlottantVerrouille::nouveau(
            Box::new(Enregistreur(&mut recu)),
            FormatOuvert::new(CADENCE, 2),
            sortie.silence_pour_une_ouverture(),
            None,
        );
        assert!(puits.ecrire(&piste));
    }
    assert_eq!(recu, piste);
}

/// `play_url` (chaque ouverture du périphérique) prend le silence par
/// `silence_pour_une_ouverture`, qui oublie le format précédent.
#[test]
fn play_url_prend_le_silence_d_une_ouverture() {
    let src: String = include_str!("../local.rs")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    assert_eq!(
        src.matches("letsilence_de_verrouillage=self.silence_pour_une_ouverture();")
            .count(),
        1,
        "play_url doit lire le silence d'une OUVERTURE"
    );
}
