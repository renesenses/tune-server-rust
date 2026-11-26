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
//! - **Plafond absolu** : quel que soit l'usage, une famille meurt
//!   [`PLAFOND_SESSION`] après la connexion (90 jours).
//! - **Réutilisation** : un jeton déjà consommé qui revient, au-delà de
//!   [`DELAI_DE_GRACE`], signe un vol (le voleur ou la victime a déjà tourné).
//!   Toute la famille est révoquée : les deux bouts doivent se reconnecter.
//! - **Rejeu idempotent** : le successeur d'un jeton est DÉRIVÉ de lui
//!   (HMAC-SHA-256 sous la clé du serveur). Une réponse de renouvellement
//!   perdue (réseau mobile) et rejouée dans le délai de grâce rend donc le
//!   MÊME jeton suivant, sans rien révoquer — à condition que ce successeur
//!   n'ait pas encore servi, et au plus une fois par
//!   [`DELAI_ENTRE_REJEUX`] dans une famille. Un second rejeu, ou le rejeu
//!   d'un jeton dont le successeur a déjà tourné, révoque la famille : c'est
//!   la course gagnée par un voleur (il tourne juste avant la victime ; la
//!   victime rejoue, reçoit le même successeur ; au tour suivant, l'un des
//!   deux rejoue de nouveau, ou présente un jeton déjà tourné — révocation).
//! - **Mot de passe** : la famille retient une empreinte du hachage du mot de
//!   passe à sa création. Un mot de passe changé, par quelque chemin que ce
//!   soit, la rend caduque au renouvellement suivant.
//! - **Stockage** : seule l'empreinte SHA-256 du jeton est en base, comme les
//!   clés Subsonic ([`super::cles_clients`]). Le jeton ne se relit jamais.
//!
//! La clé s'appelle `jeton_id`, pas `id` : sous PostgreSQL, le moteur ajoute
//! `RETURNING id` aux INSERT des tables qui ont une colonne `id` et la lit en
//! entier — un identifiant TEXTE y échouerait après coup.
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

/// Délai pendant lequel un jeton qui vient d'être consommé peut être REJOUÉ
/// (réponse perdue) : 2 minutes. Sûr parce que le rejeu ne fabrique aucun
/// jeton neuf — il rend le successeur déjà émis, qui doit être encore
/// intact — et qu'il n'est toléré qu'une fois par [`DELAI_ENTRE_REJEUX`] :
/// un voleur ne gagne rien qu'il n'ait déjà, et sa présence se trahit au
/// renouvellement suivant. 2 min couvrent un délai d'attente HTTP (30-60 s)
/// et une nouvelle tentative après changement de réseau.
pub const DELAI_DE_GRACE: i64 = 120;

/// Un seul rejeu toléré par famille sur cette fenêtre : 7 jours.
pub const DELAI_ENTRE_REJEUX: i64 = 7 * 86400;

/// Plafond absolu d'une session, depuis la connexion : 90 jours.
pub const PLAFOND_SESSION: i64 = 90 * 86400;

/// L'empreinte stockée d'un jeton : SHA-256 en hexadécimal minuscule.
pub fn empreinte(jeton: &str) -> String {
    hex(&Sha256::digest(jeton.as_bytes()))
}

fn hex(octets: &[u8]) -> String {
    octets.iter().map(|b| format!("{b:02x}")).collect()
}

/// Le successeur d'un jeton : `tune_rt_` + HMAC-SHA-256(clé du serveur,
/// jeton). Déterministe — c'est ce qui rend le rejeu idempotent — et
/// imprévisible sans la clé.
pub fn successeur(cle: &[u8], jeton: &str) -> String {
    use hmac::{Hmac, Mac};
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(cle).expect("HMAC accepte toute clé");
    mac.update(jeton.as_bytes());
    format!("{PREFIXE_JETON}{}", hex(&mac.finalize().into_bytes()))
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
    let v2 = ligne
        .first()
        .and_then(|v| v.as_string())
        .unwrap_or_default();
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
    /// Jeton expiré : 30 jours sans usage, ou plafond de 90 jours atteint.
    Expire,
    /// Famille révoquée (déconnexion, secrets renouvelés, mot de passe
    /// changé, vol détecté).
    Revoque,
    /// Jeton déjà consommé au-delà du délai de grâce, second rejeu, ou rejeu
    /// d'un jeton dont le successeur a déjà tourné : la famille vient d'être
    /// révoquée.
    Reutilise,
    /// La base n'a pas répondu.
    Base,
}

