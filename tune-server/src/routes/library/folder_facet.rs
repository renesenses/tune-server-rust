use crate::routes::panne_sql::OuDefautJournalise;
use axum::Json;
use axum::extract::{Query, RawQuery, State};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::backend::{SqlValue, ToSqlValue};
use tune_core::db::engine::Engine;
use tune_core::db::track_repo::folder_like_pattern;

use super::facets::{FacetQuery, build_conditions};
use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize)]
pub(super) struct FolderPathQuery {
    /// Absolute directory whose immediate sub-folders to list. Empty/absent →
    /// the configured library roots (top of the drill-down).
    path: Option<String>,
    /// Max child folders returned (default 1000; `<= 0` = no limit). Distinct
    /// from `FacetQuery::limit` (that one is per-facet and unused here).
    #[serde(rename = "folder_limit")]
    limit: Option<i64>,
}

/// GET /api/v1/library/folder-facet?path=<abs|empty>&<same filters as /library/tracks>
///
/// Hierarchical folder facet for the Oxygen view. Purely DB-driven (derived from
/// `tracks.file_path`) — no filesystem access, so it works for unmounted / NAS
/// libraries where `browse.rs` (which reads the disk) fails. Cumulative: every
/// other active facet (genre/year/…) narrows the child counts.
///
/// Response:
/// ```json
/// { "path": "<current abs dir|null>",
///   "crumbs": [ { "name": "Music", "path": "<abs>" }, … ],
///   "children": [ { "name": "...", "path": "<abs>", "count": 12, "has_children": true } ] }
/// ```
/// `path` is null and `crumbs` empty at the root level. `crumbs` runs from the
/// library root down to the current folder (each clickable). Selecting a child
/// means filtering `/library/tracks?folder=<child.path>` (recursive subtree).
pub(super) async fn folder_facet(
    Query(filters): Query<FacetQuery>,
    Query(p): Query<FolderPathQuery>,
    RawQuery(raw): RawQuery,
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    // Facettes multi-valeurs (#2168) : elles narrowent aussi les effectifs des
    // dossiers enfants.
    let filters = filters.hydrate(raw.as_deref())?;
    let engine = state.backend.engine();
    // Cumulative narrowing by the OTHER facets. exclude="folder" so the caller's
    // own folder selection isn't double-applied — this endpoint scopes by the
    // `path` prefix below instead. An active collection selection is resolved to
    // its member set — la MÊME résolution que la liste et que le rail (#1864),
    // collections intelligentes comprises — so it narrows the folder children too.
    let coll = filters
        .collection
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|name| super::facets::resolve_collection(&state, name));
    let (conds, params) = build_conditions(&filters, engine, "folder", coll.as_ref());

    let path = p
        .path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let limit: Option<i64> = match p.limit {
        Some(n) if n <= 0 => None,
        Some(n) => Some(n.clamp(1, 20000)),
        None => Some(1000),
    };

    match path {
        None => Ok(Json(folder_roots(&state, engine, &conds, &params))),
        Some(prefix) => Ok(Json(folder_children(
            &state, engine, &prefix, &conds, &params, limit,
        ))),
    }
}

/// Configured music directories (settings override → config fallback), same
/// source browse.rs uses so roots stay consistent between the two views.
fn music_dirs(state: &AppState) -> Vec<String> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    settings
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| state.config.music_dirs.clone())
}

/// Append the subtree-prefix predicate to the cumulative conditions and return
/// the full WHERE body plus the bound params (prefix param last, so positional
/// SQLite binding and `$n` Postgres numbering both line up).
fn where_with_prefix(
    engine: Engine,
    conds: &[String],
    params: &[SqlValue],
    like_pattern: &str,
) -> (String, Vec<SqlValue>) {
    let mut all: Vec<SqlValue> = params.to_vec();
    let like_ph = match engine {
        Engine::Sqlite => "?".to_string(),
        Engine::Postgres => format!("${}", all.len() + 1),
    };
    all.push(SqlValue::Text(like_pattern.to_string()));
    let mut parts: Vec<String> = conds.to_vec();
    parts.push(format!(
        "t.file_path LIKE {like_ph}{}",
        tune_core::db::track_repo::like_escape_clause()
    ));
    (parts.join(" AND "), all)
}

