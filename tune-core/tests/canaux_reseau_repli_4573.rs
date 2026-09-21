//! #4573 — un 5.1 servi à un renderer qui n'annonce que deux canaux part-il
//! VRAIMENT en stéréo ?
//!
//! La .159 avait livré la lecture de la déclaration (`canaux_annonces_par_le_
//! sink`, `RendererCapabilities.canaux_max`) et la règle, sans les brancher :
//! le commentaire de clôture disait lui-même « écrit, pas branché ». Ce
//! fichier mesure le bras qui manquait — celui qui décode et replie — sur un
//! vrai fichier à six voies, et non sur un tampon fabriqué en mémoire.
//!
//! Le chemin exercé est exactement celui de la lecture réseau :
//! `DecisionLocale.channels` → `transcode_source_to_file` /
//! `decode_to_pcm_streaming_tranche` → `decode_to_pcm(…, Some(channels), …)`
//! → `adapt_channels_i32` → matrice ITU-R BS.775.
//!
//! ⚠️ `audio::mixer::downmix` n'a aucun appelant en production : le mélange
//! vit dans `adapt_channels_{f32,i32}`. C'est cette porte-là qui est mesurée.

use std::io::Write;

/// Écrit un WAV PCM 24 bits entrelacé de `canaux` voies, une voie par
/// « position » : la voie `k` porte une constante, les autres sont à zéro.
/// Un fichier par voie, donc, dont on lit ensuite ce que le repli en fait.
fn wav_une_seule_voie(dossier: &std::path::Path, canaux: u16, voie_active: usize) -> String {
    let sample_rate = 48_000u32;
    let bits = 24u16;
    let trames = 480usize; // 10 ms — assez pour être décodé, assez court pour être lu.
    let bloc = canaux * bits / 8;
    let taille_donnees = trames * bloc as usize;

    let mut donnees = Vec::with_capacity(taille_donnees);
    // +0,5 pleine échelle en 24 bits, une valeur exacte en binaire : le repli
    // ne peut donc pas être « presque juste » par hasard d'arrondi.
    let valeur: i32 = 1 << 22;
    for _ in 0..trames {
        for canal in 0..canaux as usize {
            let e = if canal == voie_active { valeur } else { 0 };
            donnees.extend_from_slice(&e.to_le_bytes()[..3]);
        }
    }

    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&((36 + taille_donnees) as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM entier
    wav.extend_from_slice(&canaux.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * bloc as u32).to_le_bytes());
    wav.extend_from_slice(&bloc.to_le_bytes());
    wav.extend_from_slice(&bits.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(taille_donnees as u32).to_le_bytes());
    wav.extend_from_slice(&donnees);

    let chemin = dossier.join(format!("voie_{voie_active}_sur_{canaux}.wav"));
    let mut f = std::fs::File::create(&chemin).unwrap();
    f.write_all(&wav).unwrap();
    chemin.to_string_lossy().into_owned()
}

/// La première trame du flux replié, voie par voie.
fn premiere_trame(chemin: &str, cible: u32) -> (u32, Vec<i32>) {
    let pcm = tune_core::audio::decode::decode_to_pcm(chemin, None, Some(cible), 0.0, 0.0)
        .expect("le 5.1 doit se décoder");
    let n = pcm.channels as usize;
    assert!(pcm.samples_i32.len() >= n, "flux replié vide");
    (pcm.channels, pcm.samples_i32[..n].to_vec())
}

/// 🔴 LE témoin du lot : un fichier à SIX voies, une cible à DEUX canaux —
/// le flux qui sort en porte deux, et le contenu est bien celui de BS.775.
///
/// Avant ce lot, rien n'appelait cette cible sur le chemin réseau :
/// `DecisionLocale.channels` valait `track.channels`, donc 6, et le FLAC 5.1
/// partait tel quel (journal de Xavier du 20/09, `dlna_set_uri_ok …
/// advertised_mime=audio/flac`).
#[test]
fn un_51_decode_vers_deux_canaux_sort_bien_en_stereo() {
    let tmp = tempfile::tempdir().unwrap();
    let chemin = wav_une_seule_voie(tmp.path(), 6, 0);
    let (canaux, _) = premiere_trame(&chemin, 2);
    assert_eq!(canaux, 2, "le flux servi doit porter deux voies, pas six");
}

