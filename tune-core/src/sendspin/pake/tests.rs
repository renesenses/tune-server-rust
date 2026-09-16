use super::*;
use serde_json::Value;

fn octets(v: &Value, nom: &str) -> Vec<u8> {
    let s = v[nom].as_str().unwrap();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}
fn tableau<const N: usize>(v: &Value, nom: &str) -> [u8; N] {
    octets(v, nom).try_into().unwrap()
}
fn vecteurs(fichier: &str) -> Value {
    serde_json::from_str(fichier).unwrap()
}
fn flows() -> Vec<Value> {
    vecteurs(include_str!("flow-vectors.json"))
        .as_array()
        .unwrap()
        .clone()
}
fn format(v: &Value) -> FormatCode {
    match v["format"].as_str().unwrap() {
        "static" => FormatCode::Statique,
        "digits" => FormatCode::Dynamique,
        "qr" => FormatCode::Qr,
        _ => panic!(),
    }
}
fn suite(v: &Value) -> Suite {
    if v["suite"] == "aes" {
        Suite::AesGcm
    } else {
        Suite::ChaChaPoly
    }
}
fn serveur(v: &Value) -> PakeServeur {
    let prs = octets(v, "prs");
    let code = CodeAppairage::nouveau(format(v), &prs).unwrap();
    let contexte = ContextePake::nouveau(
        tableau(v, "h"),
        v["index"].as_u64().unwrap() as u32,
        v["tour"].as_u64().unwrap() as u32,
    )
    .unwrap();
    let liaison = (format(v) != FormatCode::Statique).then(|| LiaisonDynamique {
        nonce_a: tableau(v, "nonce_a"),
        commit_b: tableau(v, "commit_b"),
    });
    // Injection du scalaire connue uniquement dans ce module cfg(test).
    // Le constructeur public conserve son tirage CSPRNG.
    let echange = Echange::nouveau(
        &prs,
        b"",
        contexte.sid(),
        b"server",
        b"client",
        StaticSecret::from(tableau::<32>(v, "sa")),
    )
    .unwrap();
    PakeServeur {
        echange,
        contexte,
        code,
        liaison,
        suite: suite(v),
    }
}
fn nonce(v: &Value) -> Option<Vec<u8>> {
    (format(v) != FormatCode::Statique).then(|| octets(v, "wrapped_nonce"))
}

#[test]
fn i3326_pake_reproduit_le_vecteur_du_brouillon_21() {
    let v = &vecteurs(include_str!("draft-vectors.json"))["G_25519"];
    let g = generateur(&octets(v, "PRS"), &octets(v, "CI"), &octets(v, "sid")).unwrap();
    assert_eq!(
        g.as_slice(),
        octets(v, "g"),
        "generateur CPace du brouillon"
    );
    let secret = StaticSecret::from(tableau::<32>(v, "ya"));
    assert_eq!(multiplier(&secret, &g).unwrap().as_slice(), octets(v, "Ya"));
    assert_eq!(
        multiplier(&secret, &tableau(v, "Yb")).unwrap().as_slice(),
        octets(v, "K")
    );
    let e = Echange::nouveau(
        &octets(v, "PRS"),
        &octets(v, "CI"),
        octets(v, "sid"),
        &octets(v, "ADa"),
        &octets(v, "ADb"),
        secret,
    )
    .unwrap();
    let c = e.recevoir(&octets(v, "Yb")).unwrap();
    assert_eq!(
        c.isk.as_slice(),
        octets(v, "ISK_IR"),
        "ISK initiateur-repondeur du brouillon"
    );
}

#[test]
fn i3326_pake_concorde_avec_129_generateurs_independants() {
    for (i, v) in vecteurs(include_str!("generator-vectors.json"))
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let g = generateur(&octets(v, "PRS"), &octets(v, "CI"), &octets(v, "sid")).unwrap();
        assert_eq!(
            g.as_slice(),
            octets(v, "g"),
            "generateur de reference {i} : CPace ne masque que le bit 255"
        );
    }
}

#[test]
fn i3326_pake_concorde_avec_64_echanges_et_confirmations_independants() {
    for (i, v) in vecteurs(include_str!("reference-vectors.json"))
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let e = Echange::nouveau(
            &octets(v, "prs"),
            &octets(v, "ci"),
            octets(v, "sid"),
            &octets(v, "ada"),
            &octets(v, "adb"),
            StaticSecret::from(tableau::<32>(v, "sa")),
        )
        .unwrap();
        assert_eq!(e.ya.as_slice(), octets(v, "ya"), "Ya reference {i}");
        let c = e.recevoir(&octets(v, "yb")).unwrap();
        assert_eq!(c.isk.as_slice(), octets(v, "isk"), "ISK reference {i}");
        assert_eq!(
            c.tag_serveur().as_slice(),
            octets(v, "ta"),
            "Ta reference {i}"
        );
        c.verifier(&octets(v, "tb"))
            .expect("Tb produit par la reference doit verifier");
    }
}

