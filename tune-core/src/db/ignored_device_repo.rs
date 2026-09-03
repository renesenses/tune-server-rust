//! Appareils ignorés — faire taire un APPAREIL, pas chasser ses zones (#1280).
//!
//! Alex Campbell : « I have too many Sonos showing up… is there an easier way
//! to remove devices, delete zones and stop them from showing up again ».
//! Patatorz, six semaines plus tard sur un parc DLNA + AirPlay + Chromecast :
//! « ils disparaissent bien sur le coup mais réapparaissent rapidement […] je
//! voudrais juste pouvoir choisir les appareils visibles pour jouer ».
//!
//! # Pourquoi une table, et pas le masquage de zone existant
//!
//! `zones.is_hidden` (+ [`ZoneRepo::hidden_zones_by_host`], #1281) empêche
//! bien une ZONE supprimée de renaître. Mais il ne porte que ce qui a déjà une
//! zone, et seulement pour les zones réseau `dlna`/`openhome` :
//!
//! * l'appareil reste enregistré comme SORTIE — la découverte l'enregistre
//!   AVANT d'atteindre le garde-fou de zone — donc il reste proposé dans
//!   `GET /devices` et dans le sélecteur de création de zone ;
//! * un appareil dont la zone n'a jamais été créée (`zone_auto_create` à
//!   `false`, TV filtrée, AirPlay 2 sans démon) n'a AUCUNE ligne `zones` :
//!   il n'y a rien à masquer, donc rien qui puisse le faire taire ;
//! * la ligne d'origine peut disparaître (purge des zones masquées de
//!   `sql::purge`, « vider les zones ») et le marqueur mourrait avec elle.
//!
//! On reprend donc le patron `hidden_items` (#1391) : **table sans clé
//! étrangère, instantané d'identité figé à l'insertion**. Le marqueur ne
//! dépend d'aucune ligne `zones` et survit à leur purge comme à la bascule
//! SQLite → PostgreSQL.
//!
//! # L'identité retenue — aucune troisième notion
//!
//! Les deux identités déjà en service dans ce dépôt sont réutilisées telles
//! quelles, dans cet ordre :
//!
//! 1. **l'identifiant annoncé** (`device_id`) — exact, c'est celui que
//!    `is_device_hidden` teste ;
//! 2. **la MAC** — l'identité physique donnée aux appareils AirPlay/RAOP par
//!    #2803 et déjà persistée sur `zones.mac` (`find_visible_zone_by_identity`) ;
//! 3. **hôte + nom annoncé** — exactement le couple de
//!    `hidden_zones_by_host` (#1281), et pour la même raison : le nom est
//!    EXIGÉ, sans quoi un appareil DIFFÉRENT héritant de l'adresse par le
//!    DHCP serait bloqué à tort (leçon du ré-ancrage #1651 : une IP seule
//!    n'identifie rien).
//!
//! Un Sonos qui s'annonce sous trois UUID au même hôte est donc ignoré
//! **une fois** : la première identité est écrite, les autres tombent sur la
//! règle 2 ou 3.
//!
//! # Ignorer n'est pas indétectable
//!
//! Patatorz le formule mieux que le titre du ticket : « les autres peuvent
//! rester détectables mais pas visibles ». Le blocage porte donc sur la
//! PROPOSITION — enregistrement de sortie, création de zone, liste
//! d'appareils — jamais sur l'écoute SSDP/mDNS elle-même. Et il est
//! réversible : [`IgnoredDeviceRepo::unignore`] libère TOUTES les identités
//! du même appareil, sans quoi l'utilisateur se piégerait lui-même.

use std::sync::Arc;

use serde::Serialize;

use super::backend::{DbBackend, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;

/// Une entrée de la liste d'ignorés : l'identité de l'appareil FIGÉE au
/// moment où l'utilisateur l'a fait taire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IgnoredDevice {
    /// L'identité exacte visée par le geste (UUID SSDP, id mDNS…).
    pub device_id: String,
    /// MAC normalisée `AA:BB:CC:DD:EE:FF`, ou vide si l'appareil n'en annonce
    /// aucune.
    pub mac: String,
    pub host: String,
    /// Le nom ANNONCÉ, pas le nom de la zone : c'est lui que la découverte
    /// représentera au prochain scan.
    pub name: String,
    pub device_type: String,
    pub created_at: Option<String>,
}

