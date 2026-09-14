//! Registre DURABLE des serveurs multimédia (#2219, phase 1).
//!
//! Le registre vivait entièrement dans
//! `Arc<Mutex<HashMap<String, MediaServerInfo>>>`
//! (`tune-server/src/state.rs:86`) et datait sa dernière observation avec un
//! `Instant` marqué `#[serde(skip)]` (`discovery/ssdp.rs:146`). Deux
//! conséquences, et la seconde est la pire :
//!
//! 1. **rien ne survivait au redémarrage** — la liste repartait vide, puis se
//!    remplissait au gré des annonces, c'est-à-dire au hasard ;
//! 2. **la date n'était même pas représentable en absolu** — un `Instant` ne
//!    dit qu'un écart depuis un instant de la machine. Il n'y avait donc rien
//!    à écrire, et rien à relire.
//!
//! Le modèle est `network_mounts` (`db/migrations.rs:178-197`), et on en
//! reprend la séparation qui a rendu #1916 visible : `active` dit
//! l'**intention** (« ce serveur doit être proposé »), `last_state` et
//! `absence_reason` disent le **constat**. Sans elle, un serveur éteint et un
//! serveur qu'on a délibérément écarté se ressemblent — et la route ne peut
//! nommer ni l'un ni l'autre.
//!
//! Ce qu'on N'a PAS repris de `network_mounts` : sa colonne `password`, et sa
//! clef primaire entière. Ici la clef est l'**UDN**, parce que c'est la seule
//! identité d'un appareil UPnP qui survive à un changement d'adresse ou de
//! port — `discovery/redecouverte.rs:1-45` le documente et s'en sert déjà pour
//! le M-SEARCH unicast `ST: uuid:<udn>`.
//!
//! **Rien n'est jamais supprimé par ce module.** C'est la doctrine du dépôt,
//! écrite trois fois : `ssdp.rs:33-41` (« marquer ceux qui ne répondent plus
//! plutôt que de les retirer »), `zones/presence.rs` (un champ descriptif qui
//! ne masque rien), `favorites_reconcile.rs:22` (« un favori vraiment
//! introuvable n'est supprimé qu'après un scan COMPLET et sain »). Le seul
//! effet d'une absence est de cesser de proposer le serveur.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::backend::{DbBackend, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;

/// SQL agnostique du moteur.
///
/// ⚠️ Les `placeholder(n)` sont numérotés 1, 2, 3… STRICTEMENT dans l'ordre
/// d'apparition et jamais réutilisés : `SqliteDialect::placeholder` ignore
/// l'indice et rend `?` (`db/engine.rs:286-288`), donc SQLite lie par position
/// textuelle là où PostgreSQL lie par numéro. Un indice réutilisé — parfaitement
/// légal en PostgreSQL — ferait consommer deux paramètres à SQLite et un seul à
/// PostgreSQL, en silence.
pub mod sql {
    use super::SqlDialect;

    pub const COLONNES: &str = "udn, name, manufacturer, model, device_type, location, \
         content_directory_url, host, port, max_age_secs, first_seen_at, last_seen_at, \
         active, last_state, absence_reason";

