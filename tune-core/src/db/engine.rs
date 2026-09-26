//! Database engine abstraction.
//!
//! Phase 1 of the PostgreSQL support roadmap (see docs/POSTGRES-PLAN.md).
//!
//! This module defines the `Engine` enum and the `SqlDialect` trait used
//! by repos to emit engine-specific SQL fragments (placeholders, FTS
//! match clauses, JSON extraction). It is intentionally non-invasive:
//! existing repos continue to use `rusqlite::Connection` directly via
//! `SqliteDb::read` / `SqliteDb::write`; they will opt-in to the dialect
//! helpers as they are migrated repo-by-repo in subsequent phases.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Sqlite,
    Postgres,
}

impl Engine {
    pub fn as_str(&self) -> &'static str {
        match self {
            Engine::Sqlite => "sqlite",
            Engine::Postgres => "postgres",
        }
    }

    /// Parses an engine name. Accepts "sqlite", "postgres", "postgresql".
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "sqlite" => Some(Engine::Sqlite),
            "postgres" | "postgresql" => Some(Engine::Postgres),
            _ => None,
        }
    }

    /// Detects the engine from a connection string.
    ///
    /// - `postgresql://...` or `postgres://...` → Postgres
    /// - anything else (including bare paths and `sqlite://`) → SQLite
    pub fn from_connection_string(s: &str) -> Self {
        if s.starts_with("postgresql://") || s.starts_with("postgres://") {
            Engine::Postgres
        } else {
            Engine::Sqlite
        }
    }
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Strip diacritics from a string (case-preserving), so accent-insensitive
/// `LIKE` search matches `carlão` against `carlao`.
///
/// Decomposes to NFD and drops Unicode combining marks (U+0300–U+036F), which
/// turns Latin accented letters into their base ASCII letter (é→e, ã→a, ç→c,
/// É→E). Non-decomposable scripts (Cyrillic, CJK, …) pass through unchanged.
/// Case is intentionally preserved: callers wrap the result in SQL `LOWER()`,
/// and once diacritics are gone the remaining Latin text is ASCII, so SQLite's
/// ASCII-only `LOWER()` folds it correctly. This mirrors PostgreSQL's
/// `unaccent()` (which is likewise case-preserving); both engines therefore use
/// the identical SQL `LOWER(unaccent(col)) LIKE LOWER(unaccent(?))`.
pub fn fold_diacritics(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    s.nfd()
        .filter(|c| !matches!(*c, '\u{0300}'..='\u{036F}'))
        .collect()
}

/// Format a user-supplied search query for the engine's FTS dialect.
///
/// Punctuation is a **separator**, matching how the index was tokenised:
/// FTS5's `unicode61` and Postgres' `simple` dictionary both break on
/// non-alphanumerics, so `AC/DC` is stored as the two tokens `ac` and `dc`.
/// Searching for `ACDC` therefore cannot match it.
///
/// Where the query contains punctuation we emit *both* readings, OR'd:
/// split-on-punctuation for data that carries it, and glued for data that
/// does not (`rock'n'roll` typed against a stored `rocknroll`). Measured on a
/// 1080-artist MusicBrainz corpus, recall@1 on punctuation and diacritic
/// variants: glued-only 51.6%, split-only 65.6%, both 68.0% — and searching
/// an artist's *exact* name went from 47% to 100% on the 66 names containing
/// punctuation (`AC/DC`, `B.B. King`, `Camille Saint‐Saëns`,
/// `Римский‐Корсаков`…). Split-only slightly regressed the general-variant
/// bucket, hence the OR rather than a straight swap.
///
/// Per engine:
/// - SQLite FTS5: `term1 term2*`, or `(a b*) OR (ab*)`
/// - Postgres tsquery: `term1 & term2:*`, or `(a & b:*) | (ab:*)`
///
/// Returns an empty string if the input has no usable tokens, so the
/// caller can short-circuit to a LIKE-only path.
///
/// Des DOUBLES GUILLEMETS demandent une phrase exacte (Yves Corbat, point 8,
/// 17/09/2026) : `"kind of blue"` ne rend plus « Blue Kind Of… », ni les
/// titres qui portent les trois mots dans le désordre. Voir [`phrases_et_reste`].
pub fn format_fts_query(engine: Engine, raw: &str) -> String {
    if !raw.contains('"') {
        return format_fts_query_libre(engine, raw);
    }
    let (phrases, reste) = phrases_et_reste(raw);
    let mut parties: Vec<String> = phrases
        .iter()
        .filter_map(|p| format_fts_phrase(engine, p))
        .collect();
    let libre = format_fts_query_libre(engine, &reste);
    if !libre.is_empty() {
        parties.push(if libre.contains(" OR ") || libre.contains(" | ") {
            format!("({libre})")
        } else {
            libre
        });
    }
    match engine {
        Engine::Sqlite => parties.join(" "),
        Engine::Postgres => parties.join(" & "),
    }
}