/// L'identité d'un appareil tel que la découverte vient de l'annoncer.
///
/// Emprunte plutôt que de cloner : la comparaison est faite à chaque
/// annonce, sur un chemin chaud.
#[derive(Debug, Clone, Copy)]
pub struct DeviceIdentity<'a> {
    pub device_id: &'a str,
    pub mac: Option<&'a str>,
    pub host: &'a str,
    pub name: &'a str,
}

impl<'a> DeviceIdentity<'a> {
    pub fn new(device_id: &'a str, host: &'a str, name: &'a str) -> Self {
        Self {
            device_id,
            mac: None,
            host,
            name,
        }
    }

    pub fn with_mac(mut self, mac: Option<&'a str>) -> Self {
        self.mac = mac;
        self
    }
}

/// La forme courte d'un nom annoncé : « Chambre - Sonos One » → « Chambre ».
///
/// La découverte SSDP crée ses zones sous cette forme quand elle est libre
/// (`handle_ssdp_discovered`), donc l'instantané figé peut porter l'une ou
/// l'autre selon la provenance du geste (liste d'appareils ou liste de zones).
fn forme_courte(nom: &str) -> &str {
    nom.split(" - ").next().unwrap_or(nom).trim()
}

/// Deux noms désignent-ils le même appareil ?
///
/// Égalité insensible à la casse, ou l'un est la forme courte de l'autre.
/// **Jamais** « les deux formes courtes sont égales » : « Salon - Sonos One »
/// et « Salon - Denon Ceol » se réduisent tous deux à « Salon » et sont deux
/// appareils distincts.
fn noms_equivalents(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a.eq_ignore_ascii_case(b)
        || forme_courte(a).eq_ignore_ascii_case(b)
        || a.eq_ignore_ascii_case(forme_courte(b))
}

/// Le prédicat, seul et pur — c'est lui que les tests fixent.
///
/// Trois règles, dans l'ordre de fiabilité décroissante. La troisième exige
/// le NOM en plus de l'hôte : c'est le garde-fou anti-DHCP (#1651).
pub fn identity_matches(entry: &IgnoredDevice, live: DeviceIdentity<'_>) -> bool {
    // 1. L'identité exacte, celle que le geste visait.
    if !entry.device_id.is_empty() && entry.device_id == live.device_id {
        return true;
    }

    // 2. La MAC : la seule identité qu'un bail DHCP ne déplace pas. On
    //    normalise des deux côtés (`0:11:22:…` du `arp` BSD, tirets Windows,
    //    12 hexas nus du TXT AirPlay).
    let mac_vive = live
        .mac
        .and_then(crate::discovery::mac::normalize_mac)
        .unwrap_or_default();
    let mac_figee = crate::discovery::mac::normalize_mac(&entry.mac).unwrap_or_default();
    if !mac_vive.is_empty() && mac_vive == mac_figee {
        return true;
    }

    // 3. Hôte + nom. Le nom est EXIGÉ : sans lui, un appareil DIFFÉRENT
    //    héritant de l'adresse par le DHCP serait ignoré à tort.
    if !entry.host.is_empty()
        && entry.host.eq_ignore_ascii_case(live.host)
        && noms_equivalents(&entry.name, live.name)
    {
        return true;
    }

    false
}

/// Constructeurs SQL agnostiques du moteur.
pub mod sql {
    use super::SqlDialect;

    /// `ON CONFLICT … DO UPDATE` : re-ignorer la même identité RAFRAÎCHIT son
    /// instantané (l'appareil a pu changer d'adresse depuis) au lieu
    /// d'échouer. Idempotent dans les deux sens.
    pub fn ignore<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO ignored_devices (device_id, mac, host, name, device_type, created_at) \
             VALUES ({}, {}, {}, {}, {}, {}) \
             ON CONFLICT (device_id) DO UPDATE SET mac = EXCLUDED.mac, host = EXCLUDED.host, \
             name = EXCLUDED.name, device_type = EXCLUDED.device_type",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.now_iso8601(),
        )
    }

    pub fn delete_one<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM ignored_devices WHERE device_id = {}",
            d.placeholder(1),
        )
    }

    pub fn list() -> &'static str {
        "SELECT device_id, COALESCE(mac, ''), COALESCE(host, ''), COALESCE(name, ''), \
         COALESCE(device_type, ''), created_at FROM ignored_devices \
         ORDER BY created_at DESC, device_id ASC"
    }
}

