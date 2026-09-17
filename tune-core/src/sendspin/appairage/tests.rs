use super::*;
use crate::sendspin::{Identite, Suite};

fn infos(categorie: CategoriePsk) -> InfosPair {
    InfosPair {
        client_id: Identite::depuis_prive([31; 32]).id(),
        server_id: Identite::depuis_prive([32; 32]).id(),
        suite: Suite::ChaChaPoly,
        psk_id: "preuve".into(),
        categorie_psk: categorie,
        identifiant_perdu: false,
        condensat_poignee: [42; 32],
    }
}
fn debut(methode: MethodeAppairage) -> (AppairageServeur, Instant) {
    let maintenant = Instant::now();
    let (s, actions) =
        AppairageServeur::commencer(infos(methode.categorie_requise()), methode, 3, maintenant)
            .unwrap();
    assert!(
        matches!(&actions[0], ActionAppairage::Envoyer { type_message:"server/activate", payload } if payload["activities"]==json!(["pairing"]) && payload["active_roles"]==json!([]))
    );
    (s, maintenant)
}
fn init(s: &mut AppairageServeur, t: Instant) -> Vec<ActionAppairage> {
    let v = if s.methode.dynamique() {
        json!({"pairing_index":3,"commit_B":b64url(&[5;32])})
    } else {
        json!({"pairing_index":3})
    };
    s.recevoir("client/pair-init", &v, t).unwrap()
}
fn confirmation(s: &mut AppairageServeur, t: Instant) -> Vec<ActionAppairage> {
    init(s, t);
    s.saisir_code(
        if s.methode == MethodeAppairage::Statique {
            "01234567"
        } else {
            "012345"
        },
        t,
    )
    .unwrap();
    let y = Identite::depuis_prive([55; 32]);
    s.recevoir(
        "client/pair-auth",
        &json!({"pake_msg_2":b64url(y.public())}),
        t,
    )
    .unwrap()
}

#[test]
fn i3326_appairage_psk_ne_confirme_qu_apres_persistance() {
    let (mut s, t) = debut(MethodeAppairage::Psk);
    assert!(init(&mut s, t).is_empty());
    let actions = s
        .recevoir(
            "client/pair-finalize",
            &json!({"long_term_psk":b64url(&[7;32])}),
            t,
        )
        .unwrap();
    assert_eq!(
        actions.len(),
        1,
        "aucun acquittement ne doit preceder la persistance"
    );
    assert!(
        matches!(&actions[0],ActionAppairage::Persister(p) if p.secret()==&[7;32] && p.categorie()==CategoriePsk::LongueDuree)
    );
    let a = s.confirmer_persistance().unwrap();
    assert!(
        matches!(&a[0],ActionAppairage::Envoyer{type_message:"server/pair-finalize",payload} if payload==&json!({}))
    );
    assert!(matches!(&a[1],ActionAppairage::Promouvoir(p) if p.secret()==&[7;32]));
    assert!(s.echeance().is_none());
    assert!(
        s.confirmer_persistance().is_err(),
        "un record ne se confirme qu'une fois"
    );
}

#[test]
fn i3326_appairage_methode_exige_la_categorie_noise() {
    for methode in [
        MethodeAppairage::Psk,
        MethodeAppairage::Statique,
        MethodeAppairage::Dynamique,
        MethodeAppairage::Qr,
    ] {
        for categorie in [
            CategoriePsk::Sentinelle,
            CategoriePsk::Appairage,
            CategoriePsk::LongueDuree,
        ] {
            let r = AppairageServeur::commencer(infos(categorie), methode, 1, Instant::now());
            assert_eq!(r.is_ok(), categorie == methode.categorie_requise());
        }
        assert!(
            AppairageServeur::commencer(
                infos(methode.categorie_requise()),
                methode,
                0,
                Instant::now()
            )
            .is_err()
        );
    }
}

#[test]
fn i3326_appairage_pas_de_finalisation_ni_de_reprise_apres_erreur() {
    let (mut s, t) = debut(MethodeAppairage::Psk);
    assert!(
        s.confirmer_persistance().is_err(),
        "sans cle recue le pilote ne peut acquitter"
    );
    assert!(
        s.recevoir("client/pair-init", &json!({"pairing_index":3}), t)
            .is_err()
    );
    let (mut s, t) = debut(MethodeAppairage::Psk);
    assert!(
        s.recevoir(
            "client/pair-finalize",
            &json!({"long_term_psk":b64url(&[7;32])}),
            t
        )
        .is_err()
    );
    assert!(
        s.recevoir("client/pair-init", &json!({"pairing_index":3}), t)
            .is_err()
    );
}