/// Sépare les passages entre doubles guillemets du texte libre. Un guillemet
/// resté ouvert court jusqu'à la fin : on tape `"kind of` avant de fermer, la
/// recherche part à chaque frappe.
pub fn phrases_et_reste(raw: &str) -> (Vec<String>, String) {
    let mut phrases = Vec::new();
    let mut reste = String::new();
    for (i, morceau) in raw.split('"').enumerate() {
        if i % 2 == 1 {
            if !morceau.trim().is_empty() {
                phrases.push(morceau.trim().to_string());
            }
        } else {
            reste.push(' ');
            reste.push_str(morceau);
        }
    }
    (phrases, reste.trim().to_string())
}

/// Le motif `LIKE` d'une recherche : les guillemets ôtés. Laissés dans le
/// motif, ils ne correspondraient à aucun titre — `%"kind of blue"%`.
pub fn motif_like(raw: &str) -> String {
    format!("%{}%", raw.replace('"', "").trim())
}

/// Une phrase exacte : SQLite FTS5 `"a b c"`, PostgreSQL `a <-> b <-> c`.
/// Les jetons sont réduits aux alphanumériques, comme l'index les découpe —
/// aucun caractère de la saisie n'atteint la syntaxe de requête.
fn format_fts_phrase(engine: Engine, phrase: &str) -> Option<String> {
    let jetons: Vec<&str> = phrase
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    if jetons.is_empty() {
        return None;
    }
    Some(match engine {
        Engine::Sqlite => format!("\"{}\"", jetons.join(" ")),
        Engine::Postgres => jetons.join(" <-> "),
    })
}

fn format_fts_query_libre(engine: Engine, raw: &str) -> String {
    // Punctuation as separator — mirrors the index's own tokenisation.
    let split: Vec<&str> = raw
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    // Punctuation stripped inside whitespace-delimited words, for stored
    // text that has no punctuation where the query does.
    let glued: Vec<String> = raw
        .split_whitespace()
        .map(|t| {
            t.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
        })
        .filter(|t| !t.is_empty())
        .collect();

    let split_q = join_fts_tokens(
        engine,
        &split.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
    );
    let glued_q = join_fts_tokens(engine, &glued);

    match (split_q.is_empty(), glued_q.is_empty()) {
        (true, true) => String::new(),
        (true, false) => glued_q,
        (false, true) => split_q,
        // Identical whenever the query held no punctuation, which is the
        // common case — don't pay for an OR branch there.
        (false, false) if split_q == glued_q => split_q,
        (false, false) => match engine {
            Engine::Sqlite => format!("({split_q}) OR ({glued_q})"),
            Engine::Postgres => format!("({split_q}) | ({glued_q})"),
        },
    }
}

/// Les colonnes de l'index plein texte des pistes qui identifient la PISTE
/// elle-même — son titre, son artiste, son genre, son compositeur.
///
/// `album_title` en est ABSENT, et c'est tout le sujet de #4367. L'index
/// `tracks_fts` le porte (voir `crate::library::full_text_search`), et le
/// `MATCH` ne visait aucune colonne : une recherche « Wish You Were Here »
/// rendait donc, en section **Titres**, *Have A Cigar*, *Welcome To The
/// Machine* et *Shine On You Crazy Diamond* — dont pas un titre ne contient
/// un mot de la requête. Mesuré le 17/09/2026 sur les deux serveurs de
/// Bertrand, `sources=local` pour écarter les services : le .18 (SQLite) et
/// le .15 (PostgreSQL) rendent tous deux *Have a Cigar*.
///
/// L'album, lui, continue d'être trouvé par la section **Albums**, qui est sa
/// place — c'est l'argument du testeur : « l'album a déjà été trouvé dans la
/// recherche d'albums ».
pub const COLONNES_IDENTITE_PISTE: [&str; 4] = ["title", "artist_name", "genre", "composer"];

/// La requête plein texte d'une recherche de PISTES.
///
/// C'est [`format_fts_query`], puis — sous SQLite — le filtre de colonnes
/// FTS5 `{col …} : (expr)`, qui restreint la correspondance à
/// [`COLONNES_IDENTITE_PISTE`]. Le filtre vit dans la CHAÎNE passée au
/// `MATCH`, pas dans le SQL : la forme de la requête ne bouge pas d'un
/// caractère, donc le plan d'exécution non plus, et aucune base existante
/// n'a à être réindexée.
///
/// Postgres n'a pas d'équivalent dans `to_tsquery` — son `tsvector` est un
/// seul sac, sans poids. Il reçoit donc la requête inchangée, et c'est
/// [`SqlDialect::fts_piste_hors_album`] qui porte la restriction, en ET du
/// prédicat indexé.
///
/// Une requête vide reste vide : `{col} : ()` serait une erreur de syntaxe
/// FTS5 là où `` l'est déjà, et le repli du repo (`unwrap_or_default`) ne
/// changerait pas de couleur pour autant.
pub fn format_fts_query_piste(engine: Engine, raw: &str) -> String {
    let base = format_fts_query(engine, raw);
    match engine {
        Engine::Sqlite if !base.is_empty() => {
            format!("{{{}}} : ({})", COLONNES_IDENTITE_PISTE.join(" "), base)
        }
        _ => base,
    }
}