/// Une ligne de la table, décodée.
struct Ligne {
    id: String,
    famille: String,
    profile_id: i64,
    empreinte_mdp: String,
    client: Option<String>,
    expire_le: i64,
    remplace_le: Option<i64>,
    revoque_le: Option<i64>,
    session_creee_le: i64,
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
             (jeton_id, famille, profile_id, empreinte_sha256, empreinte_mot_de_passe, client, cree_le, expire_le, session_creee_le) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                &uuid::Uuid::new_v4().to_string() as &dyn ToSqlValue,
                &famille,
                &profile_id,
                &empreinte(&jeton),
                &empreinte_mdp,
                &client,
                &maintenant,
                &expire_le,
                &maintenant,
            ],
        )?;
        Ok(Emis {
            famille,
            profile_id,
            jeton,
            expire_le,
        })
    }

    /// La ligne d'un jeton, par son empreinte (lecture forte : un jeton
    /// tourné à l'instant doit se voir consommé).
    fn ligne(&self, jeton: &str) -> Result<Option<Ligne>, Refus> {
        let attendue = empreinte(jeton);
        let Some(l) = self
            .db
            .query_one_strong(
                "SELECT jeton_id, famille, profile_id, empreinte_mot_de_passe, client, expire_le, \
                 remplace_le, revoque_le, empreinte_sha256, session_creee_le \
                 FROM jetons_de_renouvellement WHERE empreinte_sha256 = ?",
                &[&attendue as &dyn ToSqlValue],
            )
            .map_err(|_| Refus::Base)?
        else {
            return Ok(None);
        };
        let stockee = l.get(8).and_then(|v| v.as_string()).unwrap_or_default();
        if !bool::from(stockee.as_bytes().ct_eq(attendue.as_bytes())) {
            return Ok(None);
        }
        Ok(Some(Ligne {
            id: l.first().and_then(|v| v.as_string()).unwrap_or_default(),
            famille: l.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
            profile_id: l.get(2).and_then(|v| v.as_i64()).unwrap_or(-1),
            empreinte_mdp: l.get(3).and_then(|v| v.as_string()).unwrap_or_default(),
            client: l.get(4).and_then(|v| v.as_string()),
            expire_le: l.get(5).and_then(|v| v.as_i64()).unwrap_or(0),
            remplace_le: l.get(6).and_then(|v| v.as_i64()),
            revoque_le: l.get(7).and_then(|v| v.as_i64()),
            session_creee_le: l.get(9).and_then(|v| v.as_i64()).unwrap_or(0),
        }))
    }

    /// Consomme `jeton` et rend son successeur (même famille), dérivé sous
    /// `cle` (secret du serveur). Un rejeu dans le délai de grâce rend le
    /// MÊME successeur (voir l'en-tête du module).
    pub fn renouveler(&self, jeton: &str, cle: &[u8], maintenant: i64) -> Result<Emis, Refus> {
        self.renouveler_essai(jeton, cle, maintenant, true)
    }

    fn renouveler_essai(
        &self,
        jeton: &str,
        cle: &[u8],
        maintenant: i64,
        premier_essai: bool,
    ) -> Result<Emis, Refus> {
        if !jeton.starts_with(PREFIXE_JETON) {
            return Err(Refus::Inconnu);
        }
        let ligne = self.ligne(jeton)?.ok_or(Refus::Inconnu)?;
        if ligne.revoque_le.is_some() {
            return Err(Refus::Revoque);
        }
        let suivant = successeur(cle, jeton);
        if let Some(quand) = ligne.remplace_le {
            return self.rejeu(&ligne, &suivant, quand, maintenant);
        }
        let plafond = ligne.session_creee_le + PLAFOND_SESSION;
        if ligne.expire_le <= maintenant || plafond <= maintenant {
            self.revoquer_famille(&ligne.famille, maintenant);
            return Err(Refus::Expire);
        }
        // Le mot de passe a changé, ou le profil n'existe plus.
        if empreinte_du_mot_de_passe(&self.db, ligne.profile_id).as_deref()
            != Some(ligne.empreinte_mdp.as_str())
        {
            self.revoquer_famille(&ligne.famille, maintenant);
            return Err(Refus::Revoque);
        }

        let nouvelle_echeance = (maintenant + DUREE_RENOUVELLEMENT).min(plafond);
        let nouvel_id = uuid::Uuid::new_v4().to_string();
        let nouvelle_empreinte = empreinte(&suivant);
        let mut pris = false;
        self.db
            .write_tx(&mut |tx| {
                // Comparer-et-échanger : de deux renouvellements simultanés du
                // même jeton, un seul le consomme ; l'autre devient un rejeu.
                let n = tx.execute(
                    "UPDATE jetons_de_renouvellement SET remplace_le = ? \
                     WHERE jeton_id = ? AND remplace_le IS NULL AND revoque_le IS NULL",
                    &[&maintenant as &dyn ToSqlValue, &ligne.id],
                )?;
                pris = n == 1;
                if !pris {
                    return Ok(());
                }
                tx.execute(
                    "INSERT INTO jetons_de_renouvellement \
                     (jeton_id, famille, profile_id, empreinte_sha256, empreinte_mot_de_passe, client, cree_le, expire_le, session_creee_le) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    &[
                        &nouvel_id as &dyn ToSqlValue,
                        &ligne.famille,
                        &ligne.profile_id,
                        &nouvelle_empreinte,
                        &ligne.empreinte_mdp,
                        &ligne.client,
                        &maintenant,
                        &nouvelle_echeance,
                        &ligne.session_creee_le,
                    ],
                )?;
                Ok(())
            })
            .map_err(|_| Refus::Base)?;
        if !pris {
            if premier_essai {
                return self.renouveler_essai(jeton, cle, maintenant, false);
            }
            return Err(Refus::Base);
        }
        Ok(Emis {
            famille: ligne.famille,
            profile_id: ligne.profile_id,
            jeton: suivant,
            expire_le: nouvelle_echeance,
        })
    }

    /// Un jeton déjà consommé revient.
    fn rejeu(
        &self,
        ligne: &Ligne,
        suivant: &str,
        consomme_le: i64,
        maintenant: i64,
    ) -> Result<Emis, Refus> {
        let refuser = |motif: &str| {
            tracing::warn!(famille = %ligne.famille, profile_id = ligne.profile_id, motif, "jeton_de_renouvellement_reutilise_famille_revoquee");
            self.revoquer_famille(&ligne.famille, maintenant);
            Err(Refus::Reutilise)
        };
        if maintenant - consomme_le > DELAI_DE_GRACE {
            return refuser("hors_delai_de_grace");
        }
        // Le successeur doit être intact : s'il a déjà tourné, quelqu'un
        // d'autre tient la session.
        let Some(succ) = self.ligne(suivant)? else {
            return refuser("successeur_introuvable");
        };
        if succ.famille != ligne.famille || succ.remplace_le.is_some() || succ.revoque_le.is_some()
        {
            return refuser("successeur_deja_tourne");
        }
        // Un seul rejeu par fenêtre dans la famille.
        let depuis = maintenant - DELAI_ENTRE_REJEUX;
        let deja = self
            .db
            .query_one_strong(
                "SELECT 1 FROM jetons_de_renouvellement \
                 WHERE famille = ? AND rejoue_le IS NOT NULL AND rejoue_le > ? LIMIT 1",
                &[&ligne.famille as &dyn ToSqlValue, &depuis],
            )
            .map_err(|_| Refus::Base)?;
        if deja.is_some() {
            return refuser("second_rejeu");
        }
        self.db
            .execute(
                "UPDATE jetons_de_renouvellement SET rejoue_le = ? WHERE jeton_id = ?",
                &[&maintenant as &dyn ToSqlValue, &ligne.id],
            )
            .map_err(|_| Refus::Base)?;
        tracing::info!(famille = %ligne.famille, "jeton_de_renouvellement_rejoue");
        Ok(Emis {
            famille: ligne.famille.clone(),
            profile_id: ligne.profile_id,
            jeton: suivant.to_string(),
            expire_le: succ.expire_le,
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

/// Révocation GLOBALE, en SQL : toutes les sessions longues inscrites (#9445)
/// et toutes les familles de jetons de renouvellement. Pour un chemin qui
/// change le secret ou le mot de passe administrateur DANS SA TRANSACTION
/// (récupération de l'accès administrateur, #9564) : à exécuter, dans
/// l'ordre, avec `maintenant` comme unique paramètre de la seconde requête.
/// Hors transaction, `tune_server::auth::revoquer_toutes_les_sessions` fait
/// la même chose et ferme en plus les WebSocket.
pub const SQL_REVOCATION_GLOBALE: [&str; 2] = [
    "DELETE FROM settings WHERE key LIKE 'auth\\_session\\_token:%' ESCAPE '\\'",
    "UPDATE jetons_de_renouvellement SET revoque_le = ? WHERE revoque_le IS NULL",
];

/// [`SQL_REVOCATION_GLOBALE`] dans la transaction `tx`.
pub fn revoquer_toutes_les_sessions_tx(
    tx: &dyn super::backend::DbTxHandle,
    maintenant: i64,
) -> Result<(), String> {
    tx.execute(SQL_REVOCATION_GLOBALE[0], &[])?;
    tx.execute(SQL_REVOCATION_GLOBALE[1], &[&maintenant as &dyn ToSqlValue])?;
    Ok(())
}

#[cfg(test)]
#[path = "jetons_de_renouvellement_tests.rs"]
mod tests;
