use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;

/// Nombre d'écritures de réglages de zone que le schéma courant n'a pas pu
/// conserver. Ce compteur de processus est volontairement monotone : un
/// rapport de bogue doit dire qu'un mensonge a eu lieu même si l'utilisateur a
/// depuis refermé l'écran concerné (#2154).
static ZONE_SETTINGS_IGNORED: AtomicU64 = AtomicU64::new(0);

/// Instantané exposé par les diagnostics du serveur.
pub fn zone_settings_ignored() -> u64 {
    ZONE_SETTINGS_IGNORED.load(Ordering::Relaxed)
}

fn missing_column(error: &str) -> bool {
    error.contains("no such column") || error.contains("does not exist")
}

/// Rend visible une écriture que l'ancien code transformait en faux succès.
///
/// La base reste utilisable et le serveur continue de tourner, mais l'appelant
/// reçoit une erreur : une route HTTP ne peut donc plus répondre « enregistré »
/// quand la valeur n'a jamais atteint le disque. Les écritures internes
/// best-effort (identité réseau) utilisent aussi cette fonction pour le journal
/// et le compteur, puis choisissent explicitement de poursuivre.
fn setting_not_persisted(id: i64, setting: &'static str, error: &str) -> String {
    let count = ZONE_SETTINGS_IGNORED.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::warn!(
        zone_id = id,
        setting,
        error = %error,
        zone_settings_ignored = count,
        "zone_setting_not_persisted"
    );
    format!("réglage de zone « {setting} » non enregistré : colonne absente du schéma ({error})")
}

fn visible_setting_write(
    id: i64,
    setting: &'static str,
    result: Result<usize, String>,
) -> Result<(), String> {
    match result {
        Ok(_) => Ok(()),
        Err(error) if missing_column(&error) => Err(setting_not_persisted(id, setting, &error)),
        Err(error) => Err(error),
    }
}

/// Engine-agnostic SQL builders for zone_repo.
pub mod sql {
    use super::Engine;
    use super::SqlDialect;

    // NOTE: autoplay_enabled intentionally omitted from COLS.
    // The column is added by migration v36, but on Windows the migration
    // can fail silently (file locking).  row_to_zone reads cols.get(16) →
    // None → defaults to false (autoplay off).  The separate
    // get_autoplay_enabled() method handles reading the actual value safely.
    const COLS: &str = "id, name, output_type, output_device_id, volume, muted, online, gapless_enabled, group_id, sync_delay_ms, last_position_ms, last_track_id, last_track_source, last_track_source_id, max_sample_rate, fixed_volume";

    pub fn get_by_id<D: SqlDialect>(d: &D) -> String {
        format!("SELECT {COLS} FROM zones WHERE id = {}", d.placeholder(1))
    }

    pub fn get_by_device_id<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT {COLS} FROM zones WHERE output_device_id = {}",
            d.placeholder(1)
        )
    }

    pub fn select_base() -> String {
        format!("SELECT {COLS} FROM zones")
    }

    pub fn list_all() -> String {
        format!("SELECT {COLS} FROM zones ORDER BY name")
    }

    pub fn list_all_including_hidden() -> String {
        format!("SELECT {COLS} FROM zones ORDER BY name")
    }

    pub fn create<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO zones (name, output_type, output_device_id) VALUES ({}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    /// Delete duplicate zones, keeping only the one with the lowest id for each
    /// output_device_id. Returns the DELETE statement.
    pub fn deduplicate() -> &'static str {
        "DELETE FROM zones WHERE id NOT IN (SELECT MIN(id) FROM zones WHERE output_device_id IS NOT NULL GROUP BY output_device_id) AND output_device_id IS NOT NULL AND output_device_id IN (SELECT output_device_id FROM zones WHERE output_device_id IS NOT NULL GROUP BY output_device_id HAVING COUNT(*) > 1)"
    }

    /// Rendre son prefixe `local:` a une zone locale qui l'a perdu.
    ///
    /// Une zone creee avec le NOM du peripherique au lieu de son identifiant de
    /// registre ne joue rien : l'orchestrateur reconnait une sortie locale au
    /// prefixe `local:`, et sans lui la zone part sur le chemin renderer
    /// reseau — telechargement complet, decodage, re-encodage, puis une URL
    /// poussee vers un appareil qui n'existe pas (DEvir, #1823). Elle echappe
    /// en prime au dedoublonnage, qui regroupe par `output_device_id` : deux
    /// valeurs differentes pour un seul appareil physique.
    ///
    /// Deux instructions, dans cet ordre :
    ///
    /// 1. supprimer le jumeau prefixe **s'il est masque** — une zone masquee
    ///    est une zone que l'utilisateur a supprimee, ses reglages sont deja
    ///    ecartes par son geste, et elle bloque l'index unique ;
    /// 2. reecrire l'identifiant, mais seulement s'il ne heurte plus rien.
    ///
    /// Le cas ou les DEUX zones sont visibles n'est volontairement pas traite :
    /// il faudrait choisir laquelle des deux configurations survit, et aucune
    /// regle automatique ne vaut mieux que la question posee a l'utilisateur.
    pub fn reparer_prefixe_local() -> [&'static str; 2] {
        [
            "DELETE FROM zones WHERE is_hidden = 1 AND output_device_id IN ( \
                SELECT 'local:' || z.output_device_id FROM zones z \
                WHERE z.output_type = 'local' \
                  AND z.output_device_id IS NOT NULL \
                  AND z.output_device_id NOT LIKE 'local:%' )",
            "UPDATE zones SET output_device_id = 'local:' || output_device_id \
             WHERE output_type = 'local' \
               AND output_device_id IS NOT NULL \
               AND output_device_id NOT LIKE 'local:%' \
               AND NOT EXISTS ( \
                SELECT 1 FROM zones d \
                WHERE d.output_device_id = 'local:' || zones.output_device_id )",
        ]
    }

    /// Colonnes reportees d'un doublon vers la zone conservee, avec la valeur
    /// qui compte pour « pas encore regle ».
    ///
    /// Le defaut declare ici est celui du schema. Il sert deux fois : a savoir
    /// si la survivante est vierge, et a savoir si le doublon apporte vraiment
    /// quelque chose. Voir `ZoneRepo::merge_duplicate_settings` pour la regle
    /// et pour l'absence deliberee de `gapless_enabled`.
    const REGLAGES_A_FUSIONNER: &[(&str, &str)] = &[
        // Drapeaux : defaut 0, donc seul un 1 se reporte.
        ("fixed_volume", "0"),
        ("alac_passthrough", "0"),
        ("aac_passthrough", "0"),
        ("autoplay_enabled", "0"),
        ("dlna_lpcm", "0"),
        ("dlna_wav24", "0"),
        ("dlna_cap_16bit", "0"),
        ("dlna_native_flac", "0"),
        // Delais et decalages : defaut 0, toute autre valeur est un reglage.
        ("dlna_play_delay_ms", "0"),
        ("sync_delay_ms", "0"),
        ("lyrics_offset_ms", "0"),
    ];

    /// Colonnes dont le defaut est NULL.
    ///
    /// `brand` et `model` figuraient ici et n'ont JAMAIS ete des colonnes de
    /// `zones` : ils vivent dans `settings`, sous `zone_{id}_brand` et
    /// `zone_{id}_model`. Les deux instructions correspondantes echouaient donc
    /// a chaque demarrage, sur chaque machine, et le garde-fou les sautait —
    /// du code mort sous une couverture apparente (#1832, decouvert dans les
    /// journaux de DEvir). Le report de ces deux reglages se fait desormais la
    /// ou ils sont reellement ranges, voir
    /// [`ZoneRepo::reporter_reglages_de_doublons`].
    const REGLAGES_NULLABLES: &[&str] = &["max_sample_rate"];

    /// Les zones en doublon, survivante d'abord dans chaque groupe.
    ///
    /// Meme regroupement que [`Self::deduplicate`] — `MIN(id)` survit — mais
    /// rendu ligne par ligne, pour pouvoir traiter les reglages qui ne sont pas
    /// des colonnes.
    pub fn doublons_par_appareil() -> &'static str {
        "SELECT output_device_id, id FROM zones \
         WHERE output_device_id IS NOT NULL \
           AND output_device_id IN ( \
             SELECT output_device_id FROM zones \
             WHERE output_device_id IS NOT NULL \
             GROUP BY output_device_id HAVING COUNT(*) > 1 ) \
         ORDER BY output_device_id, id"
    }

    /// Instructions de fusion, dans l'ordre. Chacune ne touche QUE les zones
    /// conservees d'un groupe en doublon, et seulement quand elles sont restees
    /// au defaut — un reglage explicite n'est jamais ecrase.
    pub fn merge_duplicate_settings(_engine: Engine) -> Vec<String> {
        let survivantes = "SELECT MIN(id) FROM zones \
             WHERE output_device_id IS NOT NULL \
             GROUP BY output_device_id HAVING COUNT(*) > 1";
        let mut sorties = Vec::new();

        for (colonne, defaut) in REGLAGES_A_FUSIONNER {
            sorties.push(format!(
                "UPDATE zones SET {colonne} = ( \
                    SELECT MAX(d.{colonne}) FROM zones d \
                    WHERE d.output_device_id = zones.output_device_id AND d.id <> zones.id \
                 ) \
                 WHERE id IN ({survivantes}) \
                   AND COALESCE({colonne}, {defaut}) = {defaut} \
                   AND EXISTS ( \
                    SELECT 1 FROM zones d \
                    WHERE d.output_device_id = zones.output_device_id AND d.id <> zones.id \
                      AND COALESCE(d.{colonne}, {defaut}) <> {defaut} \
                   )"
            ));
        }

        for colonne in REGLAGES_NULLABLES {
            sorties.push(format!(
                "UPDATE zones SET {colonne} = ( \
                    SELECT MAX(d.{colonne}) FROM zones d \
                    WHERE d.output_device_id = zones.output_device_id AND d.id <> zones.id \
                      AND d.{colonne} IS NOT NULL \
                 ) \
                 WHERE id IN ({survivantes}) \
                   AND {colonne} IS NULL \
                   AND EXISTS ( \
                    SELECT 1 FROM zones d \
                    WHERE d.output_device_id = zones.output_device_id AND d.id <> zones.id \
                      AND d.{colonne} IS NOT NULL \
                   )"
            ));
        }

        // `dsd_mode` a pour defaut la chaine 'auto', pas 0 ni NULL.
        sorties.push(format!(
            "UPDATE zones SET dsd_mode = ( \
                SELECT MAX(d.dsd_mode) FROM zones d \
                WHERE d.output_device_id = zones.output_device_id AND d.id <> zones.id \
                  AND d.dsd_mode IS NOT NULL AND d.dsd_mode <> 'auto' \
             ) \
             WHERE id IN ({survivantes}) \
               AND COALESCE(dsd_mode, 'auto') = 'auto' \
               AND EXISTS ( \
                SELECT 1 FROM zones d \
                WHERE d.output_device_id = zones.output_device_id AND d.id <> zones.id \
                  AND COALESCE(d.dsd_mode, 'auto') <> 'auto' \
               )"
        ));

        sorties
    }

    pub fn update_field<D: SqlDialect>(d: &D, field: &str) -> String {
        format!(
            "UPDATE zones SET {field} = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn set_online_by_device<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE zones SET online = {} WHERE output_device_id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn rename_generic_local_label<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE zones SET name = {} \
             WHERE id = {} AND name IN ('This Computer', 'Cet ordinateur')",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn hide_duplicate_generic_local<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE zones SET is_hidden = 1 \
             WHERE id <> {} AND output_type = 'local' \
             AND name IN ('This Computer', 'Cet ordinateur') \
             AND COALESCE(is_hidden, 0) = 0",
            d.placeholder(1)
        )
    }

    pub fn delete_by_id<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE zones SET is_hidden = 1 WHERE id = {}",
            d.placeholder(1)
        )
    }

    pub fn delete_all() -> &'static str {
        // last_track_id is the permanent free-tier activation marker: a
        // resurrected zone would otherwise keep consuming a quota slot on
        // its first play, defeating the whole point of the reset.
        "UPDATE zones SET is_hidden = 1, last_track_id = NULL"
    }

    pub fn unhide_by_device_id<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE zones SET is_hidden = 0 WHERE output_device_id = {} AND COALESCE(is_hidden, 0) = 1",
            d.placeholder(1)
        )
    }

    pub fn save_playback_position<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE zones SET last_position_ms = {}, last_track_id = {}, last_track_source = {}, last_track_source_id = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5)
        )
    }

    pub fn clear_playback_position<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE zones SET last_position_ms = 0, last_track_id = NULL, last_track_source = NULL, last_track_source_id = NULL WHERE id = {}",
            d.placeholder(1)
        )
    }

    pub fn update_dsp<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE zones SET dsp_preset_id = {}, dsp_enabled = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn get_dsp_config<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT dsp_preset_id, COALESCE(dsp_enabled, 0) FROM zones WHERE id = {}",
            d.placeholder(1)
        )
    }

    pub fn count() -> &'static str {
        // Exclude soft-deleted (hidden) zones so user-facing counts stay
        // consistent with list() and count_online(): a deleted zone must
        // not keep inflating system stats.
        "SELECT COUNT(*) FROM zones WHERE COALESCE(is_hidden, 0) = 0"
    }

    pub fn count_online() -> &'static str {
        "SELECT COUNT(*) FROM zones WHERE online = 1 AND COALESCE(is_hidden, 0) = 0"
    }

    /// Zones that have actually been used (played at least one track, i.e.
    /// `last_track_id` is set) and are online. Auto-discovered but never-played
    /// ("dormant") zones are excluded — they must not consume the free-tier
    /// quota. `last_track_id` is a permanent activation marker: it is only ever
    /// SET (save_playback_position), never cleared in practice.
    pub fn count_active() -> &'static str {
        "SELECT COUNT(*) FROM zones WHERE online = 1 AND COALESCE(is_hidden, 0) = 0 AND last_track_id IS NOT NULL"
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Zone {
    pub id: Option<i64>,
    pub name: String,
    pub output_type: Option<String>,
    pub output_device_id: Option<String>,
    pub volume: i32,
    pub muted: bool,
    pub online: bool,
    pub gapless_enabled: bool,
    pub group_id: Option<String>,
    pub sync_delay_ms: i32,
    pub last_position_ms: i64,
    pub last_track_id: Option<i64>,
    pub last_track_source: Option<String>,
    pub last_track_source_id: Option<String>,
    pub max_sample_rate: Option<u32>,
    pub fixed_volume: bool,
    pub autoplay_enabled: bool,
}

