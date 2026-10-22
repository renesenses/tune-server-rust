//! Écrit `tune-laissez-passer/vecteurs/` : les cas partagés avec le site.
//!
//! ```sh
//! cargo run -p tune-laissez-passer --features fabrique --example generer_vecteurs
//! ```
//!
//! Les signatures Ed25519 sont déterministes : relancer l'outil réécrit les
//! mêmes octets. La signature P-256 ne l'est pas (ECDSA tire un aléa) : elle
//! change à chaque passage, et c'est sans effet sur le résultat attendu. La
//! paire P-256, elle, est relue dans `cles_de_test.json` quand il existe.
//!
//! Le test `vecteurs` rejoue les fichiers ÉCRITS, jamais l'outil : c'est le
//! contrat tel que le site le lit.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tune_laissez_passer::fabrique::{
    AppareilDeTest, Racine, SignataireDuSite, cles_de_test::*, revendications,
};
use tune_laissez_passer::{Revendications, b64url, en_b64url};

/// Heure de vérification de tous les cas : 26/10/2026 08:53:20 UTC.
const MAINTENANT: i64 = 1_793_000_000;
const SERVEUR: &str = "6f1c1f5e-2b0a-4c2e-9d3b-7a51c0de0001";
const AUTRE_SERVEUR: &str = "6f1c1f5e-2b0a-4c2e-9d3b-7a51c0de0002";
const COMPTE: &str = "cpt_8f2a61";
const APPAREIL: &str = "app_ed_01";
const APPAREIL_P256: &str = "app_p256_01";
const DEFI: &str = "c2Vzc2lvbi1kZWZpLWRlLXRlc3QtMDEyMzQ1Njc4OTAx";

fn dossier() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("vecteurs")
}

fn ecrire(nom: &str, v: &Value) {
    let texte = serde_json::to_string_pretty(v).unwrap() + "\n";
    std::fs::write(dossier().join(nom), texte).unwrap();
}

fn pkcs8_p256() -> Vec<u8> {
    let chemin = dossier().join("cles_de_test.json");
    if let Ok(texte) = std::fs::read_to_string(&chemin) {
        let v: Value = serde_json::from_str(&texte).unwrap();
        if let Some(p) = v["appareil_p256"]["pkcs8"].as_str() {
            return b64url(p).unwrap();
        }
    }
    AppareilDeTest::nouveau_pkcs8_p256()
}

struct Cas {
    nom: &'static str,
    description: &'static str,
    publication: &'static str,
    laissez_passer: String,
    server_id: &'static str,
    grace: i64,
    defi: Option<(String, String)>,
    attendu: Result<(), &'static str>,
}