#[test]
fn i3326_appairage_code_statique_attend_le_geste_meme_si_deja_saisi() {
    let (mut s, t) = debut(MethodeAppairage::Statique);
    assert!(
        s.saisir_code("0123-4567", t).unwrap().is_empty(),
        "pas de CPace avant client/pair-init"
    );
    let a = s
        .recevoir(
            "client/pair-pending",
            &json!({"pairing_index":3,"message":format!("<b>{}","é".repeat(250))}),
            t,
        )
        .unwrap();
    assert!(
        matches!(&a[0],ActionAppairage::AttendreGeste{message:Some(m)} if m.chars().count()==200 && m.starts_with("<b>"))
    );
    let a = init(&mut s, t);
    assert!(matches!(
        &a[0],
        ActionAppairage::Envoyer {
            type_message: "server/pair-auth",
            ..
        }
    ));
    assert!(
        s.recevoir("client/pair-init", &json!({"pairing_index":3}), t)
            .is_err()
    );
}

#[test]
fn i3326_appairage_index_ancien_ignore_et_futur_refuse() {
    let (mut s, t) = debut(MethodeAppairage::Dynamique);
    assert!(
        s.recevoir(
            "client/pair-init",
            &json!({"pairing_index":2,"commit_B":false}),
            t
        )
        .unwrap()
        .is_empty()
    );
    assert!(matches!(s.etat, Etat::Init));
    assert!(
        s.recevoir("client/pair-pending", &json!({"pairing_index":4}), t)
            .is_err()
    );
    assert!(!s.active());
    for index in [
        json!(0),
        json!(-1),
        json!(1.5),
        json!(u32::MAX as u64 + 1),
        json!("3"),
    ] {
        let (mut s, t) = debut(MethodeAppairage::Psk);
        assert!(
            s.recevoir("client/pair-init", &json!({"pairing_index":index}), t)
                .is_err()
        );
    }
}

#[test]
fn i3326_appairage_annulation_ignore_la_cle_en_vol() {
    let (mut s, t) = debut(MethodeAppairage::Psk);
    init(&mut s, t);
    let a = s.annuler();
    assert!(
        matches!(&a[0],ActionAppairage::Envoyer{type_message:"pair/abort",payload} if payload["reason"]=="user_cancelled")
    );
    assert!(
        s.recevoir(
            "client/pair-finalize",
            &json!({"long_term_psk":b64url(&[7;32])}),
            t
        )
        .unwrap()
        .is_empty()
    );
    assert!(
        s.confirmer_persistance().is_err(),
        "une annulation ne persiste ni n'acquitte la cle en vol"
    );
}

#[test]
fn i3326_appairage_expiration_ne_se_repousse_pas_par_pending() {
    let (mut s, t) = debut(MethodeAppairage::Statique);
    s.recevoir(
        "client/pair-pending",
        &json!({"pairing_index":3}),
        t + Duration::from_secs(299),
    )
    .unwrap();
    assert_eq!(s.echeance(), Some(t + ATTENTE_GESTE));
    let a = s.expirer(t + ATTENTE_GESTE).unwrap();
    assert!(
        matches!(&a[0],ActionAppairage::Envoyer{type_message:"server/activate",payload} if payload["activities"]==json!([]))
    );
    assert!(s.echeance().is_none());
    let (mut s, t) = debut(MethodeAppairage::Psk);
    init(&mut s, t + Duration::from_secs(100));
    assert_eq!(s.echeance(), Some(t + Duration::from_secs(220)));
    let a = s
        .recevoir(
            "client/pair-finalize",
            &json!({"long_term_psk":b64url(&[7;32])}),
            t + Duration::from_secs(220),
        )
        .unwrap();
    assert!(!a.iter().any(|a| matches!(a, ActionAppairage::Persister(_))));
}

#[test]
fn i3326_appairage_reprise_garde_nonce_et_delai_mais_change_tour_et_partage() {
    let (mut s, t) = debut(MethodeAppairage::Dynamique);
    confirmation(&mut s, t);
    let nonce = *s.liaison.as_ref().unwrap().nonce_a();
    let echeance = s.echeance();
    let a = s
        .recevoir("client/pair-retry", &json!({}), t + Duration::from_secs(30))
        .unwrap();
    assert!(
        matches!(&a[0],ActionAppairage::Envoyer{type_message:"server/pair-init",payload} if payload==&json!({}))
    );
    assert_eq!(s.tour, 2);
    assert_eq!(*s.liaison.as_ref().unwrap().nonce_a(), nonce);
    assert_eq!(
        s.echeance(),
        echeance,
        "une reprise ne recommence pas le delai de l'essai"
    );
    assert!(matches!(
        &a[1],
        ActionAppairage::DemanderCode { tour: 2, .. }
    ));
    s.saisir_code("012345", t).unwrap();
    assert!(matches!(s.etat, Etat::Partage(_)));
    let (mut s, t) = debut(MethodeAppairage::Statique);
    confirmation(&mut s, t);
    assert!(s.recevoir("client/pair-retry", &json!({}), t).is_err());
}

