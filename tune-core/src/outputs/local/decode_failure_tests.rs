use super::{CompressedDecodeFailure, decode_compressed_stream, record_compressed_decode_failure};

const TOUS: [CompressedDecodeFailure; 4] = [
    CompressedDecodeFailure::ContainerUnrecognised,
    CompressedDecodeFailure::NoAudioTrack,
    CompressedDecodeFailure::CodecUnsupported,
    CompressedDecodeFailure::NoSamplesDecoded,
];

/// Le motif doit être NOMMÉ : quatre causes, quatre phrases distinctes.
/// Un message unique pour les quatre ne dirait rien de plus que le silence
/// qu'il remplace.
#[test]
fn chaque_cause_a_sa_phrase_et_son_evenement() {
    let mut phrases = std::collections::BTreeSet::new();
    let mut evenements = std::collections::BTreeSet::new();
    for cause in TOUS {
        let m = cause.user_message("DAC USB");
        assert!(m.contains("DAC USB"), "sortie absente de {cause:?} : {m}");
        assert!(
            m.contains("décoder"),
            "la cause doit être nommée dans {cause:?} : {m}"
        );
        phrases.insert(m);
        evenements.insert(cause.log_event());
    }
    assert_eq!(phrases.len(), 4, "deux causes rendent la même phrase");
    assert_eq!(evenements.len(), 4, "deux causes rendent le même événement");
}

/// Le canal est celui du sondeur : `take_output_failure()` le draine, une
/// seule fois. C'est la contre-épreuve du `return` nu.
#[test]
fn l_echec_de_decodage_passe_par_le_canal_que_le_sondeur_draine() {
    use super::super::traits::OutputTarget;
    let sortie = super::LocalOutput::new("DAC USB".into());
    assert!(
        sortie.take_output_failure().is_none(),
        "TÉMOIN VERT : une sortie saine ne remonte rien"
    );

    record_compressed_decode_failure(
        CompressedDecodeFailure::ContainerUnrecognised,
        "DAC USB",
        &sortie.open_failure,
    );
    let remonte = sortie
        .take_output_failure()
        .expect("l'échec de décodage doit remonter par le canal du sondeur");
    assert!(remonte.contains("DAC USB"), "got: {remonte}");
    assert!(
        sortie.take_output_failure().is_none(),
        "un échec ne doit jamais être remonté deux fois"
    );
}

/// La production, pas une maquette : des octets qui ne sont ni FLAC ni MP3
/// ni AAC doivent rendre le motif « conteneur non reconnu », et non un
/// `None` muet.
#[test]
fn un_flux_illisible_rend_le_motif_conteneur_non_reconnu() {
    let poubelle = vec![0x42u8; 8192];
    assert_eq!(
        decode_compressed_stream(&poubelle),
        Err(CompressedDecodeFailure::ContainerUnrecognised)
    );
}

/// TÉMOIN VERT : un flux vide ne doit pas, lui non plus, rendre `Ok`.
#[test]
fn un_flux_vide_ne_rend_jamais_un_succes() {
    assert!(decode_compressed_stream(&[]).is_err());
}
