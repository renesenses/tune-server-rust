//! Albums masqués — disparaître des vues sans toucher aux fichiers (#1391).
//!
//! Jean-Luc Cassé : « comment supprimer un album de la bibliothèque ? »
//! Réponse produit : on ne supprime pas, on MASQUE. Les fichiers restent
//! intacts, l'album sort des vues bibliothèque (grilles, pistes, recherche),
//! et le geste est réversible.
//!
//! # Pourquoi une table de marqueurs, pas une colonne `albums.is_hidden`
//!
//! Le modèle invoqué par l'issue, `zones.is_hidden`, tient parce qu'une ligne
//! `zones` n'est jamais supprimée (son delete est un UPDATE). Une ligne
//! `albums` l'est en routine : purge post-scan, `delete_orphans`, fusion de
//! doublons, « vider la bibliothèque ». Racine music déplacée → pistes
//! purgées → album réinséré sous un nouveau rowid → drapeau perdu. C'est mot
//! pour mot le défaut déjà payé par `favorites` (cœurs éteints, bug .18
//! v0.9.50, cf. `favorites_reconcile.rs`).
//!
//! On reprend donc la solution qui a réparé les favoris :
//! 1. **instantané d'identité** (`item_name`/`item_artist`) figé au masquage ;
//! 2. **réconciliation** au démarrage et post-scan ([`HiddenRepo::reconcile`](crate::db::hidden_repo::HiddenRepo::reconcile)),
//!    qui re-rattache chaque marqueur orphelin à l'album vivant retrouvé par
//!    identité — mêmes règles que les favoris, via
//!    `find_album_by_identity` partagé.
//!
//! Un rescan ORDINAIRE ne menace même pas le marqueur : aucune écriture de
//! scan ne touche `hidden_items`, et l'album mis à jour garde son rowid. La
//! réconciliation couvre le cas dur — l'id qui MEURT.
//!
//! # Global, pas par profil
//!
//! `profile_id` est ÉCRIT (toujours 1) mais jamais LU par les filtres :
//! aucune route de lecture bibliothèque ne connaît le profil aujourd'hui
//! (convention `active_profile.rs` — le header est une identité d'action,
//! jamais une portée de vue). Le jour où les vues porteront le profil, la
//! colonne est déjà là : bascule sans migration.
//!
//! # Masqué n'est pas supprimé
//!
//! `GET /albums/{id}`, `GET /albums/{id}/tracks` et la lecture restent
//! opérants sur un album masqué : une file d'attente ou une playlist qui le
//! référence continue de jouer. Seules les vues de DÉCOUVERTE (listes,
//! recherche, facettes) l'excluent, via les prédicats de `facet_filter`
//! (`hidden_albums_excluded` / `hidden_tracks_excluded`).

use std::sync::Arc;

use serde::Serialize;
use tracing::info;

use super::backend::{DbBackend, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::favorites_reconcile::{ReconcileStats, album_live_identity, find_album_by_identity};
use super::sqlite::SqliteDb;

/// Album masqué (#1391). La table en accepte d'autres types sans changement
/// de schéma — la piste bannie en est le second.
pub const ITEM_TYPE_ALBUM: &str = "album";

/// Titre BANNI (#4806) : jamais choisi par une sélection AUTOMATIQUE
/// (aléatoire, smart playlist, enchaînement de fin de file, radio d'artiste,
/// recommandations), sauté quand une file y arrive, mais VISIBLE — grisé —
/// dans son album, ses playlists et la recherche ; un clic délibéré le joue.
///
/// Contrairement au masquage d'album, le bannissement est PAR PROFIL :
/// `profile_id` est écrit ET lu. Le profil d'une sélection lancée par une
/// route est celui de l'action (`ActiveProfile`) ; celui d'une sélection sans
/// requête (fin de file, autoplay) est le profil actif du serveur — voir
/// [`profil_de_selection_automatique`].
///
/// Deux espaces d'identifiants, deux tables. Un titre LOCAL (`tracks.id`) vit
/// ici, dans `hidden_items` (`item_id` est un entier des deux côtés). Un titre
/// de SERVICE (Qobuz, Tidal, Bandcamp…) n'a pas d'entier : il vit dans la
/// jumelle `streaming_hidden_items`, désigné par la paire `(source,
/// source_id)` — la forme de `streaming_item_tags` et `streaming_favorites`
/// (FabienM, fil 1946 réponse 6820 : « Il faut pouvoir bannir un titre
/// service, pas uniquement local »). Jamais un entier seul : l'id local 7 et
/// le titre Qobuz « 7 » ne désignent pas le même objet.
pub const ITEM_TYPE_TRACK: &str = "track";

/// La provenance d'un titre de service telle qu'on la compare : `Qobuz`,
/// ` qobuz` et `qobuz` désignent le même service. Tout ce qui écrit ou lit
/// `streaming_hidden_items` passe par ici — la file (`queue_items.source`),
/// la lecture en cours et les favoris ne s'écrivent pas tous pareil.
pub fn source_normalisee(source: &str) -> String {
    source.trim().to_ascii_lowercase()
}

/// Un titre de SERVICE à bannir, avec l'instantané d'affichage figé au geste :
/// l'écran « Titres bannis » se rend sans interroger le service, qui peut
/// avoir retiré le titre ou être déconnecté (même raison que
/// `streaming_item_tags`).
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct TitreDeService {
    pub source: String,
    pub source_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    /// L'album CHEZ LE SERVICE, pour que l'écran ramène à l'album de service.
    #[serde(default)]
    pub album_source_id: Option<String>,
    #[serde(default)]
    pub cover_url: Option<String>,
}

/// Masquage GLOBAL : on écrit le profil pour préparer l'avenir, on ne le lit
/// jamais — même valeur que les six sites `profile_id = 1` des facettes.
const GLOBAL_PROFILE_ID: i64 = 1;

/// Le profil d'une sélection automatique qui n'a PAS de requête HTTP pour
/// dire qui agit (enchaînement de fin de file, autoplay, radio d'artiste) :
/// le profil actif du serveur (`settings.active_profile_id`), sinon 1 — le
/// même repli que l'extracteur `ActiveProfile` côté routes, sans son en-tête.
pub fn profil_de_selection_automatique(db: &Arc<dyn DbBackend>) -> i64 {
    super::settings_repo::SettingsRepo::with_backend(db.clone())
        .get("active_profile_id")
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|id| *id > 0)
        .unwrap_or(GLOBAL_PROFILE_ID)
}