/// La requête plein texte qui SÉLECTIONNE les pistes d'une recherche : la
/// piste par elle-même, OU par les termes de son chemin (#5192) — nom du
/// dernier dossier et nom du fichier, voir
/// [`crate::library::full_text_search::termes_de_chemin`].
///
/// Sous SQLite, deux branches dans la chaîne du `MATCH` :
///  - [`COLONNES_IDENTITE_PISTE`] seules, comme [`format_fts_query_piste`] ;
///  - les mêmes PLUS `path_terms`, SAUF quand le titre de l'album porte à lui
///    seul toute la requête. Les mots peuvent se répartir entre les colonnes
///    (« Mahler » dans l'artiste, « Kondrashin » dans le dossier). Le `NOT`
///    garde #4367 : un dossier nommé d'après son album (« Pink Floyd - Wish
///    You Were Here ») ferait sinon revenir *Have A Cigar* sur « Wish You Were
///    Here » — par le chemin, cette fois. L'album, lui, reste trouvé par la
///    section Albums.
///
/// Postgres reçoit la requête nue ; la même logique est dans
/// [`SqlDialect::fts_piste_ou_chemin_hors_album`].
///
/// Le CLASSEMENT (« par la piste » avant « par le chemin seul ») n'est pas
/// ici : c'est l'`ORDER BY` de la recherche de pistes, qui rejoue
/// [`format_fts_query_piste`].
pub fn format_fts_query_piste_ou_chemin(engine: Engine, raw: &str) -> String {
    let base = format_fts_query(engine, raw);
    match engine {
        Engine::Sqlite if !base.is_empty() => {
            let identite = COLONNES_IDENTITE_PISTE.join(" ");
            let chemin = crate::library::full_text_search::COLONNE_TERMES_DE_CHEMIN;
            format!(
                "({{{identite}}} : ({base})) OR \
                 (({{{identite} {chemin}}} : ({base})) NOT ({{album_title}} : ({base})))"
            )
        }
        _ => base,
    }
}

/// Le vecteur d'IDENTITÉ d'une piste sous Postgres, recalculé à la volée :
/// celui de `tracks_search_tsv_refresh` moins `album_title` et moins les
/// termes de chemin.
fn pg_vecteur_identite_piste(alias_piste: &str, alias_artiste: &str) -> String {
    format!(
        "(to_tsvector('simple', unaccent(COALESCE({alias_piste}.title, ''))) \
         || to_tsvector('simple', unaccent(COALESCE({alias_artiste}.name, ''))) \
         || to_tsvector('simple', unaccent(COALESCE({alias_piste}.genre, ''))) \
         || to_tsvector('simple', unaccent(COALESCE({alias_piste}.composer, ''))))"
    )
}

/// AND the tokens together in `engine`'s dialect, prefix-marking the last.
fn join_fts_tokens(engine: Engine, tokens: &[String]) -> String {
    let Some((last, head)) = tokens.split_last() else {
        return String::new();
    };
    match engine {
        Engine::Sqlite => {
            // FTS5 accepts space-separated tokens with implicit AND.
            let prefix = if head.is_empty() {
                String::new()
            } else {
                format!("{} ", head.join(" "))
            };
            format!("{prefix}{last}*")
        }
        Engine::Postgres => {
            // tsquery requires explicit operators between tokens; the
            // prefix marker `:*` only goes on the last token.
            let prefix = if head.is_empty() {
                String::new()
            } else {
                format!("{} & ", head.join(" & "))
            };
            format!("{prefix}{last}:*")
        }
    }
}

/// SQL dialect helpers: the small fragments that diverge between SQLite
/// and PostgreSQL. Repos that want to be engine-agnostic build their
/// queries via these helpers.
pub trait SqlDialect {
    fn engine(&self) -> Engine;

    /// Positional placeholder for parameter `idx` (1-based).
    /// SQLite: `?`. Postgres: `$1`, `$2`, ...
    fn placeholder(&self, idx: usize) -> String;

    /// Full-text MATCH clause builder (low-level column fragment).
    /// SQLite (FTS5): `<column> MATCH <placeholder>`
    /// Postgres (tsvector): `<column> @@ to_tsquery('simple', <placeholder>)`
    fn fts_match(&self, column: &str, placeholder: &str) -> String;

