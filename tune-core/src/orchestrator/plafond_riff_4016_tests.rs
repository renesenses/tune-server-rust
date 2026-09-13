//! #4016 — « FLAC 24/384 de 46 min : la lecture ne démarre jamais »
//! (Cyrille Moutia, Mac mini 2018, fil 1772).
//!
//! Deux hypothèses étaient en présence, et une seule pouvait être vraie : la
//! **cadence** — 384 kHz est le haut du spectre de ce que Tune gère — ou la
//! **durée**. Ces épreuves les départagent PAR LA MESURE, pas par le
//! raisonnement, et fixent ensuite le routage qui en découle.
//!
//! Le verdict tient en trois lignes, et il est dans
//! `le_plafond_est_un_volume_pas_une_cadence` : le même 384 kHz encode sans
//! broncher quand la piste est courte, la même durée de 46 min passe sans
//! broncher en 16/44,1, et 16/44,1 — la cadence la plus ordinaire qui soit —
//! bute à son tour dès qu'on l'étire. Ce n'est ni la cadence ni la durée
//! prises seules : c'est leur PRODUIT, le volume d'octets de PCM, face au
//! champ `data_size` d'un en-tête RIFF, qui est un `u32` par construction du
//! format WAV.

use super::use_file_transcode_for;
use crate::audio::encoder::AudioEncoder;
use crate::audio::wav::{PLAFOND_DATA_RIFF, octets_pcm_attendus, pcm_depasse_le_plafond_riff};

/// La piste de Cyrille : 46 minutes.
const DUREE_CYRILLE_MS: u64 = 46 * 60 * 1000;

/// La cadence accusée : 24 bits, 2 canaux, 384 kHz.
const SR_384: u32 = 384_000;
const CANAUX: u16 = 2;
const BD_24: u16 = 24;

/// **Mesure 1 — la cadence seule n'est pour rien dans l'échec.**
///
/// Le message journalisé chez Cyrille (« wav: pcm exceeds 4 GiB ») nomme un
/// encodage WAV en 24/384. Si 384 kHz était la cause, l'encodeur devrait
/// échouer sur cette cadence quelle que soit la longueur. Une seconde d'audio
/// à la cadence EXACTE du fichier incriminé — 2 304 000 octets — s'encode et
/// rend un en-tête RIFF juste.
#[test]
fn la_cadence_384k_seule_nempeche_aucun_encodage() {
    let octets_une_seconde = (SR_384 as usize) * (CANAUX as usize) * (BD_24 as usize / 8);
    assert_eq!(octets_une_seconde, 2_304_000);

    let mut encodeur = AudioEncoder::new("wav", SR_384, BD_24 as u32, CANAUX as u32);
    encodeur.start_sync().expect("start");
    encodeur
        .write_sync(&vec![0u8; octets_une_seconde])
        .expect("write");
    let wav = encodeur.finish_sync().expect("24/384 doit s'encoder");

    assert_eq!(&wav[0..4], b"RIFF");
    assert_eq!(
        u32::from_le_bytes(wav[40..44].try_into().unwrap()),
        octets_une_seconde as u32,
        "le data_size doit décrire exactement le PCM fourni"
    );
    assert_eq!(wav.len(), 44 + octets_une_seconde);
}

/// **Mesure 2 — le verdict : un VOLUME, pas une cadence.**
///
/// Trois points suffisent à trancher :
///
/// | durée | cadence | dépasse ? |
/// |---|---|---|
/// | 46 min | 24/384 | **oui** — le cas de Cyrille |
/// | 46 min | 16/44,1 | non — même durée, cadence ordinaire |
/// | 7 h | 16/44,1 | **oui** — cadence ordinaire, durée étirée |
///
/// La deuxième ligne réfute « c'est la durée ». La troisième réfute « c'est le
/// 384 kHz ». Seul leur produit explique les trois.
#[test]
fn le_plafond_est_un_volume_pas_une_cadence() {
    assert!(
        pcm_depasse_le_plafond_riff(DUREE_CYRILLE_MS, SR_384, CANAUX, BD_24),
        "46 min en 24/384 doivent dépasser : c'est le fichier du ticket"
    );
    assert!(
        !pcm_depasse_le_plafond_riff(DUREE_CYRILLE_MS, 44_100, 2, 16),
        "la MÊME durée en 16/44,1 passe : ce n'est donc pas la durée"
    );
    assert!(
        pcm_depasse_le_plafond_riff(7 * 3_600_000, 44_100, 2, 16),
        "16/44,1 bute à son tour quand on l'étire : ce n'est donc pas la cadence"
    );

    // Et le chiffre du ticket, à l'octet près.
    assert_eq!(
        octets_pcm_attendus(DUREE_CYRILLE_MS, SR_384, CANAUX, BD_24),
        6_359_040_000,
        "2 760 s x 2 304 000 o/s"
    );
    assert_eq!(PLAFOND_DATA_RIFF, 4_294_967_295);
}

