//! Règles de vérification, cas par cas. Les vecteurs partagés avec le site
//! sont rejoués à part (`tests/vecteurs.rs`) ; ici, ce qu'ils ne couvrent pas.

use super::*;
use crate::fabrique::{AppareilDeTest, Racine, cles_de_test::*, revendications};

const T: i64 = 1_793_000_000;
const SID: &str = "srv-1";

fn jeu() -> JeuDeCles {
    JeuDeCles::depuis_publication(&publication(), &racine().cle_publique_minisign()).unwrap()
}

fn ctx(maintenant: i64) -> Contexte<'static> {
    Contexte {
        server_id: SID,
        maintenant,
        grace_expiration: 0,
    }
}

fn appareil() -> AppareilDeTest {
    AppareilDeTest::ed25519(&GRAINE_APPAREIL)
}

fn normal() -> Revendications {
    revendications("cpt", SID, "app", appareil().confirmation(), T - 60, "j1")
}

#[test]
fn un_laissez_passer_valide_est_accepte_et_lie_a_la_cle_de_l_appareil() {
    let lp = jeu().verifier(&site().signer(&normal()), &ctx(T)).unwrap();
    assert_eq!(lp.revendications.sub, "cpt");
    assert_eq!(lp.kid, KID);
    let sig = appareil().signer_defi(SID, "d1");
    assert_eq!(lp.verifier_defi(SID, "d1", &sig), Ok(()));
    let autre = AppareilDeTest::ed25519(&GRAINE_AUTRE_APPAREIL).signer_defi(SID, "d1");
    assert_eq!(lp.verifier_defi(SID, "d1", &autre), Err(Refus::Defi));
    assert_eq!(
        lp.verifier_defi(SID, "d1", "pas-du-base64!"),
        Err(Refus::Defi)
    );
}

#[test]
fn la_tolerance_d_horloge_est_de_deux_minutes_exactement() {
    let jeton = site().signer(&normal());
    let exp = normal().exp;
    assert!(
        jeu()
            .verifier(&jeton, &ctx(exp + TOLERANCE_HORLOGE_S))
            .is_ok()
    );
    assert_eq!(
        jeu().verifier(&jeton, &ctx(exp + TOLERANCE_HORLOGE_S + 1)),
        Err(Refus::Expire)
    );
}

#[test]
fn la_grace_de_secours_prolonge_un_laissez_passer_expire_et_pas_davantage() {
    let jeton = site().signer(&normal());
    let exp = normal().exp;
    let secours = |t| Contexte {
        server_id: SID,
        maintenant: t,
        grace_expiration: 86_400,
    };
    let lp = jeu().verifier(&jeton, &secours(exp + 3600)).unwrap();
    assert!(lp.est_expire(exp + 3600));
    assert_eq!(
        jeu().verifier(&jeton, &secours(exp + 86_400 + TOLERANCE_HORLOGE_S + 1)),
        Err(Refus::Expire)
    );
}

#[test]
fn un_jeton_trop_long_ou_mal_decoupe_est_refuse_sans_etre_decode() {
    let j = jeu();
    assert_eq!(
        j.verifier(&"a".repeat(TAILLE_MAX + 1), &ctx(T)),
        Err(Refus::Format)
    );
    assert_eq!(j.verifier("a.b", &ctx(T)), Err(Refus::Format));
    assert_eq!(j.verifier("a.b.c.d", &ctx(T)), Err(Refus::Format));
    // Remplissage `=` : base64url SANS remplissage seulement.
    let jeton = site().signer(&normal());
    assert_eq!(
        j.verifier(&format!("{jeton}="), &ctx(T)),
        Err(Refus::Format)
    );
}

#[test]
fn une_cle_du_site_hors_de_sa_periode_de_validite_ne_signe_plus() {
    // Jeu où la clé n'est valable que jusqu'à T - 10 min.
    let mut doc = document(2);
    doc.cles[0].valable_au = T - 600;
    let jeu =
        JeuDeCles::depuis_publication(&racine().publier(&doc), &racine().cle_publique_minisign())
            .unwrap();
    let mut r = normal();
    r.iat = T - 1200;
    r.nbf = r.iat;
    r.exp = r.iat + 3600;
    assert_eq!(
        jeu.verifier(&site().signer(&r), &ctx(T)),
        Err(Refus::CleHorsValidite)
    );
}

#[test]
fn sans_racine_aucun_jeu_de_cles_n_est_accepte() {
    assert_eq!(
        JeuDeCles::depuis_publication(&publication(), "").unwrap_err(),
        Refus::RacineAbsente
    );
    assert_eq!(
        JeuDeCles::depuis_publication(&publication(), RACINE_EMBARQUEE).unwrap_err(),
        Refus::RacineAbsente
    );
}

#[test]
fn un_jeu_de_cles_retouche_apres_signature_est_refuse() {
    let mut p = publication();
    p.jeu = p.jeu.replace("\"version\":1", "\"version\":9");
    assert_eq!(
        JeuDeCles::depuis_publication(&p, &racine().cle_publique_minisign()).unwrap_err(),
        Refus::JeuDeClesSignature
    );
}