/// #4806 suite — ajoute à l'ensemble d'exclusion d'une radio de SERVICE les
/// `source_id` que le profil des sélections automatiques a bannis chez ce
/// service. UNE définition pour les deux radios qui tirent dans le catalogue
/// d'un service : la radio de fin de file (`poller::radio`,
/// `autoplay_streaming_radio`) et « Plus comme ça » (`GET
/// /streaming/{service}/tracks/{id}/similar`) — elles partagent déjà
/// `auto_dj::pistes_similaires_du_service`, elles partagent aussi ce filtre.
/// L'exclusion se fait AVANT le tirage : la borne se tient en titres jouables.
/// Une base illisible n'empêche pas la radio : elle est journalisée, et rien
/// n'est ajouté. Rend le nombre d'identifiants ajoutés.
pub fn exclure_les_titres_de_service_bannis(
    db: &Arc<dyn DbBackend>,
    source: &str,
    exclure: &mut std::collections::HashSet<String>,
) -> usize {
    let profil = profil_de_selection_automatique(db);
    match HiddenRepo::with_backend(db.clone()).banned_streaming_ids_for_source(profil, source) {
        Ok(bannis) => {
            let n = bannis.len();
            exclure.extend(bannis);
            n
        }
        Err(e) => {
            tracing::warn!(source, profil, error = %e, "radio_de_service_bans_illisibles");
            0
        }
    }
}

/// Un titre banni, tel que la route de révision le rend.
///
/// Les deux espaces dans une même liste, sans ambiguïté : un titre LOCAL porte
/// `track_id` et `source: null` ; un titre de SERVICE porte `track_id: null`
/// et la paire `source` + `source_id` — la règle de `GET /tags/{id}/items`.
#[derive(Debug, Clone, Serialize)]
pub struct BannedTrack {
    pub track_id: Option<i64>,
    /// Le service d'un titre de service (`qobuz`, `tidal`…), `None` en local.
    pub source: Option<String>,
    pub source_id: Option<String>,
    /// L'album chez le service, pour un titre de service.
    pub album_source_id: Option<String>,
    /// Titre vivant si la piste existe encore, sinon l'instantané figé au
    /// bannissement.
    pub title: String,
    pub artist: Option<String>,
    pub album_id: Option<i64>,
    pub album_title: Option<String>,
    /// `albums.cover_path` de l'album vivant, pour la vignette de l'écran
    /// « Titres bannis » (l'image se sert par `/library/albums/{id}/cover`).
    /// Pour un titre de service : l'URL de pochette figée au bannissement.
    pub cover_path: Option<String>,
    pub banned_at: Option<String>,
    /// `false` = marqueur orphelin : l'id ne désigne plus de piste vivante.
    /// Pas de réconciliation par identité pour une piste (titre + interprète
    /// rattacherait une autre VERSION, non visée) : le marqueur reste listé,
    /// débannissable, et c'est tout.
    pub resolved: bool,
}

/// Un album masqué, tel que la route de révision le rend.
#[derive(Debug, Clone, Serialize)]
pub struct HiddenAlbum {
    pub album_id: i64,
    /// Titre vivant si l'album existe encore, sinon l'instantané figé au
    /// masquage — la liste reste lisible même pendant qu'une racine est
    /// démontée.
    pub title: String,
    pub artist: Option<String>,
    pub hidden_at: Option<String>,
    /// `false` = marqueur orphelin (l'id ne désigne plus d'album vivant), en
    /// attente de réconciliation post-scan.
    pub resolved: bool,
}

/// Constructeurs SQL agnostiques du moteur.
pub mod sql {
    use super::SqlDialect;

    /// `ON CONFLICT … DO NOTHING` : masquer deux fois est un non-événement,
    /// pas une erreur — même convention que `favorite_facets`.
    /// `created_at` par l'expression « maintenant » du moteur plutôt que par
    /// le DEFAULT de la colonne, pour les tables montées par un autre chemin.
    pub fn hide<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO hidden_items (profile_id, item_type, item_id, item_name, item_artist, created_at) \
             VALUES ({}, {}, {}, {}, {}, {}) \
             ON CONFLICT (profile_id, item_type, item_id) DO NOTHING",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.now_iso8601(),
        )
    }

    /// Le démasquage est GLOBAL comme le masquage : pas de filtre profil.
    pub fn unhide<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM hidden_items WHERE item_type = {} AND item_id = {}",
            d.placeholder(1),
            d.placeholder(2),
        )
    }

    pub fn count_one<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COUNT(*) FROM hidden_items WHERE item_type = {} AND item_id = {}",
            d.placeholder(1),
            d.placeholder(2),
        )
    }

    /// LEFT JOIN : un marqueur orphelin (album mort) reste listé, avec son
    /// instantané — c'est ce qui permet de le démasquer quand même.
    pub fn list_albums<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT hi.item_id, hi.item_name, hi.item_artist, hi.created_at, a.id, a.title, ar.name \
             FROM hidden_items hi \
             LEFT JOIN albums a ON a.id = hi.item_id \
             LEFT JOIN artists ar ON ar.id = a.artist_id \
             WHERE hi.item_type = {} \
             ORDER BY hi.created_at DESC, hi.item_id ASC",
            d.placeholder(1),
        )
    }

    // --- Titres bannis (#4806) : tout est PAR PROFIL, contrairement à l'album.

    pub fn unban_track<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM hidden_items WHERE profile_id = {} AND item_type = {} AND item_id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
        )
    }

    pub fn count_one_for_profile<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COUNT(*) FROM hidden_items \
             WHERE profile_id = {} AND item_type = {} AND item_id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
        )
    }

    /// Les ids bannis PARMI une liste — une requête par page, jamais une par
    /// piste. Les ids sont des `i64` de confiance inscrits en clair, même
    /// raison que `TrackMetadataRepo::sql::get_key_for_tracks`.
    pub fn banned_among<D: SqlDialect>(d: &D, id_list: &str) -> String {
        format!(
            "SELECT item_id FROM hidden_items \
             WHERE profile_id = {} AND item_type = {} AND item_id IN ({id_list})",
            d.placeholder(1),
            d.placeholder(2),
        )
    }

    /// LEFT JOIN : un marqueur orphelin (piste morte) reste listé avec son
    /// instantané, donc débannissable.
    pub fn list_tracks<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT hi.item_id, hi.item_name, hi.item_artist, hi.created_at, \
                    t.id, t.title, ar.name, t.album_id, al.title, al.cover_path \
             FROM hidden_items hi \
             LEFT JOIN tracks t ON t.id = hi.item_id \
             LEFT JOIN artists ar ON ar.id = t.artist_id \
             LEFT JOIN albums al ON al.id = t.album_id \
             WHERE hi.profile_id = {} AND hi.item_type = {} \
             ORDER BY hi.created_at DESC, hi.item_id ASC",
            d.placeholder(1),
            d.placeholder(2),
        )
    }
}