    /// Full-text search WHERE clause for a base table.
    ///
    /// Different engines need fundamentally different shapes here:
    /// SQLite goes through the FTS5 virtual table so the predicate is
    /// `id IN (SELECT rowid FROM <table>_fts WHERE …)`; Postgres has
    /// the tsvector on the base table itself, so the predicate is a
    /// direct `<alias>.search_tsv @@ …`.
    ///
    /// `table_alias` is the alias used in the outer query (`a` for
    /// albums, `t` for tracks, etc.) so the SQLite branch can emit
    /// `<alias>.id IN (...)`.
    ///
    /// `query_placeholder` is the bound parameter for the search
    /// query string (e.g. `$1` or `?`). The caller is responsible for
    /// formatting the user's input so it's valid for both backends:
    /// FTS5 wants `term*`, tsquery wants `term:*`. The repos pass
    /// engine-specific strings in.
    fn fts_where(&self, table: &str, table_alias: &str, query_placeholder: &str) -> String;

    /// Ce qu'il reste à vérifier, EN PLUS de [`Self::fts_where`], pour qu'une
    /// piste doive sa correspondance à elle-même et non au titre de son
    /// album (#4367). Chaîne vide = il n'y a rien à ajouter.
    ///
    /// SQLite rend la chaîne vide : la restriction est déjà DANS la chaîne
    /// passée au `MATCH` (filtre de colonnes FTS5, voir
    /// [`format_fts_query_piste`]).
    ///
    /// Postgres n'a qu'un `tsvector` par piste, sans poids, et rien dans `@@`
    /// ne sait viser une colonne. La restriction est donc recalculée à la
    /// volée sur les seules colonnes voulues. Elle vient en ET du prédicat
    /// indexé, jamais à sa place : c'est l'index GIN qui CHOISIT les lignes,
    /// ce second prédicat ne fait que les filtrer.
    ///
    /// `alias_piste` et `alias_artiste` sont les alias de la requête
    /// englobante (`t` et `ar` dans la recherche de pistes) ; le `LEFT JOIN`
    /// sur les artistes existe déjà, cette méthode n'en demande aucun.
    fn fts_piste_hors_album(
        &self,
        alias_piste: &str,
        alias_artiste: &str,
        query_placeholder: &str,
    ) -> String;

    /// Comme [`Self::fts_piste_hors_album`], pour la branche qui SÉLECTIONNE
    /// les pistes d'une recherche : la piste par elle-même, ou par ses termes
    /// de chemin tant que le titre de l'album ne porte pas seul la requête
    /// (#5192, voir [`format_fts_query_piste_ou_chemin`]).
    ///
    /// SQLite : chaîne vide, tout est dans la chaîne du `MATCH`. Postgres :
    /// recalcul à la volée, en ET du prédicat indexé — `search_tsv` porte
    /// déjà l'album et le chemin, l'index GIN choisit donc toujours les
    /// lignes. `alias_album` est l'alias du `LEFT JOIN albums` de la requête
    /// englobante.
    fn fts_piste_ou_chemin_hors_album(
        &self,
        alias_piste: &str,
        alias_artiste: &str,
        alias_album: &str,
        query_placeholder: &str,
    ) -> String;

    /// JSON path extraction (returns text).
    /// SQLite: `json_extract(<column>, '<path>')`
    /// Postgres: `<column> #>> '{<path_parts>}'`
    fn json_extract_text(&self, column: &str, path: &str) -> String;

    /// `RETURNING id` for INSERT, when supported by the engine.
    /// SQLite: empty (use `last_insert_rowid` after the INSERT).
    /// Postgres: ` RETURNING id`.
    fn returning_id_clause(&self) -> &'static str;

    /// `ON CONFLICT (...) DO NOTHING` form.
    /// Both engines support it; included so the trait stays the single
    /// source of truth for dialect choices.
    fn on_conflict_do_nothing(&self, conflict_target: &str) -> String {
        format!(" ON CONFLICT ({conflict_target}) DO NOTHING")
    }

    /// `LIMIT ... OFFSET ...` clause. Both engines accept the same form,
    /// but this is the canonical way to opt-in to the dialect helpers.
    fn limit_offset(&self, limit: i64, offset: i64) -> String {
        format!(" LIMIT {limit} OFFSET {offset}")
    }

    /// Current UTC timestamp formatted as ISO-8601 (`YYYY-MM-DDTHH:MM:SSZ`).
    /// Used by `history_repo` (and friends) for `WHERE listened_at >=
    /// (now - N days)` aggregations.
    ///
    /// SQLite: `strftime('%Y-%m-%dT%H:%M:%SZ', 'now')`
    /// Postgres: `to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')`
    fn now_iso8601(&self) -> &'static str;