/// Ce qui se passe quand la file de lecture d'une zone se vide (#2271).
///
/// Remplace le booleen `autoplay_enabled`, qui ne savait dire que « oui » ou
/// « non » alors que la demande d'origine portait sur le CHOIX de la source de
/// continuation.
///
/// **Deux valeurs seulement, et c'est volontaire.** Le socle pose le
/// mecanisme ; il n'invente aucun mode. Les sources evoquees dans l'issue
/// (album aleatoire, artiste aleatoire, annee aleatoire, morceaux aleatoires,
/// radio, favoris, playlist) n'ont a ce jour **aucun comportement attendu
/// defini** — ni combien de titres, ni dans quel perimetre, ni s'il faut
/// reapprovisionner quand la file se revide. Les ajouter ici reviendrait a
/// trancher un arbitrage produit a la place de qui de droit. Chaque mode
/// nouveau se resume desormais a : definir son comportement, ajouter une
/// variante, ajouter un bras de `match` dans le bloc « queue ended » du
/// poller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutoplayMode {
    /// La lecture s'arrete en fin de file. Defaut historique et actuel.
    #[default]
    Off,
    /// Radio d'artistes similaires — **exactement** ce que fait Tune
    /// aujourd'hui quand `autoplay_enabled` vaut vrai, cascade de replis
    /// comprise (radio depuis l'historique si aucune graine, radio du service
    /// de streaming si l'ecoute en cours en vient, generateur genre/BPM
    /// local, puis repli streaming).
    Similar,
}

impl AutoplayMode {
    /// Le nom du mode dans l'API et dans l'interface.
    pub fn as_str(&self) -> &'static str {
        match self {
            AutoplayMode::Off => "off",
            AutoplayMode::Similar => "similar",
        }
    }

    /// L'encodage RANGE EN BASE, qui n'est pas le nom d'API.
    ///
    /// Les deux modes d'aujourd'hui recouvrent exactement l'ancien booleen :
    /// on les ecrit donc `"0"` et `"1"`, tels quels. Une version anterieure de
    /// Tune, qui lit la colonne avec `as_i64()`, continue de comprendre le
    /// reglage — une bascule vers `similar` puis un retour a une version plus
    /// ancienne ne perd pas l'autoplay. Un mode reellement nouveau s'ecrira
    /// sous son nom, et sera alors vu comme « eteint » par les versions qui ne
    /// le connaissent pas : inevitable, mais reserve aux modes qui n'existent
    /// pas encore.
    pub fn as_stocke(&self) -> &'static str {
        match self {
            AutoplayMode::Off => "0",
            AutoplayMode::Similar => "1",
        }
    }

    /// Lecture STRICTE, pour valider ce qui arrive par l'API.
    ///
    /// `None` = mode inconnu, que la route doit refuser au lieu de le ranger
    /// en base. C'est le contraire de [`ZoneRepo::get_autoplay_mode`], qui
    /// doit composer avec ce qui est deja ecrit.
    pub fn from_str_stocke(s: &str) -> Option<Self> {
        match s.trim() {
            "off" | "0" => Some(AutoplayMode::Off),
            "similar" | "1" => Some(AutoplayMode::Similar),
            _ => None,
        }
    }

    /// Les noms acceptes par `PATCH /zones/{id}`, pour le message de refus.
    pub const NOMS: [&'static str; 2] = ["off", "similar"];
}

/// La charge utile `zone` d'une zone qui vient de naitre, dans le contrat que
/// le client attend.
///
/// Ce n'est PAS `serde_json::to_value(&zone)`. La ligne de base porte le volume
/// en 0..100, le client le veut en 0..1 ; et une zone neuve doit ANNONCER son
/// etat de lecture plutot que de l'omettre — le client fusionne cette charge
/// utile sans refetch, et un champ absent y laisse la valeur precedente, celle
/// d'une autre zone.
///
/// Vit ici, a cote de `Zone`, parce que TROIS emetteurs doivent l'utiliser et
/// qu'ils ne sont pas dans le meme crate : la route `POST /zones` et la
/// decouverte (tune-server), et SlimProto (tune-core). Trois copies du meme
/// contrat rediverge toujours — c'est exactement ce que #2224 a mis au jour, et
/// le `to_value` brut de son premier correctif faisait repartir le volume a 50
/// la ou le client attend 0.5 (JP Robbe).
pub fn zone_creee_contrat_client(
    zone: Option<&Zone>,
    id: i64,
    nom_de_repli: &str,
) -> serde_json::Value {
    use serde_json::json;
    let mut v = zone
        .and_then(|z| serde_json::to_value(z).ok())
        .unwrap_or_else(|| json!({"id": id, "name": nom_de_repli}));
    if let Some(obj) = v.as_object_mut() {
        obj.insert("state".into(), json!("stopped"));
        obj.insert("current_track".into(), json!(null));
        obj.insert("position_ms".into(), json!(0));
        obj.insert("queue_length".into(), json!(0));
        // Zone qui vient de naitre : rien ne joue. Poser l'aleatoire et la
        // repetition plutot que de les omettre — meme divergence que #2092,
        // en plus discret.
        obj.insert("shuffle".into(), json!(false));
        // Le TYPE et non la chaine « off » : un renommage de variante suit ici
        // tout seul.
        obj.insert("repeat".into(), json!(crate::playback::RepeatMode::Off));
        let vol = zone.map(|z| z.volume).unwrap_or(50);
        obj.insert("volume".into(), json!(vol as f64 / 100.0));
    }
    v
}

pub struct ZoneRepo {
    db: Arc<dyn DbBackend>,
}

impl ZoneRepo {
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

    fn update_field_sql(&self, field: &str) -> String {
        match self.db.engine() {
            Engine::Sqlite => sql::update_field(&SqliteDialect, field),
            Engine::Postgres => sql::update_field(&PostgresDialect, field),
        }
    }

    pub fn get(&self, id: i64) -> Result<Option<Zone>, String> {
        let sql = self.dialect_sql(sql::get_by_id, sql::get_by_id);
        let params: [&dyn ToSqlValue; 1] = [&id];
        // Strong read (write connection) so a lagging WAL read snapshot can't
        // return a stale row. The track-resolve path reads per-zone playback
        // settings here (max_sample_rate, dsd_mode, alac_passthrough…); with the
        // weak pool read a value just changed via PATCH could be missed on the
        // very next track — the setting appeared "not to persist" (JP: échantillonnage
        // reset au morceau suivant). A weak-then-strong-on-empty fallback (as in
        // get_by_device_id) does NOT help here: the row exists, only the field is
        // stale, so the fallback never triggers. Mirror list()'s unconditional
        // strong read. A single zone by id is a tiny query.
        let rows = self.db.query_many_strong(&sql, &params)?;
        Ok(rows.first().map(row_to_zone))
    }

    /// Look up a zone by its output device id.
    pub fn get_by_device_id(&self, device_id: &str) -> Result<Option<Zone>, String> {
        let sql = self.dialect_sql(sql::get_by_device_id, sql::get_by_device_id);
        let params: [&dyn ToSqlValue; 1] = [&device_id];
        // Try read path first, fall back to strong (same pattern as list).
        if let Some(row) = self.db.query_one(&sql, &params)? {
            return Ok(Some(row_to_zone(&row)));
        }
        // Strong read to see the writer's own pending commits (WAL lag).
        let rows = self.db.query_many_strong(&sql, &params)?;
        Ok(rows.first().map(row_to_zone))
    }

    pub fn list(&self) -> Result<Vec<Zone>, String> {
        // Read via the write connection (strong) so a lagging WAL read snapshot
        // can never transiently drop a recently-created/updated zone. The old
        // code read from the pool and only fell back to strong when the result
        // was EMPTY — a partial stale read (non-empty but missing one zone)
        // slipped through, causing zones to intermittently disappear from the
        // UI after activity (reported by DEvir). Zone lists are tiny (a handful
        // of rows) and this matches the existing WAL-lag fallback pattern.
        let filtered = format!(
            "{} WHERE COALESCE(is_hidden, 0) = 0 ORDER BY name",
            sql::select_base()
        );
        match self.db.query_many_strong(&filtered, &[]) {
            Ok(rows) => Ok(rows.iter().map(row_to_zone).collect()),
            Err(_) => {
                // is_hidden column doesn't exist (pre-migration DB) — fall back
                // to the unfiltered query, still via the strong read.
                let strong = self.db.query_many_strong(&sql::list_all(), &[])?;
                Ok(strong.iter().map(row_to_zone).collect())
            }
        }
    }

    pub fn create(
        &self,
        name: &str,
        output_type: Option<&str>,
        output_device_id: Option<&str>,
    ) -> Result<i64, String> {
        // INSERT + last_insert_rowid. We deliberately do NOT use
        // write_tx here: a write tx wraps in `BEGIN DEFERRED`, which
        // fails when a SQLite-level transaction is already in progress
        // (cf. the `create_zone_during_open_transaction` test, where
        // a scan tx is active). Sequential `execute` + `last_insert_rowid`
        // each take the write lock briefly and don't try to start a
        // new tx; both calls share the same rusqlite mutex on SQLite so
        // the rowid we read reflects the INSERT we just did.
        let create_sql = self.dialect_sql(sql::create, sql::create);
        let params: [&dyn ToSqlValue; 3] = [&name, &output_type, &output_device_id];
        Ok(self.db.execute_returning_id(&create_sql, &params)?)
    }