/// SQL des titres de SERVICE bannis (#4806), table `streaming_hidden_items`.
pub mod sql_service {
    use super::SqlDialect;

    /// Rebannir met l'instantané à jour (le titre a pu être renommé chez le
    /// service) sans toucher à la date du premier bannissement.
    pub fn ban<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO streaming_hidden_items \
             (profile_id, item_type, source, source_id, title, artist, album, album_source_id, cover_url, created_at) \
             VALUES ({}, {}, {}, {}, {}, {}, {}, {}, {}, {}) \
             ON CONFLICT (profile_id, item_type, source, source_id) DO UPDATE SET \
             title = COALESCE(excluded.title, streaming_hidden_items.title), \
             artist = COALESCE(excluded.artist, streaming_hidden_items.artist), \
             album = COALESCE(excluded.album, streaming_hidden_items.album), \
             album_source_id = COALESCE(excluded.album_source_id, streaming_hidden_items.album_source_id), \
             cover_url = COALESCE(excluded.cover_url, streaming_hidden_items.cover_url)",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
            d.placeholder(8),
            d.placeholder(9),
            d.now_iso8601(),
        )
    }

    pub fn unban<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM streaming_hidden_items \
             WHERE profile_id = {} AND item_type = {} AND source = {} AND source_id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
        )
    }

    pub fn count_one<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COUNT(*) FROM streaming_hidden_items \
             WHERE profile_id = {} AND item_type = {} AND source = {} AND source_id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
        )
    }

    /// Toutes les paires bannies du profil : une requête, jamais une par ligne.
    pub fn keys<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT source, source_id FROM streaming_hidden_items \
             WHERE profile_id = {} AND item_type = {}",
            d.placeholder(1),
            d.placeholder(2),
        )
    }

    pub fn list<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT source, source_id, title, artist, album, album_source_id, cover_url, created_at \
             FROM streaming_hidden_items \
             WHERE profile_id = {} AND item_type = {} \
             ORDER BY created_at DESC, source ASC, source_id ASC",
            d.placeholder(1),
            d.placeholder(2),
        )
    }
}

pub struct HiddenRepo {
    db: Arc<dyn DbBackend>,
}

impl HiddenRepo {
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

    /// Identité vivante (titre, artiste) de l'album, ou `None` s'il n'existe
    /// pas — le masquage d'un id fantôme est refusé plutôt qu'écrit.
    fn album_identity(&self, album_id: i64) -> Result<Option<(String, String)>, String> {
        album_live_identity(self.db.as_ref(), album_id)
    }

    /// Masque un album. `Ok(false)` = id inconnu (la route rend 404).
    /// Idempotent : re-masquer un album déjà masqué réussit sans rien écrire.
    ///
    /// L'instantané d'identité est figé ICI, dans le même INSERT — pas en
    /// rattrapage différé : c'est lui qui fait survivre le marqueur au
    /// renouvellement d'id.
    pub fn hide_album(&self, album_id: i64) -> Result<bool, String> {
        let Some((title, artist)) = self.album_identity(album_id)? else {
            return Ok(false);
        };
        let sql = self.dialect_sql(sql::hide, sql::hide);
        let params: [&dyn ToSqlValue; 5] = [
            &GLOBAL_PROFILE_ID,
            &ITEM_TYPE_ALBUM,
            &album_id,
            &title,
            &artist,
        ];
        self.db.execute(&sql, &params)?;
        Ok(true)
    }

    /// Démasque. `Ok(false)` = rien n'était masqué sous cet id.
    pub fn unhide_album(&self, album_id: i64) -> Result<bool, String> {
        let sql = self.dialect_sql(sql::unhide, sql::unhide);
        let params: [&dyn ToSqlValue; 2] = [&ITEM_TYPE_ALBUM, &album_id];
        Ok(self.db.execute(&sql, &params)? > 0)
    }