    /// SQL fragment computing `<column> >= now - N days`. Used for
    /// rolling-window aggregations in `history_repo::full_dashboard`.
    ///
    /// SQLite: `<column> >= strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-{days} days')`
    /// Postgres: `<column> >= to_char(now() - interval '{days} days', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')`
    fn since_days(&self, column: &str, days: i64) -> String;

    /// Date truncation to the day, returned as ISO date `YYYY-MM-DD`.
    ///
    /// SQLite: `DATE(<column>)`
    /// Postgres: `to_char(<column>::timestamp, 'YYYY-MM-DD')`
    fn date_trunc_day(&self, column: &str) -> String;

    /// Build a predicate that's true when a JSON-array text column
    /// contains a value (case-insensitive). Used by
    /// `album_repo::list_by_genre` for the structured `genres` column.
    ///
    /// SQLite: `EXISTS (SELECT 1 FROM json_each(<column>) WHERE LOWER(value) = LOWER(<placeholder>))`
    /// Postgres: `EXISTS (SELECT 1 FROM jsonb_array_elements_text(<column>::jsonb) AS x(v) WHERE LOWER(v) = LOWER(<placeholder>))`
    fn json_array_contains_lower(&self, column: &str, placeholder: &str) -> String;

    /// Extract the hour (0-23) from a timestamp column as an integer.
    /// Used by `history_repo::full_dashboard` for hourly listening
    /// distribution.
    ///
    /// SQLite: `CAST(strftime('%H', <column>) AS INTEGER)`
    /// Postgres: `EXTRACT(HOUR FROM <column>::timestamp)::int`
    fn extract_hour(&self, column: &str) -> String;

    /// Current UTC timestamp expression, suitable for use as a DEFAULT
    /// or in INSERT VALUES.
    ///
    /// SQLite: `datetime('now')`
    /// Postgres: `NOW()`
    fn current_timestamp_expr(&self) -> &'static str;

    /// Boolean literal (for engines that don't support native booleans).
    ///
    /// SQLite: `1` / `0`
    /// Postgres: `TRUE` / `FALSE`
    fn bool_literal(&self, val: bool) -> &'static str;

    /// GROUP_CONCAT or STRING_AGG for aggregating text values.
    ///
    /// SQLite: `GROUP_CONCAT(<column>, <separator>)`
    /// Postgres: `STRING_AGG(<column>, <separator>)`
    fn group_concat(&self, column: &str, separator: &str) -> String;
}

/// Zero-cost dialect for SQLite. Repos hold one of these.
#[derive(Debug, Clone, Copy, Default)]
pub struct SqliteDialect;

impl SqlDialect for SqliteDialect {
    fn engine(&self) -> Engine {
        Engine::Sqlite
    }

    fn placeholder(&self, _idx: usize) -> String {
        "?".to_string()
    }

    fn fts_match(&self, column: &str, placeholder: &str) -> String {
        format!("{column} MATCH {placeholder}")
    }

    fn fts_where(&self, table: &str, table_alias: &str, query_placeholder: &str) -> String {
        format!(
            "{table_alias}.id IN (SELECT rowid FROM {table}_fts WHERE {table}_fts MATCH {query_placeholder})"
        )
    }

    fn fts_piste_hors_album(&self, _piste: &str, _artiste: &str, _placeholder: &str) -> String {
        // Rien ici : le filtre de colonnes voyage dans la chaîne du `MATCH`.
        String::new()
    }

    fn fts_piste_ou_chemin_hors_album(
        &self,
        _piste: &str,
        _artiste: &str,
        _album: &str,
        _placeholder: &str,
    ) -> String {
        // Idem : filtres de colonnes et `NOT` voyagent dans la chaîne.
        String::new()
    }

    fn json_extract_text(&self, column: &str, path: &str) -> String {
        // Caller is responsible for passing a path that is already
        // single-quote-safe (we don't allow user input here in practice;
        // paths are compile-time string literals).
        format!("json_extract({column}, '{path}')")
    }

    fn returning_id_clause(&self) -> &'static str {
        ""
    }

    fn now_iso8601(&self) -> &'static str {
        "strftime('%Y-%m-%dT%H:%M:%SZ', 'now')"
    }

    fn since_days(&self, column: &str, days: i64) -> String {
        // SQLite's modifier syntax: 'now', '-N days' → relative to now.
        format!("{column} >= strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-{days} days')")
    }

    fn date_trunc_day(&self, column: &str) -> String {
        format!("DATE({column})")
    }

    fn json_array_contains_lower(&self, column: &str, placeholder: &str) -> String {
        format!(
            "EXISTS (SELECT 1 FROM json_each({column}) WHERE LOWER(value) = LOWER({placeholder}))"
        )
    }

    fn extract_hour(&self, column: &str) -> String {
        format!("CAST(strftime('%H', {column}) AS INTEGER)")
    }

    fn current_timestamp_expr(&self) -> &'static str {
        "datetime('now')"
    }

    fn bool_literal(&self, val: bool) -> &'static str {
        if val { "1" } else { "0" }
    }

    fn group_concat(&self, column: &str, separator: &str) -> String {
        format!("GROUP_CONCAT({column}, '{separator}')")
    }
}