    /// Atomically get an existing zone by output_device_id, or create a new one.
    /// Returns `(zone_id, created)` where `created` is true if a new zone was inserted.
    ///
    /// If the device previously had a zone that was soft-deleted (is_hidden=1),
    /// the zone is un-hidden and returned instead of creating a duplicate.
    ///
    /// If a concurrent writer inserts the same device_id between our check and
    /// our INSERT (race), the UNIQUE index will reject the INSERT.  We catch
    /// that and return the existing zone instead of propagating the error.
    pub fn get_or_create(
        &self,
        name: &str,
        output_type: Option<&str>,
        output_device_id: &str,
    ) -> Result<(i64, bool), String> {
        // Check if a zone with this device_id already exists (including hidden).
        if let Some(existing) = self.get_by_device_id(output_device_id)? {
            if let Some(id) = existing.id {
                if self.is_device_hidden(output_device_id) {
                    tracing::debug!(
                        zone_id = id,
                        device_id = output_device_id,
                        "zone_hidden_skipping_auto_unhide"
                    );
                }
                return Ok((id, false));
            }
        }
        // No existing zone — try to create one.
        match self.create(name, output_type, Some(output_device_id)) {
            Ok(id) => Ok((id, true)),
            Err(e) if e.contains("UNIQUE constraint failed") => {
                // Race or hidden zone: another thread inserted the same
                // device_id, or a hidden zone exists with this device_id.
                // Return the existing zone as-is. A soft-deleted (hidden) zone
                // stays hidden here too — same rule as the fast path above, so
                // deleted zones don't reappear regardless of which branch runs.
                if let Some(existing) = self.get_by_device_id(output_device_id)? {
                    if let Some(id) = existing.id {
                        if self.is_device_hidden(output_device_id) {
                            tracing::debug!(
                                zone_id = id,
                                device_id = output_device_id,
                                "zone_hidden_skipping_auto_unhide_after_unique_conflict"
                            );
                        }
                        return Ok((id, false));
                    }
                }
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    /// Remove duplicate zones that share the same output_device_id, keeping only
    /// the one with the lowest id. Returns the number of duplicates removed.
    ///
    /// Les reglages des doublons sont REPORTES sur la survivante avant la
    /// suppression : voir [`Self::merge_duplicate_settings`]. Sans cela, la
    /// survivante etant choisie par son anciennete et non par ce qu'elle porte,
    /// une zone reglee par l'utilisateur pouvait etre effacee au demarrage avec
    /// tous ses reglages avances (#1774, Yves — « les parametres coches n'ont
    /// pas ete sauvegardes »).
    pub fn deduplicate(&self) -> Result<usize, String> {
        self.reparer_prefixe_local()?;
        self.merge_duplicate_settings()?;
        self.reporter_reglages_de_doublons()?;
        self.db.execute(sql::deduplicate(), &[])
    }

    /// Reporter sur la survivante les reglages de zone ranges dans `settings`.
    ///
    /// [`Self::merge_duplicate_settings`] ne sait traiter que des COLONNES. Or
    /// une zone porte une dizaine de reglages qui n'en sont pas : profil
    /// d'egaliseur, crossfeed, mode audiophile, qualite, trim de gain, profil
    /// audio, renderer UPnP, marque, modele, epingles — tous ranges dans
    /// `settings` sous `zone_{id}_{quoi}`.
    ///
    /// Aucun d'eux n'etait reporte : `zone_repo` ne connaissait pas
    /// `SettingsRepo`. Le doublon supprime, ses reglages restaient rattaches a
    /// l'identifiant d'une zone qui n'existe plus — le defaut de #1774, une
    /// couche plus bas, et le plus visible des dix est l'egaliseur (#1832).
    ///
    /// On reporte **par prefixe**, pas par liste : un onzieme reglage arrivera,
    /// et il doit etre couvert sans que personne y pense.
    ///
    /// Meme regle que pour les colonnes : la valeur du doublon ne s'applique
    /// que si la survivante n'a rien. Un reglage explicite ne cede jamais.
    ///
    /// Les cles du doublon ne sont **pas** supprimees. Un report est
    /// reversible, un effacement ne l'est pas, et rien ne presse : le menage
    /// des cles orphelines est un sujet distinct.
    pub fn reporter_reglages_de_doublons(&self) -> Result<(), String> {
        let lignes = match self.db.query_many_strong(sql::doublons_par_appareil(), &[]) {
            Ok(l) => l,
            // Base anterieure a `output_device_id` : rien a reporter.
            Err(e) if e.contains("no such column") || e.contains("does not exist") => return Ok(()),
            Err(e) => return Err(e),
        };
        if lignes.is_empty() {
            return Ok(());
        }

        // Grouper par appareil, en gardant l'ordre : la premiere est la
        // survivante (`ORDER BY output_device_id, id`).
        let mut groupes: Vec<(String, Vec<i64>)> = Vec::new();
        for ligne in &lignes {
            let appareil = ligne
                .first()
                .and_then(|v| v.as_string())
                .unwrap_or_default();
            let Some(id) = ligne.get(1).and_then(|v| v.as_i64()) else {
                continue;
            };
            match groupes.last_mut() {
                Some((precedent, ids)) if *precedent == appareil => ids.push(id),
                _ => groupes.push((appareil, vec![id])),
            }
        }

        let settings = super::settings_repo::SettingsRepo::with_backend(self.db.clone());
        // Meme garde-fou que pour les colonnes, et pour la meme raison : une
        // base incomplete ne doit pas empecher le dedoublonnage de tourner.
        // Elle le DIT, en revanche — c'est le silence qui avait rendu #1832
        // invisible.
        let manque = |e: &String| {
            e.contains("no such table")
                || e.contains("no such column")
                || e.contains("does not exist")
        };
        // Une carte, et non la liste brute : ce qu'on vient de reporter doit
        // compter comme « deja pose » pour le doublon suivant. Sinon, deux
        // doublons apportant le meme reglage, le second ecraserait le premier —
        // et le survivant heriterait du plus recent au lieu du plus ancien,
        // sans qu'aucune regle l'ait decide.
        let mut connues: std::collections::HashMap<String, String> = match settings.all() {
            Ok(v) => v.into_iter().collect(),
            Err(e) if manque(&e) => {
                tracing::warn!(error = %e, "zone_reglages_table_absente_report_saute");
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let apportees: Vec<(String, String)> = connues
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let mut reportees = 0usize;

        for (_, ids) in &groupes {
            let Some((survivante, doublons)) = ids.split_first() else {
                continue;
            };
            let prefixe_survivante = format!("zone_{survivante}_");
            for doublon in doublons {
                let prefixe_doublon = format!("zone_{doublon}_");
                for (cle, valeur) in &apportees {
                    let Some(quoi) = cle.strip_prefix(&prefixe_doublon) else {
                        continue;
                    };
                    if valeur.trim().is_empty() {
                        continue;
                    }
                    let cible = format!("{prefixe_survivante}{quoi}");
                    let deja_pose = connues.get(&cible).is_some_and(|v| !v.trim().is_empty());
                    if deja_pose {
                        continue;
                    }
                    match settings.set(&cible, valeur) {
                        Ok(()) => {}
                        Err(e) if manque(&e) => {
                            tracing::warn!(error = %e, "zone_reglages_table_absente_report_saute");
                            return Ok(());
                        }
                        Err(e) => return Err(e),
                    }
                    connues.insert(cible.clone(), valeur.clone());
                    reportees += 1;
                    tracing::info!(
                        depuis = %cle,
                        vers = %cible,
                        "zone_reglage_reporte_depuis_doublon"
                    );
                }
            }
        }

        if reportees > 0 {
            tracing::info!(reglages = reportees, "zone_reglages_reportes");
        }
        Ok(())
    }

    /// Rendre leur prefixe `local:` aux zones locales qui l'ont perdu.
    ///
    /// AVANT la fusion et le dedoublonnage, et c'est tout l'interet de
    /// l'ordre : une fois les identifiants remis en forme, les deux zones d'un
    /// meme appareil portent enfin la meme valeur, donc `deduplicate` les voit
    /// et `merge_duplicate_settings` reporte les reglages. Lancee apres, la
    /// reparation ne rattraperait plus rien. Voir [`sql::reparer_prefixe_local`].
    pub fn reparer_prefixe_local(&self) -> Result<(), String> {
        for (rang, instruction) in sql::reparer_prefixe_local().iter().enumerate() {
            match self.db.execute(instruction, &[]) {
                Ok(n) if n > 0 => {
                    tracing::info!(
                        zones = n,
                        etape = if rang == 0 {
                            "jumeau_masque_supprime"
                        } else {
                            "prefixe_rendu"
                        },
                        "zone_local_prefix_repare"
                    );
                }
                Ok(_) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Reporter sur la zone conservee les reglages non par defaut de ses
    /// doublons, avant que [`Self::deduplicate`] ne les supprime.
    ///
    /// La survivante est `MIN(id)` — la plus ANCIENNE. Rien ne garantit que ce
    /// soit celle que l'utilisateur a reglee : c'est meme le contraire quand un
    /// appareil a change d'identite et qu'une seconde zone est apparue, plus
    /// recente, devenue celle que l'interface montre. Le demarrage suivant
    /// supprimait alors la ligne configuree, en silence.
    ///
    /// La regle est la meme pour toutes les colonnes : **si la survivante est
    /// restee au defaut et qu'un doublon porte autre chose, on prend celle du
    /// doublon.** Un reglage explicite ne doit jamais ceder a une valeur que
    /// personne n'a choisie. Quand plusieurs doublons different, `MAX` tranche
    /// de facon deterministe — le cas ne se pose en pratique que si l'appareil
    /// a produit trois zones et qu'au moins deux ont ete reglees.
    ///
    /// `gapless_enabled` est volontairement ABSENT : son defaut vaut 1, donc
    /// « non par defaut » y signifie 0, et la meme regle l'ecraserait dans le
    /// mauvais sens. Le traiter demande de distinguer « jamais touche » de
    /// « desactive exprès », ce que le schema ne permet pas aujourd'hui.
    pub fn merge_duplicate_settings(&self) -> Result<(), String> {
        for instruction in sql::merge_duplicate_settings(self.db.engine()) {
            match self.db.execute(&instruction, &[]) {
                Ok(_) => {}
                // Une base ancienne peut ne pas avoir toutes ces colonnes. On
                // ne fait pas echouer le demarrage pour autant, mais on le DIT
                // — contrairement au silence qui a rendu ce defaut invisible.
                Err(e) if e.contains("no such column") || e.contains("does not exist") => {
                    tracing::warn!(error = %e, "zone_merge_column_missing_skipped");
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub fn update_volume(&self, id: i64, volume: i32) -> Result<(), String> {
        let sql = self.update_field_sql("volume");
        let params: [&dyn ToSqlValue; 2] = [&volume, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_muted(&self, id: i64, muted: bool) -> Result<(), String> {
        let val: String = if muted { "1".into() } else { "0".into() };
        let sql = self.update_field_sql("muted");
        let params: [&dyn ToSqlValue; 2] = [&val, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_name(&self, id: i64, name: &str) -> Result<(), String> {
        let sql = self.update_field_sql("name");
        let params: [&dyn ToSqlValue; 2] = [&name, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_output_device(&self, id: i64, device_id: &str) -> Result<(), String> {
        let sql = self.update_field_sql("output_device_id");
        let params: [&dyn ToSqlValue; 2] = [&device_id, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_output_type(&self, id: i64, output_type: &str) -> Result<(), String> {
        let sql = self.update_field_sql("output_type");
        let params: [&dyn ToSqlValue; 2] = [&output_type, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_online(&self, id: i64, online: bool) -> Result<(), String> {
        let val: String = if online { "1".into() } else { "0".into() };
        let sql = self.update_field_sql("online");
        let params: [&dyn ToSqlValue; 2] = [&val, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_gapless_enabled(&self, id: i64, enabled: bool) -> Result<(), String> {
        let val: String = if enabled { "1".into() } else { "0".into() };
        let sql = self.update_field_sql("gapless_enabled");
        let params: [&dyn ToSqlValue; 2] = [&val, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_fixed_volume(&self, id: i64, enabled: bool) -> Result<(), String> {
        let val: String = if enabled { "1".into() } else { "0".into() };
        let sql = self.update_field_sql("fixed_volume");
        let params: [&dyn ToSqlValue; 2] = [&val, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_autoplay_enabled(&self, id: i64, enabled: bool) -> Result<(), String> {
        let val: String = if enabled { "1".into() } else { "0".into() };
        let sql = self.update_field_sql("autoplay_enabled");
        let params: [&dyn ToSqlValue; 2] = [&val, &id];
        visible_setting_write(id, "autoplay_enabled", self.db.execute(&sql, &params))
    }

    pub fn is_device_hidden(&self, device_id: &str) -> bool {
        let placeholder = match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(1),
            Engine::Postgres => PostgresDialect.placeholder(1),
        };
        let sql = format!(
            "SELECT COALESCE(is_hidden, 0) FROM zones WHERE output_device_id = {placeholder}"
        );
        let params: [&dyn ToSqlValue; 1] = [&device_id];
        self.db
            .query_one(&sql, &params)
            .ok()
            .flatten()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .map(|v| v != 0)
            .unwrap_or(false)
    }

    /// L'identifiant d'une zone MASQUÉE portant ce nom, s'il y en a une.
    ///
    /// Supprimer une zone la masque (`is_hidden = 1`) et le garde-fou de la
    /// découverte teste `is_device_hidden(device_id)`. Mais `device_id` est
    /// dérivé de l'adresse IP : dès qu'elle change, la ligne masquée porte
    /// l'ancien identifiant, le garde-fou ne reconnaît plus rien, et le
    /// rattrapage par nom ne peut pas aider non plus — il lit `list()`, qui
    /// filtre `is_hidden = 0`. La zone supprimée renaissait donc à neuf
    /// (#1528).
    ///
    /// Cette lecture est le seul chemin qui voit les lignes masquées par leur
    /// nom. Elle sert à ré-ancrer la zone masquée sur le nouvel identifiant,
    /// **sans la démasquer** : la suppression reste une suppression, et le
    /// garde-fou redevient opérant au tour suivant.
    pub fn find_hidden_id_by_name(&self, name: &str) -> Option<i64> {
        let placeholder = match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(1),
            Engine::Postgres => PostgresDialect.placeholder(1),
        };
        let sql = format!(
            "SELECT id FROM zones WHERE name = {placeholder} \
             AND COALESCE(is_hidden, 0) = 1 ORDER BY id LIMIT 1"
        );
        let params: [&dyn ToSqlValue; 1] = [&name];
        self.db
            .query_one(&sql, &params)
            .ok()
            .flatten()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
    }

    /// Le mode de continuation de la zone quand la file se vide (#2271).
    ///
    /// Lecture TOLERANTE, par opposition a
    /// [`AutoplayMode::from_str_stocke`] qui valide une entree d'API :
    ///
    /// - colonne absente (base pre-v36) ou NULL → `Off`, l'ancien defaut ;
    /// - entier `0`, ou texte `"0"` / `"off"` → `Off` ;
    /// - entier non nul, ou texte `"1"` / `"similar"` → `Similar` ;
    /// - **tout autre texte → `Similar`**, jamais `Off`.
    ///
    /// Ce dernier point est deliberé. Un serveur plus recent peut avoir ecrit
    /// un mode que cette version ne connait pas ; retomber sur `Off`
    /// COUPERAIT la musique, ce qui est exactement l'inverse de la demande
    /// d'origine (« n'arretez pas la musique »). On enchaine avec la
    /// strategie livree plutot que de se taire.
    pub fn get_autoplay_mode(&self, id: i64) -> AutoplayMode {
        let placeholder = match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(1),
            Engine::Postgres => PostgresDialect.placeholder(1),
        };
        let sql =
            format!("SELECT COALESCE(autoplay_enabled, 0) FROM zones WHERE id = {placeholder}");
        let params: [&dyn ToSqlValue; 1] = [&id];
        let Some(val) = self
            .db
            .query_one(&sql, &params)
            .ok()
            .flatten()
            .and_then(|cols| cols.first().cloned())
        else {
            return AutoplayMode::Off;
        };
        if val.is_null() {
            return AutoplayMode::Off;
        }
        // PostgreSQL declare la colonne TEXT : `'0'` / `'1'` y arrivent en
        // texte. SQLite, par affinite INTEGER, convertit ces memes chaines en
        // entiers et ne garde en TEXT que les noms de mode. Les deux moteurs
        // passent donc par ici avec des variantes differentes pour la MEME
        // valeur logique.
        if let Some(s) = val.as_str() {
            return match s.trim() {
                "0" | "off" => AutoplayMode::Off,
                _ => AutoplayMode::Similar,
            };
        }
        match val.as_i64() {
            Some(0) | None => AutoplayMode::Off,
            Some(_) => AutoplayMode::Similar,
        }
    }

    /// Ecrit le mode de continuation d'une zone (#2271).
    ///
    /// **Aucune migration n'est consommee** : la valeur va dans la colonne
    /// `zones.autoplay_enabled` qui existe deja. Voir
    /// [`AutoplayMode::as_stocke`] pour l'encodage, choisi pour rester
    /// relisible par une version anterieure de Tune.
    pub fn update_autoplay_mode(&self, id: i64, mode: AutoplayMode) -> Result<(), String> {
        let sql = self.update_field_sql("autoplay_enabled");
        let val = mode.as_stocke().to_string();
        let params: [&dyn ToSqlValue; 2] = [&val, &id];
        visible_setting_write(id, "autoplay_mode", self.db.execute(&sql, &params))
    }

    /// Safely read autoplay_enabled for a zone.  Returns false (the default)
    /// if the column doesn't exist (pre-v36 database).
    ///
    /// #2271 — POINT DE COMPATIBILITE. Le poller interroge toujours ce
    /// booleen dans son bloc « queue ended » (`poller.rs`) ; il n'a pas a
    /// connaitre les modes tant qu'il n'en existe qu'un seul de reellement
    /// enchainable. « L'autoplay est actif » se lit desormais « le mode n'est
    /// pas `off` », ce qui reste vrai quel que soit le mode ajoute plus tard.
    pub fn get_autoplay_enabled(&self, id: i64) -> bool {
        self.get_autoplay_mode(id) != AutoplayMode::Off
    }

    pub fn get_dsd_mode(&self, id: i64) -> String {
        let placeholder = match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(1),
            Engine::Postgres => PostgresDialect.placeholder(1),
        };
        let sql = format!("SELECT COALESCE(dsd_mode, 'auto') FROM zones WHERE id = {placeholder}");
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db
            .query_one(&sql, &params)
            .ok()
            .flatten()
            .and_then(|cols| cols.first().and_then(|v| v.as_string()))
            .unwrap_or_else(|| "auto".to_string())
    }

    pub fn update_dsd_mode(&self, id: i64, mode: &str) -> Result<(), String> {
        let sql = self.update_field_sql("dsd_mode");
        let params: [&dyn ToSqlValue; 2] = [&mode.to_string(), &id];
        visible_setting_write(id, "dsd_mode", self.db.execute(&sql, &params))
    }

    /// Whether this zone forces native FLAC to a DLNA renderer even when the
    /// renderer doesn't advertise FLAC (empty/failed GetProtocolInfo Sink).
    pub fn get_dlna_native_flac(&self, id: i64) -> bool {
        let placeholder = match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(1),
            Engine::Postgres => PostgresDialect.placeholder(1),
        };
        let sql =
            format!("SELECT COALESCE(dlna_native_flac, 0) FROM zones WHERE id = {placeholder}");
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db
            .query_one(&sql, &params)
            .ok()
            .flatten()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0)
            != 0
    }

    pub fn update_dlna_native_flac(&self, id: i64, enabled: bool) -> Result<(), String> {
        let sql = self.update_field_sql("dlna_native_flac");
        let params: [&dyn ToSqlValue; 2] = [&(enabled as i64), &id];
        visible_setting_write(id, "dlna_native_flac", self.db.execute(&sql, &params))
    }

    /// Whether this zone serves ALAC straight to the renderer (bit-perfect, no
    /// FLAC transcode). Opt-in — the renderer must decode ALAC natively.
    pub fn get_alac_passthrough(&self, id: i64) -> bool {
        let placeholder = match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(1),
            Engine::Postgres => PostgresDialect.placeholder(1),
        };
        let sql =
            format!("SELECT COALESCE(alac_passthrough, 0) FROM zones WHERE id = {placeholder}");
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db
            .query_one(&sql, &params)
            .ok()
            .flatten()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0)
            != 0
    }

    pub fn update_alac_passthrough(&self, id: i64, enabled: bool) -> Result<(), String> {
        let sql = self.update_field_sql("alac_passthrough");
        let params: [&dyn ToSqlValue; 2] = [&(enabled as i64), &id];
        visible_setting_write(id, "alac_passthrough", self.db.execute(&sql, &params))
    }

    /// Servir l'AAC tel quel au renderer, au lieu de le transcoder (#1424).
    /// Opt-in — le renderer doit le décoder nativement.
    pub fn get_aac_passthrough(&self, id: i64) -> bool {
        let placeholder = match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(1),
            Engine::Postgres => PostgresDialect.placeholder(1),
        };
        let sql =
            format!("SELECT COALESCE(aac_passthrough, 0) FROM zones WHERE id = {placeholder}");
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db
            .query_one(&sql, &params)
            .ok()
            .flatten()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0)
            != 0
    }

    pub fn update_aac_passthrough(&self, id: i64, enabled: bool) -> Result<(), String> {
        let sql = self.update_field_sql("aac_passthrough");
        let params: [&dyn ToSqlValue; 2] = [&(enabled as i64), &id];
        visible_setting_write(id, "aac_passthrough", self.db.execute(&sql, &params))
    }

    /// Whether to transcode lossless to WAV/LPCM (not FLAC) for this DLNA zone.
    pub fn get_dlna_lpcm(&self, id: i64) -> bool {
        let placeholder = match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(1),
            Engine::Postgres => PostgresDialect.placeholder(1),
        };
        let sql = format!("SELECT COALESCE(dlna_lpcm, 0) FROM zones WHERE id = {placeholder}");
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db
            .query_one(&sql, &params)
            .ok()
            .flatten()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0)
            != 0
    }

    pub fn update_dlna_lpcm(&self, id: i64, enabled: bool) -> Result<(), String> {
        let sql = self.update_field_sql("dlna_lpcm");
        let params: [&dyn ToSqlValue; 2] = [&(enabled as i64), &id];
        visible_setting_write(id, "dlna_lpcm", self.db.execute(&sql, &params))
    }

    /// Whether to cap this DLNA zone's output to 16-bit. For renderers that
    /// advertise `audio/flac` but only decode 16-bit (Ruark R3, #1137): forces a
    /// 16-bit downconvert instead of serving hi-res FLAC/ALAC direct (silence).
    /// Décalage à appliquer aux paroles synchronisées, en millisecondes.
    ///
    /// Positif = les paroles sont retardées. Sert à compenser la latence entre
    /// le moment où le serveur apprend le titre en cours et celui où
    /// l'auditeur l'entend : tampon de Tune, puis tampon du renderer. Sur une
    /// radio, la « position » des paroles est l'âge de la métadonnée, donc
    /// cette latence se voit directement — les paroles défilent en avance
    /// (forum #1328 : 1 à 2 lignes sur un Node BluOS, 2 à 4 sur un Marantz).
    ///
    /// Par zone, parce que la profondeur du tampon appartient à l'appareil et
    /// qu'aucune valeur unique ne peut convenir. Distinct de `sync_delay_ms`,
    /// qui décale l'AUDIO pour aligner deux pièces : mélanger les deux ferait
    /// bouger les paroles en réglant le multiroom, et l'inverse.
    pub fn get_lyrics_offset_ms(&self, id: i64) -> i32 {
        let placeholder = match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(1),
            Engine::Postgres => PostgresDialect.placeholder(1),
        };
        let sql =
            format!("SELECT COALESCE(lyrics_offset_ms, 0) FROM zones WHERE id = {placeholder}");
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db
            .query_one(&sql, &params)
            .ok()
            .flatten()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0) as i32
    }

    pub fn update_lyrics_offset_ms(&self, id: i64, offset_ms: i32) -> Result<(), String> {
        let sql = self.update_field_sql("lyrics_offset_ms");
        let params: [&dyn ToSqlValue; 2] = [&(offset_ms as i64), &id];
        visible_setting_write(id, "lyrics_offset_ms", self.db.execute(&sql, &params))
    }

    pub fn get_dlna_cap_16bit(&self, id: i64) -> bool {
        let placeholder = match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(1),
            Engine::Postgres => PostgresDialect.placeholder(1),
        };
        let sql = format!("SELECT COALESCE(dlna_cap_16bit, 0) FROM zones WHERE id = {placeholder}");
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db
            .query_one(&sql, &params)
            .ok()
            .flatten()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0)
            != 0
    }

    pub fn update_dlna_cap_16bit(&self, id: i64, enabled: bool) -> Result<(), String> {
        let sql = self.update_field_sql("dlna_cap_16bit");
        let params: [&dyn ToSqlValue; 2] = [&(enabled as i64), &id];
        visible_setting_write(id, "dlna_cap_16bit", self.db.execute(&sql, &params))
    }

    /// Whether to serve genuine 24-bit WAV to this DLNA zone. Opt-in, only
    /// meaningful for renderers that advertise `audio/L24` (the UI gates the
    /// toggle on the capability probe). Overrides the 16-bit LPCM fallback.
    pub fn get_dlna_wav24(&self, id: i64) -> bool {
        let placeholder = match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(1),
            Engine::Postgres => PostgresDialect.placeholder(1),
        };
        let sql = format!("SELECT COALESCE(dlna_wav24, 0) FROM zones WHERE id = {placeholder}");
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db
            .query_one(&sql, &params)
            .ok()
            .flatten()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0)
            != 0
    }

    pub fn update_dlna_wav24(&self, id: i64, enabled: bool) -> Result<(), String> {
        let sql = self.update_field_sql("dlna_wav24");
        let params: [&dyn ToSqlValue; 2] = [&(enabled as i64), &id];
        visible_setting_write(id, "dlna_wav24", self.db.execute(&sql, &params))
    }

    /// Per-zone SetAVTransportURI→Play delay in ms (0 = use the config default).
    pub fn get_dlna_play_delay_ms(&self, id: i64) -> u64 {
        let placeholder = match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(1),
            Engine::Postgres => PostgresDialect.placeholder(1),
        };
        let sql =
            format!("SELECT COALESCE(dlna_play_delay_ms, 0) FROM zones WHERE id = {placeholder}");
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db
            .query_one(&sql, &params)
            .ok()
            .flatten()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0)
            .max(0) as u64
    }

    pub fn update_dlna_play_delay_ms(&self, id: i64, delay_ms: u64) -> Result<(), String> {
        let sql = self.update_field_sql("dlna_play_delay_ms");
        let params: [&dyn ToSqlValue; 2] = [&(delay_ms as i64), &id];
        visible_setting_write(id, "dlna_play_delay_ms", self.db.execute(&sql, &params))
    }

    /// Persist the renderer's host (IP) on the zone, for host-based dedup.
    /// Best-effort on a pre-migration DB, but never silent: the omission is
    /// journalised and counted for the diagnostic report (#2154).
    pub fn set_host(&self, id: i64, host: &str) -> Result<(), String> {
        let sql = self.update_field_sql("host");
        let params: [&dyn ToSqlValue; 2] = [&host, &id];
        match self.db.execute(&sql, &params) {
            Ok(_) => Ok(()),
            Err(e) if missing_column(&e) => {
                let _ = setting_not_persisted(id, "host", &e);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Persist the renderer's physical identity (host, and MAC when known) on
    /// the zone. The MAC is the durable cross-protocol key: it survives UUID
    /// changes AND DHCP renumbering, where `host` alone goes stale. A `None`
    /// or empty MAC never erases a previously stored one. Best-effort on
    /// pre-migration DBs, like [`set_host`](Self::set_host).
    pub fn set_identity(&self, id: i64, host: &str, mac: Option<&str>) -> Result<(), String> {
        self.set_host(id, host)?;
        if let Some(mac) = mac.filter(|m| !m.is_empty()) {
            let sql = self.update_field_sql("mac");
            let params: [&dyn ToSqlValue; 2] = [&mac, &id];
            match self.db.execute(&sql, &params) {
                Ok(_) => {}
                Err(e) if missing_column(&e) => {
                    let _ = setting_not_persisted(id, "mac", &e);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// A visible zone already bound to this physical device — same persisted
    /// host (IP) or same MAC, case-insensitive. Returns `(id, name,
    /// output_type)`. The cross-protocol duplicate guard uses this so a
    /// Bluesound Node seen as BluOS + DLNA + OpenHome (three names, three
    /// UUIDs) still maps to the one zone it already has (forum #1239).
    pub fn find_visible_zone_by_identity(
        &self,
        host: &str,
        mac: Option<&str>,
    ) -> Option<(i64, String, String)> {
        let mac = mac.unwrap_or("");
        if host.is_empty() && mac.is_empty() {
            return None;
        }
        // SQLite placeholders are positional (`?`), so every occurrence needs
        // its own parameter — 4 slots, [host, host, mac, mac].
        let ph = |i: usize| match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(i),
            Engine::Postgres => PostgresDialect.placeholder(i),
        };
        let (p1, p2, p3, p4) = (ph(1), ph(2), ph(3), ph(4));
        let sql = format!(
            "SELECT id, name, COALESCE(output_type, '') FROM zones \
             WHERE COALESCE(is_hidden, 0) = 0 \
             AND ((host IS NOT NULL AND host <> '' AND {p1} <> '' AND LOWER(host) = LOWER({p2})) \
               OR (mac IS NOT NULL AND mac <> '' AND {p3} <> '' AND UPPER(mac) = UPPER({p4}))) \
             ORDER BY id LIMIT 1"
        );
        let params: [&dyn ToSqlValue; 4] = [&host, &host, &mac, &mac];
        // Strong read for the same reason as zone_id_by_host: a zone created
        // moments ago must be visible to the very next discovery event.
        match self.db.query_many_strong(&sql, &params) {
            Ok(rows) => rows.first().map(|cols| {
                (
                    cols.first().and_then(|v| v.as_i64()).unwrap_or_default(),
                    cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
                )
            }),
            // Pre-migration DB without the mac column: fall back to nothing
            // rather than failing discovery.
            Err(e) => {
                tracing::debug!(error = %e, "zone_identity_lookup_failed_ignoring");
                None
            }
        }
    }

    /// The persisted physical identity of every visible zone's device:
    /// `(output_device_id, host, mac)`. Feeds the heartbeat telemetry so the
    /// admin can identify renderer brands from the MAC's OUI. Graceful on
    /// pre-migration DBs (missing column → empty list).
    pub fn device_identities(&self) -> Vec<(String, String, String)> {
        let sql = "SELECT COALESCE(output_device_id, ''), COALESCE(host, ''), \
                   COALESCE(mac, '') FROM zones WHERE COALESCE(is_hidden, 0) = 0";
        match self.db.query_many(sql, &[]) {
            Ok(rows) => rows
                .iter()
                .filter_map(|cols| {
                    let did = cols.first().and_then(|v| v.as_string()).unwrap_or_default();
                    if did.is_empty() {
                        return None;
                    }
                    Some((
                        did,
                        cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                        cols.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
                    ))
                })
                .collect(),
            Err(e) => {
                tracing::debug!(error = %e, "zone_device_identities_failed_ignoring");
                Vec::new()
            }
        }
    }

    /// Re-point a zone to a new `output_device_id`. Used when a renderer comes
    /// back with a new UPnP UUID: host-based dedup keeps the existing zone (and
    /// its per-zone settings: native FLAC, volume…) instead of spawning a
    /// duplicate, so the device_id is refreshed to the live one.
    pub fn update_device_id(&self, id: i64, device_id: &str) -> Result<(), String> {
        let sql = self.update_field_sql("output_device_id");
        let params: [&dyn ToSqlValue; 2] = [&device_id, &id];
        self.db.execute(&sql, &params).map(|_| ())
    }

    /// Find an existing (non-hidden) DLNA/OpenHome zone by physical host (IP),
    /// for dedup across rediscovery. Returns the zone id, or None if none — or
    /// if the `host` column is missing on a pre-migration DB (graceful).
    pub fn zone_id_by_host(&self, host: &str) -> Option<i64> {
        let placeholder = match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(1),
            Engine::Postgres => PostgresDialect.placeholder(1),
        };
        let sql = format!(
            "SELECT id FROM zones WHERE host = {placeholder} \
             AND output_type IN ('dlna', 'openhome') \
             AND COALESCE(is_hidden, 0) = 0 ORDER BY id LIMIT 1"
        );
        let params: [&dyn ToSqlValue; 1] = [&host];
        // Strong read so a zone created moments ago in this session is visible
        // (avoids a duplicate slipping through a lagging WAL snapshot).
        self.db
            .query_many_strong(&sql, &params)
            .ok()?
            .first()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
    }

    /// Zones réseau (DLNA/OpenHome) MASQUÉES à cet hôte — les suppressions
    /// encore actives de l'utilisateur. Garde-fou #1281 : un appareil qui
    /// s'annonce sous plusieurs identités SSDP (DLNA + OpenHome, double UUID —
    /// buchardt A700) ne doit pas ressusciter, via son identité jumelle, la
    /// zone qui vient d'être supprimée. `is_device_hidden` ne voit que
    /// l'identité exacte ; ici on retrouve la suppression par l'hôte, et
    /// l'appelant exige en plus une correspondance de NOM (une IP seule
    /// n'identifie rien — leçon du ré-ancrage #1651).
    pub fn hidden_zones_by_host(&self, host: &str) -> Vec<(i64, String)> {
        let placeholder = match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(1),
            Engine::Postgres => PostgresDialect.placeholder(1),
        };
        let sql = format!(
            "SELECT id, name FROM zones WHERE host = {placeholder} \
             AND output_type IN ('dlna', 'openhome') \
             AND COALESCE(is_hidden, 0) = 1 ORDER BY id"
        );
        let params: [&dyn ToSqlValue; 1] = [&host];
        // Strong read: la suppression vient parfois d'arriver dans la même
        // session (même motif que zone_id_by_host).
        match self.db.query_many_strong(&sql, &params) {
            Ok(rows) => rows
                .iter()
                .filter_map(|cols| {
                    let id = cols.first().and_then(|v| v.as_i64())?;
                    let name = cols.get(1).and_then(|v| v.as_string())?;
                    Some((id, name))
                })
                .collect(),
            // Colonne `host` absente (base pré-migration) : pas de garde-fou.
            Err(_) => Vec::new(),
        }
    }

    pub fn set_online_by_device(&self, device_id: &str, online: bool) -> Result<usize, String> {
        let val: String = if online { "1".into() } else { "0".into() };
        let sql = self.dialect_sql(sql::set_online_by_device, sql::set_online_by_device);
        let params: [&dyn ToSqlValue; 2] = [&val, &device_id];
        self.db.execute(&sql, &params)
    }

    /// Rename a LOCAL zone stuck on the generic default label ("This
    /// Computer" / "Cet ordinateur") to its device name. Older versions named
    /// EVERY local zone with the generic label, so a machine with several
    /// DACs showed indistinguishable twins (forum #1233, Alain Bonnel). Only
    /// the exact generic labels are touched — a user-renamed zone never is.
    pub fn rename_generic_local_label(
        &self,
        zone_id: i64,
        device_name: &str,
    ) -> Result<usize, String> {
        let sql = self.dialect_sql(
            sql::rename_generic_local_label,
            sql::rename_generic_local_label,
        );
        let params: [&dyn ToSqlValue; 2] = [&device_name, &zone_id];
        self.db.execute(&sql, &params)
    }

    /// Hide stale duplicate LOCAL zones stuck on a generic default label
    /// ("This Computer" / "Cet ordinateur"), keeping `keep_id` — the zone bound
    /// to the live default device. The local device_id is derived from the
    /// device NAME (`local:<name>`), which is localizable and user-renamable, so
    /// renaming the Mac or a macOS locale change mints a new device_id and thus
    /// a SECOND default-device zone carrying the other-locale generic label.
    /// `get_or_create`/`deduplicate` key on device_id and never merge these
    /// twins, leaving both "This Computer" and "Cet ordinateur" in the picker
    /// (Philippe Vella). Only the exact generic labels are touched — a
    /// user-renamed zone is never hidden. Returns the number hidden.
    pub fn hide_duplicate_generic_local(&self, keep_id: i64) -> Result<usize, String> {
        let sql = self.dialect_sql(
            sql::hide_duplicate_generic_local,
            sql::hide_duplicate_generic_local,
        );
        let params: [&dyn ToSqlValue; 1] = [&keep_id];
        self.db.execute(&sql, &params)
    }

    /// Soft-delete EVERY zone and clear the free-tier activation markers.
    /// A Free user whose 3-zone quota is consumed by stale renderers can
    /// wipe the slate and explicitly re-create the zones he wants: discovery
    /// never resurrects a hidden zone, only POST /zones does.
    pub fn delete_all(&self) -> Result<usize, String> {
        self.db.execute(sql::delete_all(), &[])
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        let sql = self.dialect_sql(sql::delete_by_id, sql::delete_by_id);
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn unhide(&self, id: i64) -> Result<(), String> {
        let sql = self.dialect_sql(
            |d| {
                format!(
                    "UPDATE zones SET is_hidden = 0 WHERE id = {}",
                    d.placeholder(1)
                )
            },
            |d| {
                format!(
                    "UPDATE zones SET is_hidden = 0 WHERE id = {}",
                    d.placeholder(1)
                )
            },
        );
        self.db.execute(&sql, &[&id as &dyn ToSqlValue])?;
        Ok(())
    }

    pub fn update_group(&self, id: i64, group_id: Option<&str>) -> Result<(), String> {
        let sql = self.update_field_sql("group_id");
        let params: [&dyn ToSqlValue; 2] = [&group_id, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_sync_delay(&self, id: i64, ms: i32) -> Result<(), String> {
        let sql = self.update_field_sql("sync_delay_ms");
        let params: [&dyn ToSqlValue; 2] = [&ms, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_max_sample_rate(&self, id: i64, rate: Option<u32>) -> Result<(), String> {
        let sql = self.update_field_sql("max_sample_rate");
        let rate_i64 = rate.map(|r| r as i64);
        let params: [&dyn ToSqlValue; 2] = [&rate_i64, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn save_playback_position(
        &self,
        id: i64,
        position_ms: i64,
        track_id: Option<i64>,
        source: Option<&str>,
        source_id: Option<&str>,
    ) -> Result<(), String> {
        let sql = self.dialect_sql(sql::save_playback_position, sql::save_playback_position);
        let params: [&dyn ToSqlValue; 5] = [&position_ms, &track_id, &source, &source_id, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn clear_playback_position(&self, id: i64) -> Result<(), String> {
        let sql = self.dialect_sql(sql::clear_playback_position, sql::clear_playback_position);
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_dsp(&self, id: i64, preset_id: Option<i64>, enabled: bool) -> Result<(), String> {
        let preset_str: Option<String> = preset_id.map(|v| v.to_string());
        let en: String = if enabled { "1".into() } else { "0".into() };
        let sql = self.dialect_sql(sql::update_dsp, sql::update_dsp);
        let params: [&dyn ToSqlValue; 3] = [&preset_str, &en, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn get_dsp_config(&self, id: i64) -> Result<(Option<i64>, bool), String> {
        let sql = self.dialect_sql(sql::get_dsp_config, sql::get_dsp_config);
        let params: [&dyn ToSqlValue; 1] = [&id];
        let row = self
            .db
            .query_one(&sql, &params)?
            .ok_or_else(|| format!("zone {id} not found"))?;
        let preset = row.first().and_then(|v| v.as_i64());
        let enabled = row.get(1).and_then(|v| v.as_i64()).unwrap_or(0) != 0;
        Ok((preset, enabled))
    }

    pub fn count(&self) -> Result<i64, String> {
        match self.db.query_one(sql::count(), &[])? {
            None => Ok(0),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0)),
        }
    }

    pub fn count_online(&self) -> Result<i64, String> {
        match self.db.query_one(sql::count_online(), &[])? {
            None => Ok(0),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0)),
        }
    }

    /// Count of online zones that have been played at least once (see
    /// `sql::count_active`). Used for the free-tier zone cap so dormant
    /// auto-discovered zones don't count.
    pub fn count_active(&self) -> Result<i64, String> {
        match self.db.query_one(sql::count_active(), &[])? {
            None => Ok(0),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0)),
        }
    }

    /// Persist the play state ("playing", "paused", "stopped") for a zone.
    /// Silently ignores missing column (pre-v39 database).
    pub fn save_play_state(&self, id: i64, state: &str) -> Result<(), String> {
        let sql = self.update_field_sql("last_play_state");
        let params: [&dyn ToSqlValue; 2] = [&state, &id];
        match self.db.execute(&sql, &params) {
            Ok(_) => Ok(()),
            Err(e) if e.contains("no such column") || e.contains("does not exist") => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Read the last persisted play state for a zone.
    /// Returns None if the column doesn't exist (pre-v39) or the zone is not found.
    pub fn get_last_play_state(&self, id: i64) -> Option<String> {
        let placeholder = match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(1),
            Engine::Postgres => PostgresDialect.placeholder(1),
        };
        let sql = format!("SELECT last_play_state FROM zones WHERE id = {placeholder}");
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db
            .query_one(&sql, &params)
            .ok()
            .flatten()
            .and_then(|cols| cols.first().and_then(|v| v.as_string()))
    }
}

fn row_to_zone(cols: &Vec<SqlValue>) -> Zone {
    Zone {
        id: cols.first().and_then(|v| v.as_i64()),
        name: cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
        output_type: cols.get(2).and_then(|v| v.as_string()),
        output_device_id: cols.get(3).and_then(|v| v.as_string()),
        volume: cols.get(4).and_then(|v| v.as_i64()).unwrap_or(20) as i32,
        muted: cols.get(5).and_then(|v| v.as_i64()).unwrap_or(0) != 0,
        online: cols.get(6).and_then(|v| v.as_i64()).unwrap_or(1) != 0,
        gapless_enabled: cols.get(7).and_then(|v| v.as_i64()).unwrap_or(1) != 0,
        group_id: cols.get(8).and_then(|v| v.as_string()),
        sync_delay_ms: cols.get(9).and_then(|v| v.as_i64()).unwrap_or(0) as i32,
        last_position_ms: cols.get(10).and_then(|v| v.as_i64()).unwrap_or(0),
        last_track_id: cols.get(11).and_then(|v| v.as_i64()),
        last_track_source: cols.get(12).and_then(|v| v.as_string()),
        last_track_source_id: cols.get(13).and_then(|v| v.as_string()),
        max_sample_rate: cols.get(14).and_then(|v| v.as_i64()).map(|v| v as u32),
        fixed_volume: cols.get(15).and_then(|v| v.as_i64()).unwrap_or(0) != 0,
        autoplay_enabled: cols.get(16).and_then(|v| v.as_i64()).unwrap_or(0) != 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        db
    }

    /// Base complete : `settings` n'est pas dans `init_schema`, elle vient des
    /// migrations. Les tests qui touchent aux reglages hors colonnes en ont
    /// besoin ; les autres restent sur la base minimale.
    fn test_db_migree() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        db
    }

    #[test]
    fn crud_zone() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        let id = repo
            .create("Living Room", Some("dlna"), Some("uuid:123"))
            .unwrap();
        let zone = repo.get(id).unwrap().unwrap();
        assert_eq!(zone.name, "Living Room");
        assert_eq!(zone.volume, 50);
        assert!(!zone.muted);

        repo.update_volume(id, 75).unwrap();
        repo.update_muted(id, true).unwrap();
        let updated = repo.get(id).unwrap().unwrap();
        assert_eq!(updated.volume, 75);
        assert!(updated.muted);

        // delete is a soft-delete (is_hidden=1): the row is kept for later
        // unhide/dedup, but the zone no longer appears in the visible list.
        repo.delete(id).unwrap();
        assert!(repo.list().unwrap().is_empty());
        assert!(repo.is_device_hidden("uuid:123"));
    }

    /// #1832 — les reglages ranges dans `settings` (profil d'egaliseur en
    /// tete) partaient avec le doublon supprime.
    #[test]
    fn les_reglages_hors_colonnes_suivent_la_survivante() {
        let db = test_db_migree();
        let settings = crate::db::settings_repo::SettingsRepo::new(db.clone());
        let repo = ZoneRepo::new(db);

        let survivante = repo.create("Salon", Some("dlna"), Some("uuid:1")).unwrap();
        let doublon = repo.create("Salon", Some("dlna"), Some("uuid:1")).unwrap();

        settings
            .set(&format!("zone_{doublon}_eq_profile"), "loudness")
            .unwrap();
        settings
            .set(&format!("zone_{doublon}_brand"), "Devialet")
            .unwrap();

        repo.deduplicate().unwrap();

        assert_eq!(
            settings
                .get(&format!("zone_{survivante}_eq_profile"))
                .unwrap()
                .as_deref(),
            Some("loudness"),
            "le profil d'egaliseur ne doit pas partir avec le doublon"
        );
        assert_eq!(
            settings
                .get(&format!("zone_{survivante}_brand"))
                .unwrap()
                .as_deref(),
            Some("Devialet")
        );
    }

    /// La regle des colonnes vaut aussi ici : un reglage explicite ne cede
    /// jamais a celui d'un doublon.
    #[test]
    fn un_reglage_deja_pose_sur_la_survivante_resiste() {
        let db = test_db_migree();
        let settings = crate::db::settings_repo::SettingsRepo::new(db.clone());
        let repo = ZoneRepo::new(db);

        let survivante = repo.create("Salon", Some("dlna"), Some("uuid:1")).unwrap();
        let doublon = repo.create("Salon", Some("dlna"), Some("uuid:1")).unwrap();

        settings
            .set(&format!("zone_{survivante}_eq_profile"), "plat")
            .unwrap();
        settings
            .set(&format!("zone_{doublon}_eq_profile"), "loudness")
            .unwrap();

        repo.reporter_reglages_de_doublons().unwrap();

        assert_eq!(
            settings
                .get(&format!("zone_{survivante}_eq_profile"))
                .unwrap()
                .as_deref(),
            Some("plat"),
            "ce que l'utilisateur a pose reste"
        );
    }

    /// Une valeur vide sur la survivante compte pour « pas encore regle » —
    /// la chaine vide est le marqueur d'effacement des surcharges de zone.
    #[test]
    fn une_valeur_vide_ne_bloque_pas_le_report() {
        let db = test_db_migree();
        let settings = crate::db::settings_repo::SettingsRepo::new(db.clone());
        let repo = ZoneRepo::new(db);

        let survivante = repo.create("Salon", Some("dlna"), Some("uuid:1")).unwrap();
        let doublon = repo.create("Salon", Some("dlna"), Some("uuid:1")).unwrap();

        settings
            .set(&format!("zone_{survivante}_model"), "")
            .unwrap();
        settings
            .set(&format!("zone_{doublon}_model"), "Expert 140 Pro")
            .unwrap();

        repo.reporter_reglages_de_doublons().unwrap();

        assert_eq!(
            settings
                .get(&format!("zone_{survivante}_model"))
                .unwrap()
                .as_deref(),
            Some("Expert 140 Pro")
        );
    }

    /// Une zone sans doublon ne doit rien recevoir de personne.
    #[test]
    fn une_zone_seule_ne_recoit_rien() {
        let db = test_db_migree();
        let settings = crate::db::settings_repo::SettingsRepo::new(db.clone());
        let repo = ZoneRepo::new(db);

        let seule = repo.create("Salon", Some("dlna"), Some("uuid:1")).unwrap();
        let autre = repo
            .create("Cuisine", Some("dlna"), Some("uuid:2"))
            .unwrap();
        settings
            .set(&format!("zone_{autre}_eq_profile"), "loudness")
            .unwrap();

        repo.reporter_reglages_de_doublons().unwrap();

        assert!(
            settings
                .get(&format!("zone_{seule}_eq_profile"))
                .unwrap()
                .is_none(),
            "deux appareils distincts ne se transmettent rien"
        );
    }

    /// #1823 — le panneau lateral creait la zone avec le NOM du peripherique.
    /// Sans le prefixe, l'orchestrateur la prend pour un renderer reseau.
    #[test]
    fn une_zone_locale_sans_prefixe_le_recupere() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        let id = repo
            .create("SPDIF/ADAT (1+2)", Some("local"), Some("SPDIF/ADAT (1+2)"))
            .unwrap();
        repo.reparer_prefixe_local().unwrap();

        let zone = repo.get(id).unwrap().unwrap();
        assert_eq!(
            zone.output_device_id.as_deref(),
            Some("local:SPDIF/ADAT (1+2)")
        );
    }

    /// Le cas vecu par DEvir : la zone auto-decouverte avait ete supprimee
    /// (masquee), et bloquait la place que la reparation doit rendre.
    #[test]
    fn le_jumeau_masque_cede_la_place() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        let auto = repo
            .create("Sortie", Some("local"), Some("local:Sortie"))
            .unwrap();
        repo.delete(auto).unwrap(); // suppression = masquage
        let manuelle = repo
            .create("Sortie", Some("local"), Some("Sortie"))
            .unwrap();

        repo.reparer_prefixe_local().unwrap();

        assert!(repo.get(auto).unwrap().is_none(), "le jumeau masque part");
        assert_eq!(
            repo.get(manuelle)
                .unwrap()
                .unwrap()
                .output_device_id
                .as_deref(),
            Some("local:Sortie"),
            "la zone que l'utilisateur voit garde ses reglages et devient jouable"
        );
    }

    /// Deux zones VISIBLES pour le meme appareil : on ne tranche pas a la
    /// place de l'utilisateur, et surtout on ne casse pas l'index unique.
    #[test]
    fn deux_zones_visibles_sont_laissees_intactes() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        let prefixee = repo
            .create("Sortie", Some("local"), Some("local:Sortie"))
            .unwrap();
        let nue = repo
            .create("Sortie", Some("local"), Some("Sortie"))
            .unwrap();

        repo.reparer_prefixe_local().unwrap();

        assert!(repo.get(prefixee).unwrap().is_some());
        assert_eq!(
            repo.get(nue).unwrap().unwrap().output_device_id.as_deref(),
            Some("Sortie"),
            "rien n'est ecrase tant que la question n'est pas tranchee"
        );
    }

    /// La reparation ne doit toucher QUE les sorties locales.
    #[test]
    fn une_zone_reseau_nest_pas_prefixee() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        let id = repo
            .create("Salon", Some("dlna"), Some("uuid:4aac5a61"))
            .unwrap();
        repo.reparer_prefixe_local().unwrap();

        assert_eq!(
            repo.get(id).unwrap().unwrap().output_device_id.as_deref(),
            Some("uuid:4aac5a61")
        );
    }

    /// Idempotence : une base deja saine ne bouge pas, et un second passage
    /// ne double pas le prefixe.
    #[test]
    fn la_reparation_est_idempotente() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        let id = repo
            .create("Sortie", Some("local"), Some("Sortie"))
            .unwrap();
        repo.reparer_prefixe_local().unwrap();
        repo.reparer_prefixe_local().unwrap();

        assert_eq!(
            repo.get(id).unwrap().unwrap().output_device_id.as_deref(),
            Some("local:Sortie"),
            "jamais local:local:"
        );
    }

    #[test]
    fn list_zones() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        repo.create("Zone A", None, None).unwrap();
        repo.create("Zone B", None, None).unwrap();
        let zones = repo.list().unwrap();
        assert_eq!(zones.len(), 2);
    }

    #[test]
    fn zone_count() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        assert_eq!(repo.count().unwrap(), 0);
        repo.create("Zone A", None, None).unwrap();
        repo.create("Zone B", None, None).unwrap();
        assert_eq!(repo.count().unwrap(), 2);
    }

    #[test]
    fn count_active_excludes_dormant_zones() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        // Two auto-discovered zones: online but never played (dormant).
        let a = repo
            .create("Dormant A", Some("dlna"), Some("uuid:a"))
            .unwrap();
        let b = repo
            .create("Dormant B", Some("dlna"), Some("uuid:b"))
            .unwrap();
        repo.update_online(a, true).unwrap();
        repo.update_online(b, true).unwrap();

        // Both online, none played → 0 active (dormant zones don't count).
        assert_eq!(repo.count_online().unwrap(), 2);
        assert_eq!(repo.count_active().unwrap(), 0);

        // Playing a track on A activates it (last_track_id set).
        repo.save_playback_position(a, 0, Some(42), Some("local"), None)
            .unwrap();
        assert_eq!(repo.count_active().unwrap(), 1);

        repo.save_playback_position(b, 0, Some(7), Some("local"), None)
            .unwrap();
        assert_eq!(repo.count_active().unwrap(), 2);

        // An offline (but previously played) zone no longer counts.
        repo.update_online(b, false).unwrap();
        assert_eq!(repo.count_active().unwrap(), 1);
    }

    #[test]
    fn identity_lookup_matches_host_or_mac() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        let id = repo
            .create("Node Salon", Some("bluos"), Some("bluos:node1"))
            .unwrap();
        repo.set_identity(id, "192.168.1.30", Some("90:56:82:AA:BB:CC"))
            .unwrap();

        // Host match, case-insensitive; MAC match, case-insensitive.
        let by_host = repo.find_visible_zone_by_identity("192.168.1.30", None);
        assert_eq!(by_host.as_ref().map(|z| z.0), Some(id));
        assert_eq!(by_host.unwrap().2, "bluos");
        let by_mac = repo.find_visible_zone_by_identity("10.0.0.99", Some("90:56:82:aa:bb:cc"));
        assert_eq!(by_mac.map(|z| z.0), Some(id));

        // No false positives on empty keys or unknown identity.
        assert!(repo.find_visible_zone_by_identity("", None).is_none());
        assert!(
            repo.find_visible_zone_by_identity("10.0.0.99", Some(""))
                .is_none()
        );
        assert!(
            repo.find_visible_zone_by_identity("10.0.0.98", Some("00:11:22:33:44:55"))
                .is_none()
        );

        // A hidden (deleted) zone never matches — its device may come back,
        // but the user's deletion stands.
        repo.delete(id).unwrap();
        assert!(
            repo.find_visible_zone_by_identity("192.168.1.30", Some("90:56:82:AA:BB:CC"))
                .is_none()
        );
    }

    #[test]
    fn set_identity_never_erases_known_mac() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        let id = repo
            .create("Ampli", Some("dlna"), Some("uuid:amp"))
            .unwrap();
        repo.set_identity(id, "192.168.1.40", Some("00:A0:DE:11:22:33"))
            .unwrap();
        // A later pass without a MAC (ARP miss) must keep the stored one.
        repo.set_identity(id, "192.168.1.41", None).unwrap();
        let hit = repo.find_visible_zone_by_identity("10.9.9.9", Some("00:a0:de:11:22:33"));
        assert_eq!(hit.map(|z| z.0), Some(id));
        // And the host was refreshed.
        assert_eq!(
            repo.find_visible_zone_by_identity("192.168.1.41", None)
                .map(|z| z.0),
            Some(id)
        );
    }

    #[test]
    fn delete_all_frees_quota_durably() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        let a = repo.create("Salon", Some("dlna"), Some("uuid:a")).unwrap();
        repo.update_online(a, true).unwrap();
        repo.save_playback_position(a, 0, Some(42), Some("local"), None)
            .unwrap();
        assert_eq!(repo.count_active().unwrap(), 1);

        repo.delete_all().unwrap();
        assert!(repo.list().unwrap().is_empty());
        assert_eq!(repo.count_active().unwrap(), 0);

        // Resurrect the same device (explicit POST /zones path): the
        // activation marker must be gone, so the zone is dormant again and
        // does not silently re-consume a free-tier slot.
        repo.unhide(a).unwrap();
        let zone = repo.get(a).unwrap().unwrap();
        assert_eq!(zone.last_track_id, None);
        assert_eq!(repo.count_active().unwrap(), 0);
    }

    #[test]
    fn zone_update_name() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        let id = repo.create("Old Name", None, None).unwrap();
        repo.update_name(id, "New Name").unwrap();
        let zone = repo.get(id).unwrap().unwrap();
        assert_eq!(zone.name, "New Name");
    }

    #[test]
    fn zone_update_output_device() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        let id = repo.create("Zone", Some("dlna"), Some("uuid:old")).unwrap();
        repo.update_output_device(id, "uuid:new-device").unwrap();
        let zone = repo.get(id).unwrap().unwrap();
        assert_eq!(zone.output_device_id.as_deref(), Some("uuid:new-device"));
    }

    #[test]
    fn zone_update_output_type() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        let id = repo.create("Zone", Some("local"), None).unwrap();
        repo.update_output_type(id, "dlna").unwrap();
        let zone = repo.get(id).unwrap().unwrap();
        assert_eq!(zone.output_type.as_deref(), Some("dlna"));
    }

    #[test]
    fn zone_default_values() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        let id = repo.create("Default Zone", None, None).unwrap();
        let zone = repo.get(id).unwrap().unwrap();
        assert_eq!(zone.volume, 50);
        assert!(!zone.muted);
        assert!(zone.online);
        assert!(zone.output_type.is_none());
        assert!(zone.output_device_id.is_none());
    }

    #[test]
    fn zone_mute_unmute() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        let id = repo.create("Zone", None, None).unwrap();
        assert!(!repo.get(id).unwrap().unwrap().muted);

        repo.update_muted(id, true).unwrap();
        assert!(repo.get(id).unwrap().unwrap().muted);

        repo.update_muted(id, false).unwrap();
        assert!(!repo.get(id).unwrap().unwrap().muted);
    }

    #[test]
    fn zone_volume_range() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        let id = repo.create("Zone", None, None).unwrap();

        repo.update_volume(id, 0).unwrap();
        assert_eq!(repo.get(id).unwrap().unwrap().volume, 0);

        repo.update_volume(id, 100).unwrap();
        assert_eq!(repo.get(id).unwrap().unwrap().volume, 100);
    }