pub struct IgnoredDeviceRepo {
    db: Arc<dyn DbBackend>,
}

impl IgnoredDeviceRepo {
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

    /// Fait taire un appareil. L'instantané d'identité est figé ICI, dans le
    /// même INSERT — pas en rattrapage différé : c'est lui qui reconnaîtra
    /// les autres identités du même appareil au prochain scan.
    pub fn ignore(&self, dev: &IgnoredDevice) -> Result<(), String> {
        if dev.device_id.is_empty() {
            return Err("device_id vide".into());
        }
        let mac = crate::discovery::mac::normalize_mac(&dev.mac).unwrap_or_default();
        let sql = self.dialect_sql(sql::ignore, sql::ignore);
        let params: [&dyn ToSqlValue; 5] =
            [&dev.device_id, &mac, &dev.host, &dev.name, &dev.device_type];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    /// Débloque. Rend les identités effectivement libérées.
    ///
    /// Le déblocage porte sur l'APPAREIL, comme le blocage : toutes les
    /// entrées qui désignent le même appareil physique tombent ensemble.
    /// Sinon l'utilisateur qui a ignoré son Sonos sous deux UUID devrait
    /// deviner le second pour le récupérer — il se piégerait lui-même.
    pub fn unignore(&self, device_id: &str) -> Result<Vec<String>, String> {
        let entries = self.list()?;
        let Some(cible) = entries.iter().find(|e| e.device_id == device_id) else {
            // Rien de figé sous cette identité : suppression sèche, idempotente.
            let sql = self.dialect_sql(sql::delete_one, sql::delete_one);
            let params: [&dyn ToSqlValue; 1] = [&device_id];
            self.db.execute(&sql, &params)?;
            return Ok(Vec::new());
        };
        let identite = DeviceIdentity {
            device_id: &cible.device_id,
            mac: Some(cible.mac.as_str()),
            host: cible.host.as_str(),
            name: cible.name.as_str(),
        };
        let a_liberer: Vec<String> = entries
            .iter()
            .filter(|e| identity_matches(e, identite))
            .map(|e| e.device_id.clone())
            .collect();
        let sql = self.dialect_sql(sql::delete_one, sql::delete_one);
        for id in &a_liberer {
            let params: [&dyn ToSqlValue; 1] = [id];
            self.db.execute(&sql, &params)?;
        }
        Ok(a_liberer)
    }

    pub fn list(&self) -> Result<Vec<IgnoredDevice>, String> {
        let rows = self.db.query_many(sql::list(), &[])?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                Some(IgnoredDevice {
                    device_id: r.first().and_then(|v| v.as_string())?,
                    mac: r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    host: r.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
                    name: r.get(3).and_then(|v| v.as_string()).unwrap_or_default(),
                    device_type: r.get(4).and_then(|v| v.as_string()).unwrap_or_default(),
                    created_at: r.get(5).and_then(|v| v.as_string()),
                })
            })
            .collect())
    }

    /// L'entrée qui fait taire cet appareil, s'il y en a une.
    ///
    /// Table minuscule (quelques lignes) : on la lit en entier et on compare
    /// en mémoire, plutôt que d'écrire trois `OR` en SQL dont la
    /// normalisation de MAC ne serait pas exprimable.
    pub fn matching(&self, live: DeviceIdentity<'_>) -> Option<IgnoredDevice> {
        // Une base pré-migration (table absente) ne doit JAMAIS faire tomber
        // la découverte : pas de liste, donc aucun appareil ignoré.
        let entries = match self.list() {
            Ok(entries) => entries,
            Err(e) => {
                tracing::debug!(error = %e, "ignored_devices_lookup_failed_ignoring");
                return None;
            }
        };
        entries.into_iter().find(|e| identity_matches(e, live))
    }

    pub fn is_ignored(&self, live: DeviceIdentity<'_>) -> bool {
        self.matching(live).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> IgnoredDeviceRepo {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        IgnoredDeviceRepo::new(db)
    }

    fn entree(device_id: &str, mac: &str, host: &str, name: &str) -> IgnoredDevice {
        IgnoredDevice {
            device_id: device_id.into(),
            mac: mac.into(),
            host: host.into(),
            name: name.into(),
            device_type: "dlna".into(),
            created_at: None,
        }
    }

    #[test]
    fn ignorer_puis_lister_puis_debloquer() {
        let repo = repo();
        repo.ignore(&entree(
            "uuid:sonos-dlna",
            "",
            "192.168.1.50",
            "Chambre - Sonos One",
        ))
        .unwrap();

        let liste = repo.list().unwrap();
        assert_eq!(liste.len(), 1);
        assert_eq!(liste[0].device_id, "uuid:sonos-dlna");
        assert!(repo.is_ignored(DeviceIdentity::new(
            "uuid:sonos-dlna",
            "192.168.1.50",
            "Chambre - Sonos One"
        )));

        let liberes = repo.unignore("uuid:sonos-dlna").unwrap();
        assert_eq!(liberes, vec!["uuid:sonos-dlna".to_string()]);
        assert!(repo.list().unwrap().is_empty());
        assert!(!repo.is_ignored(DeviceIdentity::new(
            "uuid:sonos-dlna",
            "192.168.1.50",
            "Chambre - Sonos One"
        )));
    }

    /// Ignorer deux fois la même identité rafraîchit l'instantané au lieu
    /// d'échouer — l'appareil a pu changer d'adresse entre-temps.
    #[test]
    fn re_ignorer_rafraichit_l_instantane() {
        let repo = repo();
        repo.ignore(&entree("uuid:x", "", "192.168.1.50", "Ampli"))
            .unwrap();
        repo.ignore(&entree("uuid:x", "", "192.168.1.77", "Ampli"))
            .unwrap();
        let liste = repo.list().unwrap();
        assert_eq!(liste.len(), 1, "une identité = une ligne");
        assert_eq!(liste[0].host, "192.168.1.77");
    }

    /// Le cœur du ticket : le Sonos annonce une SECONDE identité SSDP au même
    /// hôte, sous le même nom. Elle doit être ignorée elle aussi.
    #[test]
    fn une_autre_identite_du_meme_appareil_est_ignoree() {
        let repo = repo();
        repo.ignore(&entree(
            "uuid:sonos-dlna",
            "",
            "192.168.1.50",
            "Chambre - Sonos One",
        ))
        .unwrap();

        assert!(
            repo.is_ignored(DeviceIdentity::new(
                "uuid:sonos-openhome",
                "192.168.1.50",
                "Chambre - Sonos One"
            )),
            "la seconde identité SSDP du même appareil doit être ignorée"
        );
    }

    /// La forme courte : la zone s'appelle « Chambre », l'annonce dit
    /// « Chambre - Sonos One ». Même appareil.
    #[test]
    fn la_forme_courte_du_nom_designe_le_meme_appareil() {
        let repo = repo();
        repo.ignore(&entree("uuid:sonos", "", "192.168.1.50", "Chambre"))
            .unwrap();
        assert!(repo.is_ignored(DeviceIdentity::new(
            "uuid:autre",
            "192.168.1.50",
            "Chambre - Sonos One"
        )));
    }

    /// GARDE-FOU ANTI-DHCP (#1651) : un appareil DIFFÉRENT qui hérite de
    /// l'adresse ne doit PAS être ignoré. C'est la raison d'être de
    /// l'exigence de nom.
    #[test]
    fn un_autre_appareil_au_meme_hote_apres_bail_dhcp_n_est_pas_ignore() {
        let repo = repo();
        repo.ignore(&entree(
            "uuid:sonos-dlna",
            "",
            "192.168.1.50",
            "Chambre - Sonos One",
        ))
        .unwrap();

        assert!(
            !repo.is_ignored(DeviceIdentity::new(
                "uuid:cabasse",
                "192.168.1.50",
                "Cabasse Pearl Akoya"
            )),
            "l'adresse seule n'identifie rien : le nouvel occupant du bail \
             DHCP doit rester visible"
        );
    }

    /// Deux appareils dont les noms ne partagent QUE la forme courte
    /// (« Salon - … ») ne se confondent pas.
    #[test]
    fn deux_noms_de_meme_prefixe_ne_se_confondent_pas() {
        let repo = repo();
        repo.ignore(&entree(
            "uuid:sonos",
            "",
            "192.168.1.50",
            "Salon - Sonos One",
        ))
        .unwrap();
        assert!(!repo.is_ignored(DeviceIdentity::new(
            "uuid:denon",
            "192.168.1.50",
            "Salon - Denon Ceol"
        )));
    }

    /// La MAC prime : elle survit au changement d'adresse ET au changement de
    /// nom. C'est l'identité donnée aux appareils AirPlay/RAOP par #2803.
    #[test]
    fn la_mac_reconnait_l_appareil_qui_a_change_d_adresse_et_de_nom() {
        let repo = repo();
        repo.ignore(&entree(
            "uuid:era100",
            "80:0a:80:5d:4d:ee",
            "192.168.1.50",
            "Chambre Missou",
        ))
        .unwrap();

        assert!(
            repo.is_ignored(
                DeviceIdentity::new("uuid:autre", "192.168.1.99", "Renommé")
                    .with_mac(Some("800A805D4DEE"))
            ),
            "12 hexas nus et forme pointée désignent la même MAC"
        );
    }

    /// Une MAC vide ne fait correspondre personne — sinon toutes les entrées
    /// sans MAC se confondraient entre elles.
    #[test]
    fn une_mac_absente_ne_fait_correspondre_personne() {
        let repo = repo();
        repo.ignore(&entree("uuid:a", "", "192.168.1.50", "Ampli A"))
            .unwrap();
        assert!(!repo.is_ignored(
            DeviceIdentity::new("uuid:b", "192.168.1.99", "Ampli B").with_mac(Some(""))
        ));
    }

    /// Débloquer libère TOUTES les identités du même appareil : sans cela,
    /// l'utilisateur devrait deviner l'UUID jumeau pour récupérer son Sonos.
    #[test]
    fn debloquer_libere_toutes_les_identites_du_meme_appareil() {
        let repo = repo();
        repo.ignore(&entree(
            "uuid:sonos-dlna",
            "",
            "192.168.1.50",
            "Chambre - Sonos One",
        ))
        .unwrap();
        repo.ignore(&entree(
            "uuid:sonos-oh",
            "",
            "192.168.1.50",
            "Chambre - Sonos One",
        ))
        .unwrap();
        repo.ignore(&entree("uuid:cabasse", "", "192.168.1.77", "Cabasse"))
            .unwrap();

        let mut liberes = repo.unignore("uuid:sonos-dlna").unwrap();
        liberes.sort();
        assert_eq!(liberes, vec!["uuid:sonos-dlna", "uuid:sonos-oh"]);

        let restant = repo.list().unwrap();
        assert_eq!(restant.len(), 1);
        assert_eq!(
            restant[0].device_id, "uuid:cabasse",
            "le voisin reste bloqué"
        );
    }

    /// Débloquer une identité inconnue est un non-événement, pas une erreur.
    #[test]
    fn debloquer_une_identite_inconnue_est_idempotent() {
        let repo = repo();
        assert!(repo.unignore("uuid:jamais-vu").unwrap().is_empty());
    }

    /// CONTRE-ÉPREUVE PERMANENTE du garde-fou anti-DHCP.
    ///
    /// La règle 3 sans son exigence de nom — c'est-à-dire l'erreur qu'on
    /// aurait pu écrire — ferait correspondre le nouvel occupant du bail. Ce
    /// test le VÉRIFIE, donc il prouve que le cas de
    /// `un_autre_appareil_au_meme_hote_apres_bail_dhcp_n_est_pas_ignore` est
    /// bien discriminant : sans le nom, il serait rouge.
    #[test]
    fn sans_l_exigence_de_nom_le_bail_dhcp_serait_bloque_a_tort() {
        let figee = entree("uuid:sonos-dlna", "", "192.168.1.50", "Chambre - Sonos One");
        let nouvel_occupant =
            DeviceIdentity::new("uuid:cabasse", "192.168.1.50", "Cabasse Pearl Akoya");

        // La règle telle qu'elle est écrite : pas de correspondance.
        assert!(!identity_matches(&figee, nouvel_occupant));

        // La même règle AMPUTÉE de son exigence de nom : correspondance.
        let amputee =
            !figee.host.is_empty() && figee.host.eq_ignore_ascii_case(nouvel_occupant.host);
        assert!(
            amputee,
            "sans l'exigence de nom, l'hôte seul suffirait — le scénario du \
             test anti-DHCP est donc bien discriminant"
        );
    }
}
