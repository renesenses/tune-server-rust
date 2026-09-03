//! Critères « référence » des smart collections / smart playlists :
//! appartenance à une collection (classique ou smart), à une playlist
//! (classique ou smart) et statut favori (piste / album / artiste).
//!
//! Modèle de règle (JSON, stocké dans la colonne `rules`) :
//!
//! ```json
//! {"field": "in_collection", "op": "in",     "value": "classic:12"}
//! {"field": "in_collection", "op": "not_in", "value": "smart:3"}
//! {"field": "in_playlist",   "op": "in",     "value": "smart:7"}
//! {"field": "favorite",      "op": "is",     "value": "track"}
//! {"field": "favorite",      "op": "is_not", "value": "artist"}
//! ```
//!
//! - `value` d'une référence : `classic:<id>` (collection/playlist classique),
//!   `smart:<id>` (smart collection/playlist) ; un entier nu est toléré et
//!   traité comme `classic:<id>`.
//! - `favorite` s'évalue sur le profil actif de la requête (header
//!   `X-Profile-Id`, sinon le profil actif global, sinon le profil 1 — voir
//!   [`tune_http_types::ActiveProfile`]). Sans profil résolu
//!   (`profile_id = None`, jamais le cas via l'extracteur), le critère ne
//!   matche RIEN — y compris `is_not` : sans profil le statut favori est
//!   inconnu, on échoue fermé.
//! - Une smart collection/playlist peut en référencer une autre : la
//!   détection de cycle refuse l'enregistrement (`check_no_cycle`), et une
//!   garde de profondeur ([`MAX_REF_DEPTH`]) borne l'évaluation en défense.
//! - L'appartenance à une entité smart ignore son tri et sa limite
//!   (`max_limit`/`max_tracks`) : on considère l'adhésion complète, comme le
//!   compteur de la liste des smart collections.

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::Value;
use tune_core::db::backend::{DbBackend, ToSqlValue};

/// Profondeur maximale de références smart imbriquées à l'évaluation.
pub const MAX_REF_DEPTH: u32 = 10;

/// Champs de règle traités par ce module.
pub fn is_ref_field(field: &str) -> bool {
    matches!(field, "in_collection" | "in_playlist" | "favorite")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefKind {
    SmartCollection,
    SmartPlaylist,
}

/// Une entité smart chargée pour l'évaluation ou la détection de cycle.
pub struct SmartEntity {
    pub rules_json: String,
    pub match_mode: String,
    pub name: String,
}

/// Accès aux entités référencées. Abstrait pour les tests unitaires.
pub trait RefResolver {
    fn smart_entity(&self, kind: RefKind, id: i64) -> Option<SmartEntity>;
    /// Albums d'une collection classique (blob `collections` des settings).
    fn collection_album_ids(&self, id: i64) -> Option<Vec<i64>>;
}

/// Résolveur vide : aucune entité. Pour les chemins sans résolution
/// (et les tests des règles non-référentielles).
pub struct EmptyResolver;

impl RefResolver for EmptyResolver {
    fn smart_entity(&self, _kind: RefKind, _id: i64) -> Option<SmartEntity> {
        None
    }
    fn collection_album_ids(&self, _id: i64) -> Option<Vec<i64>> {
        None
    }
}

/// Résolveur branché sur la base (SQLite comme Postgres : `$1` est compris
/// par les deux backends).
pub struct DbRefResolver<'a> {
    backend: &'a Arc<dyn DbBackend>,
}

impl<'a> DbRefResolver<'a> {
    pub fn new(backend: &'a Arc<dyn DbBackend>) -> Self {
        Self { backend }
    }
}

impl RefResolver for DbRefResolver<'_> {
    fn smart_entity(&self, kind: RefKind, id: i64) -> Option<SmartEntity> {
        let sql = match kind {
            RefKind::SmartCollection => {
                "SELECT rules, match_mode, name FROM smart_collections WHERE id = $1"
            }
            RefKind::SmartPlaylist => {
                "SELECT rules, match_mode, name FROM smart_playlists WHERE id = $1"
            }
        };
        let row = self
            .backend
            .query_one(sql, &[&id as &dyn ToSqlValue])
            .ok()
            .flatten()?;
        Some(SmartEntity {
            rules_json: row
                .first()
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "[]".into()),
            // Tolère la forme JSON-encodée héritée (`"all"` avec guillemets).
            match_mode: row
                .get(1)
                .and_then(|v| v.as_string())
                .map(|s| s.trim_matches('"').to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "all".into()),
            name: row
                .get(2)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| format!("#{id}")),
        })
    }

    fn collection_album_ids(&self, id: i64) -> Option<Vec<i64>> {
        let raw = tune_core::db::settings_repo::SettingsRepo::with_backend(self.backend.clone())
            .get("collections")
            .ok()
            .flatten()?;
        let collections: Vec<Value> = serde_json::from_str(&raw).ok()?;
        let found = collections
            .iter()
            .find(|c| c.get("id").and_then(|v| v.as_i64()) == Some(id))?;
        Some(
            found
                .get("album_ids")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
                .unwrap_or_default(),
        )
    }
}