    #[test]
    fn zone_get_nonexistent() {
        let db = test_db();
        let repo = ZoneRepo::new(db);
        assert!(repo.get(999).unwrap().is_none());
    }

    #[test]
    fn zone_list_sorted() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        repo.create("Salon", None, None).unwrap();
        repo.create("Bureau", None, None).unwrap();
        repo.create("Chambre", None, None).unwrap();

        let zones = repo.list().unwrap();
        assert_eq!(zones[0].name, "Bureau");
        assert_eq!(zones[1].name, "Chambre");
        assert_eq!(zones[2].name, "Salon");
    }

    #[test]
    fn sql_builders_dialect_placeholders() {
        let s = SqliteDialect;
        let p = PostgresDialect;
        assert!(sql::create(&s).contains("VALUES (?, ?, ?)"));
        assert!(sql::create(&p).contains("VALUES ($1, $2, $3)"));
        assert!(sql::update_field(&s, "volume").ends_with("SET volume = ? WHERE id = ?"));
        assert!(sql::update_field(&p, "volume").ends_with("SET volume = $1 WHERE id = $2"));
    }

    #[test]
    fn create_zone_during_open_transaction() {
        // Regression test for forum P0 #2 (Dimitri) and #6 (Dominique):
        // a zone created during an open scan tx flashes green then
        // disappears because list() used the read-only snapshot that
        // pre-dated the commit.
        //
        // With the port to query_many_strong as the fallback path,
        // list() now sees the writer's own pending writes — same
        // observable behavior as the original 8af95ec fix.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = SqliteDb::open(path.to_str().unwrap()).unwrap();
        db.init_schema().unwrap();

        // Simulate the scan starting a transaction on the write conn.
        db.execute_batch("BEGIN IMMEDIATE").unwrap();

        let repo = ZoneRepo::new(db.clone());
        let id = repo
            .create("Living Room", Some("dlna"), Some("uuid:123"))
            .unwrap();
        assert!(id > 0);

        let zones_before_commit = repo.list().unwrap();
        assert_eq!(zones_before_commit.len(), 1);
        assert_eq!(zones_before_commit[0].name, "Living Room");

        db.execute_batch("COMMIT").unwrap();

        let zones_after_commit = repo.list().unwrap();
        assert_eq!(zones_after_commit.len(), 1);
        assert_eq!(zones_after_commit[0].name, "Living Room");
    }

    #[test]
    fn get_or_create_idempotent() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        let (id1, created1) = repo
            .get_or_create("Living Room", Some("dlna"), "uuid:123")
            .unwrap();
        assert!(created1);

        let (id2, created2) = repo
            .get_or_create("Living Room", Some("dlna"), "uuid:123")
            .unwrap();
        assert!(!created2);
        assert_eq!(id1, id2);

        // Only 1 zone should exist
        assert_eq!(repo.count().unwrap(), 1);
    }

    #[test]
    fn get_by_device_id() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        repo.create("Zone A", Some("dlna"), Some("uuid:aaa"))
            .unwrap();
        repo.create("Zone B", Some("dlna"), Some("uuid:bbb"))
            .unwrap();

        let found = repo.get_by_device_id("uuid:aaa").unwrap().unwrap();
        assert_eq!(found.name, "Zone A");

        let found_b = repo.get_by_device_id("uuid:bbb").unwrap().unwrap();
        assert_eq!(found_b.name, "Zone B");

        assert!(repo.get_by_device_id("uuid:nonexistent").unwrap().is_none());
    }

    #[test]
    fn zone_dedup_by_host_reconnects_on_new_uuid() {
        // #942: a Denon-like renderer comes back with a NEW UPnP UUID but the
        // same host — it must reconnect to its existing zone, not duplicate.
        let db = test_db();
        let repo = ZoneRepo::new(db);

        let zid = repo
            .create("Denon", Some("dlna"), Some("uuid:old"))
            .unwrap();
        repo.set_host(zid, "192.168.1.28").unwrap();

        // Found by host; a different host is not.
        assert_eq!(repo.zone_id_by_host("192.168.1.28"), Some(zid));
        assert_eq!(repo.zone_id_by_host("192.168.1.99"), None);

        // Re-point the existing zone to the live UUID instead of creating a dup.
        repo.update_device_id(zid, "uuid:new").unwrap();
        assert_eq!(
            repo.get_by_device_id("uuid:new").unwrap().unwrap().id,
            Some(zid)
        );
        assert!(repo.get_by_device_id("uuid:old").unwrap().is_none());
        assert_eq!(repo.list().unwrap().len(), 1, "must stay a single zone");
    }

    #[test]
    fn zone_id_by_host_only_matches_network_renderers() {
        let db = test_db();
        let repo = ZoneRepo::new(db);
        let local = repo
            .create("Speakers", Some("local"), Some("local:x"))
            .unwrap();
        repo.set_host(local, "192.168.1.5").unwrap();
        // A local (non-DLNA) zone must never be reconnected via host dedup.
        assert_eq!(repo.zone_id_by_host("192.168.1.5"), None);
    }

    #[test]
    fn deduplicate_removes_extra_zones() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        // Simulate the bug: 3 zones with the same device_id
        repo.create("Zone A", Some("dlna"), Some("uuid:123"))
            .unwrap();
        repo.create("Zone A", Some("dlna"), Some("uuid:123"))
            .unwrap();
        repo.create("Zone A", Some("dlna"), Some("uuid:123"))
            .unwrap();
        // Plus a unique zone
        repo.create("Zone B", Some("dlna"), Some("uuid:456"))
            .unwrap();
        // Plus a zone with no device (manual zone)
        repo.create("Zone C", None, None).unwrap();

        assert_eq!(repo.count().unwrap(), 5);

        let removed = repo.deduplicate().unwrap();
        assert_eq!(removed, 2); // 2 duplicate uuid:123 entries removed

        assert_eq!(repo.count().unwrap(), 3); // 1 uuid:123 + 1 uuid:456 + 1 no-device

        // The remaining uuid:123 zone should be the one with lowest id
        let zones = repo.list().unwrap();
        let z123: Vec<_> = zones
            .iter()
            .filter(|z| z.output_device_id.as_deref() == Some("uuid:123"))
            .collect();
        assert_eq!(z123.len(), 1);
    }

    #[test]
    fn with_backend_constructor() {
        let db = test_db();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = ZoneRepo::with_backend(backend);
        let id = repo.create("X", None, None).unwrap();
        assert_eq!(repo.get(id).unwrap().unwrap().name, "X");
    }

    #[test]
    fn get_or_create_keeps_deleted_zone_hidden() {
        let db = test_db();
        // Add the UNIQUE index that startup.rs normally creates
        db.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_zones_output_device_id ON zones(output_device_id) WHERE output_device_id IS NOT NULL;"
        ).unwrap();
        let repo = ZoneRepo::new(db);

        // Create a zone, customize volume, then soft-delete it
        let (id1, created) = repo
            .get_or_create("Jean-Marie DAC", Some("dlna"), "uuid:jm-dac")
            .unwrap();
        assert!(created);
        repo.update_volume(id1, 75).unwrap();

        // Soft-delete the zone
        repo.delete(id1).unwrap();
        // Zone should be hidden from list
        assert!(repo.list().unwrap().is_empty());
        // But should still be findable by device_id
        assert!(repo.get_by_device_id("uuid:jm-dac").unwrap().is_some());
        assert!(repo.is_device_hidden("uuid:jm-dac"));

        // Re-discover the same device — get_or_create returns the existing
        // (hidden) zone WITHOUT unhiding it. A soft-deleted zone must not
        // reappear on device re-enumeration (which happens on every restart);
        // otherwise deleted zones proliferate. It also must not error.
        let (id2, created2) = repo
            .get_or_create("Jean-Marie DAC", Some("dlna"), "uuid:jm-dac")
            .unwrap();
        assert!(
            !created2,
            "should reuse the existing hidden zone, not create a new one"
        );
        assert_eq!(id1, id2, "should return the same zone id");

        // Zone stays hidden — it does not reappear in the list.
        assert!(repo.list().unwrap().is_empty());
        assert!(repo.is_device_hidden("uuid:jm-dac"));
    }

    #[test]
    fn a_deleted_zone_is_findable_by_name_so_it_does_not_come_back() {
        // Le scenario de #1528 : l'utilisateur supprime une zone, puis
        // l'adresse IP de l'appareil change. La ligne masquee porte l'ancien
        // identifiant — c'est par le NOM qu'il faut la retrouver, sinon la
        // decouverte la recree a neuf.
        let repo = ZoneRepo::new(test_db());
        let id = repo
            .create("Salon", Some("bluos"), Some("bluos-192.168.1.23-11000"))
            .unwrap();
        repo.delete(id).unwrap();

        // Elle a bien disparu des listes visibles…
        assert!(repo.list().unwrap().iter().all(|z| z.name != "Salon"));
        // …mais elle reste retrouvable par son nom, c'est tout l'objet.
        assert_eq!(repo.find_hidden_id_by_name("Salon"), Some(id));
    }

    #[test]
    fn a_live_zone_is_not_reported_as_deleted() {
        let repo = ZoneRepo::new(test_db());
        repo.create("Cuisine", Some("dlna"), Some("dlna-192.168.1.9-8080"))
            .unwrap();
        assert_eq!(repo.find_hidden_id_by_name("Cuisine"), None);
        assert_eq!(repo.find_hidden_id_by_name("Inconnue"), None);
    }

    #[test]
    fn reanchoring_a_deleted_zone_keeps_it_deleted() {
        // Le point delicat du correctif : on re-ancre la zone masquee sur le
        // nouvel identifiant pour que `is_device_hidden` redevienne operant,
        // mais elle ne doit surtout pas reapparaitre au passage.
        let repo = ZoneRepo::new(test_db());
        let id = repo
            .create("Salon", Some("bluos"), Some("bluos-192.168.1.23-11000"))
            .unwrap();
        repo.delete(id).unwrap();

        repo.update_output_device(id, "bluos-192.168.1.77-11000")
            .unwrap();

        assert!(repo.list().unwrap().iter().all(|z| z.name != "Salon"));
        assert!(repo.is_device_hidden("bluos-192.168.1.77-11000"));
    }
}

