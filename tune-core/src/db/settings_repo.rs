use std::sync::Arc;

use super::backend::{DbBackend, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;

pub struct SettingsRepo {
    db: Arc<dyn DbBackend>,
}

/// Engine-agnostic SQL builders. They live as free functions so the
/// future PostgresRepo can call them with `PostgresDialect` while the
/// SQLite repo below uses `SqliteDialect`.
pub mod sql {
    use super::SqlDialect;

    pub fn get_by_key<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT value FROM settings WHERE key = {}",
            d.placeholder(1)
        )
    }

    pub fn delete_by_key<D: SqlDialect>(d: &D) -> String {
        format!("DELETE FROM settings WHERE key = {}", d.placeholder(1))
    }

    pub fn list_all() -> &'static str {
        "SELECT key, value FROM settings ORDER BY key"
    }

    /// Upsert via the SQL standard `ON CONFLICT` form (SQLite 3.24+,
    /// PostgreSQL 9.5+). Both dialects use the same placeholders.
    pub fn upsert<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO settings (key, value, updated_at) \
             VALUES ({}, {}, {}) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
        )
    }
}

impl SettingsRepo {
    /// Backward-compatible constructor for the existing call sites.
    /// Wraps the concrete `SqliteDb` in an `Arc<dyn DbBackend>` so the
    /// internal storage matches the new trait-object form. Same observable
    /// behavior as before phase 5 of the PG roadmap.
    pub fn new(db: SqliteDb) -> Self {
        Self { db: Arc::new(db) }
    }

    /// New constructor used by callers that already hold an
    /// `Arc<dyn DbBackend>` (Postgres or SQLite).
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

    pub fn get(&self, key: &str) -> Result<Option<String>, String> {
        let sql = self.dialect_sql(sql::get_by_key, sql::get_by_key);
        let params: [&dyn ToSqlValue; 1] = [&key];
        // Use query_one_strong to read through the write connection.
        // Settings are frequently read immediately after a write (e.g.
        // saving a Discogs token then checking discogs_token_set in
        // get_config). The read-only WAL snapshot may lag behind the
        // writer, returning stale NULL for a key that was just upserted.
        match self.db.query_one_strong(&sql, &params)? {
            None => Ok(None),
            Some(row) => Ok(row.first().and_then(|v| v.as_string())),
        }
    }