/// Le repli est bien celui d'ITU-R BS.775, et pas une troncature des quatre
/// voies du fond : chaque voie d'entrée se retrouve là où la norme la met.
#[test]
fn le_repli_place_chaque_voie_ou_bs775_la_met() {
    let tmp = tempfile::tempdir().unwrap();
    // 1,0 / 0,707 / 0,707, normalisés par la somme des gains de la ligne
    // (2,414) pour ne pas pouvoir écrêter : voir `build_downmix_matrix`.
    let pleine = 1i32 << 22;

    // Avant gauche (voie 0) : à GAUCHE seulement.
    let (_, t) = premiere_trame(&wav_une_seule_voie(tmp.path(), 6, 0), 2);
    assert!(t[0] > 0, "FL doit sortir à gauche : {t:?}");
    assert_eq!(t[1], 0, "FL ne doit RIEN mettre à droite : {t:?}");

    // Avant droit (voie 1) : à DROITE seulement.
    let (_, t) = premiere_trame(&wav_une_seule_voie(tmp.path(), 6, 1), 2);
    assert_eq!(t[0], 0, "FR ne doit RIEN mettre à gauche : {t:?}");
    assert!(t[1] > 0, "FR doit sortir à droite : {t:?}");

    // Centre (voie 2) : dans les DEUX, à parts égales.
    let (_, centre) = premiere_trame(&wav_une_seule_voie(tmp.path(), 6, 2), 2);
    assert!(
        centre[0] > 0 && centre[0] == centre[1],
        "centre : {centre:?}"
    );

    // 🔴 LFE (voie 3) : nulle part. La norme l'exclut du repli stéréo, et
    // c'est ce qui distingue un vrai BS.775 d'une somme naïve.
    let (_, lfe) = premiere_trame(&wav_une_seule_voie(tmp.path(), 6, 3), 2);
    assert_eq!(lfe, vec![0, 0], "le LFE ne se replie pas : {lfe:?}");

    // Surround gauche (voie 4) : à GAUCHE, et moins fort que l'avant gauche.
    let (_, sl) = premiere_trame(&wav_une_seule_voie(tmp.path(), 6, 4), 2);
    let (_, fl) = premiere_trame(&wav_une_seule_voie(tmp.path(), 6, 0), 2);
    assert!(sl[0] > 0 && sl[1] == 0, "SL doit sortir à gauche : {sl:?}");
    assert!(
        sl[0] < fl[0],
        "le surround entre à 0,707, l'avant à 1,0 : {sl:?} vs {fl:?}"
    );

    // Surround droit (voie 5) : à DROITE, symétrique.
    let (_, sr) = premiere_trame(&wav_une_seule_voie(tmp.path(), 6, 5), 2);
    assert_eq!(sr[1], sl[0], "le repli doit être symétrique : {sr:?}");
    assert_eq!(sr[0], 0, "SR ne doit RIEN mettre à gauche : {sr:?}");

    // Aucune voie seule ne peut écrêter : la matrice est normalisée.
    for voie in 0..6 {
        let (_, t) = premiere_trame(&wav_une_seule_voie(tmp.path(), 6, voie), 2);
        assert!(
            t.iter().all(|e| e.abs() <= pleine),
            "voie {voie} écrête : {t:?}"
        );
    }
}

/// 🔴 La contre-épreuve, à l'octet près : le MÊME fichier décodé SANS cible
/// de canaux garde ses six voies, intactes. Sans elle, le témoin ci-dessus
/// pourrait être vert parce que le décodeur replie tout, tout le temps.
#[test]
fn sans_cible_de_canaux_le_51_garde_ses_six_voies() {
    let tmp = tempfile::tempdir().unwrap();
    let chemin = wav_une_seule_voie(tmp.path(), 6, 4);
    let pcm = tune_core::audio::decode::decode_to_pcm(&chemin, None, None, 0.0, 0.0).unwrap();
    assert_eq!(pcm.channels, 6, "aucune cible : rien ne doit être replié");
    let trame = &pcm.samples_i32[..6];
    assert!(
        trame[4] > 0 && trame.iter().enumerate().all(|(i, e)| i == 4 || *e == 0),
        "le surround gauche doit rester SEUL sur sa voie, sans être mélangé : {trame:?}"
    );
}

/// Le 7.1 est le même dossier : la matrice existe, le flux sort en deux
/// voies. Le Denon de Xavier n'annonce pas plus pour lui.
#[test]
fn un_71_decode_vers_deux_canaux_sort_aussi_en_stereo() {
    let tmp = tempfile::tempdir().unwrap();
    let chemin = wav_une_seule_voie(tmp.path(), 8, 0);
    let (canaux, t) = premiere_trame(&chemin, 2);
    assert_eq!(canaux, 2);
    assert!(t[0] > 0 && t[1] == 0, "FL à gauche seulement : {t:?}");
}