#[cfg(test)]
mod fusion_doublons_tests {
    use super::*;

    fn repo() -> ZoneRepo {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        // `CORE_SCHEMA` ne porte PAS les colonnes de reglage avance : elles
        // arrivent par migration. Sans cet appel, `update_dlna_lpcm` echoue sur
        // « no such column », avale l'erreur, renvoie Ok(()) — et le test
        // mesure un schema incomplet en croyant mesurer la fusion.
        crate::db::migrations::run_migrations(&db).unwrap();
        ZoneRepo::new(db)
    }

    fn delai_synchro(repo: &ZoneRepo, id: i64) -> i32 {
        repo.get(id).unwrap().unwrap().sync_delay_ms
    }

    /// #1774 — Yves : « les parametres coches n'ont pas ete sauvegardes ».
    ///
    /// La deduplication garde `MIN(id)`, la zone la PLUS ANCIENNE, sans jamais
    /// regarder laquelle porte une configuration. Quand l'utilisateur a regle
    /// la zone que l'interface lui montrait — pas forcement la plus ancienne —
    /// le demarrage suivant la supprimait avec ses reglages.
    ///
    /// Ce test ECHOUE contre le code d'avant : les drapeaux revenaient a 0.
    #[test]
    fn les_reglages_du_doublon_survivent_a_la_deduplication() {
        let repo = repo();
        let ancienne = repo
            .create("DarTZeel", Some("dlna"), Some("uuid:lhc208"))
            .unwrap();
        let reglee = repo
            .create("DarTZeel", Some("dlna"), Some("uuid:lhc208"))
            .unwrap();

        // L'utilisateur configure la zone que l'interface lui montre.
        repo.update_dlna_lpcm(reglee, true).unwrap();
        repo.update_dlna_cap_16bit(reglee, true).unwrap();
        repo.update_sync_delay(reglee, 120).unwrap();

        assert_eq!(repo.deduplicate().unwrap(), 1);

        // La survivante est bien la plus ancienne...
        let restantes = repo.list().unwrap();
        assert_eq!(restantes.len(), 1);
        assert_eq!(restantes[0].id, Some(ancienne));

        // ...mais elle porte desormais les reglages de celle qui a disparu.
        assert!(
            repo.get_dlna_lpcm(ancienne),
            "le reglage LPCM a ete efface avec le doublon"
        );
        assert!(
            repo.get_dlna_cap_16bit(ancienne),
            "le plafond 16 bits a ete efface avec le doublon"
        );
        assert_eq!(
            delai_synchro(&repo, ancienne),
            120,
            "le delai de synchro a ete efface avec le doublon"
        );
    }