fn count_under(
    state: &AppState,
    engine: Engine,
    conds: &[String],
    params: &[SqlValue],
    pattern: &str,
) -> i64 {
    let (where_sql, all) = where_with_prefix(engine, conds, params, pattern);
    let sql = format!("SELECT COUNT(*) FROM tracks t WHERE {where_sql}");
    let refs: Vec<&dyn ToSqlValue> = all.iter().map(|v| v as &dyn ToSqlValue).collect();
    state
        .backend
        .query_one(&sql, &refs)
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
}

fn folder_roots(state: &AppState, engine: Engine, conds: &[String], params: &[SqlValue]) -> Value {
    let children: Vec<Value> = effective_roots(state)
        .into_iter()
        .map(|base| {
            let count = count_under(state, engine, conds, params, &folder_like_pattern(&base));
            let name = std::path::Path::new(&base)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&base)
                .to_string();
            json!({ "name": name, "path": base, "count": count, "has_children": true })
        })
        .filter(|c| c.get("count").and_then(|v| v.as_i64()).unwrap_or(0) > 0)
        .collect();
    json!({ "path": Value::Null, "crumbs": Value::Array(vec![]), "children": children })
}

/// The library roots to anchor the folder tree on: the configured `music_dirs`
/// that actually contain tracks; if none do, the real root derived from the
/// data. On some deployments `music_dirs` is stale — e.g. .18 has it set to
/// /mnt/music while files live under /data/music (the browse_root_zero_tracks
/// trap) — which would leave the facet empty. The data-derived fallback keeps it
/// working regardless of that config drift.
fn effective_roots(state: &AppState) -> Vec<String> {
    let engine = state.backend.engine();
    let mut roots: Vec<String> = music_dirs(state)
        .iter()
        .filter_map(|dir| {
            let base = tune_core::scanner::walker::normalize_path(dir)
                .trim_end_matches(['/', '\\'])
                .to_string();
            let has = count_under(state, engine, &[], &[], &folder_like_pattern(&base)) > 0;
            has.then_some(base)
        })
        .collect();
    if roots.is_empty() {
        // music_dirs stale/misconfigured → fall back to the real root derived
        // from the data (shared with browse.rs so both folder views agree).
        if let Some(r) = tune_core::db::track_repo::derive_common_root(state.backend.as_ref()) {
            roots.push(r);
        }
    }
    roots
}

/// Breadcrumb from the containing library root down to `base` (inclusive), each
/// entry an absolute path the client can drill straight to. Empty if `base`
/// isn't under any configured root (defensive — normally impossible).
fn build_crumbs(state: &AppState, base: &str, sep: char) -> Vec<Value> {
    let basename = |p: &str| {
        std::path::Path::new(p)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(p)
            .to_string()
    };
    // Anchor on whichever effective root contains `base` — the same root set
    // folder_roots exposes (config music_dirs, or the data-derived fallback), so
    // the breadcrumb stays consistent with the drill-down even when music_dirs is
    // stale.
    let root = effective_roots(state).into_iter().find(|r| {
        // `base` is derived from stored file paths; match the root as a path
        // prefix (either equal, or followed by a separator).
        base == r || base.starts_with(&format!("{r}{sep}"))
    });
    let Some(root) = root else {
        return vec![json!({ "name": basename(base), "path": base })];
    };
    let mut crumbs = vec![json!({ "name": basename(&root), "path": root.clone() })];
    let rel = base[root.len()..].trim_start_matches(['/', '\\']);
    let mut acc = root;
    for seg in rel.split(sep).filter(|s| !s.is_empty()) {
        acc = format!("{acc}{sep}{seg}");
        crumbs.push(json!({ "name": seg, "path": acc }));
    }
    crumbs
}

