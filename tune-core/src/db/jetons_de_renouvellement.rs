//! Les jetons de renouvellement de session (rc14, `POST /auth/refresh`).
//!
//! Une connexion « rester connecté » ouvre une **famille** : un jeton d'accès
//! court (le JWT de 24 h, qui porte l'identifiant de la session dans `sid`,
//! #9445) et un jeton de renouvellement opaque, valable
//! [`DUREE_RENOUVELLEMENT`] secondes. La famille porte le même identifiant que
//! la session inscrite.
//!
//! - **Rotation** : chaque renouvellement consomme le jeton présenté et en
//!   rend un nouveau, de la même famille, valable de nouveau 30 jours. La
//!   fenêtre glisse tant que la session sert ; 30 jours sans usage, elle meurt.
//! - **Réutilisation** : un jeton déjà consommé qui revient, au-delà de
//!   [`DELAI_DE_GRACE`], signe un vol (le voleur ou la victime a déjà tourné).
//!   Toute la famille est révoquée : les deux bouts doivent se reconnecter.
//!   Dans le délai de grâce, la course de deux onglets ou de deux tâches du
//!   même appareil est refusée SANS révocation.
//! - **Mot de passe** : la famille retient une empreinte du hachage du mot de
//!   passe à sa création. Un mot de passe changé, par quelque chemin que ce
//!   soit, la rend caduque au renouvellement suivant.
//! - **Stockage** : seule l'empreinte SHA-256 du jeton est en base, comme les
//!   clés Subsonic ([`super::cles_clients`]). Le jeton ne se relit jamais.
//!
//! Les horodatages sont des secondes Unix (BIGINT dans les deux moteurs).
//! Chaque fonction reçoit `maintenant` : les épreuves avancent l'horloge.

use std::sync::Arc;

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use super::backend::{DbBackend, ToSqlValue};

/// Préfixe lisible des jetons de renouvellement (repérables par un analyseur
/// de secrets, distincts des clés Subsonic `tune_sk_`).
pub const PREFIXE_JETON: &str = "tune_rt_";

/// Durée de vie d'un jeton de renouvellement, prolongée à chaque usage :
/// 30 jours.
pub const DUREE_RENOUVELLEMENT: i64 = 30 * 86400;

/// Délai pendant lequel un jeton qui vient d'être consommé peut revenir sans
/// révoquer sa famille (deux onglets, deux tâches de fond du même appareil).
pub const DELAI_DE_GRACE: i64 = 30;

/// L'empreinte stockée d'un jeton : SHA-256 en hexadécimal minuscule.
pub fn empreinte(jeton: &str) -> String {
    hex(&Sha256::digest(jeton.as_bytes()))
}

fn hex(octets: &[u8]) -> String {
    octets.iter().map(|b| format!("{b:02x}")).collect()
}

fn nouveau_jeton() -> Result<String, String> {
    let mut octets = [0u8; 32];
    getrandom::getrandom(&mut octets).map_err(|e| format!("random source unavailable: {e}"))?;
    Ok(format!("{PREFIXE_JETON}{}", hex(&octets)))
}

/// L'empreinte du mot de passe enregistré d'un profil, telle que la famille
/// la retient : un résumé des DEUX hachages (argon2 et ancien SHA-256), qui
/// change dès que l'un change. `None` : le profil n'existe plus.
pub fn empreinte_du_mot_de_passe(db: &Arc<dyn DbBackend>, profile_id: i64) -> Option<String> {
    let ligne = db
        .query_one_strong(
            "SELECT password_hash_v2, password_hash FROM profiles WHERE id = ?",
            &[&profile_id as &dyn ToSqlValue],
        )
        .ok()
        .flatten()?;
    let v2 = ligne.first().and_then(|v| v.as_string()).unwrap_or_default();
    let ancien = ligne.get(1).and_then(|v| v.as_string()).unwrap_or_default();
    Some(hex(&Sha256::digest(format!("{v2}\n{ancien}").as_bytes()))[..32].to_string())
}

/// Une session ouverte ou renouvelée : le jeton en clair (rendu au client,
/// une seule fois) et sa famille.
#[derive(Debug, Clone)]
pub struct Emis {
    pub famille: String,
    pub profile_id: i64,
    pub jeton: String,
    pub expire_le: i64,
}

/// Pourquoi un renouvellement est refusé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refus {
    /// Jeton inconnu (jamais émis, ou purgé après expiration).
    Inconnu,
    /// Jeton expiré : 30 jours sans usage.
    Expire,
    /// Famille révoquée (déconnexion, secrets renouvelés, mot de passe
    /// changé, vol détecté).
    Revoque,
    /// Jeton déjà consommé AU-DELÀ du délai de grâce : la famille vient
    /// d'être révoquée.
    Reutilise,
    /// Jeton consommé il y a moins de [`DELAI_DE_GRACE`] secondes : un autre
    /// onglet ou une autre tâche l'a déjà tourné. Rien n'est révoqué ; le
    /// client relit le jeton qu'il a rangé.
    DejaTourne,
    /// La base n'a pas répondu.
    Base,
}