    pub fn set(&self, key: &str, value: &str) -> Result<(), String> {
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let sql = self.dialect_sql(sql::upsert, sql::upsert);
        let params: [&dyn ToSqlValue; 3] = [&key, &value, &now];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn delete(&self, key: &str) -> Result<(), String> {
        let sql = self.dialect_sql(sql::delete_by_key, sql::delete_by_key);
        let params: [&dyn ToSqlValue; 1] = [&key];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn all(&self) -> Result<Vec<(String, String)>, String> {
        // Use query_many_strong for the same WAL snapshot reason as get().
        let rows = self.db.query_many_strong(sql::list_all(), &[])?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                let k = cols.first().and_then(|v| v.as_string()).unwrap_or_default();
                let v = cols.get(1).and_then(|v| v.as_string()).unwrap_or_default();
                (k, v)
            })
            .collect())
    }

    // --- AirPlay 2 pairing credentials (keyed per device_id) --------------
    //
    // Long-term secrets from a successful HomeKit-style pairing: our controller
    // Ed25519 seed + the accessory's long-term public key (`AccessoryLTPK`) +
    // its pairing identifier. Stored as JSON under `airplay2_pairing:<id>` in
    // the same key/value settings table (no schema change needed). The values
    // are populated by the pair-setup handshake in a later increment; the
    // storage/accessor plumbing lives here now.

    /// Persist pairing credentials for a device.
    pub fn set_airplay_pairing(
        &self,
        device_id: &str,
        creds: &AirplayPairingRecord,
    ) -> Result<(), String> {
        let json =
            serde_json::to_string(creds).map_err(|e| format!("serialize airplay pairing: {e}"))?;
        self.set(&airplay_pairing_key(device_id), &json)
    }

    /// Load pairing credentials for a device, if we have paired with it.
    pub fn get_airplay_pairing(
        &self,
        device_id: &str,
    ) -> Result<Option<AirplayPairingRecord>, String> {
        match self.get(&airplay_pairing_key(device_id))? {
            None => Ok(None),
            Some(json) => serde_json::from_str(&json)
                .map(Some)
                .map_err(|e| format!("deserialize airplay pairing: {e}")),
        }
    }

    /// Forget a device's pairing (e.g. user re-pairs or removes it).
    pub fn delete_airplay_pairing(&self, device_id: &str) -> Result<(), String> {
        self.delete(&airplay_pairing_key(device_id))
    }

    // --- Listes JSON stockées sous une clef de `settings` (#2795) ----------

    /// Lit une liste JSON, en distinguant « absente » d'« illisible ».
    ///
    /// `Ok(vec![])` ne veut dire qu'**une** chose : la clef n'existe pas encore
    /// (ou porte une chaîne vide). Une panne de base ou un contenu corrompu
    /// rendent `Err` — jamais une liste vide. C'est toute la différence que le
    /// `.ok().flatten().and_then(…).unwrap_or_default()` de `developer_api.rs`
    /// effaçait : la liste vide qu'il rendait servait ensuite de base à la
    /// réécriture, et remplaçait les clefs existantes par `[]`.
    ///
    /// Le message d'erreur ne contient **jamais** la valeur lue : ces listes
    /// portent des secrets (clefs d'API développeur, URL de webhook).
    pub fn get_json_list<T>(&self, key: &str) -> Result<Vec<T>, String>
    where
        T: serde::de::DeserializeOwned,
    {
        let brut = self.get(key)?;
        decode_json_list(key, brut.as_deref())
    }

    /// Lit, modifie et réécrit une liste JSON **en une seule transaction**, un
    /// écrivain à la fois.
    ///
    /// Deux garanties, celles que la #2795 demande :
    ///
    /// 1. **Rien n'est perdu par concurrence.** Un verrou de processus, propre
    ///    à la clef, sérialise les lecture-modification-écriture ; la lecture
    ///    et l'écriture vivent ensuite dans la même transaction. Deux créations
    ///    simultanées sont donc conservées toutes les deux, au lieu que la
    ///    seconde réécrive la liste qu'elle avait lue avant la première.
    ///    Le verrou de processus n'est pas décoratif : sur SQLite `write_tx`
    ///    tient déjà le verrou de la connexion d'écriture, mais sur Postgres il
    ///    prend une connexion du pool en `READ COMMITTED`, où deux transactions
    ///    peuvent parfaitement lire la même valeur avant d'écrire.
    ///
    /// 2. **Aucun succès n'est annoncé sans persistance.** Toute erreur — base,
    ///    JSON illisible, sérialisation — remonte en `Err`. L'appelant ne peut
    ///    plus répondre `201` à une écriture qui n'a pas eu lieu.
    ///
    /// La fermeture rend la valeur que l'appelant veut voir survivre à la
    /// transaction (par exemple : la clef créée, ou « cet identifiant
    /// existait-il ? »). Elle ne doit **pas** rappeler `update_json_list` sur
    /// la même clef : le verrou n'est pas réentrant.
    ///
    /// Si la sérialisation rend exactement les octets déjà stockés, aucune
    /// écriture n'est émise — une révocation qui ne trouve rien ne touche pas
    /// la ligne.
    pub fn update_json_list<T, R, F>(&self, key: &str, mutate: F) -> Result<R, String>
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
        F: FnOnce(&mut Vec<T>) -> Result<R, String>,
    {
        let verrou = key_lock(key);
        // Un empoisonnement vient d'une panique dans une AUTRE requête ; il ne
        // doit pas condamner la clef pour la vie du processus.
        let _garde = verrou.lock().unwrap_or_else(|e| e.into_inner());

        let lire = self.dialect_sql(sql::get_by_key, sql::get_by_key);
        let ecrire = self.dialect_sql(sql::upsert, sql::upsert);
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

        let mut mutate = Some(mutate);
        let mut sortie: Option<R> = None;

        self.db.write_tx(&mut |tx| {
            let params: [&dyn ToSqlValue; 1] = [&key];
            let brut = tx
                .query_one(&lire, &params)?
                .and_then(|row| row.first().and_then(|v| v.as_string()));

            let mut liste: Vec<T> = decode_json_list(key, brut.as_deref())?;

            let f = mutate.take().ok_or_else(|| {
                format!("update_json_list({key}) : la fermeture a deja ete consommee")
            })?;
            let rendu = f(&mut liste)?;

            let json = serde_json::to_string(&liste)
                .map_err(|e| format!("serialisation de la liste `{key}` : {e}"))?;

            if brut.as_deref() != Some(json.as_str()) {
                let params: [&dyn ToSqlValue; 3] = [&key, &json, &now];
                tx.execute(&ecrire, &params)?;
            }

            sortie = Some(rendu);
            Ok(())
        })?;

        sortie.ok_or_else(|| format!("update_json_list({key}) : transaction sans resultat"))
    }
}