/// Given a track's `file_path`, skip the first `plen` prefix characters (the
/// current folder + separator) and return its immediate child folder segment,
/// plus whether a further separator exists beyond it (the child has sub-folders).
/// Returns `None` when the path has no segment past the prefix, or the remainder
/// is a file sitting directly in the folder (no further separator).
fn split_child(fp: &str, plen: usize, sep: char) -> Option<(&str, bool)> {
    let (start, _) = fp.char_indices().nth(plen)?;
    let rest = &fp[start..];
    let end = rest.find(sep)?; // no further separator → direct file, not a sub-folder
    let child = &rest[..end];
    if child.is_empty() {
        return None;
    }
    let deeper = rest[end + sep.len_utf8()..].contains(sep);
    Some((child, deeper))
}

fn folder_children(
    state: &AppState,
    engine: Engine,
    prefix: &str,
    conds: &[String],
    params: &[SqlValue],
    limit: Option<i64>,
) -> Value {
    let sep = std::path::MAIN_SEPARATOR;
    let base = prefix.trim_end_matches(['/', '\\']).to_string();
    let prefix_with_sep = format!("{base}{sep}");
    let plen = prefix_with_sep.chars().count();

    // Fetch only the file paths in this subtree, narrowed by the active facets.
    let (where_sql, all) = where_with_prefix(engine, conds, params, &folder_like_pattern(&base));
    let sql = format!("SELECT t.file_path FROM tracks t WHERE {where_sql}");
    let refs: Vec<&dyn ToSqlValue> = all.iter().map(|v| v as &dyn ToSqlValue).collect();
    let rows = state.backend.query_many(&sql, &refs).ou_defaut_journalise();

    // Aggregate the immediate child segment (portable: no engine-specific SQL
    // string surgery, no case/normalization equality traps). A row with no
    // deeper separator is a file sitting directly in `base` → not a sub-folder.
    use std::collections::HashMap;
    let mut counts: HashMap<String, i64> = HashMap::new();
    let mut has_children: std::collections::HashSet<String> = std::collections::HashSet::new();
    for row in &rows {
        let Some(fp) = row.first().and_then(|v| v.as_string()) else {
            continue;
        };
        let Some((child, deeper)) = split_child(&fp, plen, sep) else {
            continue;
        };
        *counts.entry(child.to_string()).or_insert(0) += 1;
        if deeper {
            has_children.insert(child.to_string());
        }
    }

    let mut entries: Vec<(String, i64)> = counts.into_iter().collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if let Some(n) = limit {
        entries.truncate(n as usize);
    }
    let children: Vec<Value> = entries
        .into_iter()
        .map(|(name, count)| {
            let full = format!("{prefix_with_sep}{name}");
            let drillable = has_children.contains(&name);
            json!({ "name": name, "path": full, "count": count, "has_children": drillable })
        })
        .collect();

    let crumbs = build_crumbs(state, &base, sep);
    json!({ "path": base, "crumbs": crumbs, "children": children })
}

#[cfg(test)]
mod tests {
    use super::split_child;

    // plen = number of characters in "<folder><sep>".
    const SEP: char = '/';

    #[test]
    fn immediate_subfolder_with_deeper_nesting() {
        // prefix "/music/" (7 chars) → child "Jazz", which has sub-folders.
        let (child, deeper) = split_child("/music/Jazz/Miles/kind.flac", 7, SEP).unwrap();
        assert_eq!(child, "Jazz");
        assert!(deeper);
    }

    #[test]
    fn leaf_folder_no_deeper_nesting() {
        // prefix "/music/Jazz/" (12 chars) → child "Miles", file sits one level in.
        let (child, deeper) = split_child("/music/Jazz/Miles/kind.flac", 12, SEP).unwrap();
        assert_eq!(child, "Miles");
        assert!(!deeper);
    }

    #[test]
    fn direct_file_is_not_a_child() {
        // prefix "/music/Jazz/Miles/" (18 chars) → the file itself, no sub-folder.
        assert!(split_child("/music/Jazz/Miles/kind.flac", 18, SEP).is_none());
    }

    #[test]
    fn respects_multibyte_prefix_length() {
        // "/musiqué/" = 9 characters (é is one char, two bytes). Child must be
        // extracted by char count, not byte offset.
        let (child, _) = split_child("/musiqué/Éléa/track.flac", 9, SEP).unwrap();
        assert_eq!(child, "Éléa");
    }
}
