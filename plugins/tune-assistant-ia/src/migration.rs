//! Étape 2 — les anciennes clés d'API, EN CLAIR dans `settings`, passent au
//! coffre de l'assistant.
//!
//! # Ce qui existait
//!
//! Avant l'assistant, deux usages lisaient une clé en clair :
//!
//! - l'ancien assistant `POST /ai/query` (`tune-server/src/routes/ai.rs`) :
//!   `anthropic_api_key`, modèle `anthropic_model` ;
//! - la traduction des requêtes d'ambiance (`tune_core::ai::translate`) :
//!   `anthropic_api_key`, puis `openai_api_key`, puis `gemini_api_key` — la
//!   première posée gagne.
//!
//! Aucun plafond ne bornait ces appels, et une copie de `tune.db` emportait
//! les clés.
//!
//! # La migration
//!
//! Pour chaque clé héritée posée et non blanche, dans l'ordre de
//! [`CLES_HERITEES`] :
//!
//! 1. **adoptée** — l'assistant n'a pas encore de clé, et son fournisseur est
//!    vide ou celui de cette clé : la clé est SCELLÉE comme clé de
//!    l'assistant, et le fournisseur posé s'il était vide ;
//! 2. **déjà présente** — l'assistant a déjà cette même clé : rien à sceller ;
//! 3. **conservée à part** — l'assistant a une autre clé, ou un autre
//!    fournisseur : la clé héritée est scellée sous
//!    `assistant_ia_api_key_heritee_<famille>` pour ne rien perdre ; elle ne
//!    sert à rien tant que l'utilisateur ne la ressaisit pas.
//!
//! Dans les trois cas, la valeur EN CLAIR n'est effacée qu'APRÈS avoir relu
//! la valeur scellée et vérifié qu'elle s'ouvre sur la même clé. Un échec
//! (coffre illisible, disque plein) laisse la clé en clair à sa place : rien
//! n'est perdu, la migration sera retentée au prochain usage.
//!
//! Idempotente : une fois les clés en clair effacées, il n'y a plus rien à
//! migrer, et rien n'est écrit — ni réglage, ni fichier de clé maîtresse.
//!
//! Le modèle `anthropic_model` n'est PAS repris : l'ancien défaut n'a pas de
//! tarif documenté dans [`crate::couts`], et un modèle sans tarif est refusé
//! (`tarif_inconnu`). L'assistant part sur son modèle par défaut.

use std::sync::Arc;

use serde::Serialize;
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;

use crate::coffre::Coffre;
use crate::fournisseur::Famille;
use crate::reglages::{CLE_API_CHIFFREE, CLE_FOURNISSEUR};

/// Les clés héritées, dans l'ordre de préférence de l'ancienne traduction.
pub const CLES_HERITEES: [(&str, Famille); 3] = [
    ("anthropic_api_key", Famille::Anthropic),
    ("gemini_api_key", Famille::Gemini),
    ("openai_api_key", Famille::CompatibleOpenai),
];

/// Le réglage où une clé héritée est conservée, scellée, quand l'assistant
/// a déjà la sienne. Le nom porte `api_key` : [`tune_core::secrets`] le masque.
pub fn cle_conservee(famille: Famille) -> String {
    format!("assistant_ia_api_key_heritee_{}", famille.nom())
}

/// Ce qu'est devenue une clé héritée.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Issue {
    Adoptee,
    DejaPresente,
    ConserveeAPart,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Rapport {
    /// `(réglage hérité, issue)`, dans l'ordre de [`CLES_HERITEES`].
    pub migrees: Vec<(String, Issue)>,
}

/// Une clé héritée est-elle encore posée, en clair ? Lecture seule.
pub fn reste_une_cle_heritee(db: &Arc<dyn DbBackend>) -> bool {
    let s = SettingsRepo::with_backend(db.clone());
    CLES_HERITEES.iter().any(|(nom, _)| lire(&s, nom).is_some())
}