#[test]
fn i3326_pake_les_trois_formats_deverrouillent_la_psk_dans_les_deux_suites() {
    for v in flows() {
        let s = serveur(&v);
        assert_eq!(s.partage().as_slice(), octets(&v, "ya"));
        let c = s.recevoir_partage(&octets(&v, "yb")).unwrap();
        assert_eq!(c.tag_serveur().as_slice(), octets(&v, "ta"));
        let a = c
            .confirmer(&octets(&v, "tb"), nonce(&v).as_deref())
            .expect("confirmation et liaison de la reference");
        let psk = a
            .recevoir_psk(v["client_id"].as_str().unwrap(), &octets(&v, "wrapped_psk"))
            .unwrap();
        assert_eq!(
            psk.secret().as_slice(),
            octets(&v, "psk"),
            "la vraie cle de la reference doit etre retrouvee"
        );
        assert_eq!(psk.categorie(), CategoriePsk::LongueDuree);
    }
}

#[test]
fn i3326_pake_un_code_errone_ne_confirme_jamais_la_cle() {
    for v in flows() {
        let mut s = serveur(&v);
        let mut faux = octets(&v, "prs");
        faux[0] ^= 1;
        s.echange = Echange::nouveau(
            &faux,
            b"",
            s.contexte.sid(),
            b"server",
            b"client",
            StaticSecret::from(tableau::<32>(&v, "sa")),
        )
        .unwrap();
        let c = s.recevoir_partage(&octets(&v, "yb")).unwrap();
        assert!(
            matches!(
                c.confirmer(&octets(&v, "tb"), nonce(&v).as_deref()),
                Err(ErreurAppairage::CodeIncorrect)
            ),
            "un mauvais code ne doit pas donner acces a la cle finale"
        );
    }
}

#[test]
fn i3326_pake_lie_la_confirmation_a_noise_au_compteur_et_au_tour() {
    for v in flows() {
        for changement in 0..3 {
            let mut s = serveur(&v);
            match changement {
                0 => s.contexte.h[0] ^= 1,
                1 => s.contexte.index = std::num::NonZeroU32::new(4).unwrap(),
                _ => s.contexte.tour = std::num::NonZeroU32::new(2).unwrap(),
            }
            s.echange = Echange::nouveau(
                &s.code.octets,
                b"",
                s.contexte.sid(),
                b"server",
                b"client",
                StaticSecret::from(tableau::<32>(&v, "sa")),
            )
            .unwrap();
            let c = s.recevoir_partage(&octets(&v, "yb")).unwrap();
            assert!(
                matches!(
                    c.confirmer(&octets(&v, "tb"), nonce(&v).as_deref()),
                    Err(ErreurAppairage::CodeIncorrect)
                ),
                "Noise/index/round doivent etre lies a la confirmation"
            );
        }
    }
}

#[test]
fn i3326_pake_refuse_les_confirmations_falsifiees_ou_tronquees() {
    for v in flows() {
        let mut faux = octets(&v, "tb");
        faux[0] ^= 1;
        for tag in [&faux[..], &faux[..63], &faux[..0]] {
            let c = serveur(&v).recevoir_partage(&octets(&v, "yb")).unwrap();
            assert!(
                c.confirmer(tag, nonce(&v).as_deref()).is_err(),
                "une confirmation falsifiee ou tronquee ne doit jamais ouvrir le wrapping"
            );
        }
        let c = serveur(&v).recevoir_partage(&octets(&v, "yb")).unwrap();
        let propre = c.tag_serveur();
        assert!(
            c.confirmer(&propre, nonce(&v).as_deref()).is_err(),
            "le tag du serveur ne confirme pas le client"
        );
    }
}

#[test]
fn i3326_pake_refuse_la_reflexion_meme_avec_des_ad_identiques() {
    let e = Echange::nouveau(
        b"01234567",
        b"",
        b"sid".to_vec(),
        b"",
        b"",
        StaticSecret::from([21; 32]),
    )
    .unwrap();
    let ya = e.ya;
    let c = e.recevoir(&ya).unwrap();
    assert_eq!(
        c.verifier(&c.tag_serveur()),
        Err(ErreurAppairage::CodeIncorrect),
        "reflechir notre partage et notre tag ne prouve pas le code"
    );
}

