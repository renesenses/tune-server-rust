use super::*;

const V0: &str = "SP:0AAAQEAYEAUDAOCAJBIFQYDIOB4IBCEQTCQKRMFYYDENBWHA5DYP6BYPC4PSOLZXH5DU6V97M5XXO74HR6LZ7J5PW674PT6X37T6757Y";
const V1: &str = "SP:14DQ6FY7E4XTOP9HJ5LV6Z3PO57YPD4XT6T97N5Y";

#[test]
fn i3326_jeton_vecteurs_publies_et_saisies_tolerantes() {
    for jeton in [V0, V1] {
        let version = jeton.as_bytes()[3];
        let taille = if version == b'0' { 64 } else { 24 };
        let attendu = if version == b'0' {
            (0..32).chain(0xe0..=0xff).collect::<Vec<u8>>()
        } else {
            (0xe0..=0xf7).collect()
        };
        for saisie in [
            jeton.to_owned(),
            format!(" \t{}\r\n", jeton.to_ascii_lowercase()),
            jeton[3..].to_owned(),
            jeton.replace('9', "2"),
        ] {
            assert_eq!(&*decoder(&saisie, version, taille).unwrap(), &attendu);
        }
    }
}

#[test]
fn i3326_jeton_identite_et_psk_sont_liees_au_transport() {
    let j = JetonPsk::lire(V0).unwrap();
    let id = b64url(&(0..32).collect::<Vec<u8>>());
    assert_eq!(j.client_id(), id);
    let p = j.pour_pair(&id).unwrap();
    assert_eq!(p.categorie(), CategoriePsk::Appairage);
    assert_eq!(p.secret().as_slice(), &(0xe0..=0xff).collect::<Vec<u8>>());
    let autre = super::super::Identite::depuis_prive([0x56; 32]).id();
    assert!(
        JetonPsk::lire(V0).unwrap().pour_pair(&autre).is_err(),
        "le secret du jeton ne doit jamais etre utilisable pour un autre client"
    );
}

#[test]
fn i3326_jeton_refuse_les_versions_et_charges_malformees() {
    for jeton in [
        "",
        "SP:",
        "SP:2AAAA",
        "SP:0A",
        "SP:0!!!!",
        "SP:0AAAA AAAA",
        "SP:0AAAA-AAAA",
        "SP:0é",
        "SP:0ＡＡＡＡ",
        "SP:000000000",
    ] {
        assert!(JetonPsk::lire(jeton).is_err(), "jeton malforme accepte");
    }
    assert!(JetonPsk::lire(V1).is_err());
    assert!(lire_code(V0, FormatCode::Qr).is_err());
    assert!(JetonPsk::lire(&format!("{V0}!")).is_err());
    assert!(lire_code(&format!("{V1}!"), FormatCode::Qr).is_err());
}

#[test]
fn i3326_jeton_extensions_et_troncatures_concordent_avec_python() {
    let cas: serde_json::Value = serde_json::from_str(include_str!("reference.json")).unwrap();
    for v in cas.as_array().unwrap() {
        let version = v["version"].as_u64().unwrap() as u8 + b'0';
        let taille = if version == b'0' { 64 } else { 24 };
        let r = decoder(v["token"].as_str().unwrap(), version, taille);
        if v["valid"].as_bool().unwrap() {
            let attendu: Vec<u8> = v["payload"]
                .as_array()
                .unwrap()
                .iter()
                .map(|b| b.as_u64().unwrap() as u8)
                .collect();
            assert_eq!(
                r.unwrap().as_slice(),
                attendu,
                "les octets d'extension sont reserves"
            );
        } else {
            assert!(r.is_err(), "un jeton tronque ne fournit aucun secret");
        }
    }
}

#[test]
fn i3326_jeton_refuse_sentinelle_et_identite_de_faible_ordre() {
    let encoder = |octets: &[u8]| {
        format!(
            "SP:0{}",
            data_encoding::BASE32_NOPAD.encode(octets).replace('2', "9")
        )
    };
    let mut octets = vec![0; 64];
    octets[32..].fill(0x44);
    assert!(JetonPsk::lire(&encoder(&octets)).is_err());
    let identite = super::super::Identite::depuis_prive([0x56; 32]);
    octets[..32].copy_from_slice(identite.public());
    octets[32..].copy_from_slice(&super::super::psk::sentinelle());
    assert!(
        JetonPsk::lire(&encoder(&octets))
            .unwrap()
            .pour_pair(&identite.id())
            .is_err()
    );
}

#[test]
fn i3326_jeton_debug_et_erreurs_ne_revelent_pas_la_saisie() {
    let j = JetonPsk::lire(V0).unwrap();
    let debug = format!("{j:?}");
    assert!(!debug.contains(V0));
    assert!(!debug.contains(&format!("{:?}", j.secret)));
    let e = JetonPsk::lire(&format!("{V0}!")).unwrap_err();
    assert!(!format!("{e:?} {e}").contains(V0));
}
