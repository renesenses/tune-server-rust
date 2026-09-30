//! #5451 — **CoreAudio exclusif** (macOS, mode « hog ») : le blanc entre deux
//! pistes de même format.
//!
//! Comme ASIO avant #5442, le bras CoreAudio lisait sa piste jusqu'à l'EOF
//! puis rendait la main sans consommer le `next_media` préparé ;
//! `sait_enchainer` rendait `false` pour `CoreAudioExclusif`, le sondeur
//! n'armait jamais le gapless, et chaque changement de piste relâchait le
//! mode « hog » puis rouvrait le périphérique AU MÊME FORMAT.
//!
//! `bras_coreaudio.rs` ne se compile que sous macOS. Ce qui est jugé ici, sur
//! Linux, c'est ce qu'il appelle, sans rien de factice entre les deux : son
//! étage (un [`EtageDeConversion`] au format identité, monté comme le bras le
//! monte), la boucle commune (`BoucleProducteur::tourner`), la poursuite
//! partagée avec ASIO (`poursuivre_la_chaine_par_la_boucle`), la frontière
//! (`accepter_la_suivante`) et le puits de capture flottant, à la place de
//! l'anneau de l'`ExclusiveOutput`.

use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};

use super::chaine_native::{FinDeChaine, IssueDeLaChaine};
use super::chaine_par_la_boucle::poursuivre_la_chaine_par_la_boucle;
use super::empreinte_du_puits_r1::{DspAuRepos, etage};
use super::enchainement_exclusif::{BrasDeLecture, bras_de_lecture, lire_l_entete_enchainee};
use super::gapless_asio_5204::{ReserveFactice, Temoins, pcm, spec, wav};
use super::{
    AudioSpec, CompteursDePiste, Etage, EtageDeConversion, FinDeBoucle, PousseeVersLePuits,
    RoleDeLaBoucle,
};
use crate::outputs::traits::{CaptureOutput, FormatOuvert, ProfondeurPcm, PuitsDEchantillons};

// ─── La capacité publiée ────────────────────────────────────────────────────

/// LE témoin de la capacité : une sortie CoreAudio en mode exclusif annonce
/// l'enchaînement interne. Avant #5451, `sait_enchainer` rendait `false`
/// pour `CoreAudioExclusif` : le sondeur n'armait jamais le gapless.
#[test]
fn le_bras_coreaudio_annonce_l_enchainement_interne_5451() {
    let bras = bras_de_lecture("macos", false, true, "coreaudio");
    assert_eq!(bras, BrasDeLecture::CoreAudioExclusif);
    assert!(
        bras.sait_enchainer(),
        "#5451 : en CoreAudio exclusif, la sortie doit annoncer l'enchaînement \
         interne — sinon le sondeur n'arme pas le gapless et chaque piste \
         relâche puis rouvre le périphérique au même format"
    );
}

// ─── La chaîne du bras ──────────────────────────────────────────────────────

/// L'étage du bras, au format identité : la sortie est ouverte à la cadence
/// et aux canaux de la source (`EtageDeConversion` de `jouer_via_coreaudio`).
fn etage_du_bras(dsp: &DspAuRepos, cadence: u32, bits: u16, canaux: u16) -> EtageDeConversion<'_> {
    let e = etage(dsp, Vec::new(), cadence, canaux, bits, cadence, canaux);
    assert!(!e.needs_resample && !e.needs_channel_adapt());
    e
}

/// Ce que le bras fait pour la piste initiale : pousser l'amorce (le PCM lu
/// avec l'en-tête), puis lire le reste par la boucle commune jusqu'à sa fin
/// de flux. Rend les trames.
fn jouer_la_piste_initiale(
    temoins: &Temoins,
    etage: &mut EtageDeConversion<'_>,
    puits: &mut dyn PuitsDEchantillons,
    wav_initial: Vec<u8>,
) -> u64 {
    let mut lecteur = Cursor::new(wav_initial);
    let entete = lire_l_entete_enchainee(&mut lecteur, &AtomicBool::new(false)).unwrap();
    let mut compteurs = CompteursDePiste {
        total_bytes_read: 0,
        total_frames_fed: 0,
        seek_offset: 0,
        skip_bytes: 0,
        skipped_bytes: 0,
        premiere_donnee_journalisee: false,
    };
    etage.recevoir(entete.amorce());
    if let PousseeVersLePuits::Poussee { trames_source } =
        etage.pousser(&mut *puits, &mut |_, _, _| false, &mut |_| {})
    {
        compteurs.total_frames_fed += trames_source;
    }
    let fin = temoins.boucle(RoleDeLaBoucle::PisteInitiale).tourner(
        &mut lecteur,
        &mut [0; 65536],
        etage,
        &mut *puits,
        &mut |_, _, _| false,
        &mut compteurs,
        &mut |_| true,
    );
    assert!(
        matches!(fin, FinDeBoucle::FinDeFlux),
        "la piste A va au bout"
    );
    compteurs.total_frames_fed
}