    /// Enregistrer une OBSERVATION.
    ///
    /// `first_seen_at` est absent de la clause `DO UPDATE`, et c'est tout le
    /// sujet : un serveur qui revient garde sa date de première observation.
    /// C'est le travers exact que le commentaire de la migration 95 nomme
    /// (`migrations.rs:1659-1673`) — « poser la date de la mise a jour sur une
    /// zone morte depuis trois semaines la ferait passer pour recente ».
    ///
    /// `active` est absent lui aussi : c'est l'INTENTION de l'utilisateur, et
    /// une observation n'a pas à la réécrire.
    pub fn enregistrer_observation<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO media_servers \
             (udn, name, manufacturer, model, device_type, location, content_directory_url, \
              host, port, max_age_secs, first_seen_at, last_seen_at, last_state, absence_reason) \
             VALUES ({}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, 'present', NULL) \
             ON CONFLICT(udn) DO UPDATE SET \
                name = excluded.name, \
                manufacturer = excluded.manufacturer, \
                model = excluded.model, \
                device_type = excluded.device_type, \
                location = excluded.location, \
                content_directory_url = excluded.content_directory_url, \
                host = excluded.host, \
                port = excluded.port, \
                max_age_secs = excluded.max_age_secs, \
                last_seen_at = excluded.last_seen_at, \
                last_state = 'present', \
                absence_reason = NULL",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
            d.placeholder(8),
            d.placeholder(9),
            d.placeholder(10),
            d.placeholder(11),
            d.placeholder(12),
        )
    }

    /// Marquer un serveur ABSENT. Aucune date n'est touchée : `last_seen_at`
    /// reste la dernière fois qu'on l'a VU, pas la dernière fois qu'on a
    /// constaté son absence.
    pub fn marquer_absent<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE media_servers SET last_state = 'absent', absence_reason = {} WHERE udn = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    /// Marquer un serveur PRÉSENT sans le ré-observer — la bascule inverse,
    /// quand le plafond de masse retient une absence déjà écrite.
    pub fn marquer_present<D: SqlDialect>(_d: &D) -> String {
        "UPDATE media_servers SET last_state = 'present', absence_reason = NULL \
         WHERE last_state = 'absent'"
            .to_string()
    }

    pub fn lister() -> String {
        format!("SELECT {COLONNES} FROM media_servers ORDER BY name, udn")
    }

    pub fn compter() -> &'static str {
        "SELECT COUNT(*) FROM media_servers"
    }
}

/// Une ligne du registre, telle qu'elle est relue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServeurEnregistre {
    pub udn: String,
    pub name: String,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub device_type: String,
    pub location: String,
    pub content_directory_url: Option<String>,
    pub host: Option<String>,
    pub port: Option<i64>,
    pub max_age_secs: Option<i64>,
    /// Jamais réécrite : l'histoire du serveur.
    pub first_seen_at: String,
    pub last_seen_at: String,
    /// L'INTENTION (`network_mounts.active`). Vrai par défaut.
    pub active: bool,
    /// Le CONSTAT tel qu'il a été écrit la dernière fois. Il ne remplace pas le
    /// calcul de fraîcheur — c'est un cache lisible, et la vérité reste l'âge.
    pub last_state: Option<String>,
    pub absence_reason: Option<String>,
}

impl ServeurEnregistre {
    /// L'âge de la dernière observation, en secondes, calculé sur l'horloge
    /// ABSOLUE et non sur un `Instant`.
    ///
    /// C'est ce qui rend le registre relisible après un redémarrage : un
    /// `Instant` reconstruit vaudrait « vu à l'instant », ce que la
    /// contre-épreuve de la phase 1 interdit explicitement. `None` quand la
    /// date est illisible — on ne DEVINE pas une fraîcheur.
    pub fn age_secs(&self) -> Option<i64> {
        age_secs_depuis(&self.last_seen_at, chrono::Utc::now())
    }
}

/// L'âge, en secondes, d'un horodatage ISO-8601 UTC — extrait pour être
/// testable sans attendre que le temps passe.
///
/// Saturé à zéro : une horloge qui recule (NTP, machine sans pile) rendrait un
/// âge négatif, donc un serveur « vu dans le futur », donc éternellement
/// présent. On préfère « vu à l'instant » à « vu demain ».
pub fn age_secs_depuis(horodatage: &str, maintenant: chrono::DateTime<chrono::Utc>) -> Option<i64> {
    let quand = chrono::NaiveDateTime::parse_from_str(horodatage, "%Y-%m-%dT%H:%M:%SZ")
        .map(|n| n.and_utc())
        .or_else(|_| {
            chrono::DateTime::parse_from_rfc3339(horodatage).map(|d| d.with_timezone(&chrono::Utc))
        })
        .ok()?;
    Some((maintenant - quand).num_seconds().max(0))
}