    pub fn is_album_hidden(&self, album_id: i64) -> Result<bool, String> {
        let sql = self.dialect_sql(sql::count_one, sql::count_one);
        let params: [&dyn ToSqlValue; 2] = [&ITEM_TYPE_ALBUM, &album_id];
        match self.db.query_one(&sql, &params)? {
            None => Ok(false),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0) > 0),
        }
    }

    /// Tous les albums masqués, vivants (`resolved = true`) comme orphelins.
    pub fn list_hidden_albums(&self) -> Result<Vec<HiddenAlbum>, String> {
        let sql = self.dialect_sql(sql::list_albums, sql::list_albums);
        let params: [&dyn ToSqlValue; 1] = [&ITEM_TYPE_ALBUM];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let album_id = r.first().and_then(|v| v.as_i64())?;
                let snapshot_name = r.get(1).and_then(|v| v.as_string()).unwrap_or_default();
                let snapshot_artist = r.get(2).and_then(|v| v.as_string()).unwrap_or_default();
                let hidden_at = r.get(3).and_then(|v| v.as_string());
                let resolved = r.get(4).and_then(|v| v.as_i64()).is_some();
                let live_title = r.get(5).and_then(|v| v.as_string());
                let live_artist = r.get(6).and_then(|v| v.as_string());
                Some(HiddenAlbum {
                    album_id,
                    title: live_title.unwrap_or(snapshot_name),
                    artist: live_artist.or({
                        if snapshot_artist.is_empty() {
                            None
                        } else {
                            Some(snapshot_artist)
                        }
                    }),
                    hidden_at,
                    resolved,
                })
            })
            .collect())
    }

    /// Re-rattache les marqueurs orphelins aux albums vivants retrouvés par
    /// identité — le pendant de `FavoritesReconciler::run`, appelé aux mêmes
    /// endroits (démarrage, post-scan, purge de bibliothèque).
    ///
    // --- Titres bannis (#4806) ------------------------------------------

    /// Identité vivante (titre, interprète) de la piste, ou `None` si elle
    /// n'existe pas — bannir un id fantôme est refusé plutôt qu'écrit.
    fn track_identity(&self, track_id: i64) -> Result<Option<(String, String)>, String> {
        let params: [&dyn ToSqlValue; 1] = [&track_id];
        let row = self.db.query_one(
            "SELECT t.title, COALESCE(ar.name, t.album_artist, '') \
             FROM tracks t LEFT JOIN artists ar ON ar.id = t.artist_id \
             WHERE t.id = ?",
            &params,
        )?;
        Ok(row.map(|cols| {
            (
                cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
            )
        }))
    }

    /// Bannit un titre LOCAL pour ce profil. `Ok(false)` = id inconnu (la
    /// route rend 404). Idempotent : re-bannir réussit sans rien écrire.
    pub fn ban_track(&self, profile_id: i64, track_id: i64) -> Result<bool, String> {
        let Some((title, artist)) = self.track_identity(track_id)? else {
            return Ok(false);
        };
        let sql = self.dialect_sql(sql::hide, sql::hide);
        let params: [&dyn ToSqlValue; 5] =
            [&profile_id, &ITEM_TYPE_TRACK, &track_id, &title, &artist];
        self.db.execute(&sql, &params)?;
        info!(profile_id, track_id, title = %title, "track_banned");
        Ok(true)
    }

    /// Débannit. `Ok(false)` = rien n'était banni sous cet id pour ce profil.
    pub fn unban_track(&self, profile_id: i64, track_id: i64) -> Result<bool, String> {
        let sql = self.dialect_sql(sql::unban_track, sql::unban_track);
        let params: [&dyn ToSqlValue; 3] = [&profile_id, &ITEM_TYPE_TRACK, &track_id];
        let n = self.db.execute(&sql, &params)?;
        if n > 0 {
            info!(profile_id, track_id, "track_unbanned");
        }
        Ok(n > 0)
    }

    pub fn is_track_banned(&self, profile_id: i64, track_id: i64) -> Result<bool, String> {
        let sql = self.dialect_sql(sql::count_one_for_profile, sql::count_one_for_profile);
        let params: [&dyn ToSqlValue; 3] = [&profile_id, &ITEM_TYPE_TRACK, &track_id];
        match self.db.query_one(&sql, &params)? {
            None => Ok(false),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0) > 0),
        }
    }

    /// Les ids bannis parmi `track_ids`, pour ce profil — une seule requête,
    /// aucune sur une liste vide. C'est ce que les listes de pistes lisent
    /// pour poser leur drapeau `banned` sans rien filtrer.
    pub fn banned_track_ids(
        &self,
        profile_id: i64,
        track_ids: &[i64],
    ) -> Result<std::collections::HashSet<i64>, String> {
        if track_ids.is_empty() {
            return Ok(std::collections::HashSet::new());
        }
        let id_list = track_ids
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let sql = self.dialect_sql(
            |d| sql::banned_among(d, &id_list),
            |d| sql::banned_among(d, &id_list),
        );
        let params: [&dyn ToSqlValue; 2] = [&profile_id, &ITEM_TYPE_TRACK];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows
            .iter()
            .filter_map(|r| r.first().and_then(|v| v.as_i64()))
            .collect())
    }

    /// Tous les titres bannis du profil, vivants comme orphelins — les titres
    /// LOCAUX d'abord, puis les titres de SERVICE (#4806), chaque famille du
    /// plus récent au plus ancien.
    pub fn list_banned_tracks(&self, profile_id: i64) -> Result<Vec<BannedTrack>, String> {
        let mut out = self.list_banned_local_tracks(profile_id)?;
        out.extend(self.list_banned_streaming_tracks(profile_id)?);
        Ok(out)
    }

    fn list_banned_local_tracks(&self, profile_id: i64) -> Result<Vec<BannedTrack>, String> {
        let sql = self.dialect_sql(sql::list_tracks, sql::list_tracks);
        let params: [&dyn ToSqlValue; 2] = [&profile_id, &ITEM_TYPE_TRACK];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let track_id = r.first().and_then(|v| v.as_i64())?;
                let snapshot_name = r.get(1).and_then(|v| v.as_string()).unwrap_or_default();
                let snapshot_artist = r.get(2).and_then(|v| v.as_string()).unwrap_or_default();
                let banned_at = r.get(3).and_then(|v| v.as_string());
                let resolved = r.get(4).and_then(|v| v.as_i64()).is_some();
                let live_title = r.get(5).and_then(|v| v.as_string());
                let live_artist = r.get(6).and_then(|v| v.as_string());
                Some(BannedTrack {
                    track_id: Some(track_id),
                    source: None,
                    source_id: None,
                    album_source_id: None,
                    title: live_title.unwrap_or(snapshot_name),
                    artist: live_artist.or({
                        if snapshot_artist.is_empty() {
                            None
                        } else {
                            Some(snapshot_artist)
                        }
                    }),
                    album_id: r.get(7).and_then(|v| v.as_i64()),
                    album_title: r.get(8).and_then(|v| v.as_string()),
                    cover_path: r.get(9).and_then(|v| v.as_string()),
                    banned_at,
                    resolved,
                })
            })
            .collect())
    }

    // --- Titres de SERVICE bannis (#4806) ------------------------------

    /// Une ligne de file (ou la lecture en cours) est-elle bannie pour ce
    /// profil ? Une ligne LOCALE se juge sur son `track_id`, et SEULEMENT
    /// dessus ; une ligne de SERVICE (sans `track_id`) sur sa paire. Jamais
    /// l'un pour l'autre : l'id local 7 banni ne bannit pas le titre Qobuz
    /// « 7 ». Une erreur de lecture vaut « pas banni » — on joue plutôt que de
    /// sauter à tort, la sélection automatique a déjà filtré en amont.
    pub fn ligne_bannie(
        &self,
        profile_id: i64,
        track_id: Option<i64>,
        source: Option<&str>,
        source_id: Option<&str>,
    ) -> bool {
        match (track_id, source, source_id) {
            (Some(id), _, _) => self.is_track_banned(profile_id, id).unwrap_or(false),
            (None, Some(src), Some(sid)) => self
                .is_streaming_track_banned(profile_id, src, sid)
                .unwrap_or(false),
            _ => false,
        }
    }

    /// Bannit un titre de SERVICE pour ce profil. `Ok(false)` = désignation
    /// incomplète (service ou identifiant vide) : rien n'est écrit, la route
    /// rend 400. Idempotent ; rebannir rafraîchit l'instantané.
    pub fn ban_streaming_track(&self, profile_id: i64, t: &TitreDeService) -> Result<bool, String> {
        let source = source_normalisee(&t.source);
        let source_id = t.source_id.trim().to_string();
        if source.is_empty() || source_id.is_empty() {
            return Ok(false);
        }
        let sql = self.dialect_sql(sql_service::ban, sql_service::ban);
        let params: [&dyn ToSqlValue; 9] = [
            &profile_id,
            &ITEM_TYPE_TRACK,
            &source,
            &source_id,
            &t.title,
            &t.artist,
            &t.album,
            &t.album_source_id,
            &t.cover_url,
        ];
        self.db.execute(&sql, &params)?;
        info!(profile_id, source = %source, source_id = %source_id, title = ?t.title, "streaming_track_banned");
        Ok(true)
    }

    /// Débannit un titre de service. `Ok(false)` = rien n'était banni.
    pub fn unban_streaming_track(
        &self,
        profile_id: i64,
        source: &str,
        source_id: &str,
    ) -> Result<bool, String> {
        let source = source_normalisee(source);
        let source_id = source_id.trim().to_string();
        let sql = self.dialect_sql(sql_service::unban, sql_service::unban);
        let params: [&dyn ToSqlValue; 4] = [&profile_id, &ITEM_TYPE_TRACK, &source, &source_id];
        let n = self.db.execute(&sql, &params)?;
        if n > 0 {
            info!(profile_id, source = %source, source_id = %source_id, "streaming_track_unbanned");
        }
        Ok(n > 0)
    }

    pub fn is_streaming_track_banned(
        &self,
        profile_id: i64,
        source: &str,
        source_id: &str,
    ) -> Result<bool, String> {
        let source = source_normalisee(source);
        let source_id = source_id.trim().to_string();
        if source.is_empty() || source_id.is_empty() {
            return Ok(false);
        }
        let sql = self.dialect_sql(sql_service::count_one, sql_service::count_one);
        let params: [&dyn ToSqlValue; 4] = [&profile_id, &ITEM_TYPE_TRACK, &source, &source_id];
        match self.db.query_one(&sql, &params)? {
            None => Ok(false),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0) > 0),
        }
    }

    /// Toutes les paires `(source normalisée, source_id)` bannies par ce
    /// profil — ce que lisent la file et les générateurs qui épurent une
    /// liste de titres de service après coup. Une seule requête ; la liste
    /// d'un profil se compte en dizaines, pas en milliers.
    pub fn banned_streaming_keys(
        &self,
        profile_id: i64,
    ) -> Result<std::collections::HashSet<(String, String)>, String> {
        let sql = self.dialect_sql(sql_service::keys, sql_service::keys);
        let params: [&dyn ToSqlValue; 2] = [&profile_id, &ITEM_TYPE_TRACK];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let source = r.first().and_then(|v| v.as_string())?;
                let source_id = r.get(1).and_then(|v| v.as_string())?;
                Some((source_normalisee(&source), source_id))
            })
            .collect())
    }

    /// Les `source_id` bannis par ce profil CHEZ CE SERVICE — la forme que
    /// prennent les ensembles d'exclusion de la radio de service
    /// (`autoplay_streaming_radio`, « Plus comme ça »), qui ne comparent que
    /// des identifiants d'un seul service.
    pub fn banned_streaming_ids_for_source(
        &self,
        profile_id: i64,
        source: &str,
    ) -> Result<std::collections::HashSet<String>, String> {
        let source = source_normalisee(source);
        Ok(self
            .banned_streaming_keys(profile_id)?
            .into_iter()
            .filter(|(s, _)| *s == source)
            .map(|(_, id)| id)
            .collect())
    }

    fn list_banned_streaming_tracks(&self, profile_id: i64) -> Result<Vec<BannedTrack>, String> {
        let sql = self.dialect_sql(sql_service::list, sql_service::list);
        let params: [&dyn ToSqlValue; 2] = [&profile_id, &ITEM_TYPE_TRACK];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let source = r.first().and_then(|v| v.as_string())?;
                let source_id = r.get(1).and_then(|v| v.as_string())?;
                Some(BannedTrack {
                    track_id: None,
                    source: Some(source),
                    source_id: Some(source_id),
                    album_source_id: r.get(5).and_then(|v| v.as_string()),
                    title: r.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
                    artist: r.get(3).and_then(|v| v.as_string()),
                    album_id: None,
                    album_title: r.get(4).and_then(|v| v.as_string()),
                    cover_path: r.get(6).and_then(|v| v.as_string()),
                    banned_at: r.get(7).and_then(|v| v.as_string()),
                    // L'instantané EST l'identité : rien à résoudre.
                    resolved: true,
                })
            })
            .collect())
    }

    /// `delete_unresolved` ne doit être vrai qu'après un scan COMPLET et sain
    /// (même règle que les favoris, #1943) : c'est la seule situation où
    /// « introuvable » veut dire « n'existe vraiment plus ». Au démarrage ou
    /// sur un scan partiel, un marqueur orphelin est CONSERVÉ — un volume pas
    /// encore monté peut encore ramener l'album, et un album masqué qui
    /// réapparaît visible serait exactement le bug que cette table évite.
    ///
    /// Ne traite QUE les albums : un marqueur de piste bannie (#4806) n'est
    /// ni rattaché ni purgé ici — voir `BannedTrack::resolved`.
    pub fn reconcile(&self, delete_unresolved: bool) -> Result<ReconcileStats, String> {
        let params: [&dyn ToSqlValue; 1] = [&ITEM_TYPE_ALBUM];
        let rows = self.db.query_many(
            "SELECT profile_id, item_id, item_name, item_artist \
             FROM hidden_items WHERE item_type = ?",
            &params,
        )?;

        let mut stats = ReconcileStats::default();
        for row in &rows {
            let profile_id = row.first().and_then(|v| v.as_i64()).unwrap_or(1);
            let Some(item_id) = row.get(1).and_then(|v| v.as_i64()) else {
                continue;
            };
            stats.scanned += 1;
            let snapshot_name = row.get(2).and_then(|v| v.as_string()).unwrap_or_default();
            let snapshot_artist = row.get(3).and_then(|v| v.as_string()).unwrap_or_default();

            // Album encore vivant → au plus un rattrapage d'instantané.
            if let Some((title, artist)) = self.album_identity(item_id)? {
                if snapshot_name.is_empty() {
                    let params: [&dyn ToSqlValue; 5] =
                        [&title, &artist, &profile_id, &ITEM_TYPE_ALBUM, &item_id];
                    self.db.execute(
                        "UPDATE hidden_items SET item_name = ?, item_artist = ? \
                         WHERE profile_id = ? AND item_type = ? AND item_id = ?",
                        &params,
                    )?;
                    stats.snapshots_backfilled += 1;
                }
                continue;
            }

            // Orphelin : l'identité est l'instantané figé au masquage.
            let target = if snapshot_name.is_empty() {
                None
            } else {
                find_album_by_identity(self.db.as_ref(), &snapshot_name, &snapshot_artist)?
            };

            match target {
                Some(new_id) if new_id != item_id => {
                    if self.is_album_hidden(new_id)? {
                        // La cible vivante est déjà masquée : l'orphelin est
                        // un doublon, on le retire.
                        let params: [&dyn ToSqlValue; 3] =
                            [&profile_id, &ITEM_TYPE_ALBUM, &item_id];
                        self.db.execute(
                            "DELETE FROM hidden_items \
                             WHERE profile_id = ? AND item_type = ? AND item_id = ?",
                            &params,
                        )?;
                        stats.deduplicated += 1;
                    } else {
                        // Ré-instantané depuis l'album vivant : la casse du
                        // titre ou l'artiste ont pu changer au re-scan.
                        let (title, artist) = self
                            .album_identity(new_id)?
                            .unwrap_or((snapshot_name.clone(), snapshot_artist.clone()));
                        let params: [&dyn ToSqlValue; 6] = [
                            &new_id,
                            &title,
                            &artist,
                            &profile_id,
                            &ITEM_TYPE_ALBUM,
                            &item_id,
                        ];
                        self.db.execute(
                            "UPDATE hidden_items SET item_id = ?, item_name = ?, item_artist = ? \
                             WHERE profile_id = ? AND item_type = ? AND item_id = ?",
                            &params,
                        )?;
                        info!(
                            old_id = item_id,
                            new_id,
                            name = %title,
                            "hidden_album_relinked"
                        );
                        stats.relinked += 1;
                    }
                }
                _ => {
                    if delete_unresolved {
                        let params: [&dyn ToSqlValue; 3] =
                            [&profile_id, &ITEM_TYPE_ALBUM, &item_id];
                        self.db.execute(
                            "DELETE FROM hidden_items \
                             WHERE profile_id = ? AND item_type = ? AND item_id = ?",
                            &params,
                        )?;
                        stats.deleted += 1;
                    } else {
                        stats.unresolved += 1;
                    }
                }
            }
        }
        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::album_repo::AlbumRepo;
    use crate::db::migrations;

    fn test_db() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    fn insert_artist(db: &Arc<dyn DbBackend>, name: &str) -> i64 {
        let params: [&dyn ToSqlValue; 1] = [&name];
        db.execute("INSERT INTO artists (name) VALUES (?)", &params)
            .unwrap();
        db.last_insert_rowid()
    }

    fn insert_album(db: &Arc<dyn DbBackend>, title: &str, artist_id: i64) -> i64 {
        let params: [&dyn ToSqlValue; 2] = [&title, &artist_id];
        db.execute(
            "INSERT INTO albums (title, artist_id, track_count) VALUES (?, ?, 10)",
            &params,
        )
        .unwrap();
        db.last_insert_rowid()
    }

    #[test]
    fn masquer_demasquer_lister() {
        let db = test_db();
        let repo = HiddenRepo::with_backend(db.clone());
        let ar = insert_artist(&db, "Daft Punk");
        let al = insert_album(&db, "Discovery", ar);

        assert!(!repo.is_album_hidden(al).unwrap());
        assert!(repo.hide_album(al).unwrap());
        // Idempotent : re-masquer réussit sans erreur ni doublon.
        assert!(repo.hide_album(al).unwrap());
        assert!(repo.is_album_hidden(al).unwrap());

        let listed = repo.list_hidden_albums().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].album_id, al);
        assert_eq!(listed[0].title, "Discovery");
        assert_eq!(listed[0].artist.as_deref(), Some("Daft Punk"));
        assert!(listed[0].resolved);

        assert!(repo.unhide_album(al).unwrap());
        assert!(!repo.is_album_hidden(al).unwrap());
        assert!(!repo.unhide_album(al).unwrap(), "plus rien à démasquer");
    }

    #[test]
    fn masquer_un_id_fantome_est_refuse() {
        let db = test_db();
        let repo = HiddenRepo::with_backend(db);
        assert!(!repo.hide_album(4242).unwrap());
        assert!(repo.list_hidden_albums().unwrap().is_empty());
    }

    /// LE cas pour lequel la table existe : l'album meurt (racine déplacée,
    /// « vider la bibliothèque ») puis renaît sous un NOUVEL id — le marqueur
    /// doit suivre, comme les favoris depuis le bug .18.
    #[test]
    fn reconcile_suit_le_renouvellement_d_id() {
        let db = test_db();
        let repo = HiddenRepo::with_backend(db.clone());
        let ar = insert_artist(&db, "Talvin Singh");
        let old_id = insert_album(&db, "OK", ar);
        assert!(repo.hide_album(old_id).unwrap());

        // Rescan destructeur simulé : la ligne meurt, l'album renaît ailleurs.
        let params: [&dyn ToSqlValue; 1] = [&old_id];
        db.execute("DELETE FROM albums WHERE id = ?", &params)
            .unwrap();
        let new_id = insert_album(&db, "OK", ar);
        assert_ne!(old_id, new_id);

        // Avant réconciliation : marqueur orphelin, listé comme non résolu.
        let listed = repo.list_hidden_albums().unwrap();
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].resolved);
        assert_eq!(listed[0].title, "OK", "l'instantané garde la liste lisible");

        let stats = repo.reconcile(false).unwrap();
        assert_eq!(stats.relinked, 1);
        assert!(
            repo.is_album_hidden(new_id).unwrap(),
            "le masquage doit suivre le nouvel id"
        );
        assert!(!repo.is_album_hidden(old_id).unwrap());
    }

    /// Un orphelin introuvable n'est supprimé QUE sur un scan complet sain —
    /// jamais au démarrage : un NAS pas encore monté peut encore le ramener.
    #[test]
    fn reconcile_ne_supprime_l_introuvable_que_sur_scan_complet() {
        let db = test_db();
        let repo = HiddenRepo::with_backend(db.clone());
        let ar = insert_artist(&db, "daoud");
        let al = insert_album(&db, "ok", ar);
        assert!(repo.hide_album(al).unwrap());
        let params: [&dyn ToSqlValue; 1] = [&al];
        db.execute("DELETE FROM albums WHERE id = ?", &params)
            .unwrap();

        // Démarrage / scan partiel : conservé.
        let stats = repo.reconcile(false).unwrap();
        assert_eq!((stats.deleted, stats.unresolved), (0, 1));
        assert_eq!(repo.list_hidden_albums().unwrap().len(), 1);

        // Scan complet sain : purgé.
        let stats = repo.reconcile(true).unwrap();
        assert_eq!(stats.deleted, 1);
        assert!(repo.list_hidden_albums().unwrap().is_empty());
    }

    /// Deux marqueurs qui convergent vers le même album vivant : le doublon
    /// est retiré au lieu de violer la clé primaire.
    #[test]
    fn reconcile_dedoublonne_vers_la_meme_cible() {
        let db = test_db();
        let repo = HiddenRepo::with_backend(db.clone());
        let ar = insert_artist(&db, "Boards of Canada");
        let a1 = insert_album(&db, "Geogaddi", ar);
        let a2 = insert_album(&db, "Geogaddi", ar);
        assert!(repo.hide_album(a1).unwrap());
        assert!(repo.hide_album(a2).unwrap());

        // La fusion de doublons du scan supprime la ligne perdante.
        let params: [&dyn ToSqlValue; 1] = [&a1];
        db.execute("DELETE FROM albums WHERE id = ?", &params)
            .unwrap();

        let stats = repo.reconcile(false).unwrap();
        assert_eq!(stats.deduplicated, 1);
        let listed = repo.list_hidden_albums().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].album_id, a2);
    }

    /// Le piège principal de l'issue : un RESCAN ordinaire (l'album garde son
    /// rowid, ses colonnes sont réécrites) ne doit pas ressusciter l'album.
    #[test]
    fn un_rescan_ordinaire_ne_ressuscite_pas_le_masquage() {
        let db = test_db();
        let repo = HiddenRepo::with_backend(db.clone());
        let album_repo = AlbumRepo::with_backend(db.clone());
        let ar = insert_artist(&db, "Air");
        let al = insert_album(&db, "Moon Safari", ar);
        assert!(repo.hide_album(al).unwrap());

        // Ce que fait un rescan sur un album existant : update nommé des
        // colonnes (jamais d'INSERT OR REPLACE sur albums).
        let mut refreshed = album_repo.get(al).unwrap().unwrap();
        refreshed.year = Some(1998);
        refreshed.genre = Some("Downtempo".into());
        album_repo.update(&refreshed).unwrap();

        assert!(
            repo.is_album_hidden(al).unwrap(),
            "le marqueur doit survivre au rescan"
        );
        let stats = repo.reconcile(false).unwrap();
        assert_eq!(stats.changed(), 0, "rien à réparer après un simple update");
        assert!(repo.is_album_hidden(al).unwrap());
    }

    fn insert_track(db: &Arc<dyn DbBackend>, title: &str, artist_id: i64, album_id: i64) -> i64 {
        let params: [&dyn ToSqlValue; 4] = [&title, &artist_id, &album_id, &title];
        db.execute(
            "INSERT INTO tracks (title, artist_id, album_id, file_path) VALUES (?, ?, ?, '/m/' || ? || '.flac')",
            &params,
        )
        .unwrap();
        db.last_insert_rowid()
    }

    /// #4806 — bannir, lister, débannir : par PROFIL (g), idempotent, et
    /// l'album masqué (type `album`) et le titre banni (type `track`) de
    /// même id numérique ne se confondent pas dans la même table.
    #[test]
    fn bannir_lister_debannir_par_profil() {
        let db = test_db();
        let repo = HiddenRepo::with_backend(db.clone());
        let ar = insert_artist(&db, "Portishead");
        let al = insert_album(&db, "Dummy", ar);
        let t1 = insert_track(&db, "Roads", ar, al);
        let t2 = insert_track(&db, "Glory Box", ar, al);

        assert!(!repo.is_track_banned(1, t1).unwrap());
        assert!(repo.ban_track(1, t1).unwrap());
        assert!(repo.ban_track(1, t1).unwrap(), "idempotent");
        assert!(repo.is_track_banned(1, t1).unwrap());
        assert!(
            !repo.is_track_banned(2, t1).unwrap(),
            "(g) pas banni chez le profil 2"
        );
        assert!(!repo.is_track_banned(1, t2).unwrap());

        let parmi = repo.banned_track_ids(1, &[t1, t2, 999]).unwrap();
        assert_eq!(parmi.len(), 1);
        assert!(parmi.contains(&t1));
        assert!(repo.banned_track_ids(2, &[t1, t2]).unwrap().is_empty());
        assert!(repo.banned_track_ids(1, &[]).unwrap().is_empty());

        let liste = repo.list_banned_tracks(1).unwrap();
        assert_eq!(liste.len(), 1);
        assert_eq!(liste[0].track_id, Some(t1));
        assert_eq!(liste[0].title, "Roads");
        assert_eq!(liste[0].artist.as_deref(), Some("Portishead"));
        assert_eq!(liste[0].album_id, Some(al));
        assert_eq!(liste[0].album_title.as_deref(), Some("Dummy"));
        assert!(liste[0].resolved);
        assert!(repo.list_banned_tracks(2).unwrap().is_empty());

        // Même table, deux types : masquer l'album d'id `t1` (s'il existait)
        // ne bannit pas la piste, et bannir la piste ne masque aucun album.
        assert!(!repo.is_album_hidden(t1).unwrap());
        assert!(repo.list_hidden_albums().unwrap().is_empty());

        assert!(repo.unban_track(1, t1).unwrap());
        assert!(!repo.unban_track(1, t1).unwrap(), "plus rien à débannir");
        assert!(!repo.is_track_banned(1, t1).unwrap());
    }

    #[test]
    fn bannir_un_id_fantome_est_refuse() {
        let db = test_db();
        let repo = HiddenRepo::with_backend(db);
        assert!(!repo.ban_track(1, 4242).unwrap());
        assert!(repo.list_banned_tracks(1).unwrap().is_empty());
    }

    /// #4806 suite — un titre de SERVICE, désigné par sa paire : bannir,
    /// normaliser la provenance, lister avec l'instantané, par profil,
    /// débannir ; et jamais de confusion avec l'id local de même valeur.
    #[test]
    fn bannir_un_titre_de_service_par_sa_paire() {
        let db = test_db();
        let repo = HiddenRepo::with_backend(db.clone());
        let ar = insert_artist(&db, "Alice Coltrane");
        let al = insert_album(&db, "Journey", ar);
        let locale = insert_track(&db, "Shiva-Loka", ar, al);
        let homonyme = locale.to_string();
        let titre = |id: &str| TitreDeService {
            source: " Qobuz ".into(),
            source_id: id.into(),
            title: Some(format!("Titre {id}")),
            artist: Some("Alice Coltrane".into()),
            album: Some("Journey".into()),
            album_source_id: Some("alb-9".into()),
            cover_url: Some("https://exemple.invalid/c.jpg".into()),
        };

        assert!(
            !repo
                .is_streaming_track_banned(1, "qobuz", &homonyme)
                .unwrap()
        );
        assert!(repo.ban_streaming_track(1, &titre(&homonyme)).unwrap());
        assert!(
            repo.ban_streaming_track(1, &titre(&homonyme)).unwrap(),
            "idempotent"
        );
        assert!(
            repo.is_streaming_track_banned(1, "QOBUZ", &homonyme)
                .unwrap()
        );
        assert!(
            !repo
                .is_streaming_track_banned(1, "tidal", &homonyme)
                .unwrap()
        );
        assert!(
            !repo
                .is_streaming_track_banned(2, "qobuz", &homonyme)
                .unwrap()
        );
        // Deux espaces : le titre Qobuz « <id> » ne bannit pas la piste <id>.
        assert!(!repo.is_track_banned(1, locale).unwrap());
        assert!(!repo.ligne_bannie(1, Some(locale), Some("qobuz"), Some(&homonyme)));
        assert!(repo.ligne_bannie(1, None, Some("Qobuz"), Some(&homonyme)));

        let cles = repo.banned_streaming_keys(1).unwrap();
        assert!(cles.contains(&("qobuz".to_string(), homonyme.clone())));
        assert_eq!(
            repo.banned_streaming_ids_for_source(1, "Qobuz")
                .unwrap()
                .len(),
            1
        );
        assert!(
            repo.banned_streaming_ids_for_source(1, "tidal")
                .unwrap()
                .is_empty()
        );

        let liste = repo.list_banned_tracks(1).unwrap();
        assert_eq!(liste.len(), 1, "une seule ligne malgré deux bannissements");
        assert_eq!(liste[0].track_id, None);
        assert_eq!(liste[0].source.as_deref(), Some("qobuz"));
        assert_eq!(liste[0].source_id.as_deref(), Some(homonyme.as_str()));
        assert_eq!(liste[0].title, format!("Titre {homonyme}"));
        assert_eq!(liste[0].album_source_id.as_deref(), Some("alb-9"));
        assert_eq!(
            liste[0].cover_path.as_deref(),
            Some("https://exemple.invalid/c.jpg")
        );
        assert!(repo.list_banned_tracks(2).unwrap().is_empty());

        // Désignation incomplète : rien n'est écrit.
        assert!(
            !repo
                .ban_streaming_track(
                    1,
                    &TitreDeService {
                        source: "qobuz".into(),
                        source_id: "  ".into(),
                        ..Default::default()
                    }
                )
                .unwrap()
        );

        assert!(repo.unban_streaming_track(1, "QOBUZ", &homonyme).unwrap());
        assert!(!repo.unban_streaming_track(1, "qobuz", &homonyme).unwrap());
        assert!(repo.list_banned_tracks(1).unwrap().is_empty());
    }

    /// Le prédicat SQL des favoris de service, sur une vraie base : il écarte
    /// la paire bannie (provenance écrite en majuscules dans le favori), et
    /// seulement pour le bon profil.
    #[test]
    fn le_predicat_des_titres_de_service_bannis_filtre_en_sql() {
        let db = test_db();
        for (id, titre) in [("f1", "Un"), ("f2", "Deux")] {
            let params: [&dyn ToSqlValue; 2] = [&id, &titre];
            db.execute(
                "INSERT INTO streaming_favorites (profile_id, item_type, service, service_id, title) \
                 VALUES (1, 'track', 'Qobuz', ?, ?)",
                &params,
            )
            .unwrap();
        }
        let repo = HiddenRepo::with_backend(db.clone());
        let compte = |profil: i64| -> i64 {
            let sql = format!(
                "SELECT COUNT(*) FROM streaming_favorites sf WHERE {}",
                crate::db::facet_filter::banned_streaming_excluded(
                    profil,
                    "sf.service",
                    "sf.service_id"
                )
            );
            db.query_one(&sql, &[]).unwrap().unwrap()[0]
                .as_i64()
                .unwrap()
        };
        assert_eq!(compte(1), 2, "témoin");
        repo.ban_streaming_track(
            1,
            &TitreDeService {
                source: "qobuz".into(),
                source_id: "f1".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(compte(1), 1);
        assert_eq!(compte(2), 2, "autre profil");
    }

    /// Le profil des sélections sans requête : le réglage `active_profile_id`,
    /// sinon 1.
    #[test]
    fn profil_de_selection_automatique_lit_le_reglage() {
        let db = test_db();
        assert_eq!(profil_de_selection_automatique(&db), 1);
        crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
            .set("active_profile_id", "3")
            .unwrap();
        assert_eq!(profil_de_selection_automatique(&db), 3);
    }
}