    /// Un reglage explicite de la survivante ne cede jamais a celui d'un
    /// doublon : la fusion comble un vide, elle n'arbitre pas.
    #[test]
    fn un_reglage_deja_pose_sur_la_survivante_n_est_pas_ecrase() {
        let repo = repo();
        let survivante = repo
            .create("Ampli", Some("dlna"), Some("uuid:ampli"))
            .unwrap();
        let doublon = repo
            .create("Ampli", Some("dlna"), Some("uuid:ampli"))
            .unwrap();

        repo.update_sync_delay(survivante, 40).unwrap();
        repo.update_sync_delay(doublon, 250).unwrap();

        repo.deduplicate().unwrap();

        assert_eq!(
            delai_synchro(&repo, survivante),
            40,
            "la valeur choisie sur la zone conservee doit primer"
        );
    }

    /// Une zone sans doublon n'est jamais touchee par la fusion.
    #[test]
    fn une_zone_seule_n_est_pas_modifiee() {
        let repo = repo();
        let seule = repo
            .create("Salon", Some("dlna"), Some("uuid:salon"))
            .unwrap();
        repo.update_dlna_lpcm(seule, true).unwrap();

        repo.deduplicate().unwrap();

        assert!(repo.get_dlna_lpcm(seule));
        assert_eq!(repo.count().unwrap(), 1);
    }
}