/// Zero-cost dialect for Postgres.
#[derive(Debug, Clone, Copy, Default)]
pub struct PostgresDialect;

impl SqlDialect for PostgresDialect {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    fn placeholder(&self, idx: usize) -> String {
        format!("${idx}")
    }

    fn fts_match(&self, column: &str, placeholder: &str) -> String {
        // We use the 'simple' dictionary by default; per-language
        // configuration (french, english, ...) is a follow-up decided in
        // the FTS migration phase.
        format!("{column} @@ to_tsquery('simple', unaccent({placeholder}))")
    }

    fn fts_where(&self, _table: &str, table_alias: &str, query_placeholder: &str) -> String {
        // PG has the tsvector on the base table itself (see
        // tune-core/migrations/postgres/002_fts_tsvector.sql), so the
        // predicate is a direct @@ on <alias>.search_tsv. Wrapping the
        // placeholder in unaccent() makes the search accent-insensitive,
        // matching the behaviour of FTS5's `tokenize='unicode61
        // remove_diacritics 2'`.
        format!("{table_alias}.search_tsv @@ to_tsquery('simple', unaccent({query_placeholder}))")
    }

    fn fts_piste_hors_album(
        &self,
        alias_piste: &str,
        alias_artiste: &str,
        query_placeholder: &str,
    ) -> String {
        // Le MÊME vecteur que celui de `tracks_search_tsv_refresh`
        // (migrations/postgres/002_fts_tsvector.sql), moins `album_title`.
        // Recalculé, et non lu dans `search_tsv` : la colonne stockée n'a pas
        // de poids, donc rien n'y distingue plus le titre de l'album du
        // reste, et lui en donner exigerait de réécrire le tsvector de toutes
        // les pistes de toutes les bases.
        format!(
            "{} @@ to_tsquery('simple', unaccent({query_placeholder}))",
            pg_vecteur_identite_piste(alias_piste, alias_artiste)
        )
    }

    fn fts_piste_ou_chemin_hors_album(
        &self,
        alias_piste: &str,
        alias_artiste: &str,
        alias_album: &str,
        query_placeholder: &str,
    ) -> String {
        let requete = format!("to_tsquery('simple', unaccent({query_placeholder}))");
        let identite = pg_vecteur_identite_piste(alias_piste, alias_artiste);
        let chemin = format!(
            "to_tsvector('simple', unaccent({}))",
            crate::library::full_text_search::sql_termes_de_chemin_de_piste(alias_piste)
        );
        let album = format!("to_tsvector('simple', unaccent(COALESCE({alias_album}.title, '')))");
        format!(
            "({identite} @@ {requete} OR \
             (({identite} || {chemin}) @@ {requete} AND NOT ({album} @@ {requete})))"
        )
    }

    fn json_extract_text(&self, column: &str, path: &str) -> String {
        // Path arrives as a JSON pointer (e.g. "foo.bar") and is
        // translated to a Postgres path array literal.
        let parts: Vec<&str> = path.split('.').collect();
        let path_array = parts.join(",");
        format!("{column} #>> '{{{path_array}}}'")
    }