/// Ce que le puits reçoit d'une seule piste A+B, par le même étage.
fn temoin_d_une_seule_piste(
    dsp: &DspAuRepos,
    cadence: u32,
    bits: u16,
    canaux: u16,
    a_puis_b: Vec<u8>,
) -> CaptureOutput {
    let mut temoin = CaptureOutput::ouvert(FormatOuvert::new(cadence, canaux));
    let mut e = etage_du_bras(dsp, cadence, bits, canaux);
    e.recevoir(&a_puis_b);
    let puits: &mut dyn PuitsDEchantillons = &mut temoin;
    assert!(matches!(
        e.pousser(puits, &mut |_, _, _| false, &mut |_| {}),
        PousseeVersLePuits::Poussee { .. }
    ));
    temoin
}

/// Joue A, puis la réserve (B, …) par la poursuite du bras. Rend l'issue, le
/// puits et la réserve.
fn jouer_a_puis_la_reserve(
    dsp: &DspAuRepos,
    temoins: &Temoins,
    (cadence, bits, canaux): (u32, u16, u16),
    wav_a: Vec<u8>,
    suivantes: Vec<Vec<u8>>,
) -> (IssueDeLaChaine, CaptureOutput, ReserveFactice, AudioSpec) {
    let mut puits = CaptureOutput::ouvert(FormatOuvert::new(cadence, canaux));
    let mut e = etage_du_bras(dsp, cadence, bits, canaux);
    let trames_a = jouer_la_piste_initiale(temoins, &mut e, &mut puits, wav_a);
    let mut reserve = ReserveFactice::default();
    reserve.reserve.extend(suivantes);
    let issue = poursuivre_la_chaine_par_la_boucle(
        &mut reserve,
        &mut e,
        &mut puits as &mut dyn PuitsDEchantillons,
        &temoins.boucle(RoleDeLaBoucle::PisteEnchainee),
        &mut [0; 65536],
        |lecteur| lecteur,
        trames_a,
    );
    (issue, puits, reserve, e.spec)
}

/// LE témoin de l'enchaînement CoreAudio : deux pistes de même format
/// passent dans le MÊME puits — l'anneau du périphérique ouvert en « hog » —
/// sans qu'il soit refermé, et le puits reçoit exactement les mots des deux
/// pistes, bout à bout, à l'octet près.
///
/// Avant la correction, le bras rendait la main à l'EOF de A : B n'atteignait
/// jamais ce puits (le sondeur la relançait par `play_url`, qui rouvrait le
/// périphérique).
#[test]
fn coreaudio_deux_pistes_de_meme_format_passent_dans_le_meme_anneau_5451() {
    let dsp = DspAuRepos::neuf();
    let temoins = Temoins::neufs();
    let pcm_a = pcm(4 * 3_000, 1);
    let pcm_b = pcm(4 * 2_000, 2);

    let (issue, puits, reserve, _) = jouer_a_puis_la_reserve(
        &dsp,
        &temoins,
        (44_100, 16, 2),
        wav(44_100, 16, 2, &pcm_a),
        vec![wav(44_100, 16, 2, &pcm_b)],
    );

    assert_eq!(
        issue.pistes_enchainees, 1,
        "#5451 : en CoreAudio exclusif, la piste suivante de même format doit \
         être enchaînée dans l'anneau ouvert, pas laissée au sondeur \
         (relâche du « hog » + réouverture du périphérique)"
    );
    assert_eq!(
        reserve.enchainements, 1,
        "le morceau suivant est publié une fois"
    );
    assert_eq!(issue.fin, FinDeChaine::RienEnReserve);
    assert!(issue.http_eof, "la dernière piste a atteint sa fin de flux");
    assert_eq!(issue.trames, 2_000, "la position est celle de la piste B");
    assert_eq!(puits.trames(), 5_000, "l'anneau ouvert a reçu A puis B");
    assert_eq!(
        temoins.position.load(Ordering::SeqCst),
        45,
        "la boucle commune a publié la position DANS la piste B (2 000 trames \
         à 44,1 kHz)"
    );
    assert!(
        temoins.constat.lock().unwrap().is_none(),
        "aucun constat de famine ni de piste tronquée"
    );

    let a_puis_b: Vec<u8> = pcm_a.iter().chain(pcm_b.iter()).copied().collect();
    let temoin = temoin_d_une_seule_piste(&dsp, 44_100, 16, 2, a_puis_b);
    assert_eq!(temoin.mots(), puits.mots());
    assert_eq!(
        puits.empreinte(),
        temoin.empreinte(),
        "A puis B doivent arriver dans l'anneau à l'octet près comme une seule \
         piste A+B"
    );
}