fn main() {
    std::fs::create_dir_all(dossier()).unwrap();
    let site = site();
    let racine = racine();
    let autre_racine = Racine::depuis_graine(*b"autreRAC", &GRAINE_AUTRE_RACINE);
    let appareil = AppareilDeTest::ed25519(&GRAINE_APPAREIL);
    let autre_appareil = AppareilDeTest::ed25519(&GRAINE_AUTRE_APPAREIL);
    let pkcs8 = pkcs8_p256();
    let appareil_p256 = AppareilDeTest::p256(&pkcs8);
    let serveur = tune_laissez_passer::CleServeur::depuis_graine(&GRAINE_SERVEUR).unwrap();
    let faux_site = SignataireDuSite::depuis_graine(KID, &GRAINE_AUTRE_APPAREIL);

    ecrire(
        "cles_de_test.json",
        &json!({
            "avertissement": "CLES DE TEST PUBLIQUES. Ne jamais les utiliser hors des tests.",
            "racine_minisign": racine.cle_publique_minisign(),
            "racine_graine_hex": hex(&GRAINE_RACINE),
            "racine_id_hex": hex(&ID_RACINE),
            "autre_racine_minisign": autre_racine.cle_publique_minisign(),
            "site": {"kid": KID, "pk": site.cle_publique(), "graine_hex": hex(&GRAINE_SITE),
                      "valable_du": VALABLE_DU, "valable_au": VALABLE_AU},
            "appareil_ed25519": {"did": APPAREIL, "cnf": appareil.confirmation(), "graine_hex": hex(&GRAINE_APPAREIL)},
            "autre_appareil_ed25519": {"cnf": autre_appareil.confirmation(), "graine_hex": hex(&GRAINE_AUTRE_APPAREIL)},
            "appareil_p256": {"did": APPAREIL_P256, "cnf": appareil_p256.confirmation(), "pkcs8": en_b64url(&pkcs8)},
            "serveur": {"server_id": SERVEUR, "pk": serveur.cle_publique(), "graine_hex": hex(&GRAINE_SERVEUR)},
        }),
    );
    ecrire(
        "jeu_de_cles.json",
        &serde_json::to_value(racine.publier(&document(1))).unwrap(),
    );
    ecrire(
        "jeu_de_cles_autre_racine.json",
        &serde_json::to_value(autre_racine.publier(&document(1))).unwrap(),
    );

    let iat = MAINTENANT - 600;
    let base = |cnf, did: &str| revendications(COMPTE, SERVEUR, did, cnf, iat, "jti-0001");
    let normal = || base(appareil.confirmation(), APPAREIL);
    let avec = |f: &dyn Fn(&mut Revendications)| {
        let mut r = normal();
        f(&mut r);
        site.signer(&r)
    };
    let defi_ok = Some((DEFI.to_string(), appareil.signer_defi(SERVEUR, DEFI)));
    let charge_normale = serde_json::to_string(&normal()).unwrap();

    let mut cas = vec![
        Cas {
            nom: "valide",
            description: "Laissez-passer valide, clé d'appareil Ed25519, défi signé par cette clé.",
            publication: "jeu_de_cles.json",
            laissez_passer: site.signer(&normal()),
            server_id: SERVEUR,
            grace: 0,
            defi: defi_ok.clone(),
            attendu: Ok(()),
        },
        Cas {
            nom: "valide_p256",
            description: "Laissez-passer valide, clé d'appareil P-256 (repli), défi signé en ECDSA r||s.",
            publication: "jeu_de_cles.json",
            laissez_passer: site.signer(&base(appareil_p256.confirmation(), APPAREIL_P256)),
            server_id: SERVEUR,
            grace: 0,
            defi: Some((DEFI.to_string(), appareil_p256.signer_defi(SERVEUR, DEFI))),
            attendu: Ok(()),
        },
        Cas {
            nom: "expire",
            description: "exp dépassé de 10 min, au-delà de la tolérance de 2 min.",
            publication: "jeu_de_cles.json",
            laissez_passer: avec(&|r| {
                r.iat = MAINTENANT - 4200;
                r.nbf = r.iat;
                r.exp = MAINTENANT - 600;
            }),
            server_id: SERVEUR,
            grace: 0,
            defi: defi_ok.clone(),
            attendu: Err("expire"),
        },
        Cas {
            nom: "expire_dans_la_tolerance",
            description: "exp dépassé de 60 s : dans la tolérance d'horloge de 2 min, accepté.",
            publication: "jeu_de_cles.json",
            laissez_passer: avec(&|r| {
                r.iat = MAINTENANT - 3660;
                r.nbf = r.iat;
                r.exp = MAINTENANT - 60;
            }),
            server_id: SERVEUR,
            grace: 0,
            defi: defi_ok.clone(),
            attendu: Ok(()),
        },
        Cas {
            nom: "expire_en_secours",
            description: "Le cas « expire », vérifié avec la fenêtre de secours de 24 h (site injoignable) : accepté.",
            publication: "jeu_de_cles.json",
            laissez_passer: avec(&|r| {
                r.iat = MAINTENANT - 4200;
                r.nbf = r.iat;
                r.exp = MAINTENANT - 600;
            }),
            server_id: SERVEUR,
            grace: 86_400,
            defi: defi_ok.clone(),
            attendu: Ok(()),
        },
        Cas {
            nom: "mauvaise_signature",
            description: "Signé par une autre clé que celle du kid annoncé.",
            publication: "jeu_de_cles.json",
            laissez_passer: faux_site.signer(&normal()),
            server_id: SERVEUR,
            grace: 0,
            defi: defi_ok.clone(),
            attendu: Err("signature"),
        },
        Cas {
            nom: "charge_modifiee",
            description: "Charge remplacée après la signature (sub changé) : la signature ne couvre plus la charge.",
            publication: "jeu_de_cles.json",
            laissez_passer: {
                let jeton = site.signer(&normal());
                let mut r = normal();
                r.sub = "cpt_autre".into();
                let p: Vec<&str> = jeton.split('.').collect();
                format!(
                    "{}.{}.{}",
                    p[0],
                    en_b64url(serde_json::to_string(&r).unwrap().as_bytes()),
                    p[2]
                )
            },
            server_id: SERVEUR,
            grace: 0,
            defi: defi_ok.clone(),
            attendu: Err("signature"),
        },
        Cas {
            nom: "mauvais_alg_none",
            description: "alg = none, sans signature.",
            publication: "jeu_de_cles.json",
            laissez_passer: format!(
                "{}.{}.",
                en_b64url(
                    json!({"alg":"none","typ":"tune-pass+jwt","kid":KID})
                        .to_string()
                        .as_bytes()
                ),
                en_b64url(charge_normale.as_bytes())
            ),
            server_id: SERVEUR,
            grace: 0,
            defi: defi_ok.clone(),
            attendu: Err("alg"),
        },
        Cas {
            nom: "mauvais_alg_hs256",
            description: "alg = HS256, HMAC calculé avec la clé publique du site comme secret (confusion d'algorithme).",
            publication: "jeu_de_cles.json",
            laissez_passer: {
                let signe = format!(
                    "{}.{}",
                    en_b64url(
                        json!({"alg":"HS256","typ":"tune-pass+jwt","kid":KID})
                            .to_string()
                            .as_bytes()
                    ),
                    en_b64url(charge_normale.as_bytes())
                );
                let cle = ring::hmac::Key::new(
                    ring::hmac::HMAC_SHA256,
                    &b64url(&site.cle_publique()).unwrap(),
                );
                let mac = ring::hmac::sign(&cle, signe.as_bytes());
                format!("{signe}.{}", en_b64url(mac.as_ref()))
            },
            server_id: SERVEUR,
            grace: 0,
            defi: defi_ok.clone(),
            attendu: Err("alg"),
        },
        Cas {
            nom: "mauvais_alg_es256",
            description: "alg = ES256, signature Ed25519 valable par ailleurs : seul EdDSA est accepté.",
            publication: "jeu_de_cles.json",
            laissez_passer: site.signer_brut(
                &json!({"alg":"ES256","typ":"tune-pass+jwt","kid":KID}).to_string(),
                &charge_normale,
            ),
            server_id: SERVEUR,
            grace: 0,
            defi: defi_ok.clone(),
            attendu: Err("alg"),
        },
        Cas {
            nom: "mauvais_typ",
            description: "typ = JWT au lieu de tune-pass+jwt.",
            publication: "jeu_de_cles.json",
            laissez_passer: site.signer_brut(
                &json!({"alg":"EdDSA","typ":"JWT","kid":KID}).to_string(),
                &charge_normale,
            ),
            server_id: SERVEUR,
            grace: 0,
            defi: defi_ok.clone(),
            attendu: Err("typ"),
        },
        Cas {
            nom: "entete_avec_jwk",
            description: "Entête portant une clé (jwk) : tout champ d'entête inconnu est refusé, la clé vient du jeu de clés.",
            publication: "jeu_de_cles.json",
            laissez_passer: site.signer_brut(
                &json!({"alg":"EdDSA","typ":"tune-pass+jwt","kid":KID,
                        "jwk":{"kty":"OKP","crv":"Ed25519","x":site.cle_publique()}})
                .to_string(),
                &charge_normale,
            ),
            server_id: SERVEUR,
            grace: 0,
            defi: defi_ok.clone(),
            attendu: Err("format"),
        },
        Cas {
            nom: "kid_inconnu",
            description: "kid absent du jeu de clés.",
            publication: "jeu_de_cles.json",
            laissez_passer: SignataireDuSite::depuis_graine("inconnu-2099", &GRAINE_SITE)
                .signer(&normal()),
            server_id: SERVEUR,
            grace: 0,
            defi: defi_ok.clone(),
            attendu: Err("kid_inconnu"),
        },
        Cas {
            nom: "mauvaise_audience",
            description: "aud = tune-server : un autre jeton signé par le site.",
            publication: "jeu_de_cles.json",
            laissez_passer: avec(&|r| r.aud = "tune-server".into()),
            server_id: SERVEUR,
            grace: 0,
            defi: defi_ok.clone(),
            attendu: Err("audience"),
        },
        Cas {
            nom: "mauvais_emetteur",
            description: "iss autre que https://mozaiklabs.fr.",
            publication: "jeu_de_cles.json",
            laissez_passer: avec(&|r| r.iss = "https://mozaiklabs.example".into()),
            server_id: SERVEUR,
            grace: 0,
            defi: defi_ok.clone(),
            attendu: Err("emetteur"),
        },
        Cas {
            nom: "mauvais_serveur",
            description: "sid d'un autre serveur que celui visé.",
            publication: "jeu_de_cles.json",
            laissez_passer: site.signer(&normal()),
            server_id: AUTRE_SERVEUR,
            grace: 0,
            defi: Some((DEFI.to_string(), appareil.signer_defi(AUTRE_SERVEUR, DEFI))),
            attendu: Err("serveur"),
        },
        Cas {
            nom: "mauvaise_version",
            description: "v = 2 : version de format inconnue.",
            publication: "jeu_de_cles.json",
            laissez_passer: avec(&|r| r.v = 2),
            server_id: SERVEUR,
            grace: 0,
            defi: defi_ok.clone(),
            attendu: Err("version"),
        },
        Cas {
            nom: "duree_trop_longue",
            description: "exp - iat = 2 h : au-delà de la durée maximale d'1 h.",
            publication: "jeu_de_cles.json",
            laissez_passer: avec(&|r| r.exp = r.iat + 7200),
            server_id: SERVEUR,
            grace: 0,
            defi: defi_ok.clone(),
            attendu: Err("duree"),
        },
        Cas {
            nom: "pas_encore_valide",
            description: "nbf dans 10 min, au-delà de la tolérance.",
            publication: "jeu_de_cles.json",
            laissez_passer: avec(&|r| {
                r.iat = MAINTENANT + 600;
                r.nbf = r.iat;
                r.exp = r.iat + 3600;
            }),
            server_id: SERVEUR,
            grace: 0,
            defi: defi_ok.clone(),
            attendu: Err("pas_encore_valide"),
        },
        Cas {
            nom: "cnf_mauvaise_taille",
            description: "cnf.pk de 31 octets pour une clé Ed25519.",
            publication: "jeu_de_cles.json",
            laissez_passer: avec(&|r| r.cnf.pk = en_b64url(&[7u8; 31])),
            server_id: SERVEUR,
            grace: 0,
            defi: defi_ok.clone(),
            attendu: Err("cle_appareil"),
        },
        Cas {
            nom: "cle_appareil_differente",
            description: "Laissez-passer valide, mais défi signé par une autre clé que cnf.pk : le laissez-passer seul ne suffit pas.",
            publication: "jeu_de_cles.json",
            laissez_passer: site.signer(&normal()),
            server_id: SERVEUR,
            grace: 0,
            defi: Some((DEFI.to_string(), autre_appareil.signer_defi(SERVEUR, DEFI))),
            attendu: Err("defi"),
        },
        Cas {
            nom: "defi_autre_serveur",
            description: "Défi signé par la bonne clé, mais pour un autre server_id.",
            publication: "jeu_de_cles.json",
            laissez_passer: site.signer(&normal()),
            server_id: SERVEUR,
            grace: 0,
            defi: Some((DEFI.to_string(), appareil.signer_defi(AUTRE_SERVEUR, DEFI))),
            attendu: Err("defi"),
        },
        Cas {
            nom: "defi_autre_valeur",
            description: "Défi signé par la bonne clé, mais pour une autre valeur de défi (rejeu).",
            publication: "jeu_de_cles.json",
            laissez_passer: site.signer(&normal()),
            server_id: SERVEUR,
            grace: 0,
            defi: Some((
                DEFI.to_string(),
                appareil.signer_defi(SERVEUR, "un-ancien-defi"),
            )),
            attendu: Err("defi"),
        },
        Cas {
            nom: "jeu_non_signe_par_la_racine",
            description: "Le jeu de clés est signé par une autre racine : il est refusé, donc rien ne passe.",
            publication: "jeu_de_cles_autre_racine.json",
            laissez_passer: site.signer(&normal()),
            server_id: SERVEUR,
            grace: 0,
            defi: defi_ok.clone(),
            attendu: Err("jeu_de_cles_signature"),
        },
    ];

    let mut index = Vec::new();
    for c in cas.drain(..) {
        let fichier = format!("{}.json", c.nom);
        let attendu = match c.attendu {
            Ok(()) => json!({"resultat": "accepte", "sub": COMPTE, "jti": "jti-0001"}),
            Err(code) => json!({"resultat": "refuse", "code": code}),
        };
        ecrire(
            &fichier,
            &json!({
                "nom": c.nom,
                "description": c.description,
                "publication": c.publication,
                "laissez_passer": c.laissez_passer,
                "contexte": {"server_id": c.server_id, "maintenant": MAINTENANT, "grace_expiration": c.grace},
                "defi": c.defi.map(|(d, s)| json!({"defi": d, "signature": s})),
                "attendu": attendu,
            }),
        );
        index.push(fichier);
    }

    // Défi d'enregistrement du serveur sur le relais.
    let defi_srv = "ZW5yZWdpc3RyZW1lbnQtZGUtdGVzdA";
    ecrire(
        "defi_serveur.json",
        &json!({
            "nom": "defi_serveur",
            "description": "Le serveur signe le défi d'enregistrement du relais avec sa clé Ed25519. Message : « tune-remote/serveur/v1\\n<server_id>\\n<défi> ».",
            "server_id": SERVEUR,
            "cle_serveur": serveur.cle_publique(),
            "defi": defi_srv,
            "signature": serveur.signer_defi(SERVEUR, defi_srv),
            "signature_autre_serveur": serveur.signer_defi(AUTRE_SERVEUR, defi_srv),
            "inscription": {
                "description": "Preuve d'inscription de la clé sur le compte. Message : « tune-remote/inscription/v1\\n<server_id>\\n<clé publique> ».",
                "preuve": serveur.signer(&tune_laissez_passer::message_inscription_serveur(SERVEUR, &serveur.cle_publique())),
            },
        }),
    );
    ecrire(
        "index.json",
        &json!({"racine": "cles_de_test.json", "cas": index, "defi_serveur": "defi_serveur.json"}),
    );
    println!("vecteurs écrits dans {}", dossier().display());
}

fn hex(octets: &[u8]) -> String {
    octets.iter().map(|o| format!("{o:02x}")).collect()
}