    fn returning_id_clause(&self) -> &'static str {
        " RETURNING id"
    }

    fn now_iso8601(&self) -> &'static str {
        // The `T` and `Z` literals need to be quoted within to_char's
        // format string.
        "to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')"
    }

    fn since_days(&self, column: &str, days: i64) -> String {
        format!(
            "{column} >= to_char(now() - interval '{days} days', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')"
        )
    }

    fn date_trunc_day(&self, column: &str) -> String {
        // `listened_at` is stored as TEXT on both engines (ISO-8601),
        // so cast to timestamp before format.
        format!("to_char({column}::timestamp, 'YYYY-MM-DD')")
    }

    fn json_array_contains_lower(&self, column: &str, placeholder: &str) -> String {
        format!(
            "EXISTS (SELECT 1 FROM jsonb_array_elements_text({column}::jsonb) AS x(v) WHERE LOWER(v) = LOWER({placeholder}))"
        )
    }

    fn extract_hour(&self, column: &str) -> String {
        format!("EXTRACT(HOUR FROM {column}::timestamp)::int")
    }

    fn current_timestamp_expr(&self) -> &'static str {
        "NOW()"
    }

    fn bool_literal(&self, val: bool) -> &'static str {
        if val { "TRUE" } else { "FALSE" }
    }

    fn group_concat(&self, column: &str, separator: &str) -> String {
        format!("STRING_AGG({column}, '{separator}')")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_diacritics_strips_latin_accents() {
        assert_eq!(fold_diacritics("Carlão"), "Carlao");
        assert_eq!(fold_diacritics("Beyoncé"), "Beyonce");
        assert_eq!(fold_diacritics("Motörhead"), "Motorhead");
        assert_eq!(fold_diacritics("Édith Piaf"), "Edith Piaf");
        assert_eq!(fold_diacritics("Sigur Rós"), "Sigur Ros");
        // Non-Latin scripts pass through unchanged (no false folding).
        assert_eq!(fold_diacritics("Мельница"), "Мельница");
        // ASCII is untouched.
        assert_eq!(fold_diacritics("Pink Floyd"), "Pink Floyd");
    }

    #[test]
    fn engine_from_str_accepts_aliases() {
        assert_eq!(Engine::from_str("sqlite"), Some(Engine::Sqlite));
        assert_eq!(Engine::from_str("SQLITE"), Some(Engine::Sqlite));
        assert_eq!(Engine::from_str("postgres"), Some(Engine::Postgres));
        assert_eq!(Engine::from_str("postgresql"), Some(Engine::Postgres));
        assert_eq!(Engine::from_str("mysql"), None);
    }

    #[test]
    fn engine_from_connection_string_routes_correctly() {
        assert_eq!(
            Engine::from_connection_string("/var/lib/tune.db"),
            Engine::Sqlite
        );
        assert_eq!(
            Engine::from_connection_string("postgresql://localhost/tune"),
            Engine::Postgres
        );
        assert_eq!(
            Engine::from_connection_string("postgres://u:p@host/db"),
            Engine::Postgres
        );
    }

    #[test]
    fn sqlite_dialect_emits_question_marks() {
        let d = SqliteDialect;
        assert_eq!(d.placeholder(1), "?");
        assert_eq!(d.placeholder(42), "?");
        assert_eq!(
            d.fts_match("tracks_fts", d.placeholder(1).as_str()),
            "tracks_fts MATCH ?"
        );
        assert_eq!(d.returning_id_clause(), "");
    }

    #[test]
    fn postgres_dialect_emits_numbered_placeholders() {
        let d = PostgresDialect;
        assert_eq!(d.placeholder(1), "$1");
        assert_eq!(d.placeholder(7), "$7");
        assert_eq!(
            d.fts_match("search_tsv", d.placeholder(1).as_str()),
            "search_tsv @@ to_tsquery('simple', unaccent($1))"
        );
        assert_eq!(d.returning_id_clause(), " RETURNING id");
    }

    #[test]
    fn fts_where_uses_engine_specific_shape() {
        let s = SqliteDialect;
        assert_eq!(
            s.fts_where("artists", "a", &s.placeholder(1)),
            "a.id IN (SELECT rowid FROM artists_fts WHERE artists_fts MATCH ?)"
        );
        let p = PostgresDialect;
        assert_eq!(
            p.fts_where("artists", "a", &p.placeholder(1)),
            "a.search_tsv @@ to_tsquery('simple', unaccent($1))"
        );
    }

    #[test]
    fn des_guillemets_demandent_une_phrase_exacte() {
        assert_eq!(
            format_fts_query(Engine::Sqlite, "\"kind of blue\""),
            "\"kind of blue\""
        );
        assert_eq!(
            format_fts_query(Engine::Postgres, "\"kind of blue\""),
            "kind <-> of <-> blue"
        );
        // Phrase ET mots libres : les deux s'imposent.
        assert_eq!(
            format_fts_query(Engine::Sqlite, "\"kind of blue\" miles"),
            "\"kind of blue\" miles*"
        );
        assert_eq!(
            format_fts_query(Engine::Postgres, "miles \"so what\""),
            "so <-> what & miles:*"
        );
        // Guillemet resté ouvert pendant la frappe.
        assert_eq!(format_fts_query(Engine::Sqlite, "\"kind of"), "\"kind of\"");
        // Rien d'utilisable entre les guillemets : rien.
        assert_eq!(format_fts_query(Engine::Sqlite, "\"  !! \""), "");
        // Sans guillemets, rien ne change.
        assert_eq!(
            format_fts_query(Engine::Sqlite, "kind of blue"),
            "kind of blue*"
        );
        assert_eq!(motif_like("\"kind of blue\""), "%kind of blue%");
    }

    #[test]
    fn format_fts_query_single_word() {
        assert_eq!(format_fts_query(Engine::Sqlite, "miles"), "miles*");
        assert_eq!(format_fts_query(Engine::Postgres, "miles"), "miles:*");
    }

    #[test]
    fn format_fts_query_multi_word() {
        // Multi-word inputs are AND-joined per engine syntax. This is
        // the bug that crashed tsquery in the v0.8.28 PG smoke test.
        assert_eq!(
            format_fts_query(Engine::Sqlite, "miles davis"),
            "miles davis*"
        );
        assert_eq!(
            format_fts_query(Engine::Postgres, "miles davis"),
            "miles & davis:*"
        );
    }

    #[test]
    fn format_fts_query_strips_punctuation() {
        assert_eq!(
            format_fts_query(Engine::Postgres, "rock & roll!"),
            "rock & roll:*"
        );
        // The user's `&` is stripped along with other non-alphanumerics
        // before we re-introduce it as the tsquery AND operator.
    }

    #[test]
    fn format_fts_query_empty_returns_empty() {
        assert_eq!(format_fts_query(Engine::Sqlite, ""), "");
        assert_eq!(format_fts_query(Engine::Postgres, "   !!!"), "");
    }

    #[test]
    fn format_fts_query_handles_unicode_letters() {
        // is_alphanumeric() accepts the Stromaé é; that's fine — we
        // rely on PG's unaccent() to handle the diacritics downstream
        // at query time.
        assert_eq!(format_fts_query(Engine::Postgres, "stromaé"), "stromaé:*");
    }

    #[test]
    fn format_fts_query_treats_punctuation_as_a_separator() {
        // The index has these as *two* tokens — FTS5's unicode61 breaks on
        // `/` — so the glued `ACDC` alone could never match. Both readings
        // are emitted: split for data carrying the punctuation, glued for
        // data that does not.
        assert_eq!(
            format_fts_query(Engine::Sqlite, "AC/DC"),
            "(AC DC*) OR (ACDC*)"
        );
        assert_eq!(
            format_fts_query(Engine::Postgres, "AC/DC"),
            "(AC & DC:*) | (ACDC:*)"
        );
    }

    #[test]
    fn format_fts_query_splits_inside_words_not_just_on_spaces() {
        // Before: `B.B. King` collapsed to `BB King`, which misses an index
        // holding `b`, `b`, `king`. Same for hyphenated names — the reason
        // `Saint-Saens` returned nothing at all.
        assert_eq!(
            format_fts_query(Engine::Sqlite, "B.B. King"),
            "(B B King*) OR (BB King*)"
        );
        assert_eq!(
            format_fts_query(Engine::Sqlite, "Saint-Saens"),
            "(Saint Saens*) OR (SaintSaens*)"
        );
    }

    #[test]
    fn format_fts_query_skips_the_or_branch_without_punctuation() {
        // The common case must not pay for a second branch.
        assert_eq!(
            format_fts_query(Engine::Sqlite, "miles davis"),
            "miles davis*"
        );
        assert_eq!(
            format_fts_query(Engine::Postgres, "miles davis"),
            "miles & davis:*"
        );
    }

    #[test]
    fn json_extract_dialect_specific() {
        let s = SqliteDialect;
        assert_eq!(
            s.json_extract_text("meta", "artist.name"),
            "json_extract(meta, 'artist.name')"
        );
        let p = PostgresDialect;
        assert_eq!(
            p.json_extract_text("meta", "artist.name"),
            "meta #>> '{artist,name}'"
        );
    }

    #[test]
    fn engine_display_matches_as_str() {
        assert_eq!(format!("{}", Engine::Sqlite), "sqlite");
        assert_eq!(format!("{}", Engine::Postgres), "postgres");
    }

    #[test]
    fn current_timestamp_expr_dialect() {
        let s = SqliteDialect;
        assert_eq!(s.current_timestamp_expr(), "datetime('now')");
        let p = PostgresDialect;
        assert_eq!(p.current_timestamp_expr(), "NOW()");
    }

    #[test]
    fn bool_literal_dialect() {
        let s = SqliteDialect;
        assert_eq!(s.bool_literal(true), "1");
        assert_eq!(s.bool_literal(false), "0");
        let p = PostgresDialect;
        assert_eq!(p.bool_literal(true), "TRUE");
        assert_eq!(p.bool_literal(false), "FALSE");
    }

    #[test]
    fn group_concat_dialect() {
        let s = SqliteDialect;
        assert_eq!(s.group_concat("name", ", "), "GROUP_CONCAT(name, ', ')");
        let p = PostgresDialect;
        assert_eq!(p.group_concat("name", ", "), "STRING_AGG(name, ', ')");
    }

    #[test]
    fn date_helpers_dialect_specific() {
        let s = SqliteDialect;
        assert_eq!(s.now_iso8601(), "strftime('%Y-%m-%dT%H:%M:%SZ', 'now')");
        assert_eq!(
            s.since_days("listened_at", 7),
            "listened_at >= strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-7 days')"
        );
        assert_eq!(s.date_trunc_day("listened_at"), "DATE(listened_at)");

        let p = PostgresDialect;
        assert_eq!(
            p.now_iso8601(),
            "to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')"
        );
        assert!(
            p.since_days("listened_at", 30)
                .contains("interval '30 days'")
        );
        assert!(
            p.date_trunc_day("listened_at")
                .starts_with("to_char(listened_at::timestamp")
        );
    }
}