#[test]
fn i3326_pake_un_tag_valide_ne_suffit_pas_sans_le_commitment() {
    for v in flows()
        .into_iter()
        .filter(|v| format(v) != FormatCode::Statique)
    {
        let mut s = serveur(&v);
        s.liaison.as_mut().unwrap().commit_b[0] ^= 1;
        let c = s.recevoir_partage(&octets(&v, "yb")).unwrap();
        assert!(
            matches!(
                c.confirmer(&octets(&v, "tb"), nonce(&v).as_deref()),
                Err(ErreurAppairage::Protocole("commitment du nonce client"))
            ),
            "le commitment doit etre verifie avant de lire une PSK"
        );
    }
}

#[test]
fn i3326_pake_un_tag_et_un_commitment_valides_ne_suffisent_pas_sans_la_liaison_du_code() {
    for v in flows()
        .into_iter()
        .filter(|v| format(v) != FormatCode::Statique)
    {
        let mut s = serveur(&v);
        s.liaison.as_mut().unwrap().nonce_a[0] ^= 1;
        let c = s.recevoir_partage(&octets(&v, "yb")).unwrap();
        assert!(
            matches!(
                c.confirmer(&octets(&v, "tb"), nonce(&v).as_deref()),
                Err(ErreurAppairage::Protocole(
                    "code non lie aux nonces et a Noise"
                ))
            ),
            "le code saisi doit etre derive des deux nonces et de Noise avant toute PSK"
        );
    }
}

#[test]
fn i3326_pake_les_champs_nonce_et_psk_ne_sont_pas_interchangeables() {
    for v in flows()
        .into_iter()
        .filter(|v| format(v) != FormatCode::Statique)
    {
        let c = serveur(&v).recevoir_partage(&octets(&v, "yb")).unwrap();
        assert!(
            c.confirmer(&octets(&v, "tb"), Some(&octets(&v, "wrapped_psk")))
                .is_err(),
            "le domaine PSK ne doit pas ouvrir un nonce"
        );
        let c = serveur(&v).recevoir_partage(&octets(&v, "yb")).unwrap();
        let a = c
            .confirmer(&octets(&v, "tb"), nonce(&v).as_deref())
            .unwrap();
        assert!(
            a.recevoir_psk(
                v["client_id"].as_str().unwrap(),
                &octets(&v, "wrapped_nonce")
            )
            .is_err(),
            "le domaine nonce ne doit pas ouvrir une PSK"
        );
    }
}

#[test]
fn i3326_pake_refuse_les_chiffres_alteres_et_les_mauvaises_tailles() {
    for v in flows() {
        for position in [0, 31, 47, 48, 49] {
            let c = serveur(&v).recevoir_partage(&octets(&v, "yb")).unwrap();
            let a = c
                .confirmer(&octets(&v, "tb"), nonce(&v).as_deref())
                .unwrap();
            let mut chiffre = octets(&v, "wrapped_psk");
            if position < 48 {
                chiffre[position] ^= 1;
            } else if position == 48 {
                chiffre.pop();
            } else {
                chiffre.push(0);
            }
            assert!(
                a.recevoir_psk(v["client_id"].as_str().unwrap(), &chiffre)
                    .is_err(),
                "un champ AEAD altere ne doit pas devenir une PSK"
            );
        }
    }
}

#[test]
fn i3326_pake_la_presence_du_nonce_depend_du_format() {
    for v in flows() {
        let c = serveur(&v).recevoir_partage(&octets(&v, "yb")).unwrap();
        let contraire = (format(&v) == FormatCode::Statique).then(|| octets(&v, "wrapped_nonce"));
        assert!(
            c.confirmer(&octets(&v, "tb"), contraire.as_deref())
                .is_err()
        );
    }
}

#[test]
fn i3326_pake_refuse_les_points_de_faible_ordre_sans_rejeter_les_variantes_rfc7748_valides() {
    let v = &vecteurs(include_str!("draft-vectors.json"))["X25519_points"];
    let scalaire: [u8;32] = octets(&serde_json::json!({"s":"af46e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449aff"}),"s").try_into().unwrap();
    let s = StaticSecret::from(scalaire);
    for i in [0, 1, 2, 3, 4, 5, 7] {
        assert!(
            multiplier(&s, &tableau(v, &format!("Invalid Y{i}"))).is_err(),
            "un point de faible ordre doit etre refuse"
        );
    }
    for (i, attendu) in [
        (
            6,
            "d8e2c776bbacd510d09fd9278b7edcd25fc5ae9adfba3b6e040e8d3b71b21806",
        ),
        (
            8,
            "c85c655ebe8be44ba9c0ffde69f2fe10194458d137f09bbff725ce58803cdb38",
        ),
        (
            9,
            "db64dafa9b8fdd136914e61461935fe92aa372cb056314e1231bc4ec12417456",
        ),
        (
            10,
            "e062dcd5376d58297be2618c7498f55baa07d7e03184e8aada20bca28888bf7a",
        ),
        (
            11,
            "993c6ad11c4c29da9a56f7691fd0ff8d732e49de6250b6c2e80003ff4629a175",
        ),
    ] {
        let attendu = octets(&serde_json::json!({"s":attendu}), "s");
        assert_eq!(
            multiplier(&s, &tableau(v, &format!("Invalid Y{i}")))
                .unwrap()
                .as_slice(),
            attendu,
            "le bit 255 doit etre ignore comme dans RFC7748"
        );
    }
    for n in [0, 31, 33] {
        assert!(serveur(&flows()[0]).recevoir_partage(&vec![0; n]).is_err());
    }
}