/// Décode une liste JSON en séparant « absente » d'« illisible ».
///
/// Le contenu n'apparaît pas dans l'erreur : `serde_json` ne cite que la
/// position (ligne/colonne), et on n'ajoute rien. Ces listes portent des
/// secrets.
fn decode_json_list<T>(key: &str, brut: Option<&str>) -> Result<Vec<T>, String>
where
    T: serde::de::DeserializeOwned,
{
    match brut {
        None => Ok(Vec::new()),
        Some(s) if s.trim().is_empty() => Ok(Vec::new()),
        Some(s) => serde_json::from_str(s)
            .map_err(|e| format!("la valeur de `{key}` n'est pas une liste JSON lisible : {e}")),
    }
}

/// Le verrou qui sérialise les lecture-modification-écriture d'une clef.
type VerrouDeClef = Arc<std::sync::Mutex<()>>;

/// Les verrous vivants, un par clef rencontrée.
type TableDesVerrous = std::collections::HashMap<String, VerrouDeClef>;

/// Le verrou de la clef donnée, créé à la première demande.
///
/// Un verrou par clef et non un verrou global : `developer_api_keys`,
/// `developer_webhooks` et `marketplace_installed` n'ont aucune raison
/// d'attendre l'une pour l'autre.
fn key_lock(key: &str) -> VerrouDeClef {
    static VERROUS: std::sync::OnceLock<std::sync::Mutex<TableDesVerrous>> =
        std::sync::OnceLock::new();

    let table = VERROUS.get_or_init(Default::default);
    let mut table = table.lock().unwrap_or_else(|e| e.into_inner());
    table.entry(key.to_string()).or_default().clone()
}

/// Settings key namespace for AirPlay 2 pairing records.
fn airplay_pairing_key(device_id: &str) -> String {
    format!("airplay2_pairing:{device_id}")
}