/// L'horodatage ISO-8601 UTC d'un évènement survenu il y a `age_secs`
/// secondes.
///
/// C'est ce que la synchronisation du registre écrit : la découverte tient un
/// ÂGE (`MediaServerInfo::age()`, dérivé d'un `Instant`), et la table veut une
/// DATE. Traduire l'un en l'autre ici, plutôt que chez l'appelant, évite
/// d'ajouter `chrono` aux dépendances de `tune-server` — il n'y est
/// aujourd'hui que pour les tests.
///
/// Un âge négatif est traité comme nul : on n'écrit jamais une observation
/// dans le futur.
pub fn horodatage_il_y_a(age_secs: i64) -> String {
    let quand = chrono::Utc::now() - chrono::Duration::seconds(age_secs.max(0));
    quand.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Ce qu'une observation apporte au registre.
#[derive(Debug, Clone, Default)]
pub struct ObservationServeurRecue {
    pub udn: String,
    pub name: String,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub device_type: String,
    pub location: String,
    pub content_directory_url: Option<String>,
    pub host: Option<String>,
    pub port: Option<i64>,
    pub max_age_secs: Option<i64>,
}

pub struct MediaServerRepo {
    db: Arc<dyn DbBackend>,
}

impl MediaServerRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db: Arc::new(db) }
    }

    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        Self { db }
    }

    fn dialect_sql<F1, F2>(&self, sqlite: F1, postgres: F2) -> String
    where
        F1: FnOnce(&SqliteDialect) -> String,
        F2: FnOnce(&PostgresDialect) -> String,
    {
        match self.db.engine() {
            Engine::Sqlite => sqlite(&SqliteDialect),
            Engine::Postgres => postgres(&PostgresDialect),
        }
    }

    /// Enregistrer une observation. Le serveur redevient `present`, et sa date
    /// de PREMIÈRE observation est préservée s'il était déjà connu.
    pub fn enregistrer_observation(&self, obs: &ObservationServeurRecue) -> Result<(), String> {
        self.enregistrer_observation_a(obs, &maintenant_iso())
    }

    /// La même, avec l'horodatage fourni — c'est celle que les témoins
    /// appellent, pour dater une observation dans le passé sans attendre.
    pub fn enregistrer_observation_a(
        &self,
        obs: &ObservationServeurRecue,
        quand: &str,
    ) -> Result<(), String> {
        let sql = self.dialect_sql(sql::enregistrer_observation, sql::enregistrer_observation);
        let device_type = if obs.device_type.is_empty() {
            "upnp_media_server"
        } else {
            obs.device_type.as_str()
        };
        let params: [&dyn ToSqlValue; 12] = [
            &obs.udn,
            &obs.name,
            &obs.manufacturer,
            &obs.model,
            &device_type,
            &obs.location,
            &obs.content_directory_url,
            &obs.host,
            &obs.port,
            &obs.max_age_secs,
            &quand, // first_seen_at, posé une seule fois
            &quand, // last_seen_at, réécrit à chaque observation
        ];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    /// Marquer un serveur absent, avec la raison. Il reste dans le registre.
    pub fn marquer_absent(&self, udn: &str, raison: &str) -> Result<(), String> {
        let sql = self.dialect_sql(sql::marquer_absent, sql::marquer_absent);
        let params: [&dyn ToSqlValue; 2] = [&raison, &udn];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    /// Rendre présents tous les serveurs marqués absents — la bascule que le
    /// plafond de masse impose quand il retient une absence.
    pub fn marquer_tous_presents(&self) -> Result<(), String> {
        let sql = self.dialect_sql(sql::marquer_present, sql::marquer_present);
        self.db.execute(&sql, &[])?;
        Ok(())
    }

    pub fn lister(&self) -> Result<Vec<ServeurEnregistre>, String> {
        let rows = self.db.query_many(&sql::lister(), &[])?;
        Ok(rows.iter().filter_map(ligne_vers_serveur).collect())
    }

    pub fn compter(&self) -> Result<i64, String> {
        Ok(self
            .db
            .query_one(sql::compter(), &[])?
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap_or(0))
    }
}

fn maintenant_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn ligne_vers_serveur(cols: &Vec<super::backend::SqlValue>) -> Option<ServeurEnregistre> {
    if cols.len() < 15 {
        return None;
    }
    let texte = |i: usize| cols.get(i).and_then(|v| v.as_string());
    Some(ServeurEnregistre {
        udn: texte(0)?,
        name: texte(1).unwrap_or_default(),
        manufacturer: texte(2),
        model: texte(3),
        device_type: texte(4).unwrap_or_else(|| "upnp_media_server".into()),
        location: texte(5).unwrap_or_default(),
        content_directory_url: texte(6),
        host: texte(7),
        port: cols.get(8).and_then(|v| v.as_i64()),
        max_age_secs: cols.get(9).and_then(|v| v.as_i64()),
        first_seen_at: texte(10).unwrap_or_default(),
        last_seen_at: texte(11).unwrap_or_default(),
        // `COALESCE`-libre à dessein : la colonne est NOT NULL DEFAULT 1.
        active: cols.get(12).and_then(|v| v.as_i64()).unwrap_or(1) != 0,
        last_state: texte(13),
        absence_reason: texte(14),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        db
    }

    fn observation(udn: &str) -> ObservationServeurRecue {
        ObservationServeurRecue {
            udn: udn.into(),
            name: "Tune Server".into(),
            manufacturer: Some("MozAIk Labs".into()),
            model: Some("Tune".into()),
            device_type: "upnp_media_server".into(),
            location: "http://192.168.1.42:8888/upnp/description.xml".into(),
            content_directory_url: Some("http://192.168.1.42:8888/upnp/cd/control".into()),
            host: Some("192.168.1.42".into()),
            port: Some(8888),
            max_age_secs: Some(1800),
        }
    }

    /// Témoin 1 — le registre SURVIT au redémarrage.
    ///
    /// « Redémarrer » se joue ici en rouvrant un dépôt sur la MÊME base : le
    /// registre en mémoire, lui, est reconstruit à neuf à chaque démarrage, et
    /// c'était tout le défaut. Ce qui est prouvé, c'est que la ligne est en
    /// base et se relit telle quelle, sans qu'aucun M-SEARCH n'ait eu lieu.
    #[test]
    fn un_serveur_observe_survit_au_redemarrage() {
        let db = test_db();
        MediaServerRepo::new(db.clone())
            .enregistrer_observation(&observation("uuid:2c35bec3"))
            .unwrap();

        // Nouveau dépôt : personne n'a rien gardé en mémoire.
        let apres_redemarrage = MediaServerRepo::new(db.clone());
        let liste = apres_redemarrage.lister().unwrap();
        assert_eq!(liste.len(), 1, "la liste doit être là AVANT tout M-SEARCH");
        let s = &liste[0];
        assert_eq!(s.udn, "uuid:2c35bec3");
        assert_eq!(s.name, "Tune Server");
        assert_eq!(s.device_type, "upnp_media_server");
        assert_eq!(
            s.location, "http://192.168.1.42:8888/upnp/description.xml",
            "l'adresse de description est le minimum exigé par la phase 1"
        );
        assert_eq!(s.port, Some(8888));
        assert!(!s.first_seen_at.is_empty(), "première observation datée");
        assert!(!s.last_seen_at.is_empty(), "dernière observation datée");
        assert!(s.active, "l'intention vaut « proposé » par défaut");
        assert_eq!(s.last_state.as_deref(), Some("present"));
    }

    /// Témoin 3 — un serveur qui revient redevient présent SANS perdre sa date
    /// de première observation.
    #[test]
    fn un_serveur_qui_revient_garde_sa_premiere_observation() {
        let db = test_db();
        let repo = MediaServerRepo::new(db.clone());

        repo.enregistrer_observation_a(&observation("uuid:2c35bec3"), "2026-09-01T08:00:00Z")
            .unwrap();
        let premiere = repo.lister().unwrap()[0].first_seen_at.clone();
        assert_eq!(premiere, "2026-09-01T08:00:00Z");

        // Il disparaît, on le marque absent — rien n'est supprimé.
        repo.marquer_absent("uuid:2c35bec3", "silence_prolonge")
            .unwrap();
        let pendant = &repo.lister().unwrap()[0];
        assert_eq!(pendant.last_state.as_deref(), Some("absent"));
        assert_eq!(pendant.absence_reason.as_deref(), Some("silence_prolonge"));
        assert_eq!(
            pendant.last_seen_at, "2026-09-01T08:00:00Z",
            "marquer absent ne touche PAS la date de dernière observation"
        );
        assert_eq!(pendant.first_seen_at, premiere);

        // Il revient.
        repo.enregistrer_observation_a(&observation("uuid:2c35bec3"), "2026-09-13T10:00:00Z")
            .unwrap();
        let apres = &repo.lister().unwrap()[0];
        assert_eq!(
            apres.first_seen_at, premiere,
            "l'histoire du serveur n'est pas perdue"
        );
        assert_eq!(apres.last_seen_at, "2026-09-13T10:00:00Z");
        assert_eq!(apres.last_state.as_deref(), Some("present"));
        assert_eq!(
            apres.absence_reason, None,
            "la raison d'une absence révolue ne doit pas rester affichée"
        );
        assert_eq!(
            repo.compter().unwrap(),
            1,
            "aucun doublon : l'UDN est la clef"
        );
    }

    /// Une observation ne réécrit pas l'INTENTION — c'est la séparation que
    /// `network_mounts` a payée avec #1916.
    #[test]
    fn une_observation_ne_reecrit_pas_l_intention() {
        let db = test_db();
        let repo = MediaServerRepo::new(db.clone());
        repo.enregistrer_observation(&observation("uuid:a"))
            .unwrap();
        db.execute_batch("UPDATE media_servers SET active = 0")
            .unwrap();
        repo.enregistrer_observation(&observation("uuid:a"))
            .unwrap();
        assert!(
            !repo.lister().unwrap()[0].active,
            "le constat ne doit pas écraser l'intention"
        );
    }

    /// L'âge se lit sur l'horloge absolue, pas sur un `Instant` — c'est ce qui
    /// permet de le relire après un redémarrage.
    #[test]
    fn l_age_se_calcule_sur_l_horloge_absolue() {
        let maintenant = chrono::DateTime::parse_from_rfc3339("2026-09-13T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            age_secs_depuis("2026-09-13T10:36:46Z", maintenant),
            Some(4_994)
        );
        // Les 84 194 s mesurées sur le `.18` le 13/09.
        assert_eq!(
            age_secs_depuis("2026-09-12T12:36:46Z", maintenant),
            Some(84_194)
        );
        // Horloge qui recule : saturé à zéro, jamais « vu demain ».
        assert_eq!(age_secs_depuis("2026-09-14T00:00:00Z", maintenant), Some(0));
        // Date illisible : on ne devine pas une fraîcheur.
        assert_eq!(age_secs_depuis("bientôt", maintenant), None);
    }

    /// Les deux dialectes produisent bien deux formes de paramètres, et
    /// STRICTEMENT dans l'ordre — le piège `placeholder` de `engine.rs:286`.
    #[test]
    fn les_parametres_sont_numerotes_dans_l_ordre_sur_les_deux_moteurs() {
        let sqlite = sql::enregistrer_observation(&SqliteDialect);
        let pg = sql::enregistrer_observation(&PostgresDialect);
        assert!(
            sqlite.contains("VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'present', NULL)"),
            "{sqlite}"
        );
        assert!(
            pg.contains(
                "VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, 'present', NULL)"
            ),
            "{pg}"
        );
        assert!(
            !sqlite.contains("first_seen_at = excluded"),
            "first_seen_at ne doit JAMAIS être réécrit"
        );
        assert!(
            !pg.contains("active = excluded"),
            "l'intention ne doit JAMAIS être réécrite par une observation"
        );
    }
}
