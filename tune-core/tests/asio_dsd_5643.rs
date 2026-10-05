//! #5643 — DSD natif par ASIO, lots A et B : les fonctions pures.
//!
//! Le chemin ASIO de CPAL ne se compile que sous Windows (et l'ASIO SDK n'existe
//! pas sur Shrek). Ses parties sans pilote sont donc isolées dans deux fichiers
//! autonomes, sans `crate::` ni liaison générée, que ce banc compile TELS QUELS
//! sous Linux par `#[path]` — c'est le même fichier source que le binaire
//! Windows embarque, pas une copie :
//!
//! - `vendor/cpal/src/host/asio/dsd.rs` : types d'échantillon ASIO DSD, ordre
//!   des bits, cadences, conversion octets <-> échantillons ASIO, écriture d'un
//!   canal ;
//! - `vendor/asio-sys/src/bindings/io_format.rs` : sélecteurs `ASIOFuture`,
//!   structure `ASIOIoFormat`, lecture du code de `kAsioCanDoIoFormat`.
//!
//! Ce banc ne prouve RIEN sur un pilote réel : voir vendor/asio-sys/TUNE-PATCH.md.

#[path = "../../vendor/cpal/src/host/asio/dsd.rs"]
mod dsd;

#[path = "../../vendor/asio-sys/src/bindings/io_format.rs"]
mod io_format;

use dsd::DsdLayout;
use io_format::{AsioIoFormat, AsioIoFormatType};

/// Retourne un octet bit à bit, sans `reverse_bits` : la référence ne doit pas
/// être la fonction testée.
fn retourne(octet: u8) -> u8 {
    let mut r = 0u8;
    for i in 0..8 {
        if octet & (1 << i) != 0 {
            r |= 1 << (7 - i);
        }
    }
    r
}

#[test]
fn les_trois_types_dsd_de_l_asio_sdk_sont_reconnus() {
    // Valeurs de `ASIOSampleType` dans asio.h.
    assert_eq!(DsdLayout::from_asio_type(33), Some(DsdLayout::Msb1));
    assert_eq!(DsdLayout::from_asio_type(32), Some(DsdLayout::Lsb1));
    assert_eq!(DsdLayout::from_asio_type(40), Some(DsdLayout::Ner8));
}

#[test]
fn aucun_type_pcm_n_est_pris_pour_du_dsd() {
    let pcm = [
        0, 1, 2, 3, 4, 8, 9, 10, 11, 16, 17, 18, 19, 20, 24, 25, 26, 27,
    ];
    for code in pcm {
        assert_eq!(
            DsdLayout::from_asio_type(code),
            None,
            "le type PCM {code} est lu comme DSD"
        );
    }
    for code in [-1, 5, 31, 34, 39, 41, 1000] {
        assert_eq!(DsdLayout::from_asio_type(code), None, "type {code}");
    }
}

#[test]
fn seuls_les_types_1_bit_portent_du_dsd_u8() {
    assert!(DsdLayout::Msb1.carries_dsd_u8());
    assert!(DsdLayout::Lsb1.carries_dsd_u8());
    assert!(
        !DsdLayout::Ner8.carries_dsd_u8(),
        "NER8 (mots DSD de 8 bits) ne doit pas recevoir de DSD 1 bit empaqueté"
    );
}

#[test]
fn msb1_copie_l_octet_et_lsb1_en_inverse_les_bits() {
    for octet in 0..=255u8 {
        assert_eq!(DsdLayout::Msb1.encode(octet), octet, "MSB1 {octet:#04x}");
        assert_eq!(
            DsdLayout::Lsb1.encode(octet),
            retourne(octet),
            "LSB1 doit retourner les bits de {octet:#04x}"
        );
    }
    assert_eq!(DsdLayout::Lsb1.encode(0x01), 0x80);
    assert_eq!(DsdLayout::Lsb1.encode(0xF0), 0x0F);
}

#[test]
fn le_silence_dsd_suit_l_ordre_des_bits_du_pilote() {
    assert_eq!(dsd::DSD_SILENCE_MSB_FIRST, 0x69);
    assert_eq!(DsdLayout::Msb1.silence(), 0x69);
    assert_eq!(DsdLayout::Lsb1.silence(), 0x96);
}

#[test]
fn seules_les_cadences_dsd64_128_256_sont_ouvertes() {
    for rate in [2_822_400, 5_644_800, 11_289_600] {
        assert!(dsd::is_dsd_rate(rate), "{rate}");
    }
    for rate in [
        0, 44_100, 176_400, 352_800, 384_000, 705_600, 1_411_200, 2_822_401, 3_072_000, 22_579_200,
    ] {
        assert!(!dsd::is_dsd_rate(rate), "{rate} accepté comme cadence DSD");
    }
}

#[test]
fn un_octet_par_canal_vaut_huit_echantillons_asio() {
    assert_eq!(dsd::asio_samples_for_bytes(1), Some(8));
    assert_eq!(dsd::asio_samples_for_bytes(512), Some(4096));
    assert_eq!(dsd::asio_samples_for_bytes(0), None);
    // Le `long` du pilote est sur 32 bits signés.
    assert_eq!(dsd::asio_samples_for_bytes(1 << 28), None);
    assert_eq!(
        dsd::asio_samples_for_bytes((1 << 28) - 1),
        Some(i32::MAX - 7)
    );
    assert_eq!(dsd::asio_samples_for_bytes(u32::MAX), None);

    assert_eq!(dsd::bytes_for_asio_samples(8), Some(1));
    assert_eq!(dsd::bytes_for_asio_samples(4096), Some(512));
    assert_eq!(dsd::bytes_for_asio_samples(0), None);
    assert_eq!(dsd::bytes_for_asio_samples(-8), None);
    for pas_entier in [1, 7, 12, 4097] {
        assert_eq!(
            dsd::bytes_for_asio_samples(pas_entier),
            None,
            "{pas_entier} échantillons ne font pas un nombre entier d'octets"
        );
    }
    for octets in [1u32, 64, 512, 2048, 16_384] {
        let samples = dsd::asio_samples_for_bytes(octets).unwrap();
        assert_eq!(dsd::bytes_for_asio_samples(samples), Some(octets as usize));
    }
}