/// Stored AirPlay 2 / HomeKit pairing credentials for one accessory.
///
/// Byte arrays are hex-encoded strings so the record is human-readable JSON in
/// the settings table. Kept independent of `tune_core::outputs::airplay2` so the
/// DB layer has no dependency on the crypto module.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AirplayPairingRecord {
    /// Our controller Ed25519 seed (32 bytes, hex) — secret.
    pub our_ed25519_seed_hex: String,
    /// The accessory's long-term public key (`AccessoryLTPK`, 32 bytes, hex).
    pub accessory_ltpk_hex: String,
    /// The accessory pairing identifier (its `AccessoryPairingID` string).
    pub accessory_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;

    fn fresh_repo() -> SettingsRepo {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        SettingsRepo::new(db)
    }

    #[test]
    fn settings_crud() {
        let repo = fresh_repo();

        assert!(repo.get("music_dirs").unwrap().is_none());

        repo.set("music_dirs", r#"["/music"]"#).unwrap();
        assert_eq!(repo.get("music_dirs").unwrap().unwrap(), r#"["/music"]"#);

        repo.set("music_dirs", r#"["/music","/nas"]"#).unwrap();
        assert_eq!(
            repo.get("music_dirs").unwrap().unwrap(),
            r#"["/music","/nas"]"#
        );

        let all = repo.all().unwrap();
        assert_eq!(all.len(), 1);

        repo.delete("music_dirs").unwrap();
        assert!(repo.get("music_dirs").unwrap().is_none());
    }

    #[test]
    fn settings_multiple_keys() {
        let repo = fresh_repo();
        repo.set("key1", "value1").unwrap();
        repo.set("key2", "value2").unwrap();
        repo.set("key3", "value3").unwrap();

        let all = repo.all().unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].0, "key1");
        assert_eq!(all[1].0, "key2");
        assert_eq!(all[2].0, "key3");
    }

    #[test]
    fn settings_overwrite() {
        let repo = fresh_repo();
        repo.set("theme", "dark").unwrap();
        repo.set("theme", "light").unwrap();
        assert_eq!(repo.get("theme").unwrap().unwrap(), "light");
        assert_eq!(repo.all().unwrap().len(), 1);
    }

    #[test]
    fn settings_delete_nonexistent() {
        let repo = fresh_repo();
        repo.delete("nonexistent").unwrap();
    }

    #[test]
    fn settings_empty_value() {
        let repo = fresh_repo();
        repo.set("empty", "").unwrap();
        assert_eq!(repo.get("empty").unwrap().unwrap(), "");
    }

    #[test]
    fn settings_json_value() {
        let repo = fresh_repo();
        let json = r#"{"enabled":true,"services":["tidal","qobuz"]}"#;
        repo.set("streaming_config", json).unwrap();
        assert_eq!(repo.get("streaming_config").unwrap().unwrap(), json);
    }

    #[test]
    fn settings_unicode_key_and_value() {
        let repo = fresh_repo();
        repo.set("nom_utilisateur", "Rene").unwrap();
        assert_eq!(repo.get("nom_utilisateur").unwrap().unwrap(), "Rene");
    }

    #[test]
    fn with_backend_constructor() {
        // Verify the new `Arc<dyn DbBackend>` constructor works too.
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = SettingsRepo::with_backend(backend);
        repo.set("k", "v").unwrap();
        assert_eq!(repo.get("k").unwrap().unwrap(), "v");
    }

    #[test]
    fn sql_builders_emit_sqlite_placeholders() {
        let d = SqliteDialect;
        assert_eq!(
            sql::get_by_key(&d),
            "SELECT value FROM settings WHERE key = ?"
        );
        assert_eq!(sql::delete_by_key(&d), "DELETE FROM settings WHERE key = ?");
        assert_eq!(
            sql::list_all(),
            "SELECT key, value FROM settings ORDER BY key"
        );
    }

    #[test]
    fn sql_builders_emit_postgres_placeholders() {
        let d = PostgresDialect;
        assert_eq!(
            sql::get_by_key(&d),
            "SELECT value FROM settings WHERE key = $1"
        );
        assert_eq!(
            sql::delete_by_key(&d),
            "DELETE FROM settings WHERE key = $1"
        );
    }

    #[test]
    fn airplay_pairing_roundtrip_per_device() {
        let repo = fresh_repo();
        let dev = "airplay2:AA-BB-CC";

        // Nothing stored yet.
        assert!(repo.get_airplay_pairing(dev).unwrap().is_none());

        let rec = AirplayPairingRecord {
            our_ed25519_seed_hex: "00".repeat(32),
            accessory_ltpk_hex: "ab".repeat(32),
            accessory_id: "AABBCCDDEEFF".into(),
        };
        repo.set_airplay_pairing(dev, &rec).unwrap();

        // Round-trips to the exact same record.
        assert_eq!(repo.get_airplay_pairing(dev).unwrap().unwrap(), rec);

        // A different device is isolated.
        assert!(
            repo.get_airplay_pairing("airplay2:other")
                .unwrap()
                .is_none()
        );

        // Deletion forgets it.
        repo.delete_airplay_pairing(dev).unwrap();
        assert!(repo.get_airplay_pairing(dev).unwrap().is_none());
    }

    // --- Listes JSON : « absente » n'est pas « illisible » (#2795) ---------

    #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Jeton {
        id: String,
        secret: String,
    }

    fn jeton(id: &str) -> Jeton {
        Jeton {
            id: id.into(),
            secret: format!("tunedev_{id}"),
        }
    }

    #[test]
    fn liste_absente_rend_une_liste_vide() {
        let repo = fresh_repo();
        let liste: Vec<Jeton> = repo.get_json_list("jetons").unwrap();
        assert!(liste.is_empty());
    }

    /// La distinction que la #2795 réclame : un contenu illisible remonte en
    /// `Err`, il ne se déguise pas en liste vide — sans quoi l'écriture qui
    /// suit remplace tout par `[]`.
    #[test]
    fn liste_illisible_rend_une_erreur_sans_citer_le_contenu() {
        let repo = fresh_repo();
        repo.set("jetons", "{ ceci n'est pas une liste }").unwrap();

        let erreur = repo
            .get_json_list::<Jeton>("jetons")
            .expect_err("un JSON invalide doit remonter, pas se taire");
        assert!(
            erreur.contains("jetons"),
            "l'erreur doit nommer la clef : {erreur}"
        );
        assert!(
            !erreur.contains("ceci n'est pas"),
            "le message ne doit JAMAIS citer la valeur : ces listes portent des secrets ({erreur})"
        );
    }

    /// Le témoin anti-régression du geste : sur une valeur illisible,
    /// `update_json_list` refuse d'écrire. Avant la #2795, la lecture rendait
    /// `[]` et l'écriture qui suivait effaçait la ligne pour de bon.
    #[test]
    fn une_liste_illisible_n_est_jamais_ecrasee() {
        let repo = fresh_repo();
        let intact = r#"[{"id":"a"}]"#; // champ `secret` manquant : illisible pour Jeton
        repo.set("jetons", intact).unwrap();

        let erreur = repo
            .update_json_list::<Jeton, _, _>("jetons", |liste| {
                liste.push(jeton("b"));
                Ok(())
            })
            .expect_err("une liste illisible doit bloquer l'ecriture");
        assert!(erreur.contains("jetons"), "{erreur}");

        assert_eq!(
            repo.get("jetons").unwrap().as_deref(),
            Some(intact),
            "la valeur d'origine doit etre intacte : c'est elle qu'on refuse de perdre"
        );
    }

    #[test]
    fn une_panne_de_base_remonte_au_lieu_de_rendre_une_liste_vide() {
        let repo = fresh_repo();
        repo.update_json_list::<Jeton, _, _>("jetons", |l| {
            l.push(jeton("a"));
            Ok(())
        })
        .unwrap();

        // La table disparaît : la forme la plus simple d'une panne SQL.
        repo.db.execute_batch("DROP TABLE settings").unwrap();

        assert!(
            repo.get_json_list::<Jeton>("jetons").is_err(),
            "une table absente doit rendre Err, jamais Ok(vec![])"
        );
        assert!(
            repo.update_json_list::<Jeton, _, _>("jetons", |l| {
                l.push(jeton("b"));
                Ok(())
            })
            .is_err(),
            "aucun succes ne doit etre rendu quand l'ecriture est impossible"
        );
    }

    /// Le critère de la #2795 : deux créations concurrentes sont TOUTES LES
    /// DEUX conservées. Le read-modify-write naïf en perdait une sur deux.
    #[test]
    fn deux_creations_concurrentes_sont_toutes_les_deux_conservees() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);

        const ECRIVAINS: usize = 8;
        const PAR_ECRIVAIN: usize = 12;

        let mut fils = Vec::new();
        for e in 0..ECRIVAINS {
            let backend = backend.clone();
            fils.push(std::thread::spawn(move || {
                let repo = SettingsRepo::with_backend(backend);
                for i in 0..PAR_ECRIVAIN {
                    repo.update_json_list::<Jeton, _, _>("jetons", |liste| {
                        liste.push(jeton(&format!("{e}-{i}")));
                        Ok(())
                    })
                    .expect("chaque creation doit reussir");
                }
            }));
        }
        for f in fils {
            f.join().unwrap();
        }

        let repo = SettingsRepo::with_backend(backend);
        let liste: Vec<Jeton> = repo.get_json_list("jetons").unwrap();
        assert_eq!(
            liste.len(),
            ECRIVAINS * PAR_ECRIVAIN,
            "des creations se sont ecrasees : {} sur {}",
            liste.len(),
            ECRIVAINS * PAR_ECRIVAIN
        );

        let ids: std::collections::HashSet<&str> = liste.iter().map(|j| j.id.as_str()).collect();
        assert_eq!(ids.len(), liste.len(), "des identifiants sont en double");
    }

    /// Une modification qui ne change rien ne touche pas la ligne : une
    /// révocation qui ne trouve rien ne doit pas réécrire les autres clefs.
    ///
    /// Le témoin est `updated_at`, marqué à la main puis relu : la valeur, elle,
    /// serait identique dans les deux cas et ne prouverait rien.
    #[test]
    fn une_modification_sans_effet_n_ecrit_pas() {
        let repo = fresh_repo();
        repo.update_json_list::<Jeton, _, _>("jetons", |l| {
            l.push(jeton("a"));
            Ok(())
        })
        .unwrap();

        const TEMOIN: &str = "1970-01-01T00:00:00Z";
        let marquer = |horodatage: &str| {
            repo.db
                .execute(
                    "UPDATE settings SET updated_at = ? WHERE key = ?",
                    &[&horodatage, &"jetons"],
                )
                .unwrap();
        };
        let horodatage = || {
            repo.db
                .query_one(
                    "SELECT updated_at FROM settings WHERE key = ?",
                    &[&"jetons"],
                )
                .unwrap()
                .and_then(|r| r.first().and_then(|v| v.as_string()))
                .unwrap()
        };

        marquer(TEMOIN);
        let trouve = repo
            .update_json_list::<Jeton, _, _>("jetons", |liste| {
                let n = liste.len();
                liste.retain(|j| j.id != "inconnu");
                Ok(liste.len() != n)
            })
            .unwrap();

        assert!(!trouve);
        assert_eq!(
            horodatage(),
            TEMOIN,
            "la ligne a ete reecrite alors que rien n'avait change"
        );

        // Et la contre-épreuve : une modification RÉELLE, elle, écrit bien.
        let retire = repo
            .update_json_list::<Jeton, _, _>("jetons", |liste| {
                let n = liste.len();
                liste.retain(|j| j.id != "a");
                Ok(liste.len() != n)
            })
            .unwrap();
        assert!(retire);
        assert_eq!(repo.get("jetons").unwrap().as_deref(), Some("[]"));
        assert_ne!(
            horodatage(),
            TEMOIN,
            "une modification reelle doit, elle, toucher la ligne"
        );
    }

    /// La valeur rendue par la fermeture survit à la transaction — c'est elle
    /// qui permet au gestionnaire de répondre 404 plutôt que 500.
    #[test]
    fn la_fermeture_rend_sa_valeur_a_l_appelant() {
        let repo = fresh_repo();
        repo.update_json_list::<Jeton, _, _>("jetons", |l| {
            l.push(jeton("a"));
            Ok(())
        })
        .unwrap();

        let retire = repo
            .update_json_list::<Jeton, _, _>("jetons", |liste| {
                let n = liste.len();
                liste.retain(|j| j.id != "a");
                Ok(liste.len() != n)
            })
            .unwrap();
        assert!(retire);
        assert!(repo.get_json_list::<Jeton>("jetons").unwrap().is_empty());
    }

    /// Une fermeture qui refuse laisse la ligne telle quelle : la transaction
    /// est annulée, pas à moitié appliquée.
    #[test]
    fn un_refus_de_la_fermeture_annule_l_ecriture() {
        let repo = fresh_repo();
        repo.update_json_list::<Jeton, _, _>("jetons", |l| {
            l.push(jeton("a"));
            Ok(())
        })
        .unwrap();

        let erreur = repo
            .update_json_list::<Jeton, (), _>("jetons", |liste| {
                liste.clear();
                Err("refus deliberé".to_string())
            })
            .expect_err("le refus doit remonter");
        assert_eq!(erreur, "refus deliberé");

        let liste: Vec<Jeton> = repo.get_json_list("jetons").unwrap();
        assert_eq!(liste, vec![jeton("a")]);
    }

    #[test]
    fn airplay_pairing_key_is_namespaced() {
        assert_eq!(
            airplay_pairing_key("airplay2:x"),
            "airplay2_pairing:airplay2:x"
        );
    }
}
