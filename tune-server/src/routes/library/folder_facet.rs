use axum::Json;
use axum::extract::{Query, RawQuery, State};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_http_types::panne_sql::OuDefautJournalise;

use tune_core::db::backend::{SqlValue, ToSqlValue};
use tune_core::db::engine::Engine;
use tune_core::db::track_repo::folder_like_pattern;

use std::collections::HashSet;

use super::facets::{FacetQuery, SocleResolu, build_conditions, hors_executeur};
use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize, Default)]
pub(super) struct FolderPathQuery {
    /// Absolute directory whose immediate sub-folders to list. Empty/absent →
    /// the configured library roots (top of the drill-down).
    pub(super) path: Option<String>,
    /// Max child folders returned (default 1000; `<= 0` = no limit). Distinct
    /// from `FacetQuery::limit` (that one is per-facet and unused here).
    #[serde(rename = "folder_limit")]
    pub(super) limit: Option<i64>,
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
    // #5438 — la résolution de la collection et les comptes par dossier sont
    // des lectures synchrones : hors de l'exécuteur, comme le rail.
    hors_executeur("folder_facet", move || {
        lire_les_dossiers(&state, filters, p)
    })
    .await
    .map(Json)
}

/// Le corps de `GET /library/folder-facet`, exécuté HORS de l'exécuteur.
pub(super) fn lire_les_dossiers(
    state: &AppState,
    filters: FacetQuery,
    p: FolderPathQuery,
) -> Value {
    lire_les_dossiers_avec(state, filters, p, None)
}

/// [`lire_les_dossiers`], le socle imposé — pour que les témoins de #5993
/// comparent le calcul sans sonde au socle posé en SQL.
pub(super) fn lire_les_dossiers_sur(
    state: &AppState,
    filters: FacetQuery,
    p: FolderPathQuery,
    socle: &SocleResolu,
) -> Value {
    lire_les_dossiers_avec(state, filters, p, Some(socle))
}