/// #2271 — le mode de continuation remplace le booleen, SANS migration.
///
/// La colonne `zones.autoplay_enabled` existe deja : `INTEGER DEFAULT 0` en
/// SQLite, `TEXT DEFAULT '0'` en PostgreSQL. L'affinite SQLite range une
/// chaine non numerique telle quelle (`typeof('similar') = 'text'`) tout en
/// convertissant `'1'`/`'0'` en entiers — la meme colonne porte donc l'ancien
/// booleen ET le nouveau mode. Aucun numero de migration n'est consomme.
#[cfg(test)]
mod autoplay_mode_tests {
    use super::*;

    fn repo() -> ZoneRepo {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        ZoneRepo::new(db)
    }

    fn zone(repo: &ZoneRepo) -> i64 {
        repo.create("Salon", Some("dlna"), Some("uuid:salon"))
            .unwrap()
    }

    /// Defaut inchange : une zone neuve n'enchaine rien.
    #[test]
    fn zone_neuve_est_eteinte() {
        let repo = repo();
        let id = zone(&repo);
        assert_eq!(repo.get_autoplay_mode(id), AutoplayMode::Off);
        assert!(!repo.get_autoplay_enabled(id));
    }

    /// LE TEST DE NON-REGRESSION. Une zone dont l'autoplay etait actif avant
    /// #2271 (booleen ecrit en base par `update_autoplay_enabled`) doit
    /// continuer a se comporter EXACTEMENT comme avant : le poller lit
    /// `get_autoplay_enabled` et doit toujours y voir `true`, et le mode
    /// resolu doit etre la strategie d'aujourd'hui — la radio d'artistes
    /// similaires.
    #[test]
    fn heritage_booleen_actif_se_comporte_comme_avant() {
        let repo = repo();
        let id = zone(&repo);

        repo.update_autoplay_enabled(id, true).unwrap();

        assert_eq!(
            repo.get_autoplay_mode(id),
            AutoplayMode::Similar,
            "un `1` en base est la strategie livree : radio d'artistes similaires"
        );
        assert!(
            repo.get_autoplay_enabled(id),
            "REGRESSION : la zone jouait toute seule, elle doit continuer"
        );
    }