pub struct JetonsDeRenouvellementRepo {
    db: Arc<dyn DbBackend>,
}

impl JetonsDeRenouvellementRepo {
    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        Self { db }
    }

    /// Ouvre une famille pour `profile_id` (à la connexion). Purge au passage
    /// les jetons expirés de toutes les familles.
    pub fn ouvrir(
        &self,
        profile_id: i64,
        client: Option<&str>,
        maintenant: i64,
    ) -> Result<Emis, String> {
        let famille = uuid::Uuid::new_v4().simple().to_string();
        self.ouvrir_famille(&famille, profile_id, client, maintenant)
    }

    /// Ouvre la famille `famille` — côté serveur, l'identifiant de la session
    /// inscrite (`sid`, #9445) : une session, une famille.
    pub fn ouvrir_famille(
        &self,
        famille: &str,
        profile_id: i64,
        client: Option<&str>,
        maintenant: i64,
    ) -> Result<Emis, String> {
        self.purger(maintenant);
        let empreinte_mdp = empreinte_du_mot_de_passe(&self.db, profile_id)
            .ok_or_else(|| "profile not found".to_string())?;
        let famille = famille.to_string();
        let jeton = nouveau_jeton()?;
        let expire_le = maintenant + DUREE_RENOUVELLEMENT;
        let client: Option<String> = client.map(|c| c.chars().take(120).collect());
        self.db.execute(
            "INSERT INTO jetons_de_renouvellement \
             (id, famille, profile_id, empreinte_sha256, empreinte_mot_de_passe, client, cree_le, expire_le) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                &uuid::Uuid::new_v4().to_string() as &dyn ToSqlValue,
                &famille,
                &profile_id,
                &empreinte(&jeton),
                &empreinte_mdp,
                &client,
                &maintenant,
                &expire_le,
            ],
        )?;
        Ok(Emis {
            famille,
            profile_id,
            jeton,
            expire_le,
        })
    }

    /// Consomme `jeton` et en rend un nouveau de la même famille.
    pub fn renouveler(&self, jeton: &str, maintenant: i64) -> Result<Emis, Refus> {
        if !jeton.starts_with(PREFIXE_JETON) {
            return Err(Refus::Inconnu);
        }
        let attendue = empreinte(jeton);
        // Lecture forte : un jeton tourné à l'instant par la connexion
        // d'écriture doit se voir consommé (instantané WAL des lectures).
        let ligne = self
            .db
            .query_one_strong(
                "SELECT id, famille, profile_id, empreinte_mot_de_passe, client, expire_le, \
                 remplace_le, revoque_le, empreinte_sha256 \
                 FROM jetons_de_renouvellement WHERE empreinte_sha256 = ?",
                &[&attendue as &dyn ToSqlValue],
            )
            .map_err(|_| Refus::Base)?
            .ok_or(Refus::Inconnu)?;
        let stockee = ligne.get(8).and_then(|v| v.as_string()).unwrap_or_default();
        if !bool::from(stockee.as_bytes().ct_eq(attendue.as_bytes())) {
            return Err(Refus::Inconnu);
        }
        let id = ligne.first().and_then(|v| v.as_string()).unwrap_or_default();
        let famille = ligne.get(1).and_then(|v| v.as_string()).unwrap_or_default();
        let profile_id = ligne.get(2).and_then(|v| v.as_i64()).unwrap_or(-1);
        let empreinte_mdp = ligne.get(3).and_then(|v| v.as_string()).unwrap_or_default();
        let client = ligne.get(4).and_then(|v| v.as_string());
        let expire_le = ligne.get(5).and_then(|v| v.as_i64()).unwrap_or(0);
        let remplace_le = ligne.get(6).and_then(|v| v.as_i64());
        let revoque_le = ligne.get(7).and_then(|v| v.as_i64());

        if revoque_le.is_some() {
            return Err(Refus::Revoque);
        }
        if let Some(quand) = remplace_le {
            if maintenant - quand <= DELAI_DE_GRACE {
                return Err(Refus::DejaTourne);
            }
            tracing::warn!(famille = %famille, profile_id, "jeton_de_renouvellement_reutilise_famille_revoquee");
            self.revoquer_famille(&famille, maintenant);
            return Err(Refus::Reutilise);
        }
        if expire_le <= maintenant {
            self.revoquer_famille(&famille, maintenant);
            return Err(Refus::Expire);
        }
        // Le mot de passe a changé, ou le profil n'existe plus.
        if empreinte_du_mot_de_passe(&self.db, profile_id).as_deref() != Some(empreinte_mdp.as_str())
        {
            self.revoquer_famille(&famille, maintenant);
            return Err(Refus::Revoque);
        }

        let nouveau = nouveau_jeton().map_err(|_| Refus::Base)?;
        let nouvelle_echeance = maintenant + DUREE_RENOUVELLEMENT;
        let nouvel_id = uuid::Uuid::new_v4().to_string();
        let nouvelle_empreinte = empreinte(&nouveau);
        let mut pris = false;
        self.db
            .write_tx(&mut |tx| {
                // Comparer-et-échanger : de deux renouvellements simultanés du
                // même jeton, un seul le consomme.
                let n = tx.execute(
                    "UPDATE jetons_de_renouvellement SET remplace_le = ? \
                     WHERE id = ? AND remplace_le IS NULL AND revoque_le IS NULL",
                    &[&maintenant as &dyn ToSqlValue, &id],
                )?;
                pris = n == 1;
                if !pris {
                    return Ok(());
                }
                tx.execute(
                    "INSERT INTO jetons_de_renouvellement \
                     (id, famille, profile_id, empreinte_sha256, empreinte_mot_de_passe, client, cree_le, expire_le) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                    &[
                        &nouvel_id as &dyn ToSqlValue,
                        &famille,
                        &profile_id,
                        &nouvelle_empreinte,
                        &empreinte_mdp,
                        &client,
                        &maintenant,
                        &nouvelle_echeance,
                    ],
                )?;
                Ok(())
            })
            .map_err(|_| Refus::Base)?;
        if !pris {
            return Err(Refus::DejaTourne);
        }
        Ok(Emis {
            famille,
            profile_id,
            jeton: nouveau,
            expire_le: nouvelle_echeance,
        })
    }

    /// La famille et le profil du jeton présenté, s'il est connu
    /// (déconnexion).
    pub fn session_du_jeton(&self, jeton: &str) -> Option<(String, i64)> {
        if !jeton.starts_with(PREFIXE_JETON) {
            return None;
        }
        let ligne = self
            .db
            .query_one_strong(
                "SELECT famille, profile_id FROM jetons_de_renouvellement WHERE empreinte_sha256 = ?",
                &[&empreinte(jeton) as &dyn ToSqlValue],
            )
            .ok()
            .flatten()?;
        Some((
            ligne.first().and_then(|v| v.as_string())?,
            ligne.get(1).and_then(|v| v.as_i64())?,
        ))
    }

    /// La famille est-elle vivante : ni révoquée, ni expirée ?
    pub fn famille_active(&self, famille: &str, maintenant: i64) -> bool {
        self.db
            .query_one_frais(
                "SELECT 1 FROM jetons_de_renouvellement \
                 WHERE famille = ? AND revoque_le IS NULL AND expire_le > ? LIMIT 1",
                &[&famille as &dyn ToSqlValue, &maintenant],
            )
            .ok()
            .flatten()
            .is_some()
    }

    /// Révoque une famille : tous ses jetons, consommés ou non.
    pub fn revoquer_famille(&self, famille: &str, maintenant: i64) -> usize {
        self.db
            .execute(
                "UPDATE jetons_de_renouvellement SET revoque_le = ? \
                 WHERE famille = ? AND revoque_le IS NULL",
                &[&maintenant as &dyn ToSqlValue, &famille],
            )
            .unwrap_or(0)
    }

    /// Révoque toutes les familles d'un profil (« se déconnecter partout »).
    pub fn revoquer_profil(&self, profile_id: i64, maintenant: i64) -> usize {
        self.db
            .execute(
                "UPDATE jetons_de_renouvellement SET revoque_le = ? \
                 WHERE profile_id = ? AND revoque_le IS NULL",
                &[&maintenant as &dyn ToSqlValue, &profile_id],
            )
            .unwrap_or(0)
    }

    /// Révoque TOUTES les familles (secrets renouvelés).
    pub fn revoquer_tout(&self, maintenant: i64) -> usize {
        self.db
            .execute(
                "UPDATE jetons_de_renouvellement SET revoque_le = ? WHERE revoque_le IS NULL",
                &[&maintenant as &dyn ToSqlValue],
            )
            .unwrap_or(0)
    }

    /// Efface les jetons expirés (consommés, révoqués ou non) : au-delà de
    /// leur échéance, ils ne servent plus même à détecter une réutilisation.
    pub fn purger(&self, maintenant: i64) -> usize {
        self.db
            .execute(
                "DELETE FROM jetons_de_renouvellement WHERE expire_le <= ?",
                &[&maintenant as &dyn ToSqlValue],
            )
            .unwrap_or(0)
    }
}

#[cfg(test)]
#[path = "jetons_de_renouvellement_tests.rs"]
mod tests;