#[test]
fn i3326_pake_valide_les_formats_et_tire_un_ephemere_frais() {
    for (f, bons) in [
        (FormatCode::Statique, b"01234567".as_slice()),
        (FormatCode::Dynamique, b"000123".as_slice()),
        (FormatCode::Qr, [42; 24].as_slice()),
    ] {
        let contexte = ContextePake::nouveau([11; 32], 1, 1).unwrap();
        let creer = || {
            PakeServeur::demarrer(
                CodeAppairage::nouveau(f, bons).unwrap(),
                contexte,
                Suite::ChaChaPoly,
                (f != FormatCode::Statique).then(|| LiaisonDynamique::nouvelle([9; 32])),
            )
            .unwrap()
        };
        assert_ne!(
            creer().partage(),
            creer().partage(),
            "chaque tentative doit tirer un scalaire frais"
        );
        assert!(CodeAppairage::nouveau(f, &bons[..bons.len() - 1]).is_err());
    }
    assert!(CodeAppairage::nouveau(FormatCode::Statique, b"1234567x").is_err());
    assert!(CodeAppairage::nouveau(FormatCode::Dynamique, "１２３".as_bytes()).is_err());
    assert!(ContextePake::nouveau([0; 32], 0, 1).is_err());
    assert!(ContextePake::nouveau([0; 32], 1, 0).is_err());
    let v = flows()[0].clone();
    assert_eq!(
        serveur(&v).contexte.sid(),
        octets(&v, "sid"),
        "index et tour doivent etre big endian"
    );
    assert!(
        PakeServeur::demarrer(
            CodeAppairage::nouveau(FormatCode::Statique, b"01234567").unwrap(),
            ContextePake::nouveau([0; 32], 1, 2).unwrap(),
            Suite::ChaChaPoly,
            None
        )
        .is_err()
    );
}

#[test]
fn i3326_pake_ne_publie_pas_les_secrets_dans_debug() {
    for v in flows() {
        let s = serveur(&v);
        let mut texte = format!("{s:?} {:?} {:?}", s.code, s.liaison);
        let c = s.recevoir_partage(&octets(&v, "yb")).unwrap();
        texte.push_str(&format!("{c:?}"));
        let a = c
            .confirmer(&octets(&v, "tb"), nonce(&v).as_deref())
            .unwrap();
        texte.push_str(&format!("{a:?}"));
        for nom in ["prs", "sa", "isk", "psk"] {
            assert!(!texte.contains(v[nom].as_str().unwrap()));
            assert!(!texte.contains(&format!("{:?}", octets(&v, nom))));
        }
    }
}

#[test]
fn i3326_pake_saisie_operateur_preserve_les_octets_du_code() {
    use crate::sendspin::jeton::lire_code;
    for (saisie, format, attendu) in [
        (" 0123-4567 ", FormatCode::Statique, b"01234567".as_slice()),
        ("\t012-345\n", FormatCode::Dynamique, b"012345".as_slice()),
        ("0 1 2 3 4 5", FormatCode::Dynamique, b"012345".as_slice()),
    ] {
        assert_eq!(
            lire_code(saisie, format).unwrap().octets.as_slice(),
            attendu
        );
    }
    let qr = lire_code(
        " sp:14dq6fy7e4xtop9hj5lv6z3po57ypd4xt6t97n5y ",
        FormatCode::Qr,
    )
    .unwrap();
    assert_eq!(qr.octets.as_slice(), &(0xe0..=0xf7).collect::<Vec<u8>>());
    for code in [
        "12345",
        "1234567",
        "１２３４５６",
        "12x345",
        "12\t345",
        "12_345",
    ] {
        assert!(lire_code(code, FormatCode::Dynamique).is_err());
    }
}