    /// Le pendant : un `0` herite reste eteint.
    #[test]
    fn heritage_booleen_eteint_reste_eteint() {
        let repo = repo();
        let id = zone(&repo);
        repo.update_autoplay_enabled(id, true).unwrap();
        repo.update_autoplay_enabled(id, false).unwrap();

        assert_eq!(repo.get_autoplay_mode(id), AutoplayMode::Off);
        assert!(!repo.get_autoplay_enabled(id));
    }

    /// Le mode ecrit par la nouvelle voie est relu tel quel, et le pont de
    /// compatibilite que lit le poller le voit actif.
    #[test]
    fn mode_similar_est_actif() {
        let repo = repo();
        let id = zone(&repo);

        repo.update_autoplay_mode(id, AutoplayMode::Similar)
            .unwrap();

        assert_eq!(repo.get_autoplay_mode(id), AutoplayMode::Similar);
        assert!(
            repo.get_autoplay_enabled(id),
            "le poller lit encore le booleen : il doit voir le mode actif"
        );
    }

    /// L'ECRITURE reste celle de l'ancien booleen. Une version anterieure de
    /// Tune lit cette colonne avec `as_i64()` : si on ecrivait `"similar"` en
    /// toutes lettres, un retour arriere de version eteindrait l'autoplay en
    /// silence. Ce test verrouille l'encodage, pas seulement l'aller-retour.
    #[test]
    fn les_deux_modes_restent_encodes_comme_l_ancien_booleen() {
        assert_eq!(AutoplayMode::Off.as_stocke(), "0");
        assert_eq!(AutoplayMode::Similar.as_stocke(), "1");

        let repo = repo();
        let id = zone(&repo);
        repo.update_autoplay_mode(id, AutoplayMode::Similar)
            .unwrap();

        // Relu comme l'ancien code le relisait : par `as_i64()`.
        let sql = "SELECT autoplay_enabled FROM zones WHERE id = ?1";
        let params: [&dyn ToSqlValue; 1] = [&id];
        let brut = repo
            .db
            .query_one(sql, &params)
            .unwrap()
            .unwrap()
            .first()
            .and_then(|v| v.as_i64());
        assert_eq!(
            brut,
            Some(1),
            "une version anterieure doit encore y voir un autoplay actif"
        );
    }

    #[test]
    fn mode_off_est_eteint() {
        let repo = repo();
        let id = zone(&repo);
        repo.update_autoplay_mode(id, AutoplayMode::Similar)
            .unwrap();
        repo.update_autoplay_mode(id, AutoplayMode::Off).unwrap();

        assert_eq!(repo.get_autoplay_mode(id), AutoplayMode::Off);
        assert!(!repo.get_autoplay_enabled(id));
    }

    /// Aller-retour texte : ce que l'API accepte est ce que la base rend.
    #[test]
    fn aller_retour_des_noms_de_mode() {
        assert_eq!(
            AutoplayMode::from_str_stocke("off"),
            Some(AutoplayMode::Off)
        );
        assert_eq!(
            AutoplayMode::from_str_stocke("similar"),
            Some(AutoplayMode::Similar)
        );
        assert_eq!(AutoplayMode::Off.as_str(), "off");
        assert_eq!(AutoplayMode::Similar.as_str(), "similar");
        assert_eq!(AutoplayMode::from_str_stocke("random_album"), None);
    }

    /// Une valeur inconnue en base — un serveur plus recent a ecrit un mode
    /// que cette version ne connait pas, puis on est redescendu de version —
    /// ne doit pas COUPER la musique : la demande de Sergio etait « n'arretez
    /// pas la musique ». On retombe sur la strategie livree, pas sur `off`.
    #[test]
    fn valeur_inconnue_ne_coupe_pas_la_musique() {
        let repo = repo();
        let id = zone(&repo);
        let sql = repo.update_field_sql("autoplay_enabled");
        let val = "random_album".to_string();
        let params: [&dyn ToSqlValue; 2] = [&val, &id];
        repo.db.execute(&sql, &params).unwrap();

        assert_eq!(repo.get_autoplay_mode(id), AutoplayMode::Similar);
        assert!(repo.get_autoplay_enabled(id));
    }
}

/// #2154 — une écriture impossible ne doit jamais devenir un faux succès.
#[cfg(test)]
mod ignored_zone_settings_tests {
    use super::*;

    /// Schéma volontairement antérieur aux colonnes de réglage. Il représente
    /// exactement une migration absente, sans dépendre du numéro courant des
    /// migrations SQLite.
    fn pre_migration_repo() -> (ZoneRepo, i64) {
        let db = SqliteDb::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE zones (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                output_type TEXT,
                output_device_id TEXT
            );",
        )
        .unwrap();
        let repo = ZoneRepo::new(db);
        let id = repo
            .create("Ancienne base", Some("dlna"), Some("uuid:ancienne"))
            .unwrap();
        (repo, id)
    }

    #[test]
    fn les_reglages_utilisateur_refusent_le_faux_succes() {
        let (repo, id) = pre_migration_repo();
        let avant = zone_settings_ignored();
        let resultats = [
            ("autoplay_enabled", repo.update_autoplay_enabled(id, true)),
            (
                "autoplay_mode",
                repo.update_autoplay_mode(id, AutoplayMode::Similar),
            ),
            ("dsd_mode", repo.update_dsd_mode(id, "dop")),
            ("dlna_native_flac", repo.update_dlna_native_flac(id, true)),
            ("alac_passthrough", repo.update_alac_passthrough(id, true)),
            ("aac_passthrough", repo.update_aac_passthrough(id, true)),
            ("dlna_lpcm", repo.update_dlna_lpcm(id, true)),
            ("lyrics_offset_ms", repo.update_lyrics_offset_ms(id, 250)),
            ("dlna_cap_16bit", repo.update_dlna_cap_16bit(id, true)),
            ("dlna_wav24", repo.update_dlna_wav24(id, true)),
            (
                "dlna_play_delay_ms",
                repo.update_dlna_play_delay_ms(id, 800),
            ),
        ];

        for (reglage, resultat) in &resultats {
            let erreur = resultat
                .as_ref()
                .expect_err("une colonne absente ne peut pas répondre succès");
            assert!(erreur.contains(reglage), "{reglage}: {erreur}");
            assert!(erreur.contains("non enregistré"), "{reglage}: {erreur}");
        }
        assert!(
            zone_settings_ignored() >= avant + resultats.len() as u64,
            "chaque omission doit apparaître dans le compteur de diagnostic"
        );
    }

    #[test]
    fn les_deux_messages_de_moteur_sont_reconnus() {
        assert!(missing_column("execute: no such column: zones.dlna_wav24"));
        assert!(missing_column(
            "db error: column \"dlna_wav24\" does not exist"
        ));
        assert!(!missing_column("database is locked"));

        let erreur = visible_setting_write(
            7,
            "dlna_wav24",
            Err("db error: column \"dlna_wav24\" does not exist".into()),
        )
        .expect_err("PostgreSQL ne doit pas transformer l'absence en succès");
        assert!(erreur.contains("dlna_wav24"), "{erreur}");
    }

    #[test]
    fn l_identite_interne_reste_best_effort_mais_devient_visible() {
        let (repo, id) = pre_migration_repo();
        let avant = zone_settings_ignored();

        repo.set_identity(id, "192.0.2.10", Some("00:11:22:33:44:55"))
            .expect("une ancienne base ne doit pas casser la découverte");

        assert!(
            zone_settings_ignored() >= avant + 2,
            "host et mac absents doivent être comptés"
        );
    }
}