/// **Mesure 3 — où tombe exactement le seuil en 24/384.**
///
/// 4 294 967 295 / 2 304 000 = 1 864,1 s. La seconde 1 864 passe, la 1 865
/// ne passe pas : 31 min 04 s. Le ticket l'avait calculé ; ceci le mesure sur
/// la fonction qui décide réellement.
#[test]
fn le_seuil_en_24_384_tombe_a_31_min_04_s() {
    assert_eq!(
        octets_pcm_attendus(1_864_000, SR_384, CANAUX, BD_24),
        4_294_656_000
    );
    assert!(!pcm_depasse_le_plafond_riff(
        1_864_000, SR_384, CANAUX, BD_24
    ));
    assert!(pcm_depasse_le_plafond_riff(
        1_865_000, SR_384, CANAUX, BD_24
    ));
    assert_eq!(1_864 / 60, 31);
    assert_eq!(1_864 % 60, 4);
}

/// **Le correctif — une piste hors plafond ne part plus par le fichier.**
///
/// C'est le témoin ROUGE avant, VERT après : la règle de routage renvoyait un
/// WAV pour renderer LPCM au bras « fichier temporaire », qui écrit un WAV
/// COMPLET et ne peut donc pas exister ici. Elle le renvoie désormais à la
/// session progressive — la même sortie qu'un `.ape` ou qu'un DSD→LPCM
/// empruntent déjà vers un renderer réseau.
#[test]
fn un_wav_hors_plafond_riff_ne_part_plus_par_le_fichier_temporaire() {
    let hors_plafond = pcm_depasse_le_plafond_riff(DUREE_CYRILLE_MS, SR_384, CANAUX, BD_24);
    assert!(hors_plafond, "prérequis de l'épreuve");
    assert!(
        !use_file_transcode_for(
            /* is_network */ true,
            /* target_is_wav */ true,
            /* dlna_needs_wav */ true,
            /* wav_diffusable */ false,
            /* wav_hors_plafond_riff */ hors_plafond,
            /* dsp_active */ false,
        ),
        "un WAV de 5,9 GiB ne peut pas s'écrire en fichier : il doit diffuser"
    );
}

/// **Contre-épreuve — en deçà du plafond, RIEN ne bouge.**
///
/// Le détournement doit être aussi étroit que le défaut. Une piste de 30 min en
/// 24/384, et la piste de 46 min de Cyrille servie en 16/44,1, restent l'une et
/// l'autre sur le bras fichier, exactement comme avant.
#[test]
fn en_deca_du_plafond_le_bras_fichier_ne_bouge_pas() {
    for (duree_ms, sr, ch, bd, quoi) in [
        (30 * 60 * 1000u64, SR_384, CANAUX, BD_24, "30 min en 24/384"),
        (DUREE_CYRILLE_MS, 44_100, 2u16, 16u16, "46 min en 16/44,1"),
    ] {
        let hors_plafond = pcm_depasse_le_plafond_riff(duree_ms, sr, ch, bd);
        assert!(!hors_plafond, "{quoi} tient dans un en-tête RIFF");
        assert!(
            use_file_transcode_for(true, true, true, false, hors_plafond, false),
            "{quoi} doit rester sur le bras fichier"
        );
    }
}

/// **Contre-épreuve — le détournement ne touche que la cible WAV.**
///
/// Une cible FLAC est encodée EN FLUX par l'encodeur et n'a aucun champ de
/// taille en 32 bits : elle n'a rien à faire ici. Même armé de force, le
/// drapeau ne doit pas la sortir du bras fichier.
#[test]
fn le_plafond_riff_ne_detourne_pas_une_cible_flac() {
    assert!(use_file_transcode_for(
        true, false, false, false, false, false
    ));
    assert!(use_file_transcode_for(
        true, false, false, false, true, false
    ));
    assert!(use_file_transcode_for(
        true, false, false, false, true, true
    ));
}

/// **Contre-épreuve — hors sortie réseau, la règle est inchangée.**
///
/// Le bras fichier exige déjà une sortie réseau (ou un traitement actif sur une
/// cible non WAV). Le nouveau drapeau ne doit rien ouvrir de ce côté.
#[test]
fn hors_sortie_reseau_le_nouveau_drapeau_ne_change_rien() {
    for hors_plafond in [false, true] {
        assert_eq!(
            use_file_transcode_for(false, true, true, false, hors_plafond, false),
            use_file_transcode_for(false, true, true, false, false, false),
        );
        assert_eq!(
            use_file_transcode_for(false, true, true, false, hors_plafond, true),
            use_file_transcode_for(false, true, true, false, false, true),
        );
    }
}

/// **La formule n'a qu'une seule copie.**
///
/// `StreamInfo::wav_content_length` — ce qui est annoncé en `Content-Length` et
/// dans l'attribut `size` de la DIDL — doit décrire le MÊME nombre d'octets que
/// celui qui décide du routage, en-tête de 44 octets en plus. Deux copies de
/// cette arithmétique auraient divergé (feedback « deux copies périmées »).
#[test]
fn la_longueur_annoncee_et_la_decision_comptent_les_memes_octets() {
    let info = crate::http::streamer::StreamInfo {
        sample_rate: SR_384,
        bit_depth: BD_24,
        channels: CANAUX,
        duration_ms: Some(DUREE_CYRILLE_MS),
        ..Default::default()
    };
    assert_eq!(
        info.wav_content_length(),
        Some(44 + octets_pcm_attendus(DUREE_CYRILLE_MS, SR_384, CANAUX, BD_24)),
    );
    // Et elle tient en `u64` : c'est le champ RIFF qui est étroit, pas la
    // longueur HTTP.
    assert!(info.wav_content_length().unwrap() > PLAFOND_DATA_RIFF);
}