/// 24 bits stéréo 96 kHz — le cas nominal d'une sortie exclusive. L'amorce de
/// B (4 052 octets lus avec l'en-tête) coupe une trame en deux, et la sonde
/// DoP/PCM de 32 trames repart pour B : rien ne doit être perdu ni décalé.
#[test]
fn coreaudio_24_bits_a_puis_b_a_l_octet_pres_5451() {
    let dsp = DspAuRepos::neuf();
    let temoins = Temoins::neufs();
    let pcm_a = pcm(6 * 3_000, 3);
    let pcm_b = pcm(6 * 2_500, 4);
    assert_ne!((4096 - 44) % 6, 0, "l'amorce de B coupe une trame");

    let (issue, puits, reserve, _) = jouer_a_puis_la_reserve(
        &dsp,
        &temoins,
        (96_000, 24, 2),
        wav(96_000, 24, 2, &pcm_a),
        vec![wav(96_000, 24, 2, &pcm_b)],
    );

    assert_eq!(issue.pistes_enchainees, 1);
    assert_eq!(reserve.enchainements, 1);
    assert_eq!(issue.fin, FinDeChaine::RienEnReserve);
    assert_eq!(issue.trames, 2_500);
    assert_eq!(puits.trames(), 5_500);

    let a_puis_b: Vec<u8> = pcm_a.iter().chain(pcm_b.iter()).copied().collect();
    let temoin = temoin_d_une_seule_piste(&dsp, 96_000, 24, 2, a_puis_b);
    assert_eq!(temoin.mots(), puits.mots());
    assert_eq!(puits.empreinte(), temoin.empreinte());
}

/// Trois pistes de même format : la chaîne ne s'arrête pas à la deuxième.
#[test]
fn coreaudio_la_chaine_continue_tant_que_le_format_ne_change_pas_5451() {
    let dsp = DspAuRepos::neuf();
    let temoins = Temoins::neufs();
    let (issue, puits, reserve, _) = jouer_a_puis_la_reserve(
        &dsp,
        &temoins,
        (88_200, 24, 2),
        wav(88_200, 24, 2, &pcm(6 * 700, 1)),
        vec![
            wav(88_200, 24, 2, &pcm(6 * 600, 2)),
            wav(88_200, 24, 2, &pcm(6 * 500, 3)),
        ],
    );
    assert_eq!(issue.pistes_enchainees, 2);
    assert_eq!(reserve.enchainements, 2);
    assert_eq!(issue.fin, FinDeChaine::RienEnReserve);
    assert_eq!(puits.trames(), 1_800);
    assert_eq!(issue.trames, 500);
}

/// La contrepartie : à format différent — cadence, profondeur OU canaux — la
/// suivante n'entre PAS dans l'anneau ouvert. La chaîne rend la main en le
/// disant ; la fin naturelle rouvrira le périphérique au nouveau format,
/// comme avant #5451.
#[test]
fn coreaudio_un_autre_format_rouvre_le_peripherique_5451() {
    let ouvert = (44_100, 16, 2);
    let cas = [
        ("cadence", wav(48_000, 16, 2, &pcm(4 * 1_000, 2))),
        ("profondeur", wav(44_100, 24, 2, &pcm(6 * 1_000, 2))),
        ("canaux", wav(44_100, 16, 1, &pcm(2 * 1_000, 2))),
    ];
    for (ce_qui_change, suivante) in cas {
        let dsp = DspAuRepos::neuf();
        let temoins = Temoins::neufs();
        let (issue, puits, reserve, spec_finale) = jouer_a_puis_la_reserve(
            &dsp,
            &temoins,
            ouvert,
            wav(44_100, 16, 2, &pcm(4 * 1_000, 1)),
            vec![suivante],
        );
        assert_eq!(
            issue.fin,
            FinDeChaine::FormatDifferent,
            "{ce_qui_change} : le périphérique doit être rouvert"
        );
        assert!(
            issue.http_eof,
            "{ce_qui_change} : A s'est terminée normalement"
        );
        assert_eq!(issue.pistes_enchainees, 0, "{ce_qui_change}");
        assert_eq!(
            reserve.enchainements, 0,
            "{ce_qui_change} : A reste publiée"
        );
        assert_eq!(issue.trames, 1_000, "{ce_qui_change}");
        assert_eq!(
            puits.trames(),
            1_000,
            "{ce_qui_change} : rien de B dans l'anneau ouvert"
        );
        assert_eq!(
            spec_finale,
            spec(44_100, ProfondeurPcm::Entier16, 2),
            "{ce_qui_change} : l'étage n'a pas été touché"
        );
    }
}