#[test]
fn i3326_appairage_limite_vingt_tours() {
    let (mut s, t) = debut(MethodeAppairage::Dynamique);
    confirmation(&mut s, t);
    for tour in 2..=20 {
        s.recevoir("client/pair-retry", &json!({}), t).unwrap();
        assert_eq!(s.tour, tour);
        s.saisir_code("012345", t).unwrap();
        s.recevoir(
            "client/pair-auth",
            &json!({"pake_msg_2":b64url(Identite::depuis_prive([55;32]).public())}),
            t,
        )
        .unwrap();
    }
    assert!(s.recevoir("client/pair-retry", &json!({}), t).is_err());
}

#[test]
fn i3326_appairage_confirmation_fausse_abandonne_sans_lire_la_psk() {
    let (mut s, t) = debut(MethodeAppairage::Statique);
    confirmation(&mut s, t);
    let a = s
        .recevoir(
            "client/pair-confirm",
            &json!({"client_kc":b64url(&[0;64])}),
            t,
        )
        .unwrap();
    assert!(
        matches!(&a[0],ActionAppairage::Envoyer{type_message:"pair/abort",payload} if payload["reason"]=="pairing_code_mismatch")
    );
    assert!(
        s.recevoir(
            "client/pair-finalize",
            &json!({"wrapped_psk":b64url(&[7;48])}),
            t
        )
        .unwrap()
        .is_empty()
    );
    assert!(s.confirmer_persistance().is_err());
}

#[test]
fn i3326_appairage_champs_interdits_et_non_canoniques_ferment() {
    for payload in [
        json!({"long_term_psk":b64url(&[7;32]),"wrapped_psk":null}),
        json!({"long_term_psk":format!("{}=",b64url(&[7;32]))}),
        json!({"long_term_psk":b64url(&[7;31])}),
        json!({"long_term_psk":b64url(&super::super::psk::sentinelle())}),
    ] {
        let (mut s, t) = debut(MethodeAppairage::Psk);
        init(&mut s, t);
        assert!(s.recevoir("client/pair-finalize", &payload, t).is_err());
        assert!(s.confirmer_persistance().is_err());
    }
    let (mut s, t) = debut(MethodeAppairage::Statique);
    assert!(
        s.recevoir(
            "client/pair-init",
            &json!({"pairing_index":3,"commit_B":null}),
            t
        )
        .is_err()
    );
}

#[test]
fn i3326_appairage_erreur_de_saisie_corrigeable_et_abandon_concurrent() {
    let (mut s, t) = debut(MethodeAppairage::Statique);
    init(&mut s, t);
    assert!(s.saisir_code("abc", t).is_err());
    assert!(s.saisir_code("01234567", t).is_ok());
    let a = s
        .recevoir("pair/abort", &json!({"reason":"concurrent_attempt"}), t)
        .unwrap();
    assert!(matches!(a[0], ActionAppairage::Fermer));
}

#[test]
fn i3326_appairage_nouvel_essai_attend_la_reconnaissance_avant_les_messages_non_indexes() {
    let (mut s, t) = debut(MethodeAppairage::Psk);
    init(&mut s, t);
    s.annuler();
    let (mut s, _) = s.recommencer(MethodeAppairage::Psk, t).unwrap();
    assert_eq!(s.index, 4);
    assert!(
        s.recevoir(
            "client/pair-finalize",
            &json!({"long_term_psk":b64url(&[7;32])}),
            t
        )
        .unwrap()
        .is_empty()
    );
    assert!(
        s.recevoir("client/pair-auth", &json!({}), t)
            .unwrap()
            .is_empty()
    );
    assert!(
        s.recevoir("client/pair-init", &json!({"pairing_index":3}), t)
            .unwrap()
            .is_empty()
    );
    assert!(matches!(s.etat, Etat::Init));
    s.recevoir("client/pair-init", &json!({"pairing_index":4}), t)
        .unwrap();
    let actions = s
        .recevoir(
            "client/pair-finalize",
            &json!({"long_term_psk":b64url(&[7;32])}),
            t,
        )
        .unwrap();
    assert!(matches!(&actions[0], ActionAppairage::Persister(_)));
    assert!(
        s.recevoir("client/pair-auth", &json!({}), t).is_err(),
        "un message hors sequence n'est plus ignore apres reconnaissance"
    );
}

#[test]
fn i3326_appairage_reprise_ignore_les_champs_du_futur() {
    let (mut s, t) = debut(MethodeAppairage::Dynamique);
    confirmation(&mut s, t);
    let a = s.recevoir("client/pair-retry", &json!({"future_extension":{"n":1}}), t);
    assert!(
        a.is_ok(),
        "un champ payload inconnu doit etre ignore sans fermer le tour"
    );
    assert_eq!(s.tour, 2);
}