#[test]
fn un_jeu_de_cles_signe_en_mode_minisign_ancien_est_refuse() {
    // Mode « legacy » (`Ed`) : signature Ed25519 directe des données, sans
    // prédigest. Fabriquée à la main, correcte en tout point sauf le mode.
    let paire = ring::signature::Ed25519KeyPair::from_seed_unchecked(&GRAINE_RACINE).unwrap();
    let jeu = publication().jeu;
    let sig = paire.sign(jeu.as_bytes());
    let mut bin1 = b"Ed".to_vec();
    bin1.extend_from_slice(&ID_RACINE);
    bin1.extend_from_slice(sig.as_ref());
    let commentaire = "x";
    let mut globale = sig.as_ref().to_vec();
    globale.extend_from_slice(commentaire.as_bytes());
    let minisig = format!(
        "untrusted comment: legacy\n{}\ntrusted comment: {commentaire}\n{}\n",
        en_b64(&bin1),
        en_b64(paire.sign(&globale).as_ref())
    );
    let racine = Racine::depuis_graine(ID_RACINE, &GRAINE_RACINE).cle_publique_minisign();
    // Témoin : la même signature, acceptée si l'on tolérait le mode ancien.
    let cle = minisign_verify::PublicKey::from_base64(&racine).unwrap();
    let s = minisign_verify::Signature::decode(&minisig).unwrap();
    assert!(cle.verify(jeu.as_bytes(), &s, true).is_ok());
    assert_eq!(
        JeuDeCles::depuis_publication(&Publication { jeu, minisig }, &racine).unwrap_err(),
        Refus::JeuDeClesSignature
    );
}

#[test]
fn un_jeu_de_cles_avec_deux_fois_le_meme_kid_est_refuse() {
    let mut doc = document(3);
    doc.cles.push(doc.cles[0].clone());
    assert_eq!(
        JeuDeCles::depuis_publication(&racine().publier(&doc), &racine().cle_publique_minisign())
            .unwrap_err(),
        Refus::JeuDeClesFormat
    );
}

#[test]
fn le_kid_se_lit_sans_verification_pour_decider_d_un_rafraichissement() {
    let jeton =
        crate::fabrique::SignataireDuSite::depuis_graine("k-neuf", &GRAINE_SITE).signer(&normal());
    assert_eq!(JeuDeCles::kid_de(&jeton).as_deref(), Some("k-neuf"));
    assert!(!jeu().connait("k-neuf"));
    assert!(jeu().connait(KID));
    assert_eq!(JeuDeCles::kid_de("n'importe quoi"), None);
}

#[test]
fn une_cle_de_serveur_mal_formee_est_refusee() {
    let mut r = normal();
    r.srv_pk = Some("court".into());
    assert_eq!(
        jeu().verifier(&site().signer(&r), &ctx(T)),
        Err(Refus::Format)
    );
    let cle = CleServeur::depuis_graine(&GRAINE_SERVEUR).unwrap();
    r.srv_pk = Some(cle.cle_publique());
    assert!(jeu().verifier(&site().signer(&r), &ctx(T)).is_ok());
}

#[test]
fn la_cle_du_serveur_signe_un_defi_que_seule_sa_cle_publique_verifie() {
    let graine = CleServeur::nouvelle_graine().unwrap();
    let cle = CleServeur::depuis_graine(&graine).unwrap();
    let sig = cle.signer_defi(SID, "d");
    assert_eq!(
        verifier_defi_serveur(&cle.cle_publique(), SID, "d", &sig),
        Ok(())
    );
    // Un défi de SESSION signé par la même clé ne vaut pas défi de serveur :
    // les domaines diffèrent.
    let sig_session = cle.signer(&message_defi_session(SID, "d"));
    assert_eq!(
        verifier_defi_serveur(&cle.cle_publique(), SID, "d", &sig_session),
        Err(Refus::Defi)
    );
    assert!(CleServeur::depuis_graine(&[0u8; 31]).is_err());
}

#[test]
fn les_codes_de_refus_sont_stables_et_distincts() {
    let tous = [
        Refus::Format,
        Refus::Alg,
        Refus::Typ,
        Refus::KidInconnu,
        Refus::CleHorsValidite,
        Refus::Signature,
        Refus::Version,
        Refus::Emetteur,
        Refus::Audience,
        Refus::Serveur,
        Refus::PasEncoreValide,
        Refus::Expire,
        Refus::Duree,
        Refus::CleAppareil,
        Refus::Defi,
        Refus::RacineAbsente,
        Refus::JeuDeClesSignature,
        Refus::JeuDeClesFormat,
    ];
    let mut codes: Vec<&str> = tous.iter().map(Refus::code).collect();
    codes.sort();
    codes.dedup();
    assert_eq!(codes.len(), tous.len());
}