/// Contexte d'évaluation : résolveur + profil actif + profondeur courante.
pub struct RefCtx<'a> {
    pub resolver: &'a dyn RefResolver,
    pub profile_id: Option<i64>,
    pub depth: u32,
}

impl<'a> RefCtx<'a> {
    pub fn root(resolver: &'a dyn RefResolver, profile_id: Option<i64>) -> Self {
        Self {
            resolver,
            profile_id,
            depth: 0,
        }
    }

    fn child(&self) -> RefCtx<'a> {
        RefCtx {
            resolver: self.resolver,
            profile_id: self.profile_id,
            depth: self.depth + 1,
        }
    }
}

/// `classic:12` / `smart:3` / `12` → (is_smart, id).
fn parse_ref_value(value: &str) -> Option<(bool, i64)> {
    let v = value.trim();
    if let Some(rest) = v.strip_prefix("smart:") {
        return rest.trim().parse().ok().map(|id| (true, id));
    }
    if let Some(rest) = v.strip_prefix("classic:") {
        return rest.trim().parse().ok().map(|id| (false, id));
    }
    v.parse().ok().map(|id| (false, id))
}

fn is_negated(op: &str) -> bool {
    matches!(op, "not_in" | "is_not" | "!=" | "neq" | "ne" | "not_equals")
}

/// Condition d'appartenance `col IN (sub)`, avec négation et gestion des
/// colonnes NULLables (un `col NOT IN` sur colonne NULL exclurait la ligne ;
/// « PAS dans X » doit inclure les lignes sans album/artiste).
fn membership(col: &str, sub: &str, neg: bool, nullable_col: bool) -> String {
    if !neg {
        format!("{col} IN ({sub})")
    } else if nullable_col {
        format!("({col} IS NULL OR {col} NOT IN ({sub}))")
    } else {
        format!("{col} NOT IN ({sub})")
    }
}

/// Pour une règle négée, la référence introuvable (entité supprimée, valeur
/// invalide) matche tout ; sinon rien. La garde de profondeur échoue fermé
/// dans les deux cas (`1=0`).
fn degenerate(neg: bool) -> String {
    if neg { "1=1".into() } else { "1=0".into() }
}

/// Sous-requête listant les ids (albums ou pistes) d'une entité smart,
/// construite récursivement avec la garde de profondeur.
fn nested_smart_ids_sql(kind: RefKind, id: i64, ctx: &RefCtx) -> Result<Option<String>, ()> {
    if ctx.depth + 1 > MAX_REF_DEPTH {
        tracing::warn!(
            depth = ctx.depth,
            "smart_refs: profondeur maximale atteinte, référence ignorée (cycle ?)"
        );
        return Err(());
    }
    let Some(entity) = ctx.resolver.smart_entity(kind, id) else {
        return Ok(None);
    };
    let child = ctx.child();
    let sql = match kind {
        RefKind::SmartCollection => {
            let (where_clause, _order, _limit) = crate::smart_collections::build_album_query(
                &entity.rules_json,
                &entity.match_mode,
                "title",
                "asc",
                None,
                &child,
            );
            format!(
                "SELECT DISTINCT al.id FROM albums al \
                 LEFT JOIN artists ar ON al.artist_id = ar.id \
                 LEFT JOIN tracks t ON t.album_id = al.id {where_clause}"
            )
        }
        RefKind::SmartPlaylist => {
            let (where_clause, _order, _limit) = crate::smart_playlists::build_smart_query(
                &entity.rules_json,
                &entity.match_mode,
                "title",
                "asc",
                None,
                &child,
            );
            format!(
                "SELECT DISTINCT t.id FROM tracks t \
                 LEFT JOIN albums al ON t.album_id = al.id \
                 LEFT JOIN artists ar ON t.artist_id = ar.id {where_clause}"
            )
        }
    };
    Ok(Some(sql))
}

