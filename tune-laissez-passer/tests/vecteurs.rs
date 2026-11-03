//! Rejoue les fichiers de `vecteurs/` TELS QU'ILS SONT ÉCRITS — le contrat
//! que le site PHP rejoue de son côté.
//!
//! Le test lit `index.json`, puis chaque cas : il charge le jeu de clés avec
//! la racine de `cles_de_test.json`, vérifie le laissez-passer dans le
//! contexte donné, puis la signature du défi, et compare au résultat attendu.

use std::path::{Path, PathBuf};

use serde_json::Value;
use tune_laissez_passer::{Contexte, JeuDeCles, Publication, Refus, verifier_defi_serveur};

fn dossier() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("vecteurs")
}

fn lire(nom: &str) -> Value {
    let chemin = dossier().join(nom);
    let texte = std::fs::read_to_string(&chemin)
        .unwrap_or_else(|e| panic!("{} illisible : {e}", chemin.display()));
    serde_json::from_str(&texte).unwrap()
}

/// Le résultat d'un cas : `Ok(revendications)` ou le code du refus.
fn jouer(cas: &Value, racine: &str) -> Result<Value, &'static str> {
    let publication: Publication =
        serde_json::from_value(lire(cas["publication"].as_str().unwrap())).unwrap();
    let jeu = JeuDeCles::depuis_publication(&publication, racine).map_err(|r| r.code())?;
    let ctx = &cas["contexte"];
    let server_id = ctx["server_id"].as_str().unwrap();
    let lp = jeu
        .verifier(
            cas["laissez_passer"].as_str().unwrap(),
            &Contexte {
                server_id,
                maintenant: ctx["maintenant"].as_i64().unwrap(),
                grace_expiration: ctx["grace_expiration"].as_i64().unwrap(),
            },
        )
        .map_err(|r| r.code())?;
    if let Some(defi) = cas["defi"].as_object() {
        lp.verifier_defi(
            server_id,
            defi["defi"].as_str().unwrap(),
            defi["signature"].as_str().unwrap(),
        )
        .map_err(|r| r.code())?;
    }
    Ok(serde_json::to_value(&lp.revendications).unwrap())
}

#[test]
fn chaque_vecteur_donne_le_resultat_attendu() {
    let index = lire("index.json");
    let racine = lire(index["racine"].as_str().unwrap())["racine_minisign"]
        .as_str()
        .unwrap()
        .to_string();
    let cas = index["cas"].as_array().unwrap();
    // Le cahier en exige au moins sept familles : valide, expiré, mauvaise
    // signature, mauvais alg, mauvaise audience, clé d'appareil, défi.
    assert!(cas.len() >= 7, "index trop court : {}", cas.len());

    let mut ecarts = Vec::new();
    for nom in cas {
        let v = lire(nom.as_str().unwrap());
        let attendu = &v["attendu"];
        let obtenu = jouer(&v, &racine);
        let conforme = match (attendu["resultat"].as_str().unwrap(), &obtenu) {
            ("accepte", Ok(r)) => r["sub"] == attendu["sub"] && r["jti"] == attendu["jti"],
            ("refuse", Err(code)) => attendu["code"].as_str() == Some(*code),
            _ => false,
        };
        if !conforme {
            ecarts.push(format!(
                "{} : attendu {attendu}, obtenu {obtenu:?}",
                v["nom"]
            ));
        }
    }
    assert!(
        ecarts.is_empty(),
        "vecteurs en écart :\n{}",
        ecarts.join("\n")
    );
}

#[test]
fn les_familles_exigees_sont_presentes() {
    let index = lire("index.json");
    let noms: Vec<&str> = index["cas"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_str().unwrap())
        .collect();
    for exige in [
        "valide.json",
        "expire.json",
        "mauvaise_signature.json",
        "mauvais_alg_none.json",
        "mauvais_alg_hs256.json",
        "mauvaise_audience.json",
        "cle_appareil_differente.json",
        "kid_inconnu.json",
        "mauvais_serveur.json",
        "jeu_non_signe_par_la_racine.json",
    ] {
        assert!(noms.contains(&exige), "{exige} manque à l'index");
    }
}

#[test]
fn le_defi_du_serveur_se_verifie_et_ne_vaut_que_pour_son_server_id() {
    let v = lire("defi_serveur.json");
    let cle = v["cle_serveur"].as_str().unwrap();
    let sid = v["server_id"].as_str().unwrap();
    let defi = v["defi"].as_str().unwrap();
    assert_eq!(
        verifier_defi_serveur(cle, sid, defi, v["signature"].as_str().unwrap()),
        Ok(())
    );
    assert_eq!(
        verifier_defi_serveur(
            cle,
            sid,
            defi,
            v["signature_autre_serveur"].as_str().unwrap()
        ),
        Err(Refus::Defi)
    );
    let preuve = v["inscription"]["preuve"].as_str().unwrap();
    assert_eq!(
        tune_laissez_passer::verifier_inscription_serveur(cle, sid, preuve),
        Ok(())
    );
    // La preuve d'inscription n'est pas une signature de défi, et inversement.
    assert_eq!(
        verifier_defi_serveur(cle, sid, defi, preuve),
        Err(Refus::Defi)
    );
}

/// Les clés de test publiées sont bien celles que la fabrique utilise : un
/// test du relais qui signe avec `fabrique::cles_de_test` parle le même
/// langage que les vecteurs.
#[test]
fn les_cles_publiees_sont_celles_de_la_fabrique() {
    use tune_laissez_passer::fabrique::cles_de_test;
    let cles = lire("cles_de_test.json");
    assert_eq!(
        cles["racine_minisign"].as_str().unwrap(),
        cles_de_test::racine().cle_publique_minisign()
    );
    assert_eq!(
        cles["site"]["pk"].as_str().unwrap(),
        cles_de_test::site().cle_publique()
    );
    assert_eq!(cles["site"]["kid"].as_str().unwrap(), cles_de_test::KID);
}