fn lire_les_dossiers_avec(
    state: &AppState,
    filters: FacetQuery,
    p: FolderPathQuery,
    impose: Option<&SocleResolu>,
) -> Value {
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
        .map(|name| super::facets::resolve_collection(state, name));
    // #5977 — le socle de la liste. #5993 — sans sonde par piste : on lit sur
    // le socle réduit aux albums masqués, puis l'on RETRANCHE les pistes
    // repliées. Un effectif de dossier est un nombre de pistes : la différence
    // est exacte. `conds_complets` (le socle posé en SQL) ne sert que si le
    // socle n'a pas pu être résolu.
    let (conds, params) = build_conditions(
        &filters,
        engine,
        "folder",
        coll.as_ref(),
        &SocleResolu::masques_seuls(),
    );
    let (conds_complets, _) = build_conditions(
        &filters,
        engine,
        "folder",
        coll.as_ref(),
        &SocleResolu::EnSql,
    );

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
        None => {
            let global;
            let socle = match impose {
                Some(s) => s,
                None => {
                    global = SocleResolu::resoudre(state);
                    &global
                }
            };
            match (socle.ids(), socle.liste_des_ecartees()) {
                (None, _) => folder_roots(state, engine, &conds_complets, None, &params),
                (Some(_), None) => folder_roots(state, engine, &conds, None, &params),
                (Some(_), Some(liste)) => {
                    let mut retrait = conds.clone();
                    retrait.push(format!("t.id IN ({liste})"));
                    folder_roots(state, engine, &conds, Some(&retrait), &params)
                }
            }
        }
        Some(prefix) => {
            // Les pistes repliées parmi les albums du dossier, connus une fois
            // ses pistes lues : pas de seconde lecture du dossier.
            let ecart = |albums: &[i64]| -> Option<HashSet<i64>> {
                match impose {
                    Some(s) => s.ids().map(|ids| ids.iter().copied().collect()),
                    None => SocleResolu::ecartees_parmi(state, albums),
                }
            };
            folder_children(
                state,
                engine,
                &prefix,
                (conds.as_slice(), conds_complets.as_slice()),
                &params,
                limit,
                &ecart,
            )
        }
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

fn folder_roots(
    state: &AppState,
    engine: Engine,
    conds: &[String],
    retrait: Option<&[String]>,
    params: &[SqlValue],
) -> Value {
    let children: Vec<Value> = effective_roots(state)
        .into_iter()
        .map(|base| {
            let motif = folder_like_pattern(&base);
            // #5993 — moins les pistes repliées du dossier.
            let count = count_under(state, engine, conds, params, &motif)
                - retrait.map_or(0, |r| count_under(state, engine, r, params, &motif));
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

/// Le préfixe sous LA FORME QUE PORTE LA BASE, et sa longueur en caractères.
///
/// 🔴 Les deux vont ensemble, et c'est leur désaccord qui a produit #5354.
///
/// macOS rend ses chemins DÉCOMPOSÉS (NFD) : dans « CDThèque », le `è` s'écrit
/// `e` + U+0300, soit neuf scalaires au lieu de huit. Le motif `LIKE` est bâti
/// en NFC par [`folder_like_pattern`](tune_core::db::track_repo::folder_like_pattern),
/// donc la recherche trouvait les bonnes lignes ; mais la longueur était
/// comptée sur la chaîne BRUTE, et [`split_child`] sautait un caractère de trop
/// PAR lettre accentuée. Yves Corbat voyait « arillion » au lieu de Marillion.
///
/// Normaliser ici aligne les QUATRE usages de `base` : la longueur du découpage,
/// le motif SQL, le `path` rendu au client, et l'ancrage du fil d'Ariane —
/// `build_crumbs` compare par `starts_with`, et échouait entre deux formes,
/// d'où un fil d'Ariane réduit à une seule miette.
fn prefixe_pour_decoupe(prefix: &str, sep: char) -> (String, String, usize) {
    use unicode_normalization::UnicodeNormalization as _;
    let base: String = prefix.trim_end_matches(['/', '\\']).nfc().collect();
    let prefix_with_sep = format!("{base}{sep}");
    let plen = prefix_with_sep.chars().count();
    (base, prefix_with_sep, plen)
}

fn folder_children(
    state: &AppState,
    engine: Engine,
    prefix: &str,
    (conds, conds_complets): (&[String], &[String]),
    params: &[SqlValue],
    limit: Option<i64>,
    ecart: &dyn Fn(&[i64]) -> Option<HashSet<i64>>,
) -> Value {
    let sep = std::path::MAIN_SEPARATOR;
    let (base, prefix_with_sep, plen) = prefixe_pour_decoupe(prefix, sep);

    // Fetch only the file paths in this subtree, narrowed by the active facets.
    let lire = |conds: &[String]| {
        let (where_sql, all) =
            where_with_prefix(engine, conds, params, &folder_like_pattern(&base));
        let sql = format!("SELECT t.id, t.album_id, t.file_path FROM tracks t WHERE {where_sql}");
        let refs: Vec<&dyn ToSqlValue> = all.iter().map(|v| v as &dyn ToSqlValue).collect();
        state.backend.query_many(&sql, &refs).ou_defaut_journalise()
    };
    let mut rows = lire(conds);
    // #5993 — les pistes repliées des albums de CE dossier, retranchées ici.
    let mut albums: Vec<i64> = rows
        .iter()
        .filter_map(|r| r.get(1).and_then(|v| v.as_i64()))
        .collect();
    albums.sort_unstable();
    albums.dedup();
    let ecartees = match ecart(&albums) {
        Some(e) => e,
        None => {
            rows = lire(conds_complets);
            HashSet::new()
        }
    };

    use std::collections::HashMap;
    let mut counts: HashMap<String, i64> = HashMap::new();
    let mut has_children: HashSet<String> = HashSet::new();
    for row in &rows {
        if row
            .first()
            .and_then(|v| v.as_i64())
            .is_some_and(|id| ecartees.contains(&id))
        {
            continue;
        }
        let Some(fp) = row.get(2).and_then(|v| v.as_string()) else {
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
    use super::{prefixe_pour_decoupe, split_child};

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

    /// 🔴 #5354 — LE PRÉFIXE DEMANDÉ N'EST PAS FORCÉMENT SOUS LA FORME DE LA BASE.
    ///
    /// Le témoin voisin (`respects_multibyte_prefix_length`) garde le découpage
    /// par CARACTÈRES et non par octets — c'était déjà juste. Il ne dit rien du
    /// cas où la requête arrive DÉCOMPOSÉE, qui est celui de tout client macOS.
    #[test]
    fn un_prefixe_nfd_decoupe_comme_un_prefixe_nfc() {
        // « /musiqué/ » sous ses deux formes : composée (9 caractères) et
        // décomposée (10 — le « é » y occupe deux scalaires).
        let nfc = "/musiqu\u{e9}/";
        let nfd = "/musique\u{301}/";
        assert_eq!(nfc.chars().count(), 9);
        assert_eq!(nfd.chars().count(), 10, "le NFD doit bien être plus long");

        // La base stocke du NFC — c'est la forme que `folder_like_pattern` cherche.
        let stocke = "/musiqu\u{e9}/\u{c9}l\u{e9}a/track.flac";

        let (_, _, plen_nfd) = prefixe_pour_decoupe(nfd.trim_end_matches('/'), SEP);
        let (child, _) = split_child(stocke, plen_nfd, SEP).expect("aucun enfant découpé");
        assert_eq!(
            child, "\u{c9}l\u{e9}a",
            "une requête NFD perd une lettre par accent : c'est le défaut d'Yves (#5354)"
        );

        // Et la forme composée donne exactement le même résultat.
        let (_, _, plen_nfc) = prefixe_pour_decoupe(nfc.trim_end_matches('/'), SEP);
        assert_eq!(
            plen_nfc, plen_nfd,
            "les deux formes doivent donner la MÊME longueur"
        );
    }

    /// Le `path` rendu au client et le motif SQL doivent parler la même langue :
    /// un chemin composite (préfixe NFD + nom NFC) ne se retrouve nulle part au
    /// forage suivant.
    #[test]
    fn le_prefixe_rendu_est_normalise() {
        let (base, avec_sep, _) = prefixe_pour_decoupe("/musique\u{301}", SEP);
        assert_eq!(
            base, "/musiqu\u{e9}",
            "le chemin rendu au client reste décomposé"
        );
        assert_eq!(avec_sep, "/musiqu\u{e9}/");
    }

    #[test]
    fn respects_multibyte_prefix_length() {
        // "/musiqué/" = 9 characters (é is one char, two bytes). Child must be
        // extracted by char count, not byte offset.
        let (child, _) = split_child("/musiqué/Éléa/track.flac", 9, SEP).unwrap();
        assert_eq!(child, "Éléa");
    }
}