/// Un arrêt pendant la piste enchaînée : la boucle commune le voit, la chaîne
/// s'arrête SANS fin naturelle (on n'avance pas la file sur un arrêt).
#[test]
fn coreaudio_un_arret_pendant_la_piste_enchainee_n_est_pas_une_fin_naturelle_5451() {
    let dsp = DspAuRepos::neuf();
    let temoins = Temoins::neufs();
    let mut puits = CaptureOutput::ouvert(FormatOuvert::new(44_100, 2));
    let mut e = etage_du_bras(&dsp, 44_100, 16, 2);
    let trames_a = jouer_la_piste_initiale(
        &temoins,
        &mut e,
        &mut puits,
        wav(44_100, 16, 2, &pcm(4 * 500, 1)),
    );
    let mut reserve = ReserveFactice::default();
    reserve
        .reserve
        .push_back(wav(44_100, 16, 2, &pcm(4 * 5_000, 2)));

    // `stop()` tombe au moment où le flux de B est confié à la boucle.
    let issue = poursuivre_la_chaine_par_la_boucle(
        &mut reserve,
        &mut e,
        &mut puits as &mut dyn PuitsDEchantillons,
        &temoins.boucle(RoleDeLaBoucle::PisteEnchainee),
        &mut [0; 65536],
        |lecteur| {
            temoins.arret.store(true, Ordering::SeqCst);
            lecteur
        },
        trames_a,
    );

    assert_eq!(issue.fin, FinDeChaine::Interrompue);
    assert!(!issue.http_eof, "un arrêt n'est pas une fin naturelle");
    assert_eq!(issue.pistes_enchainees, 1);
}

// ─── Le branchement dans le bras ────────────────────────────────────────────

fn compact(src: &str) -> String {
    src.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Le bras CoreAudio n'est compilé que par la porte macOS de la CI : cette
/// garde dit, sur Linux, qu'il appelle bien la poursuite à la fin de flux
/// avec la réserve HTTP, qu'il lève `chain_exhausted` en fin de chaîne, et
/// que `play_url` lui confie la réserve.
#[test]
fn branchement_5451_le_bras_coreaudio_enchaine_a_la_fin_de_flux() {
    let bras = compact(include_str!("bras_coreaudio.rs"));
    let chaine = bras
        .find("ifhttp_eof_excl{letproducteur_enchaine=BoucleProducteur{role:RoleDeLaBoucle::PisteEnchainee,")
        .expect("#5451 — la chaîne part de la fin de flux de la piste initiale");
    let appel = bras[chaine..]
        .find("poursuivre_la_chaine_par_la_boucle(&mutreserve,&mutetage,&mut*puits,&producteur_enchaine,")
        .expect("#5451 — le bras poursuit la chaîne avec SON étage et SON puits");
    assert!(
        bras[chaine..chaine + appel].contains("letmutreserve=ReserveHttp{"),
        "la réserve est celle de `set_next_media` (ReserveHttp)"
    );
    assert!(
        bras.contains("http_eof_excl=issue.http_eof;"),
        "la fin naturelle suit la DERNIÈRE piste de la chaîne"
    );
    assert_eq!(
        bras.matches("chain_exhausted.store(true,Ordering::SeqCst);")
            .count(),
        1,
        "la fin de chaîne se déclare au sondeur (#1919)"
    );
    assert!(
        !bras.contains("report_incomplete_local_pcm_probe("),
        "le geste de fin de flux de chaque piste appartient à la poursuite \
         (`finir_la_piste`), pas au bras"
    );

    let local = compact(include_str!("../local.rs"));
    let appel = local
        .find("bras_coreaudio::jouer_via_coreaudio(bras_coreaudio::EntreesCoreAudio{")
        .expect("l'appel du bras CoreAudio");
    let fin = local[appel..].find("});").expect("fin de l'appel") + appel;
    assert!(
        local[appel..fin]
            .contains("next_media:next_media_ref,chain_exhausted:chain_exhausted_ref,"),
        "play_url confie la réserve au bras CoreAudio"
    );
}