#[test]
fn write_channel_desentrelace_un_canal_et_convertit_l_ordre_des_bits() {
    // Stéréo, 4 trames : L0 R0 L1 R1 L2 R2 L3 R3.
    let entrelace = [0x01, 0x10, 0x02, 0x20, 0x03, 0x30, 0x69, 0xF0];

    let mut gauche = [0u8; 4];
    assert_eq!(
        dsd::write_channel(&entrelace, 2, 0, DsdLayout::Msb1, &mut gauche),
        4
    );
    assert_eq!(gauche, [0x01, 0x02, 0x03, 0x69]);

    let mut droite = [0u8; 4];
    assert_eq!(
        dsd::write_channel(&entrelace, 2, 1, DsdLayout::Msb1, &mut droite),
        4
    );
    assert_eq!(droite, [0x10, 0x20, 0x30, 0xF0]);

    let mut droite_lsb = [0u8; 4];
    dsd::write_channel(&entrelace, 2, 1, DsdLayout::Lsb1, &mut droite_lsb);
    assert_eq!(droite_lsb, [0x08, 0x04, 0x0C, 0x0F]);
}

#[test]
fn write_channel_ne_deborde_jamais() {
    let entrelace = [1u8, 2, 3, 4, 5, 6];
    // Tampon ASIO plus court que la donnée : on s'arrête à sa taille.
    let mut court = [0u8; 2];
    assert_eq!(
        dsd::write_channel(&entrelace, 2, 0, DsdLayout::Msb1, &mut court),
        2
    );
    assert_eq!(court, [1, 3]);
    // Tampon plus long : la fin n'est pas touchée.
    let mut long = [0xAAu8; 5];
    assert_eq!(
        dsd::write_channel(&entrelace, 2, 1, DsdLayout::Msb1, &mut long),
        3
    );
    assert_eq!(long, [2, 4, 6, 0xAA, 0xAA]);
    // Canal hors de la trame, ou zéro canal : rien n'est écrit.
    let mut rien = [0xAAu8; 3];
    assert_eq!(
        dsd::write_channel(&entrelace, 2, 2, DsdLayout::Msb1, &mut rien),
        0
    );
    assert_eq!(
        dsd::write_channel(&entrelace, 0, 0, DsdLayout::Msb1, &mut rien),
        0
    );
    assert_eq!(rien, [0xAA; 3]);
}

#[test]
fn les_selecteurs_asio_future_sont_ceux_du_sdk() {
    assert_eq!(io_format::K_ASIO_SET_IO_FORMAT, 0x23111961);
    assert_eq!(io_format::K_ASIO_GET_IO_FORMAT, 0x23111983);
    assert_eq!(io_format::K_ASIO_CAN_DO_IO_FORMAT, 0x23112004);
    assert_eq!(io_format::ASE_SUCCESS, 0x3f4847a0);
}

#[test]
fn asio_io_format_fait_512_octets_comme_dans_asio_h() {
    assert_eq!(std::mem::size_of::<AsioIoFormat>(), 512);
    assert_eq!(std::mem::align_of::<AsioIoFormat>(), 4);

    let dsd = AsioIoFormat::new(AsioIoFormatType::Dsd);
    assert_eq!(dsd.format_type, 1);
    assert!(dsd.future.iter().all(|&b| b == 0));
    assert_eq!(AsioIoFormat::new(AsioIoFormatType::Pcm).format_type, 0);
    // Un pilote muet sur kAsioGetIoFormat ne doit pas se lire « PCM ».
    assert_eq!(AsioIoFormat::invalid().format_type, -1);
    assert_eq!(
        AsioIoFormatType::from_raw(AsioIoFormat::invalid().format_type),
        None
    );
}

#[test]
fn le_type_de_format_fait_l_aller_retour() {
    for t in [AsioIoFormatType::Pcm, AsioIoFormatType::Dsd] {
        assert_eq!(AsioIoFormatType::from_raw(t.raw()), Some(t));
    }
    assert_eq!(AsioIoFormatType::from_raw(2), None);
}

#[test]
fn la_reponse_de_kasio_can_do_io_format_est_lue_sans_contresens() {
    assert_eq!(io_format::can_do_from_code(0x3f4847a0), Some(true));
    assert_eq!(io_format::can_do_from_code(0), Some(true));
    // Pilote qui refuse le format, ou qui ignore le sélecteur (ASIO 2.0).
    assert_eq!(io_format::can_do_from_code(-1000), Some(false));
    assert_eq!(io_format::can_do_from_code(-998), Some(false));
    // Toute autre réponse est une erreur, pas un « non ».
    for code in [-999, -997, -996, -995, -994, 1, 0x3f4847a1] {
        assert_eq!(io_format::can_do_from_code(code), None, "code {code}");
    }
}