fn favorites_sub(profile_id: i64, item_type: &str) -> String {
    format!(
        "SELECT item_id FROM favorites WHERE profile_id = {profile_id} \
         AND item_type = '{item_type}'"
    )
}

/// Sous-requête des PISTES favorites d'un profil : favoris locaux (table
/// `favorites`) + pistes locales correspondant à un favori STREAMING du même
/// profil (titre + artiste normalisés). Un favori Qobuz/Tidal dont on possède
/// la copie locale compte ainsi comme favori dans les règles — avant, seule
/// la table locale était vue (point 6, revue 2026-08-15). Le rapprochement
/// est volontairement exact-normalisé (lower/trim) : en SQL portable
/// SQLite/PG, pas de fuzzy — un titre orthographié différemment ne matche
/// pas, c'est assumé.
fn track_favorites_sub(profile_id: i64) -> String {
    format!(
        "{local} UNION SELECT t9.id FROM tracks t9 \
         LEFT JOIN artists ar9 ON t9.artist_id = ar9.id \
         JOIN streaming_favorites sf9 ON sf9.profile_id = {profile_id} \
         AND sf9.item_type = 'track' \
         AND sf9.title IS NOT NULL \
         AND lower(trim(t9.title)) = lower(trim(sf9.title)) \
         AND lower(trim(coalesce(ar9.name, ''))) = lower(trim(coalesce(sf9.artist, '')))",
        local = favorites_sub(profile_id, "track")
    )
}

/// Condition SQL pour une règle référence au niveau ALBUM (smart collections).
/// Requêtes basées sur les alias `al` (albums), `ar` (artists), `t` (tracks)
/// de `build_album_query`.
pub(crate) fn album_ref_condition(field: &str, op: &str, value: &str, ctx: &RefCtx) -> String {
    let neg = is_negated(op);
    match field {
        "favorite" => {
            // Sans profil résolu, statut favori inconnu → aucun résultat,
            // même pour `is_not` (échec fermé, documenté).
            let Some(pid) = ctx.profile_id else {
                return "1=0".into();
            };
            match value {
                // L'album contient au moins une piste favorite (locale ou
                // correspondant à un favori streaming).
                "track" => {
                    let fav = track_favorites_sub(pid);
                    let sub = format!(
                        "SELECT DISTINCT t2.album_id FROM tracks t2 \
                         WHERE t2.album_id IS NOT NULL AND t2.id IN ({fav})"
                    );
                    membership("al.id", &sub, neg, false)
                }
                "album" => membership("al.id", &favorites_sub(pid, "album"), neg, false),
                // L'artiste de l'album est favori. `ar.id` est NULLable
                // (LEFT JOIN) : « PAS artiste favori » inclut les albums
                // sans artiste.
                "artist" => membership("ar.id", &favorites_sub(pid, "artist"), neg, true),
                _ => "1=0".into(),
            }
        }
        "in_collection" => {
            let Some((is_smart, id)) = parse_ref_value(value) else {
                return degenerate(neg);
            };
            if is_smart {
                match nested_smart_ids_sql(RefKind::SmartCollection, id, ctx) {
                    Ok(Some(sub)) => membership("al.id", &sub, neg, false),
                    Ok(None) => degenerate(neg),
                    Err(()) => "1=0".into(),
                }
            } else {
                match ctx.resolver.collection_album_ids(id) {
                    Some(ids) if !ids.is_empty() => {
                        let list = ids
                            .iter()
                            .map(|i| i.to_string())
                            .collect::<Vec<_>>()
                            .join(",");
                        membership("al.id", &list, neg, false)
                    }
                    _ => degenerate(neg),
                }
            }
        }
        "in_playlist" => {
            let Some((is_smart, id)) = parse_ref_value(value) else {
                return degenerate(neg);
            };
            if is_smart {
                // L'album a au moins une piste dans la smart playlist.
                match nested_smart_ids_sql(RefKind::SmartPlaylist, id, ctx) {
                    Ok(Some(track_sql)) => {
                        let sub = format!(
                            "SELECT DISTINCT t2.album_id FROM tracks t2 \
                             WHERE t2.album_id IS NOT NULL AND t2.id IN ({track_sql})"
                        );
                        membership("al.id", &sub, neg, false)
                    }
                    Ok(None) => degenerate(neg),
                    Err(()) => "1=0".into(),
                }
            } else {
                // L'album a au moins une piste dans la playlist classique.
                let sub = format!(
                    "SELECT DISTINCT t2.album_id FROM tracks t2 \
                     JOIN playlist_tracks pt ON pt.track_id = t2.id \
                     WHERE pt.playlist_id = {id} AND t2.album_id IS NOT NULL"
                );
                membership("al.id", &sub, neg, false)
            }
        }
        _ => degenerate(neg),
    }
}