fn lire(s: &SettingsRepo, nom: &str) -> Option<String> {
    s.get(nom)
        .ok()
        .flatten()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Scelle `clair`, range la valeur sous `reglage`, la RELIT et vérifie qu'elle
/// s'ouvre sur `clair`. Rien n'est effacé ici.
fn sceller_et_verifier(
    s: &SettingsRepo,
    coffre: &Coffre,
    reglage: &str,
    clair: &str,
) -> Result<(), String> {
    let scelle = coffre.sceller(clair)?;
    s.set(reglage, &scelle)?;
    let relu = s
        .get(reglage)?
        .ok_or_else(|| format!("{reglage} : valeur scellée introuvable après écriture"))?;
    if coffre.ouvrir(&relu)? != clair {
        return Err(format!("{reglage} : la valeur scellée ne rend pas la clé"));
    }
    Ok(())
}

/// Migre les clés héritées. Voir le module.
pub fn migrer(db: &Arc<dyn DbBackend>, coffre: &Coffre) -> Result<Rapport, String> {
    let s = SettingsRepo::with_backend(db.clone());
    let mut rapport = Rapport::default();
    for (nom, famille) in CLES_HERITEES {
        let Some(clair) = lire(&s, nom) else {
            continue;
        };
        let fournisseur = lire(&s, CLE_FOURNISSEUR).and_then(|f| Famille::depuis(&f));
        // La clé de l'assistant, si elle est posée ET s'ouvre. Une clé
        // illisible (fichier de clé maîtresse perdu) ne vaut pas mieux
        // qu'aucune : la clé héritée peut la remplacer.
        let actuelle = lire(&s, CLE_API_CHIFFREE).and_then(|v| coffre.ouvrir(&v).ok());
        let issue = match (&actuelle, fournisseur) {
            (Some(a), _) if *a == clair => Issue::DejaPresente,
            (None, None) => Issue::Adoptee,
            (None, Some(f)) if f == famille => Issue::Adoptee,
            _ => Issue::ConserveeAPart,
        };
        match issue {
            Issue::Adoptee => {
                sceller_et_verifier(&s, coffre, CLE_API_CHIFFREE, &clair)?;
                if fournisseur.is_none() {
                    s.set(CLE_FOURNISSEUR, famille.nom())?;
                }
            }
            Issue::DejaPresente => {}
            Issue::ConserveeAPart => {
                sceller_et_verifier(&s, coffre, &cle_conservee(famille), &clair)?;
            }
        }
        // La valeur scellée est relue et s'ouvre : la valeur en clair peut
        // partir.
        s.delete(nom)?;
        tracing::info!(
            reglage = nom,
            issue = ?issue,
            "assistant_ia_cle_heritee_migree"
        );
        rapport.migrees.push((nom.to_string(), issue));
    }
    Ok(rapport)
}

/// Les réglages où le greffon range une valeur SCELLÉE.
pub fn reglages_scelles() -> Vec<String> {
    std::iter::once(CLE_API_CHIFFREE.to_string())
        .chain(CLES_HERITEES.iter().map(|(_, f)| cle_conservee(*f)))
        .collect()
}

/// Le passage au coffre partagé (jonction avec `tune_core::coffre`) : chaque
/// valeur que l'ancienne clé du greffon (`assistant-ia.cle`) a scellée est
/// rescellée sous la clé partagée du serveur, rangée, RELUE, et vérifiée.
///
/// Rend le nombre de valeurs passées. Une valeur déjà sous la clé partagée
/// n'est pas réécrite ; une valeur qu'aucune clé n'ouvre est laissée telle
/// quelle (l'écran dit déjà `cle_illisible`). Un échec d'écriture laisse
/// l'ancienne valeur, que l'ancienne clé ouvre toujours : rien n'est perdu,
/// le passage sera retenté au prochain démarrage. Idempotent.
pub fn passer_au_coffre_partage(db: &Arc<dyn DbBackend>, coffre: &Coffre) -> Result<usize, String> {
    let s = SettingsRepo::with_backend(db.clone());
    let mut passees = 0;
    for nom in reglages_scelles() {
        let Some(scelle) = lire(&s, &nom) else {
            continue;
        };
        let nouvelle = match coffre.resceller(&scelle) {
            Ok(Some(n)) => n,
            Ok(None) => continue,
            Err(_) => {
                // Sans la valeur, ni l'erreur (elle ne la porte pas, mais rien
                // à gagner à l'écrire).
                tracing::warn!(reglage = %nom, "assistant_ia_valeur_scellee_illisible");
                continue;
            }
        };
        s.set(&nom, &nouvelle)?;
        let relue = s
            .get(&nom)?
            .ok_or_else(|| format!("{nom} : valeur scellée introuvable après écriture"))?;
        if coffre.resceller(&relue)?.is_some() {
            return Err(format!(
                "{nom} : la valeur relue n'est pas sous la clé partagée"
            ));
        }
        passees += 1;
        tracing::info!(reglage = %nom, "assistant_ia_cle_passee_au_coffre_partage");
    }
    Ok(passees)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reglages::{EtatCle, cle_en_clair};
    use tune_core::db::sqlite::SqliteDb;

    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    fn reglage(db: &Arc<dyn DbBackend>, nom: &str) -> Option<String> {
        SettingsRepo::with_backend(db.clone()).get(nom).unwrap()
    }

    const CLE: &str = "sk-ant-api03-HERITEE-EN-CLAIR-1234";

    #[test]
    fn aller_la_cle_en_clair_est_scellee_puis_effacee() {
        let db = base();
        let dir = tempfile::tempdir().unwrap();
        let coffre = crate::coffre::du_greffon(dir.path());
        SettingsRepo::with_backend(db.clone())
            .set("anthropic_api_key", CLE)
            .unwrap();

        let r = migrer(&db, &coffre).unwrap();

        assert_eq!(
            r.migrees,
            vec![("anthropic_api_key".to_string(), Issue::Adoptee)]
        );
        assert_eq!(
            reglage(&db, "anthropic_api_key"),
            None,
            "la valeur en clair doit partir"
        );
        assert_eq!(reglage(&db, CLE_FOURNISSEUR).as_deref(), Some("anthropic"));
        let scelle = reglage(&db, CLE_API_CHIFFREE).unwrap();
        assert!(!scelle.contains("HERITEE"), "{scelle}");
        match cle_en_clair(&db, &coffre) {
            EtatCle::Ouverte(k) => assert_eq!(k, CLE),
            _ => panic!("la clé migrée doit s'ouvrir"),
        }
    }

    #[test]
    fn idempotence_une_seconde_migration_ne_change_rien() {
        let db = base();
        let dir = tempfile::tempdir().unwrap();
        let coffre = crate::coffre::du_greffon(dir.path());
        SettingsRepo::with_backend(db.clone())
            .set("anthropic_api_key", CLE)
            .unwrap();
        migrer(&db, &coffre).unwrap();
        let scelle = reglage(&db, CLE_API_CHIFFREE).unwrap();

        let r = migrer(&db, &coffre).unwrap();

        assert!(r.migrees.is_empty(), "{r:?}");
        assert_eq!(
            reglage(&db, CLE_API_CHIFFREE).unwrap(),
            scelle,
            "la valeur scellée ne doit pas être réécrite"
        );
        // La même clé, reposée en clair par un ancien client : elle est
        // reconnue, et seulement effacée.
        SettingsRepo::with_backend(db.clone())
            .set("anthropic_api_key", CLE)
            .unwrap();
        let r = migrer(&db, &coffre).unwrap();
        assert_eq!(
            r.migrees,
            vec![("anthropic_api_key".to_string(), Issue::DejaPresente)]
        );
        assert_eq!(reglage(&db, CLE_API_CHIFFREE).unwrap(), scelle);
        assert_eq!(reglage(&db, "anthropic_api_key"), None);
    }

    #[test]
    fn cle_absente_rien_n_est_ecrit_pas_meme_la_cle_maitresse() {
        let db = base();
        let dir = tempfile::tempdir().unwrap();
        let coffre = crate::coffre::du_greffon(dir.path());
        // Une clé blanche ne compte pas.
        SettingsRepo::with_backend(db.clone())
            .set("openai_api_key", "   ")
            .unwrap();

        let r = migrer(&db, &coffre).unwrap();

        assert!(r.migrees.is_empty());
        assert_eq!(reglage(&db, CLE_API_CHIFFREE), None);
        assert_eq!(reglage(&db, CLE_FOURNISSEUR), None);
        for fichier in [crate::coffre::FICHIER_CLE, tune_core::coffre::FICHIER_CLE] {
            assert!(
                !dir.path().join(fichier).exists(),
                "aucune clé maîtresse ne doit naître sans rien à sceller ({fichier})"
            );
        }
        assert_eq!(passer_au_coffre_partage(&db, &coffre).unwrap(), 0);
        assert!(!dir.path().join(tune_core::coffre::FICHIER_CLE).exists());
        assert!(!reste_une_cle_heritee(&db));
    }

    #[test]
    fn une_autre_cle_de_l_assistant_n_est_pas_ecrasee_l_heritee_est_conservee_scellee() {
        let db = base();
        let dir = tempfile::tempdir().unwrap();
        let coffre = crate::coffre::du_greffon(dir.path());
        let s = SettingsRepo::with_backend(db.clone());
        s.set(CLE_FOURNISSEUR, "gemini").unwrap();
        s.set(
            CLE_API_CHIFFREE,
            &coffre.sceller("cle-google-AIza").unwrap(),
        )
        .unwrap();
        s.set("anthropic_api_key", CLE).unwrap();
        s.set("gemini_api_key", "cle-google-AIza").unwrap();

        let r = migrer(&db, &coffre).unwrap();

        assert_eq!(
            r.migrees,
            vec![
                ("anthropic_api_key".to_string(), Issue::ConserveeAPart),
                ("gemini_api_key".to_string(), Issue::DejaPresente),
            ]
        );
        assert_eq!(reglage(&db, CLE_FOURNISSEUR).as_deref(), Some("gemini"));
        match cle_en_clair(&db, &coffre) {
            EtatCle::Ouverte(k) => assert_eq!(k, "cle-google-AIza"),
            _ => panic!("la clé de l'assistant doit rester"),
        }
        let conservee = reglage(&db, &cle_conservee(Famille::Anthropic)).unwrap();
        assert_eq!(coffre.ouvrir(&conservee).unwrap(), CLE);
        assert!(!reste_une_cle_heritee(&db));
        assert!(tune_core::secrets::est_secret(&cle_conservee(
            Famille::Anthropic
        )));
    }
}