/// Condition SQL pour une règle référence au niveau PISTE (smart playlists).
/// Alias `t` (tracks), `al` (albums), `ar` (artists) de `build_smart_query`.
pub(crate) fn track_ref_condition(field: &str, op: &str, value: &str, ctx: &RefCtx) -> String {
    let neg = is_negated(op);
    match field {
        "favorite" => {
            let Some(pid) = ctx.profile_id else {
                return "1=0".into();
            };
            match value {
                "track" => membership("t.id", &track_favorites_sub(pid), neg, false),
                "album" => membership("t.album_id", &favorites_sub(pid, "album"), neg, true),
                "artist" => membership("t.artist_id", &favorites_sub(pid, "artist"), neg, true),
                _ => "1=0".into(),
            }
        }
        "in_collection" => {
            let Some((is_smart, id)) = parse_ref_value(value) else {
                return degenerate(neg);
            };
            if is_smart {
                // La piste appartient à un album de la smart collection.
                match nested_smart_ids_sql(RefKind::SmartCollection, id, ctx) {
                    Ok(Some(sub)) => membership("t.album_id", &sub, neg, true),
                    Ok(None) => degenerate(neg),
                    Err(()) => "1=0".into(),
                }
            } else {
                match ctx.resolver.collection_album_ids(id) {
                    Some(ids) if !ids.is_empty() => {
                        let list = ids
                            .iter()
                            .map(|i| i.to_string())
                            .collect::<Vec<_>>()
                            .join(",");
                        membership("t.album_id", &list, neg, true)
                    }
                    _ => degenerate(neg),
                }
            }
        }
        "in_playlist" => {
            let Some((is_smart, id)) = parse_ref_value(value) else {
                return degenerate(neg);
            };
            if is_smart {
                match nested_smart_ids_sql(RefKind::SmartPlaylist, id, ctx) {
                    Ok(Some(sub)) => membership("t.id", &sub, neg, false),
                    Ok(None) => degenerate(neg),
                    Err(()) => "1=0".into(),
                }
            } else {
                let sub = format!("SELECT track_id FROM playlist_tracks WHERE playlist_id = {id}");
                membership("t.id", &sub, neg, false)
            }
        }
        _ => degenerate(neg),
    }
}

/// Références SMART (les seules pouvant créer un cycle) contenues dans un
/// jeu de règles.
fn referenced_smart_refs(rules_json: &str) -> Vec<(RefKind, i64)> {
    let rules: Vec<Value> = serde_json::from_str(rules_json).unwrap_or_default();
    rules
        .iter()
        .filter_map(|rule| {
            let field = rule.get("field")?.as_str()?;
            let kind = match field {
                "in_collection" => RefKind::SmartCollection,
                "in_playlist" => RefKind::SmartPlaylist,
                _ => return None,
            };
            let value = rule.get("value")?.as_str()?;
            match parse_ref_value(value) {
                Some((true, id)) => Some((kind, id)),
                _ => None,
            }
        })
        .collect()
}

/// Détection de cycle à l'enregistrement d'une smart collection/playlist.
///
/// `self_id` est `None` à la création (l'entité ne peut alors pas faire
/// partie d'un cycle, mais on refuse quand même un cycle préexistant
/// atteignable depuis les nouvelles règles). Retourne un message d'erreur
/// prêt à afficher, ex. :
/// `Référence circulaire : « A » ⊂ « B » ⊂ « A » — enregistrement refusé`.
pub fn check_no_cycle(
    resolver: &dyn RefResolver,
    self_kind: RefKind,
    self_id: Option<i64>,
    self_name: &str,
    rules_json: &str,
) -> Result<(), String> {
    fn dfs(
        resolver: &dyn RefResolver,
        node: (RefKind, i64),
        path: &mut Vec<(RefKind, i64, String)>,
        done: &mut HashSet<(RefKind, i64)>,
    ) -> Result<(), String> {
        if let Some(pos) = path.iter().position(|(k, i, _)| (*k, *i) == node) {
            let mut names: Vec<String> = path[pos..]
                .iter()
                .map(|(_, _, n)| format!("« {n} »"))
                .collect();
            names.push(format!("« {} »", path[pos].2));
            return Err(format!(
                "Référence circulaire : {} — enregistrement refusé",
                names.join(" ⊂ ")
            ));
        }
        if done.contains(&node) {
            return Ok(());
        }
        let Some(entity) = resolver.smart_entity(node.0, node.1) else {
            // Référence morte : pas de cycle possible par ici.
            done.insert(node);
            return Ok(());
        };
        path.push((node.0, node.1, entity.name.clone()));
        for next in referenced_smart_refs(&entity.rules_json) {
            dfs(resolver, next, path, done)?;
        }
        path.pop();
        done.insert(node);
        Ok(())
    }

    // Sentinelle -1 à la création : jamais égale à un id existant.
    let mut path = vec![(self_kind, self_id.unwrap_or(-1), self_name.to_string())];
    let mut done = HashSet::new();
    for next in referenced_smart_refs(rules_json) {
        dfs(resolver, next, &mut path, &mut done)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct FakeResolver {
        entities: HashMap<(RefKind, i64), (String, String, String)>, // rules, mode, name
        collections: HashMap<i64, Vec<i64>>,
    }

    impl FakeResolver {
        fn new() -> Self {
            Self {
                entities: HashMap::new(),
                collections: HashMap::new(),
            }
        }
        fn with_entity(mut self, kind: RefKind, id: i64, name: &str, rules: &str) -> Self {
            self.entities
                .insert((kind, id), (rules.to_string(), "all".into(), name.into()));
            self
        }
        fn with_collection(mut self, id: i64, albums: Vec<i64>) -> Self {
            self.collections.insert(id, albums);
            self
        }
    }

    impl RefResolver for FakeResolver {
        fn smart_entity(&self, kind: RefKind, id: i64) -> Option<SmartEntity> {
            self.entities
                .get(&(kind, id))
                .map(|(rules, mode, name)| SmartEntity {
                    rules_json: rules.clone(),
                    match_mode: mode.clone(),
                    name: name.clone(),
                })
        }
        fn collection_album_ids(&self, id: i64) -> Option<Vec<i64>> {
            self.collections.get(&id).cloned()
        }
    }

    // ---- conditions niveau album -------------------------------------------

    #[test]
    fn album_favorite_album_uses_profile() {
        let r = EmptyResolver;
        let ctx = RefCtx::root(&r, Some(7));
        let cond = album_ref_condition("favorite", "is", "album", &ctx);
        assert!(cond.contains("al.id IN"), "{cond}");
        assert!(cond.contains("profile_id = 7"), "{cond}");
        assert!(cond.contains("item_type = 'album'"), "{cond}");
    }

    #[test]
    fn album_favorite_track_means_at_least_one_favorite_track() {
        let r = EmptyResolver;
        let ctx = RefCtx::root(&r, Some(1));
        let cond = album_ref_condition("favorite", "is", "track", &ctx);
        assert!(cond.contains("t2.album_id IS NOT NULL"), "{cond}");
        assert!(cond.contains("item_type = 'track'"), "{cond}");
    }

    #[test]
    fn album_favorite_without_profile_matches_nothing_even_negated() {
        let r = EmptyResolver;
        let ctx = RefCtx::root(&r, None);
        assert_eq!(album_ref_condition("favorite", "is", "track", &ctx), "1=0");
        assert_eq!(
            album_ref_condition("favorite", "is_not", "track", &ctx),
            "1=0"
        );
    }

    #[test]
    fn album_in_classic_collection_inlines_ids() {
        let r = FakeResolver::new().with_collection(12, vec![4, 8, 15]);
        let ctx = RefCtx::root(&r, Some(1));
        let cond = album_ref_condition("in_collection", "in", "classic:12", &ctx);
        assert_eq!(cond, "al.id IN (4,8,15)");
        let neg = album_ref_condition("in_collection", "not_in", "classic:12", &ctx);
        assert_eq!(neg, "al.id NOT IN (4,8,15)");
    }

    #[test]
    fn album_in_missing_collection_fails_closed_open() {
        let r = FakeResolver::new();
        let ctx = RefCtx::root(&r, Some(1));
        // « dans X » avec X inexistante → rien ; « pas dans X » → tout.
        assert_eq!(
            album_ref_condition("in_collection", "in", "classic:99", &ctx),
            "1=0"
        );
        assert_eq!(
            album_ref_condition("in_collection", "not_in", "classic:99", &ctx),
            "1=1"
        );
    }

    #[test]
    fn album_in_classic_playlist_via_tracks() {
        let r = EmptyResolver;
        let ctx = RefCtx::root(&r, Some(1));
        let cond = album_ref_condition("in_playlist", "in", "classic:5", &ctx);
        assert!(cond.contains("playlist_tracks"), "{cond}");
        assert!(cond.contains("pt.playlist_id = 5"), "{cond}");
        assert!(cond.contains("t2.album_id IS NOT NULL"), "{cond}");
    }

    #[test]
    fn album_in_smart_playlist_nests_track_query() {
        let r = FakeResolver::new().with_entity(
            RefKind::SmartPlaylist,
            7,
            "Jazz doux",
            r#"[{"field":"genre","op":"contains","value":"Jazz"}]"#,
        );
        let ctx = RefCtx::root(&r, Some(1));
        let cond = album_ref_condition("in_playlist", "in", "smart:7", &ctx);
        assert!(cond.starts_with("al.id IN ("), "{cond}");
        assert!(
            cond.contains("SELECT DISTINCT t.id FROM tracks t"),
            "{cond}"
        );
        assert!(cond.contains("Jazz"), "{cond}");
    }

    // ---- conditions niveau piste -------------------------------------------

    #[test]
    fn track_favorite_track_direct() {
        let r = EmptyResolver;
        let ctx = RefCtx::root(&r, Some(3));
        let cond = track_ref_condition("favorite", "is", "track", &ctx);
        // Favoris locaux…
        assert!(
            cond.contains(
                "SELECT item_id FROM favorites WHERE profile_id = 3 AND item_type = 'track'"
            ),
            "{cond}"
        );
        // …UNION les pistes locales correspondant à un favori streaming du
        // même profil (titre+artiste normalisés).
        assert!(cond.contains("UNION"), "{cond}");
        assert!(
            cond.contains("streaming_favorites sf9 ON sf9.profile_id = 3"),
            "{cond}"
        );
        assert!(cond.starts_with("t.id IN ("), "{cond}");
    }

    #[test]
    fn album_favorite_track_includes_streaming_matches() {
        let r = EmptyResolver;
        let ctx = RefCtx::root(&r, Some(7));
        let cond = album_ref_condition("favorite", "is", "track", &ctx);
        assert!(
            cond.contains("streaming_favorites sf9 ON sf9.profile_id = 7"),
            "{cond}"
        );
    }

    #[test]
    fn track_favorite_artist_negation_keeps_null_artists() {
        let r = EmptyResolver;
        let ctx = RefCtx::root(&r, Some(3));
        let cond = track_ref_condition("favorite", "is_not", "artist", &ctx);
        assert!(cond.starts_with("(t.artist_id IS NULL OR"), "{cond}");
        assert!(cond.contains("NOT IN"), "{cond}");
    }

    #[test]
    fn track_in_smart_collection_nests_album_query() {
        let r = FakeResolver::new().with_entity(
            RefKind::SmartCollection,
            4,
            "Hi-Res",
            r#"[{"field":"sample_rate","op":">=","value":96000}]"#,
        );
        let ctx = RefCtx::root(&r, Some(1));
        let cond = track_ref_condition("in_collection", "in", "smart:4", &ctx);
        assert!(cond.starts_with("t.album_id IN ("), "{cond}");
        assert!(
            cond.contains("SELECT DISTINCT al.id FROM albums al"),
            "{cond}"
        );
    }

    #[test]
    fn track_in_classic_playlist_direct() {
        let r = EmptyResolver;
        let ctx = RefCtx::root(&r, Some(1));
        let cond = track_ref_condition("in_playlist", "in", "classic:9", &ctx);
        assert_eq!(
            cond,
            "t.id IN (SELECT track_id FROM playlist_tracks WHERE playlist_id = 9)"
        );
    }

    #[test]
    fn bare_numeric_value_is_classic() {
        let r = FakeResolver::new().with_collection(3, vec![1]);
        let ctx = RefCtx::root(&r, Some(1));
        assert_eq!(
            album_ref_condition("in_collection", "in", "3", &ctx),
            "al.id IN (1)"
        );
    }

    // ---- garde de profondeur ------------------------------------------------

    #[test]
    fn self_referencing_smart_playlist_terminates_and_fails_closed() {
        // Cycle direct (données héritées, hors validation) : l'évaluation doit
        // se terminer grâce à la garde de profondeur et échouer fermé.
        let r = FakeResolver::new().with_entity(
            RefKind::SmartPlaylist,
            1,
            "Boucle",
            r#"[{"field":"in_playlist","op":"in","value":"smart:1"}]"#,
        );
        let ctx = RefCtx::root(&r, Some(1));
        let cond = track_ref_condition("in_playlist", "in", "smart:1", &ctx);
        // Terminaison : la référence la plus profonde vaut 1=0.
        assert!(cond.contains("1=0"), "{cond}");
        // Profondeur bornée : pas plus de MAX_REF_DEPTH imbrications.
        assert!(
            cond.matches("SELECT DISTINCT t.id").count() <= MAX_REF_DEPTH as usize,
            "trop d'imbrications: {}",
            cond.matches("SELECT DISTINCT t.id").count()
        );
    }

    // ---- détection de cycle -------------------------------------------------

    #[test]
    fn direct_self_reference_rejected() {
        let r = FakeResolver::new().with_entity(RefKind::SmartCollection, 1, "A", "[]");
        let err = check_no_cycle(
            &r,
            RefKind::SmartCollection,
            Some(1),
            "A",
            r#"[{"field":"in_collection","op":"in","value":"smart:1"}]"#,
        )
        .unwrap_err();
        assert!(err.contains("Référence circulaire"), "{err}");
        assert!(err.contains("« A »"), "{err}");
    }

    #[test]
    fn two_step_cycle_rejected_with_names() {
        // B référence déjà A ; on tente d'enregistrer A ⊃ B → A ⊂ B ⊂ A.
        let r = FakeResolver::new()
            .with_entity(RefKind::SmartCollection, 1, "A", "[]")
            .with_entity(
                RefKind::SmartCollection,
                2,
                "B",
                r#"[{"field":"in_collection","op":"in","value":"smart:1"}]"#,
            );
        let err = check_no_cycle(
            &r,
            RefKind::SmartCollection,
            Some(1),
            "A",
            r#"[{"field":"in_collection","op":"in","value":"smart:2"}]"#,
        )
        .unwrap_err();
        assert!(err.contains("« A » ⊂ « B » ⊂ « A »"), "{err}");
    }

    #[test]
    fn cross_kind_cycle_rejected() {
        // Smart playlist P référence smart collection C ; enregistrer C ⊃ P
        // boucle à travers les deux familles.
        let r = FakeResolver::new()
            .with_entity(RefKind::SmartCollection, 1, "C", "[]")
            .with_entity(
                RefKind::SmartPlaylist,
                9,
                "P",
                r#"[{"field":"in_collection","op":"in","value":"smart:1"}]"#,
            );
        let err = check_no_cycle(
            &r,
            RefKind::SmartCollection,
            Some(1),
            "C",
            r#"[{"field":"in_playlist","op":"in","value":"smart:9"}]"#,
        )
        .unwrap_err();
        assert!(err.contains("Référence circulaire"), "{err}");
    }

    #[test]
    fn acyclic_references_accepted() {
        let r = FakeResolver::new()
            .with_entity(RefKind::SmartCollection, 2, "B", "[]")
            .with_entity(RefKind::SmartPlaylist, 3, "P", "[]");
        assert!(
            check_no_cycle(
                &r,
                RefKind::SmartCollection,
                Some(1),
                "A",
                r#"[{"field":"in_collection","op":"in","value":"smart:2"},
                    {"field":"in_playlist","op":"in","value":"smart:3"},
                    {"field":"in_collection","op":"in","value":"classic:2"}]"#,
            )
            .is_ok()
        );
    }

    #[test]
    fn creation_without_id_accepts_dag() {
        let r = FakeResolver::new().with_entity(RefKind::SmartPlaylist, 5, "X", "[]");
        assert!(
            check_no_cycle(
                &r,
                RefKind::SmartPlaylist,
                None,
                "Nouvelle",
                r#"[{"field":"in_playlist","op":"in","value":"smart:5"}]"#,
            )
            .is_ok()
        );
    }
}
