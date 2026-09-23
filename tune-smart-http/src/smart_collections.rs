use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::backend::ToSqlValue;

use crate::SmartHttpState;
use crate::catalogue;
use crate::smart_refs::{self, DbRefResolver, RefCtx, RefKind, RefResolver};
use crate::source_streaming::{self, Objet};
use tune_http_types::{ActiveProfile, AppError};

#[derive(Deserialize)]
struct CreateCollection {
    name: String,
    rules: Value,
    match_mode: Option<String>,
    sort_by: Option<String>,
    sort_order: Option<String>,
    max_limit: Option<i64>,
    description: Option<String>,
    icon: Option<String>,
    color: Option<String>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct UpdateCollection {
    name: Option<String>,
    rules: Option<Value>,
    match_mode: Option<String>,
    sort_by: Option<String>,
    sort_order: Option<String>,
    max_limit: Option<i64>,
    description: Option<String>,
    icon: Option<String>,
    color: Option<String>,
}

#[derive(Deserialize)]
struct PreviewRequest {
    rules: Value,
    match_mode: Option<String>,
    sort_by: Option<String>,
    sort_order: Option<String>,
    max_limit: Option<i64>,
}

pub fn router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    SmartHttpState: axum::extract::FromRef<S>,
    ActiveProfile: axum::extract::FromRequestParts<S>,
    <ActiveProfile as axum::extract::FromRequestParts<S>>::Rejection: IntoResponse,
{
    Router::new()
        .route("/", get(list_collections).post(create_collection))
        .route(
            "/{id}",
            get(get_collection)
                .put(update_collection)
                .delete(delete_collection),
        )
        .route("/{id}/albums", get(resolve_albums))
        .route("/preview", post(preview_albums))
}

/// Normalize a stored `sort_order` value to the bare `asc`/`desc` the
/// SortOrder enum expects. The tune-core save path stores it JSON-encoded
/// (`"asc"` with quotes) while this route's save path stores it raw (`asc`);
/// stripping surrounding quotes tolerates both, avoiding the compile error
/// `unknown variant "asc", expected asc or desc`.
fn normalize_sort_order(raw: Option<String>) -> String {
    raw.map(|s| s.trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "asc".into())
}

/// Decode a row from `smart_collections` into a JSON object.
/// Column order: id(0), name(1), rules(2), match_mode(3), sort_by(4),
/// sort_order(5), max_limit(6), description(7), icon(8), color(9), created_at(10).
fn decode_collection_row(r: &[tune_core::db::backend::SqlValue]) -> Value {
    let rules_str = r
        .get(2)
        .and_then(|v| v.as_string())
        .unwrap_or_else(|| "[]".into());
    let rules = serde_json::from_str::<Value>(&rules_str).unwrap_or(json!([]));
    let nom = r.get(1).and_then(|v| v.as_string());
    let description = r.get(7).and_then(|v| v.as_string());
    let mut objet = json!({
        "id": r.get(0).and_then(|v| v.as_i64()),
        "name": nom,
        "rules": rules,
        "match_mode": r.get(3).and_then(|v| v.as_string()).unwrap_or_else(|| "all".into()),
        "sort_by": r.get(4).and_then(|v| v.as_string()),
        "sort_order": normalize_sort_order(r.get(5).and_then(|v| v.as_string())),
        "max_limit": r.get(6).and_then(|v| v.as_i64()),
        "description": description,
        "icon": r.get(8).and_then(|v| v.as_string()),
        "color": r.get(9).and_then(|v| v.as_string()),
        "created_at": r.get(10).and_then(|v| v.as_string()),
    });
    // Les seize collections du semis sont nommées en français en base
    // (`tune-core/src/db/migrations.rs:546` et `:614`). Leur clé stable part
    // À CÔTÉ du nom, jamais à sa place : le client la traduit et retombe sur
    // `name` quand elle manque. Une collection renommée par l'utilisateur ne
    // ressemble plus au semis et n'en reçoit aucune — c'est ainsi que
    // « ne rien renommer » est tenu, sans écrire une ligne en base.
    let (cle_nom, cle_description) = crate::collections_par_defaut::cles(
        nom.as_deref().unwrap_or_default(),
        description.as_deref(),
    );
    if let Some(cle) = cle_nom {
        objet["name_key"] = json!(cle);
    }
    if let Some(cle) = cle_description {
        objet["description_key"] = json!(cle);
    }
    objet
}

/// Le nombre d'albums d'une collection : ceux de la bibliothèque, **plus** les
/// favoris de service que la même règle sélectionne.
///
/// 🔴 #1231 — Bertrand, 18/09/2026 : « Smart Collection, source qobuz retourne
/// 0 album ». Mesuré sur le .18 : la collection rend bien ses 3 albums Qobuz
/// quand on l'ouvre, et la liste annonçait `"album_count": 0`. Le SQL ci-dessus
/// compte dans `albums`, où un favori de service n'est jamais — il vient de
/// `streaming_favorites`, ajouté APRÈS par `source_streaming`. La liste était
/// juste, son compteur mentait.
///
/// Fonction à part, et synchrone, pour être éprouvée : une garde textuelle
/// laissait passer le débranchement de l'addition sans rien voir.
pub(crate) fn compte_albums(
    backend: &dyn tune_core::db::backend::DbBackend,
    sql_bibliotheque: &str,
    rules_json: &str,
    match_mode: &str,
    profile_id: i64,
) -> i64 {
    let (en_base, en_service) = compte_albums_ventile(
        backend,
        sql_bibliotheque,
        rules_json,
        match_mode,
        profile_id,
    );
    en_base + en_service
}

/// Le même compte, mais SÉPARÉ : ce que la bibliothèque apporte, et ce que les
/// favoris de service ajoutent.
///
/// 🔴 #4466 — la seconde moitié du ticket. `album_count` sait additionner les
/// deux depuis #4470 ; `track_count` ne le peut pas, et c'est structurel :
/// `streaming_favorites` (`tune-core/src/db/migrations.rs:739`) ne porte que
/// service, identifiant, titre, artiste, album et pochette. **Un album favori
/// d'un service n'a pas de nombre de pistes à ajouter** — d'où le
/// `"track_count": 0` de `source_streaming::album_json`.
///
/// Savoir si le service contribue est donc ce qui décide si le compte de
/// pistes de la bibliothèque est le compte COMPLET, ou seulement une moitié.
/// Le corps de l'issue tranchait déjà : « qu'ils passent par le même chemin
/// complet que la vue — ou, à défaut, qu'ils ne soient pas rendus plutôt que
/// rendus faux ». Il n'y a rien à additionner : reste à ne pas rendre.
pub(crate) fn compte_albums_ventile(
    backend: &dyn tune_core::db::backend::DbBackend,
    sql_bibliotheque: &str,
    rules_json: &str,
    match_mode: &str,
    profile_id: i64,
) -> (i64, i64) {
    let un = |sql: &str| {
        backend
            .query_many(sql, &[])
            .ok()
            .and_then(|rs| rs.first().and_then(|r| r.first()).and_then(|v| v.as_i64()))
            .unwrap_or(0)
    };
    let en_base = un(sql_bibliotheque);
    let en_service =
        source_streaming::requete_compte(rules_json, match_mode, Objet::Album, profile_id)
            .map(|sql| un(&sql))
            .unwrap_or(0);
    (en_base, en_service)
}

async fn list_collections(
    State(state): State<SmartHttpState>,
    profile: ActiveProfile,
) -> Result<Json<Value>, AppError> {
    let rows = state
        .backend
        .query_many(
            "SELECT id, name, rules, match_mode, sort_by, sort_order, max_limit, \
         description, icon, color, created_at \
         FROM smart_collections ORDER BY name",
            &[],
        )
        .map_err(AppError::internal)?;

    let resolver = DbRefResolver::new(&state.backend);
    let ctx = RefCtx::root(&resolver, Some(profile.id()));
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut col = decode_collection_row(r);
            let rules_str = r
                .get(2)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "[]".into());
            // Count with the SAME album-query engine as resolve_albums so the
            // count is always produced and matches the album view. The old
            // SmartCollection::compile_sql path silently failed to deserialize
            // richer rule operators (>=, <=, in, is_null…) and then dropped
            // album_count entirely, so a collection using those showed no count
            // (Jean Marie). build_album_query inlines escaped values → no bound
            // params (mirrors execute_album_query). max_limit=None reports the
            // full membership, not the capped view.
            let match_mode = col["match_mode"].as_str().unwrap_or("all");
            let sort_by = col["sort_by"].as_str().unwrap_or("title");
            let sort_order = col["sort_order"].as_str().unwrap_or("asc");
            let (where_clause, _order, _limit) =
                build_album_query(&rules_str, match_mode, sort_by, sort_order, None, &ctx);

            let album_count_sql = format!(
                "SELECT COUNT(DISTINCT al.id) FROM albums al \
                 LEFT JOIN artists ar ON al.artist_id = ar.id \
                 LEFT JOIN tracks t ON t.album_id = al.id {where_clause}"
            );
            let (albums_en_base, albums_de_service) = compte_albums_ventile(
                &*state.backend,
                &album_count_sql,
                &rules_str,
                match_mode,
                profile.id(),
            );
            col["album_count"] = json!(albums_en_base + albums_de_service);
            // 🔴 #4466 — `track_count` ne compte QUE la base. Tant que les
            // favoris de service n'entrent pas dans la collection, c'est le
            // compte complet et il se rend. Dès qu'ils y entrent, il ne
            // couvre plus qu'une part du contenu — et une collection faite de
            // 3 albums favoris Qobuz affichait « 3 albums · 0 piste ».
            //
            // Il n'y a rien à additionner : `streaming_favorites` ne porte
            // aucun nombre de pistes. On ne rend donc PAS le champ, ce que le
            // corps de l'issue demandait explicitement à défaut du compte
            // complet — « un 0 sur une collection pleine est pire qu'une
            // absence de compte ». Le client teste déjà `track_count != null`
            // (`SmartCollectionsView.svelte`) : la mention de pistes
            // disparaît, le nombre d'albums reste.
            let compte_complet = albums_de_service == 0;
            if compte_complet {
                let track_count_sql = format!(
                    "SELECT COUNT(DISTINCT t.id) FROM albums al \
                     LEFT JOIN artists ar ON al.artist_id = ar.id \
                     LEFT JOIN tracks t ON t.album_id = al.id {where_clause}"
                );
                if let Ok(rs) = state.backend.query_many(&track_count_sql, &[]) {
                    col["track_count"] = json!(
                        rs.first()
                            .and_then(|r| r.first())
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0)
                    );
                }
            } else {
                // Et on DIT pourquoi, plutôt que de laisser un champ
                // manquant s'expliquer tout seul.
                col["track_count_partiel"] = json!(true);
            }
            // #4473, arbitrage 2 : le catalogue n'est PAS compté (un appel
            // réseau par collection et par affichage), mais la liste le dit,
            // pour que l'écran annonce « + catalogue Qobuz » sans chiffre.
            col["catalogue_service"] = json!(catalogue::service_du_catalogue(&rules_str));
            col
        })
        .collect();
    Ok(Json(json!(items)))
}

/// La borne `max_limit` d'une collection intelligente, ou 400.
///
/// 🔴 #2732 — `max_limit` était recopié TEL QUEL dans le SQL
/// (`build_album_query` : `format!("LIMIT {n}")`), sans qu'aucune écriture ne
/// le regarde. Le formulaire annonce `min="1"`, mais rien ne l'appliquait côté
/// serveur et l'API acceptait n'importe quel entier. Une borne que le serveur
/// ne peut pas honorer s'enregistrait donc en silence, et se relisait au
/// formulaire comme une valeur normale — la collection, elle, ne rendait pas ce
/// que l'écran annonçait :
///
/// - `0` ⇒ `LIMIT 0` : la collection résout ZÉRO album, sans message. Une
///   borne « aucun album » n'a aucun sens, et rien ne la distingue à l'écran
///   d'une règle qui ne ramène rien ;
/// - négatif ⇒ les deux moteurs DIVERGENT. SQLite lit un `LIMIT` négatif comme
///   « pas de limite » — la borne enregistrée ne borne alors rien —, tandis que
///   PostgreSQL refuse la requête et la route rend 500. C'est exactement la
///   classe d'écart que #1752 a coûté : la même donnée, deux comportements.
///
/// La doctrine du dépôt est de REFUSER, jamais d'ignorer : une valeur ignorée
/// devient un réglage annoncé qui ne s'applique pas (`ints` dans
/// `routes/library/query_multi.rs`). `None` reste « pas de borne ».
fn borne_valide(max_limit: Option<i64>) -> Result<Option<i64>, AppError> {
    match max_limit {
        Some(n) if n <= 0 => Err(AppError::bad_request(format!(
            "max_limit doit etre strictement positif (recu {n}) ; omettre le champ signifie « pas de borne »"
        ))),
        autre => Ok(autre),
    }
}

async fn create_collection(
    State(state): State<SmartHttpState>,
    Json(body): Json<CreateCollection>,
) -> Result<impl IntoResponse, AppError> {
    borne_valide(body.max_limit)?;
    let rules_json = body.rules.to_string();
    let match_mode = body.match_mode.clone().unwrap_or_else(|| "all".into());
    let sort_by = body.sort_by.clone();
    let sort_order = body.sort_order.clone().unwrap_or_else(|| "asc".into());

    // Refuse les références circulaires (A ⊂ B ⊂ A) entre entités smart.
    let resolver = DbRefResolver::new(&state.backend);
    smart_refs::check_no_cycle(
        &resolver,
        RefKind::SmartCollection,
        None,
        &body.name,
        &rules_json,
    )
    .map_err(AppError::bad_request)?;

    let id = state
        .backend
        .execute_returning_id(
            "INSERT INTO smart_collections \
         (name, rules, match_mode, sort_by, sort_order, max_limit, description, icon, color) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            &[
                &body.name as &dyn ToSqlValue,
                &rules_json as &dyn ToSqlValue,
                &match_mode as &dyn ToSqlValue,
                &sort_by as &dyn ToSqlValue,
                &sort_order as &dyn ToSqlValue,
                &body.max_limit as &dyn ToSqlValue,
                &body.description as &dyn ToSqlValue,
                &body.icon as &dyn ToSqlValue,
                &body.color as &dyn ToSqlValue,
            ],
        )
        .map_err(AppError::internal)?;

    // Relire l'objet persisté au lieu de fabriquer une réponse partielle :
    // le client réutilise immédiatement ce contrat et `created_at` fait partie
    // des champs annoncés par SmartCollection (#2732).
    let row = state
        .backend
        .query_one(
            "SELECT id, name, rules, match_mode, sort_by, sort_order, max_limit, \
         description, icon, color, created_at \
         FROM smart_collections WHERE id = $1",
            &[&id as &dyn ToSqlValue],
        )
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::internal("collection créée mais introuvable"))?;

    Ok((StatusCode::CREATED, Json(decode_collection_row(&row))).into_response())
}

async fn get_collection(
    State(state): State<SmartHttpState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let row = state
        .backend
        .query_one(
            "SELECT id, name, rules, match_mode, sort_by, sort_order, max_limit, \
         description, icon, color, created_at \
         FROM smart_collections WHERE id = $1",
            &[&id as &dyn ToSqlValue],
        )
        .map_err(AppError::internal)?;

    match row {
        Some(r) => Ok(Json(decode_collection_row(&r)).into_response()),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

async fn update_collection(
    State(state): State<SmartHttpState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateCollection>,
) -> Result<impl IntoResponse, AppError> {
    // Une borne inapplicable est refusée AVANT toute écriture : la mise à jour
    // écrit champ par champ, un refus tardif laisserait la collection à
    // moitié modifiée (#2732).
    borne_valide(body.max_limit)?;
    // Refuse les références circulaires avant d'écrire quoi que ce soit.
    if let Some(ref rules) = body.rules {
        let self_name = body
            .name
            .clone()
            .or_else(|| {
                DbRefResolver::new(&state.backend)
                    .smart_entity(RefKind::SmartCollection, id)
                    .map(|e| e.name)
            })
            .unwrap_or_else(|| format!("#{id}"));
        let resolver = DbRefResolver::new(&state.backend);
        smart_refs::check_no_cycle(
            &resolver,
            RefKind::SmartCollection,
            Some(id),
            &self_name,
            &rules.to_string(),
        )
        .map_err(AppError::bad_request)?;
    }
    if let Some(ref name) = body.name {
        state.backend.execute(
            "UPDATE smart_collections SET name = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[name as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }
    if let Some(ref rules) = body.rules {
        let rules_json = rules.to_string();
        state.backend.execute(
            "UPDATE smart_collections SET rules = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[&rules_json as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }
    if let Some(ref match_mode) = body.match_mode {
        state.backend.execute(
            "UPDATE smart_collections SET match_mode = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[match_mode as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }
    if let Some(ref sort_by) = body.sort_by {
        state.backend.execute(
            "UPDATE smart_collections SET sort_by = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[sort_by as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }
    if let Some(ref sort_order) = body.sort_order {
        state.backend.execute(
            "UPDATE smart_collections SET sort_order = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[sort_order as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }
    if let Some(ref max_limit) = body.max_limit {
        state.backend.execute(
            "UPDATE smart_collections SET max_limit = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[max_limit as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }
    if let Some(ref description) = body.description {
        state.backend.execute(
            "UPDATE smart_collections SET description = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[description as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }
    if let Some(ref icon) = body.icon {
        state.backend.execute(
            "UPDATE smart_collections SET icon = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[icon as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }
    if let Some(ref color) = body.color {
        state.backend.execute(
            "UPDATE smart_collections SET color = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[color as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }

    // Return the updated collection as JSON
    let row = state
        .backend
        .query_one(
            "SELECT id, name, rules, match_mode, sort_by, sort_order, max_limit, \
         description, icon, color, created_at \
         FROM smart_collections WHERE id = $1",
            &[&id as &dyn ToSqlValue],
        )
        .map_err(AppError::internal)?;

    match row {
        Some(r) => Ok(Json(decode_collection_row(&r)).into_response()),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

async fn delete_collection(
    State(state): State<SmartHttpState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    state
        .backend
        .execute(
            "DELETE FROM smart_collections WHERE id = $1",
            &[&id as &dyn ToSqlValue],
        )
        .ok();
    Json(json!({"deleted": true, "id": id}))
}

fn resolve_timestamp_sql(input: &str) -> String {
    // Relative forms: "now-90d", "90d", "90" — N days ago. The seeded
    // "🆕 Récents" collection stores the bare "90d" form, which used to fall
    // through to a literal string ('90d') that no date ever compares against.
    let rest = input.strip_prefix("now-").unwrap_or(input);
    let digits = rest.strip_suffix('d').unwrap_or(rest);
    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        let days: i64 = digits.parse().unwrap_or(30);
        return format!("DATETIME('now', '-{days} days')");
    }
    format!("'{}'", input.replace('\'', "''"))
}

/// Build WHERE, ORDER, LIMIT clauses from smart collection criteria (album-level).
///
/// This is THE smart-collection rule engine: the list/albums/preview endpoints,
/// the Oxygen `collection` facet and `/library/tracks?collection=` all go
/// through it, so a collection always counts and filters the same set
/// everywhere. (The legacy `SmartCollection::compile_sql` in tune-core diverged
/// — raw `any` match_mode read as ALL, `added_at`/`rating`/`play_count` hit
/// phantom `tracks` columns, unknown fields fell back to `t.title` — which made
/// whole collections vanish from the facet or count the entire library.)
/// The WHERE references aliases `al` (albums), `ar` (artists), `t` (tracks).
pub fn build_album_query(
    rules_json: &str,
    match_mode: &str,
    sort_by: &str,
    sort_order: &str,
    max_limit: Option<i64>,
    ctx: &RefCtx,
) -> (String, String, String) {
    let rules: Vec<Value> = serde_json::from_str(rules_json).unwrap_or_default();

    let mut conditions = Vec::new();
    for rule in &rules {
        let field = rule.get("field").and_then(|v| v.as_str()).unwrap_or("");
        let raw_op = rule
            .get("operator")
            .or_else(|| rule.get("op"))
            .and_then(|v| v.as_str())
            .unwrap_or("contains");
        let op = match raw_op {
            "=" | "eq" | "equals" => "=",
            "!=" | "ne" | "not_equals" => "!=",
            ">=" | "gte" | "greater_than" | "greater_equal" => ">=",
            ">" | "gt" => ">",
            "<=" | "lte" | "less_than" | "less_equal" => "<=",
            "<" | "lt" => "<",
            // tune-core seed/editor spelling — same semantics as is_null.
            "is_empty" | "empty" => "is_null",
            "is_not_empty" | "not_empty" => "is_not_null",
            other => other,
        };
        let value_raw = rule.get("value");
        let value = value_raw
            .map(|v| match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => v.to_string(),
            })
            .unwrap_or_default();
        // DEUX échappements, et les confondre casse quelque chose dans les
        // deux sens.
        //
        // `esc` — les apostrophes seulement. C'est ce qu'une comparaison
        // d'ÉGALITÉ demande : `=` compare des chaînes entières, `%` et `_` n'y
        // sont pas des jokers. Y neutraliser les jokers ferait comparer
        // `100\% Live` à `100% Live`, qui ne se ressemblent plus.
        //
        // `esc_like` — pour les motifs `LIKE`, où les mêmes caractères
        // deviennent des jokers :
        //
        //  - `%` et `_` non neutralisés font qu'un filtre rend PLUS que demandé
        //    — `100% Live` ramenait aussi `1000 Autres` (#3101) ;
        //  - l'antislash, lui, fait rendre MOINS : Postgres le traite comme son
        //    caractère d'échappement, SQLite non. Un chemin Windows
        //    `G:\Jazz\%` dégénère côté PG en `G:Jazz%` et ne correspond à
        //    RIEN — les quatre racines de JF affichées « 0 piste » sur une
        //    bibliothèque parfaitement scannée (#1752).
        //
        // Le second point n'était théorique que tant qu'aucun champ ne portait
        // de chemin. Le champ « répertoire » en porte un, donc il l'arme.
        let esc = value.replace('\'', "''");
        let esc_like = tune_core::db::track_repo::echapper_jokers_like(&value).replace('\'', "''");
        // « l'antislash échappe » — la clause dit à Postgres ce qu'il fait déjà
        // et à SQLite ce qu'il ne faisait pas. Les deux moteurs lisent enfin la
        // même chose.
        let esc_clause = tune_core::db::track_repo::like_escape_clause();

        // --- règles « référence » (collection / playlist / favori) ---
        if smart_refs::is_ref_field(field) {
            conditions.push(smart_refs::album_ref_condition(field, op, &value, ctx));
            continue;
        }

        // --- credit rules use a subquery, handle separately ---
        if field == "credit" {
            if op == "has" {
                if let Some(obj) = value_raw.and_then(|v| v.as_object()) {
                    let mut sub_conds = Vec::new();
                    if let Some(role) = obj
                        .get("role")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        sub_conds.push(format!(
                            "LOWER(tc.role) LIKE LOWER('%{}%')",
                            role.replace('\'', "''")
                        ));
                    }
                    if let Some(artist) = obj
                        .get("artist_name")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        sub_conds.push(format!(
                            "LOWER(tc.artist_name) LIKE LOWER('%{}%')",
                            artist.replace('\'', "''")
                        ));
                    }
                    if let Some(instr) = obj
                        .get("instrument")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        // MÊME canonisation qu'à l'écriture des crédits
                        // (#2799 §4). L'enrichissement range désormais
                        // « grand piano » / « electric piano » sous `piano` ;
                        // si la règle cherchait le libellé brut saisi par
                        // l'utilisateur, une collection `instrument: Grand
                        // Piano` ne trouverait plus rien alors que les lignes
                        // existent. Deux normalisations, deux résultats.
                        let canon = tune_core::metadata::instruments::canoniser_instrument(instr);
                        let motif = if canon.is_empty() { instr } else { &canon };
                        sub_conds.push(format!(
                            "LOWER(tc.instrument) LIKE LOWER('%{}%')",
                            motif.replace('\'', "''")
                        ));
                    }
                    if !sub_conds.is_empty() {
                        conditions.push(format!(
                            "al.id IN (SELECT DISTINCT t2.album_id FROM tracks t2 \
                             JOIN track_credits tc ON tc.track_id = t2.id WHERE {})",
                            sub_conds.join(" AND ")
                        ));
                    }
                }
            }
            continue;
        }

        // --- added_at / last_played_at use timestamp logic ---
        if field == "added_at" {
            let ts = resolve_timestamp_sql(&value);
            // NB: "greater_than"/"less_than" normalize to ">="/"<=" above, so
            // both spellings must be matched here — the seeded "🆕 Récents"
            // (added_at greater_than 90d) used to fall through `_ => continue`,
            // dropping its only rule and matching the ENTIRE library.
            let cond = match op {
                ">" | ">=" => format!(
                    "al.id IN (SELECT DISTINCT t2.album_id FROM tracks t2 \
                     WHERE DATETIME(t2.file_mtime, 'unixepoch') {op} {ts})"
                ),
                "<" | "<=" => format!(
                    "al.id IN (SELECT DISTINCT t2.album_id FROM tracks t2 \
                     WHERE DATETIME(t2.file_mtime, 'unixepoch') {op} {ts})"
                ),
                "between" => {
                    if let Some(arr) = value_raw.and_then(|v| v.as_array()) {
                        let lo = arr.first().and_then(|v| v.as_str()).unwrap_or("2000-01-01");
                        let hi = arr.get(1).and_then(|v| v.as_str()).unwrap_or("2099-01-01");
                        let lo_sql = resolve_timestamp_sql(lo);
                        let hi_sql = resolve_timestamp_sql(hi);
                        format!(
                            "al.id IN (SELECT DISTINCT t2.album_id FROM tracks t2 \
                             WHERE DATETIME(t2.file_mtime, 'unixepoch') BETWEEN {lo_sql} AND {hi_sql})"
                        )
                    } else {
                        continue;
                    }
                }
                _ => continue,
            };
            conditions.push(cond);
            continue;
        }

        if field == "play_count" || field == "last_played_at" {
            let int_v = value.parse::<i64>().unwrap_or(0);
            let cond = match (field, op) {
                ("play_count", "=" | "==") if int_v == 0 => format!(
                    "al.id NOT IN (SELECT DISTINCT t3.album_id FROM tracks t3 \
                     JOIN listen_history lh ON lh.track_id = t3.id)"
                ),
                ("play_count", "=") => format!(
                    "al.id IN (SELECT t3.album_id FROM tracks t3 \
                     JOIN listen_history lh ON lh.track_id = t3.id \
                     GROUP BY t3.album_id HAVING COUNT(*) = {int_v})"
                ),
                ("play_count", ">=") => format!(
                    "al.id IN (SELECT t3.album_id FROM tracks t3 \
                     JOIN listen_history lh ON lh.track_id = t3.id \
                     GROUP BY t3.album_id HAVING COUNT(*) >= {int_v})"
                ),
                ("play_count", ">") => format!(
                    "al.id IN (SELECT t3.album_id FROM tracks t3 \
                     JOIN listen_history lh ON lh.track_id = t3.id \
                     GROUP BY t3.album_id HAVING COUNT(*) > {int_v})"
                ),
                ("play_count", "<") => format!(
                    "al.id NOT IN (SELECT t3.album_id FROM tracks t3 \
                     JOIN listen_history lh ON lh.track_id = t3.id \
                     GROUP BY t3.album_id HAVING COUNT(*) >= {int_v})"
                ),
                ("last_played_at", ">" | ">=") => {
                    let ts = resolve_timestamp_sql(&value);
                    format!(
                        "al.id IN (SELECT t3.album_id FROM tracks t3 \
                         JOIN listen_history lh ON lh.track_id = t3.id \
                         WHERE lh.listened_at {op} {ts} GROUP BY t3.album_id)"
                    )
                }
                ("last_played_at", "<" | "<=") => {
                    let ts = resolve_timestamp_sql(&value);
                    format!(
                        "al.id IN (SELECT t3.album_id FROM tracks t3 \
                         JOIN listen_history lh ON lh.track_id = t3.id \
                         GROUP BY t3.album_id HAVING MAX(lh.listened_at) {op} {ts})"
                    )
                }
                ("last_played_at", "is_null") => format!(
                    "al.id NOT IN (SELECT DISTINCT t3.album_id FROM tracks t3 \
                     JOIN listen_history lh ON lh.track_id = t3.id)"
                ),
                _ => continue,
            };
            conditions.push(cond);
            continue;
        }

        let col = match field {
            "genre" => "t.genre",
            "artist" | "artist_name" => "ar.name",
            "album" | "album_title" | "title" => "al.title",
            "composer" => "t.composer",
            "label" => "al.label",
            "format" => "t.format",
            "source" => "t.source",
            // #REPERTOIRE — la LOCALISATION de la piste sur le disque. Le seul
            // champ dont la valeur est un CHEMIN, donc le seul qui porte des
            // antislashs sur Windows : c'est lui qui rend l'échappement `LIKE`
            // ci-dessous obligatoire et non cosmétique (#1752).
            "folder" | "file_path" => "t.file_path",
            "cover_path" => "al.cover_path",
            // Fall back to the track year: many albums have a NULL al.year even
            // though the tracks carry the year in their tags (Elie — genre, a
            // track-level field, matched but year, album-level, didn't).
            "year" => "CAST(COALESCE(al.year, t.year) AS INTEGER)",
            "sample_rate" => "CAST(t.sample_rate AS INTEGER)",
            "bit_depth" => "CAST(t.bit_depth AS INTEGER)",
            "track_count" => "al.track_count",
            "duration" => "CAST(t.duration_ms AS INTEGER)",
            "track_number" => "CAST(t.track_number AS INTEGER)",
            "disc_number" => "CAST(t.disc_number AS INTEGER)",
            "bpm" => "CAST(t.bpm AS INTEGER)",
            "rating" => "CAST(t.rating AS INTEGER)",
            _ => continue,
        };

        let is_text = matches!(
            field,
            "genre"
                | "artist"
                | "artist_name"
                | "album"
                | "album_title"
                | "title"
                | "composer"
                | "label"
                | "format"
                | "source"
                | "folder"
                | "file_path"
        );
        let int_val = || value.parse::<i64>().unwrap_or(0);

        let cond = match op {
            "=" if is_text => format!("LOWER({col}) = LOWER('{esc}')"),
            "!=" if is_text => format!("LOWER({col}) != LOWER('{esc}')"),
            "contains" => format!("LOWER({col}) LIKE LOWER('%{esc_like}%'){esc_clause}"),
            "starts_with" => format!("LOWER({col}) LIKE LOWER('{esc_like}%'){esc_clause}"),
            "is_null" => format!("({col} IS NULL OR {col} = '')"),
            "is_not_null" => format!("({col} IS NOT NULL AND {col} != '')"),
            "=" => format!("{col} = {}", int_val()),
            "!=" => format!("{col} != {}", int_val()),
            ">=" => format!("{col} >= {}", int_val()),
            ">" => format!("{col} > {}", int_val()),
            "<=" => format!("{col} <= {}", int_val()),
            "<" => format!("{col} < {}", int_val()),
            "between" => {
                if let Some(arr) = value_raw.and_then(|v| v.as_array()) {
                    let lo = arr.first().and_then(|v| v.as_i64()).unwrap_or(0);
                    let hi = arr.get(1).and_then(|v| v.as_i64()).unwrap_or(i64::MAX);
                    format!("{col} BETWEEN {lo} AND {hi}")
                } else {
                    let parts: Vec<&str> = value.splitn(2, ',').collect();
                    if parts.len() == 2 {
                        let lo = parts[0].trim().parse::<i64>().unwrap_or(0);
                        let hi = parts[1].trim().parse::<i64>().unwrap_or(i64::MAX);
                        format!("{col} BETWEEN {lo} AND {hi}")
                    } else {
                        format!("{col} = {}", int_val())
                    }
                }
            }
            "in" => {
                if let Some(arr) = value_raw.and_then(|v| v.as_array()) {
                    let items: Vec<String> = arr
                        .iter()
                        .map(|v| {
                            if let Some(s) = v.as_str() {
                                format!("'{}'", s.replace('\'', "''"))
                            } else {
                                v.to_string()
                            }
                        })
                        .collect();
                    format!("{col} IN ({})", items.join(","))
                } else {
                    let items: Vec<String> = value
                        .split(',')
                        .map(|s| format!("'{}'", s.trim().replace('\'', "''")))
                        .collect();
                    format!("LOWER({col}) IN ({})", items.join(","))
                }
            }
            _ => continue,
        };
        conditions.push(cond);
    }

    let joiner = if match_mode == "any" { " OR " } else { " AND " };

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(joiner))
    };

    let order = if sort_by == "random" {
        "ORDER BY RANDOM()".to_string()
    } else {
        format!(
            "ORDER BY {} {}",
            match sort_by {
                "artist" | "artist_name" => "ar.name",
                "album" | "title" => "al.title",
                "year" => "al.year",
                "added_at" => "al.id",
                "track_count" => "track_count",
                "sample_rate" => "t.sample_rate",
                "label" => "al.label",
                _ => "al.title",
            },
            if sort_order == "desc" { "DESC" } else { "ASC" }
        )
    };

    // 🔴 #2732 — `filter(|n| *n > 0)` est la moitié LECTURE de la garde posée à
    // l'écriture par `borne_valide`. Les lignes déjà enregistrées avec `0` ou
    // une valeur négative — écrites avant cette garde — rendaient une
    // collection VIDE sur SQLite et un 500 sur PostgreSQL. Elles se lisent
    // désormais comme « pas de borne », ce qui est le seul repli qui ne perde
    // aucun album ; les nouvelles écritures, elles, sont refusées à la porte.
    let limit_clause = max_limit
        .filter(|n| *n > 0)
        .map(|n| format!("LIMIT {n}"))
        .unwrap_or_default();

    (where_clause, order, limit_clause)
}

/// Execute a smart album query and return album rows as JSON values.
fn execute_album_query(
    state: &SmartHttpState,
    where_clause: &str,
    order: &str,
    limit_clause: &str,
) -> Result<Vec<Value>, AppError> {
    let sql = format!(
        // `al.is_compilation` (#1957) est dans le GROUP BY comme les autres
        // colonnes d'album : PostgreSQL refuse une colonne ni groupée ni
        // agrégée, SQLite l'accepterait. Écrire pour les deux.
        "SELECT al.id, al.title, ar.name, al.year, al.cover_path, al.genre, \
         COUNT(t.id) AS track_count, al.is_compilation \
         FROM albums al \
         LEFT JOIN artists ar ON al.artist_id = ar.id \
         LEFT JOIN tracks t ON t.album_id = al.id \
         {} \
         GROUP BY al.id, al.title, ar.name, al.year, al.cover_path, al.genre, al.is_compilation \
         {} {}",
        where_clause, order, limit_clause
    );
    tracing::debug!(sql = %sql, "smart_collection_album_query");

    let rows = state
        .backend
        .query_many(&sql, &[])
        .map_err(AppError::internal)?;

    Ok(rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "title": r.get(1).and_then(|v| v.as_string()),
                "artist_name": r.get(2).and_then(|v| v.as_string()),
                "year": r.get(3).and_then(|v| v.as_i64()),
                "cover_path": r.get(4).and_then(|v| v.as_string()),
                "genre": r.get(5).and_then(|v| v.as_string()),
                "track_count": r.get(6).and_then(|v| v.as_i64()).unwrap_or(0),
                // Même nom et même décodeur que `Album::to_json` (#1957) :
                // une collection intelligente rend le drapeau comme la liste
                // d'albums. NUL ⇒ `false`, jamais `null`.
                "is_compilation": tune_core::db::album_repo::drapeau_compilation(r.get(7)),
            })
        })
        .collect())
}

/// Ajoute les ALBUMS favoris des services que nomme une règle « Source »
/// (#4299) — voir `source_streaming`. Les albums de la bibliothèque d'abord,
/// puis ceux des services ; la borne s'applique à l'ensemble.
#[allow(clippy::too_many_arguments)]
fn avec_albums_de_service(
    state: &SmartHttpState,
    mut albums: Vec<Value>,
    rules_json: &str,
    match_mode: &str,
    profile_id: i64,
    sort_by: &str,
    sort_order: &str,
    max_limit: Option<i64>,
) -> Result<Vec<Value>, AppError> {
    let Some(sql) = source_streaming::requete(
        rules_json,
        match_mode,
        Objet::Album,
        profile_id,
        sort_by,
        sort_order,
        max_limit,
    ) else {
        return Ok(albums);
    };
    let lignes = state
        .backend
        .query_many(&sql, &[])
        .map_err(AppError::internal)?;
    albums.extend(lignes.iter().map(|c| source_streaming::album_json(c)));
    if let Some(n) = max_limit.filter(|n| *n >= 0) {
        albums.truncate(n as usize);
    }
    Ok(albums)
}

/// Load a smart collection's criteria from the DB.
fn load_collection_criteria(
    state: &SmartHttpState,
    id: i64,
) -> Result<Option<(String, String, String, String, Option<i64>)>, AppError> {
    let row = state
        .backend
        .query_one(
            "SELECT rules, match_mode, sort_by, sort_order, max_limit \
         FROM smart_collections WHERE id = $1",
            &[&id as &dyn ToSqlValue],
        )
        .map_err(AppError::internal)?;

    Ok(row.map(|r| {
        (
            r.get(0)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "[]".into()),
            r.get(1)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "all".into()),
            r.get(2)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "title".into()),
            r.get(3)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "asc".into()),
            r.get(4).and_then(|v| v.as_i64()),
        )
    }))
}

/// Les albums du CATALOGUE d'un service que les règles demandent — #4473.
///
/// Rend `Ok(albums)` inchangé quand aucune règle ne demande de catalogue.
///
/// 🔴 Trois refus EXPLICITES, parce qu'une règle qu'on ne sait pas honorer ne
/// doit ni rendre tout ni rendre vide en silence (leçon de #4469) :
///
/// * aucune cible — ni artiste ni album nommé par une égalité : un service ne
///   sait pas énumérer son catalogue, il n'y a pas de requête à faire ;
/// * pas de registre de services — l'état n'en porte pas ;
/// * le service demandé ne répond pas : là, on rend une liste vide SANS
///   refuser, car une panne de réseau ne doit pas faire échouer une collection
///   qui a par ailleurs des albums locaux.
async fn avec_albums_de_catalogue(
    state: &SmartHttpState,
    mut albums: Vec<Value>,
    rules_json: &str,
    max_limit: Option<i64>,
) -> Result<Vec<Value>, AppError> {
    // 🔴 La lecture des règles est celle de `catalogue::lire`, partagée avec le
    // chemin des PISTES : le service, la cible et les refus se décident à un
    // seul endroit (#4473, second volet).
    let demande = match catalogue::lire(rules_json, catalogue::Objet::Album) {
        catalogue::Lecture::Aucune => return Ok(albums),
        catalogue::Lecture::Refus(motif) => return Err(AppError::bad_request(motif)),
        catalogue::Lecture::Demande(d) => d,
    };
    // 🔴 On CLONE l'Arc au lieu d'en garder une référence : ce qui vit en
    // travers d'un `.await` doit être `Send`, et une référence à l'état ne
    // l'est pas ici. Sans ça, axum refuse le handler tout entier.
    let Some(distant) = state.catalogue.clone() else {
        return Err(AppError::bad_request(
            "Le catalogue des services n'est pas disponible ici.",
        ));
    };

    let service = demande.service;
    let mut trouves = match &demande.cible {
        catalogue::Cible::Artiste(nom) => distant.albums_par_artiste(&service, nom).await,
        catalogue::Cible::Album(titre) => distant.albums_par_titre(&service, titre).await,
    };
    // « artiste Coltrane ET album Blue Train » : la recherche part de
    // l'artiste, le titre trie encore ce qu'elle rend.
    if let (catalogue::Cible::Artiste(_), Some(titre)) = (&demande.cible, &demande.titre_album) {
        let titre = titre.to_lowercase();
        trouves.retain(|a| a.title.trim().to_lowercase() == titre);
    }
    albums.extend(trouves.into_iter().map(|a| {
        json!({
            "id": Value::Null,
            "source": a.service,
            "source_id": a.source_id,
            "title": a.title,
            "artist_name": a.artist,
            "year": a.year,
            "cover_path": a.cover_url,
            "genre": Value::Null,
            "track_count": 0,
            "is_compilation": false,
        })
    }));
    if let Some(n) = max_limit.filter(|n| *n >= 0) {
        albums.truncate(n as usize);
    }
    Ok(albums)
}

async fn resolve_albums(
    State(state): State<SmartHttpState>,
    profile: ActiveProfile,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let Some((rules_json, match_mode, sort_by, sort_order, max_limit)) =
        load_collection_criteria(&state, id)?
    else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };

    // Le résolveur et son contexte tiennent des RÉFÉRENCES à l'état : ils
    // doivent mourir avant le `.await` du catalogue, sinon le futur n'est plus
    // `Send` et axum refuse le handler.
    let (where_clause, order, limit_clause) = {
        let resolver = DbRefResolver::new(&state.backend);
        let ctx = RefCtx::root(&resolver, Some(profile.id()));
        build_album_query(
            &rules_json,
            &match_mode,
            &sort_by,
            &sort_order,
            max_limit,
            &ctx,
        )
    };
    let albums = execute_album_query(&state, &where_clause, &order, &limit_clause)?;
    let albums = avec_albums_de_service(
        &state,
        albums,
        &rules_json,
        &match_mode,
        profile.id(),
        &sort_by,
        &sort_order,
        max_limit,
    )?;
    let albums = avec_albums_de_catalogue(&state, albums, &rules_json, max_limit).await?;

    // Return a bare array, matching the regular collections endpoint
    // (GET /library/collections/{id}/albums). The previous {"albums":[…],
    // "total":N} wrapper made the iOS client fail to decode
    // (DecodingError.typeMismatch: expected Array, found dictionary) when
    // opening a smart collection in remote mode; the web client already
    // accepts either shape.
    Ok(Json(albums).into_response())
}

async fn preview_albums(
    State(state): State<SmartHttpState>,
    profile: ActiveProfile,
    Json(body): Json<PreviewRequest>,
) -> Result<Json<Value>, AppError> {
    // La prévisualisation doit refuser la même borne que l'enregistrement :
    // sinon l'écran montrerait un aperçu que la collection enregistrée ne sait
    // pas reproduire (#2732).
    borne_valide(body.max_limit)?;
    let rules_json = body.rules.to_string();
    let match_mode = body.match_mode.as_deref().unwrap_or("all");
    let sort_by = body.sort_by.as_deref().unwrap_or("title");
    let sort_order = body.sort_order.as_deref().unwrap_or("asc");

    // Les références à l'état meurent avant le `.await` du catalogue (voir
    // `resolve_albums`).
    let (where_clause, order, limit_clause) = {
        let resolver = DbRefResolver::new(&state.backend);
        let ctx = RefCtx::root(&resolver, Some(profile.id()));
        build_album_query(
            &rules_json,
            match_mode,
            sort_by,
            sort_order,
            body.max_limit,
            &ctx,
        )
    };
    let albums = execute_album_query(&state, &where_clause, &order, &limit_clause)?;
    let albums = avec_albums_de_service(
        &state,
        albums,
        &rules_json,
        match_mode,
        profile.id(),
        sort_by,
        sort_order,
        body.max_limit,
    )?;
    // 🔴 #4473 — l'aperçu de l'éditeur est ce que la collection rendra : sans
    // cet appel, une règle « catalogue » s'y montrait VIDE et sans refus, puis
    // la collection enregistrée rendait des albums, ou un 400.
    let albums = avec_albums_de_catalogue(&state, albums, &rules_json, body.max_limit).await?;

    Ok(Json(json!({"albums": albums, "total": albums.len()})))
}

#[cfg(test)]
mod tests {
    use super::{build_album_query, normalize_sort_order, resolve_timestamp_sql};
    use crate::smart_refs::{EmptyResolver, RefCtx};

    /// Le champ « répertoire » compile sur `t.file_path`, et sur lui seul.
    ///
    /// C'est la colonne qui porte la LOCALISATION de la piste. La désigner
    /// autrement — `al.cover_path` est le seul autre chemin du schéma — rendrait
    /// une collection qui trie sur la pochette au lieu du fichier.
    #[test]
    fn le_repertoire_compile_sur_le_chemin_de_la_piste() {
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let rules = r#"[{"field":"folder","operator":"starts_with","value":"/data/music/Jazz"}]"#;
        let (ou, _, _) = build_album_query(rules, "all", "title", "asc", None, &ctx);
        assert!(ou.contains("t.file_path"), "{ou}");
        assert!(ou.contains("/data/music/Jazz%"), "préfixe attendu : {ou}");
    }

    /// ⭐ #1752 — un chemin WINDOWS doit survivre au compilateur.
    ///
    /// Postgres traite l'antislash comme son caractère d'échappement dans
    /// `LIKE` ; SQLite n'en a aucun. Un motif brut `G:\Jazz - Vocal\%` se
    /// dégrade donc, côté Postgres SEULEMENT, en la chaîne littérale
    /// `G:Jazz - Vocal%` — qui ne correspond à rien. C'est ce qui avait fait
    /// afficher « Dossier vide — 0 pistes » aux quatre racines de JF Paquet sur
    /// une bibliothèque parfaitement scannée.
    ///
    /// Le défaut n'était visible que pour un utilisateur Windows **ET**
    /// Postgres : les Windows sont en SQLite, les serveurs PG sont sous Linux
    /// avec des chemins POSIX. D'où des mois d'invisibilité — et la raison pour
    /// laquelle cette garde est textuelle plutôt que fonctionnelle : reproduire
    /// la panne demanderait un Postgres ET des chemins Windows.
    ///
    /// Les deux moitiés du contrat sont indissociables et doivent être
    /// vérifiées ENSEMBLE : la clause dit « l'antislash échappe », la valeur
    /// doit donc doubler les siens. L'une sans l'autre est pire que rien.
    #[test]
    fn un_chemin_windows_survit_au_compilateur() {
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let rules = r#"[{"field":"folder","operator":"starts_with","value":"G:\\Jazz - Vocal"}]"#;
        let (ou, _, _) = build_album_query(rules, "all", "title", "asc", None, &ctx);
        assert!(
            ou.contains(r"G:\\Jazz - Vocal"),
            "l'antislash doit sortir DOUBLÉ, sinon Postgres l'avale : {ou}"
        );
        assert!(
            ou.contains(r"ESCAPE '\'"),
            "sans la clause, doubler l'antislash le rend visible au lieu de \
             littéral sur SQLite : {ou}"
        );
    }

    /// #3101 — les jokers de `LIKE` sont neutralisés dans la VALEUR.
    ///
    /// Sans ça, un filtre rend PLUS que demandé : `%` avale n'importe quelle
    /// suite, donc « contient 100% Live » ramenait aussi `1000 Autres`. Sur des
    /// bibliothèques de dizaines de milliers de fichiers, `%` et `_` sont
    /// partout dans les noms de dossiers.
    #[test]
    fn les_jokers_ne_sont_plus_des_jokers() {
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let rules = r#"[{"field":"title","operator":"contains","value":"100% Live"}]"#;
        let (ou, _, _) = build_album_query(rules, "all", "title", "asc", None, &ctx);
        assert!(
            ou.contains(r"100\% Live"),
            "le % doit être neutralisé : {ou}"
        );
    }

    /// TÉMOIN — l'ÉGALITÉ, elle, ne neutralise rien.
    ///
    /// C'est la moitié qu'on casse en corrigeant l'autre trop largement : `=`
    /// compare des chaînes entières, `%` n'y est pas un joker. Y appliquer
    /// l'échappement ferait comparer `100\% Live` à `100% Live`, deux chaînes
    /// qui ne se ressemblent plus — un titre qui matchait cesserait de matcher.
    #[test]
    fn l_egalite_ne_neutralise_pas_les_jokers() {
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let rules = r#"[{"field":"title","operator":"=","value":"100% Live"}]"#;
        let (ou, _, _) = build_album_query(rules, "all", "title", "asc", None, &ctx);
        assert!(
            ou.contains("100% Live") && !ou.contains(r"100\% Live"),
            "l'égalité doit comparer la valeur TELLE QUELLE : {ou}"
        );
        assert!(
            !ou.contains("ESCAPE"),
            "une égalité n'est pas un LIKE, elle n'a pas de clause ESCAPE : {ou}"
        );
    }

    #[test]
    fn resolve_timestamp_relative_forms() {
        // "now-Nd" (editor form) and bare "Nd" (seeded "🆕 Récents") are both
        // N-days-ago; anything else stays a quoted literal.
        assert_eq!(
            resolve_timestamp_sql("now-30d"),
            "DATETIME('now', '-30 days')"
        );
        assert_eq!(resolve_timestamp_sql("90d"), "DATETIME('now', '-90 days')");
        assert_eq!(resolve_timestamp_sql("90"), "DATETIME('now', '-90 days')");
        assert_eq!(resolve_timestamp_sql("2024-01-01"), "'2024-01-01'");
    }

    #[test]
    fn added_at_greater_than_compiles_instead_of_matching_everything() {
        // Seeded "🆕 Récents": greater_than normalizes to ">=", which the
        // added_at branch used to drop entirely — empty WHERE — so the
        // collection counted the ENTIRE library.
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let rules = r#"[{"field":"added_at","operator":"greater_than","value":"90d"}]"#;
        let (where_clause, _, _) = build_album_query(rules, "all", "title", "asc", None, &ctx);
        assert!(
            where_clause.contains("DATETIME('now', '-90 days')"),
            "added_at rule must compile: {where_clause}"
        );
        assert!(where_clause.contains(">="));
    }

    /// #2799 §4 — la règle `credit`/`instrument` doit chercher le MÊME libellé
    /// que celui que l'enrichissement écrit.
    ///
    /// L'enrichissement range désormais `grand piano` sous `piano` : une règle
    /// qui compilerait le libellé BRUT (`%grand piano%`) ne trouverait plus
    /// aucune ligne, alors que les crédits sont bien là. Deux normalisations
    /// des deux côtés, et la collection reste vide sans rien signaler.
    #[test]
    fn regle_credit_instrument_canonisee_comme_a_l_ecriture() {
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let rules = r#"[{"field":"credit","operator":"has","value":{"instrument":"Grand Piano"}}]"#;
        let (where_clause, _, _) = build_album_query(rules, "all", "title", "asc", None, &ctx);
        assert!(
            where_clause.contains("LOWER(tc.instrument) LIKE LOWER('%piano%')"),
            "l'instrument doit etre canonise avant compilation : {where_clause}"
        );
        assert!(
            !where_clause.contains("grand piano"),
            "le libelle brut ne doit plus servir de motif : {where_clause}"
        );
    }

    /// TÉMOIN ANTI-RÉGRESSION : les deux autres clés de la règle `credit` sont
    /// intactes — seul `instrument` est canonisé. Un nom d'artiste passé à la
    /// moulinette des instruments serait détruit.
    #[test]
    fn temoin_regle_credit_role_et_artiste_inchanges() {
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let rules = r#"[{"field":"credit","operator":"has","value":{"role":"producer","artist_name":"Teo Macero"}}]"#;
        let (where_clause, _, _) = build_album_query(rules, "all", "title", "asc", None, &ctx);
        assert!(
            where_clause.contains("LOWER(tc.role) LIKE LOWER('%producer%')"),
            "{where_clause}"
        );
        assert!(
            where_clause.contains("LOWER(tc.artist_name) LIKE LOWER('%Teo Macero%')"),
            "{where_clause}"
        );
        assert!(
            where_clause.contains("JOIN track_credits tc ON tc.track_id = t2.id"),
            "{where_clause}"
        );
    }

    #[test]
    fn is_not_empty_alias_compiles() {
        // tune-core spelling ("is_not_empty") used to be dropped — the seeded
        // "🖼️ Sans pochette" placeholder rule then matched the whole library.
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let rules = r#"[{"field":"cover_path","operator":"is_empty","value":""}]"#;
        let (where_clause, _, _) = build_album_query(rules, "all", "title", "asc", None, &ctx);
        assert!(
            where_clause.contains("al.cover_path IS NULL"),
            "is_empty alias must compile: {where_clause}"
        );

        let rules = r#"[{"field":"format","operator":"is_not_empty","value":""}]"#;
        let (where_clause, _, _) = build_album_query(rules, "all", "title", "asc", None, &ctx);
        assert!(where_clause.contains("t.format IS NOT NULL"));
    }

    #[test]
    fn any_mode_joins_with_or() {
        // Raw 'any' from the seed rows (unquoted in DB) must keep OR semantics;
        // the legacy tune-core engine silently fell back to ALL.
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let rules = r#"[{"field":"genre","operator":"contains","value":"soul"},
                        {"field":"genre","operator":"contains","value":"funk"}]"#;
        let (where_clause, _, _) = build_album_query(rules, "any", "title", "asc", None, &ctx);
        assert!(where_clause.contains(" OR "), "{where_clause}");
        assert!(!where_clause.contains(" AND "));
    }

    #[test]
    fn artist_name_field_compiles() {
        // Web-editor rules use field "artist_name" and op "=" (Coltrane); the
        // legacy engine fell back to t.title and matched nothing.
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let rules = r#"[{"field":"artist_name","op":"=","value":"John Coltrane"}]"#;
        let (where_clause, _, _) = build_album_query(rules, "all", "random", "desc", None, &ctx);
        assert!(
            where_clause.contains("LOWER(ar.name) = LOWER('John Coltrane')"),
            "{where_clause}"
        );
    }

    #[test]
    fn favorite_rule_flows_into_album_where_clause() {
        let ctx = RefCtx::root(&EmptyResolver, Some(2));
        let (w, _o, _l) = build_album_query(
            r#"[{"field":"favorite","op":"is","value":"album"}]"#,
            "all",
            "title",
            "asc",
            None,
            &ctx,
        );
        assert!(w.contains("favorites"), "{w}");
        assert!(w.contains("profile_id = 2"), "{w}");
    }

    #[test]
    fn ref_rule_combines_with_classic_rule() {
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let (w, _o, _l) = build_album_query(
            r#"[{"field":"genre","op":"contains","value":"Jazz"},
                {"field":"in_playlist","op":"in","value":"classic:4"}]"#,
            "all",
            "title",
            "asc",
            None,
            &ctx,
        );
        assert!(w.contains(" AND "), "{w}");
        assert!(w.contains("playlist_tracks"), "{w}");
    }

    /// #1426 (Jean Valjean, forum « F5 obligatoire ») : « Dans la Smart
    /// Collection "World Music" [il] n'a pas les bons albums, c'est un peu
    /// mélangé (Folk, Folk Métal, Folk Rock) ».
    ///
    /// Le défaut est dans le PRÉRÉGLAGE, pas dans le moteur : `contient folk`
    /// compile en `LIKE '%folk%'`, ce qui ramasse « Folk Metal » et « Folk
    /// Rock » par construction. La migration SQLite 93 (jumelle PG 045) resserre
    /// `folk` en égalité stricte et laisse `world` / `ethnic` en « contient ».
    ///
    /// On rejoue la chaîne ENTIÈRE, parce que c'est le seul niveau où le défaut
    /// est visible : migrations tune-core → règles LUES EN BASE →
    /// `build_album_query` (le compilateur qui sert réellement l'écran, et non
    /// celui de `tune-core/library/smart_collections.rs`) → SQL exécuté sur une
    /// bibliothèque témoin. Un test qui se contenterait de comparer la chaîne
    /// de règles ne dirait rien de ce que l'utilisateur voit.
    #[test]
    fn le_prereglage_world_music_ne_ramasse_plus_folk_metal_ni_folk_rock() {
        use tune_core::db::backend::DbBackend;
        use tune_core::db::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();

        // Un album par genre, une piste chacun — le genre vit sur la PISTE
        // (`t.genre`), c'est la colonne que compile la règle.
        db.execute_batch(
            "INSERT INTO albums (id, title) VALUES \
               (1,'Kanyaleng'),(2,'Ethnic Jazz Session'),(3,'Chants de Bretagne'), \
               (4,'Tuonela'),(5,'Sweetheart of the Rodeo'),(6,'Kind of Blue'); \
             INSERT INTO tracks (album_id, title, genre, file_path) VALUES \
               (1,'a','World','/m/1.flac'), \
               (2,'b','Ethnic Jazz','/m/2.flac'), \
               (3,'c','Folk','/m/3.flac'), \
               (4,'d','Folk Metal','/m/4.flac'), \
               (5,'e','Folk Rock','/m/5.flac'), \
               (6,'f','Jazz','/m/6.flac');",
        )
        .unwrap();

        // Les règles telles qu'elles SONT EN BASE après migrations — pas une
        // copie recollée ici, sinon le test ne garde plus le préréglage livré.
        let rows = db
            .query_many(
                "SELECT rules, match_mode FROM smart_collections WHERE name LIKE '%World%'",
                &[],
            )
            .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "le préréglage « World Music » doit exister en un seul exemplaire"
        );
        let regles = rows[0][0].as_string().unwrap_or_default();
        let mode = rows[0][1].as_string().unwrap_or_default();

        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let (where_clause, order, _) =
            build_album_query(&regles, &mode, "title", "asc", None, &ctx);
        let sql = format!(
            "SELECT al.title FROM albums al \
             LEFT JOIN artists ar ON al.artist_id = ar.id \
             LEFT JOIN tracks t ON t.album_id = al.id \
             {where_clause} GROUP BY al.id, al.title {order}"
        );
        let titres: Vec<String> = db
            .query_many(&sql, &[])
            .unwrap()
            .iter()
            .map(|r| r[0].as_string().unwrap_or_default())
            .collect();

        for attendu in ["Kanyaleng", "Ethnic Jazz Session", "Chants de Bretagne"] {
            assert!(
                titres.iter().any(|t| t == attendu),
                "« {attendu} » doit rester dans World Music : {titres:?}\n{sql}"
            );
        }
        for indesirable in ["Tuonela", "Sweetheart of the Rodeo", "Kind of Blue"] {
            assert!(
                !titres.iter().any(|t| t == indesirable),
                "« {indesirable} » n'a rien à faire dans World Music (#1426) : \
                 {titres:?}\n{sql}"
            );
        }
    }

    #[test]
    fn normalize_sort_order_tolerates_encodings() {
        // Raw form (this route's save path).
        assert_eq!(normalize_sort_order(Some("asc".into())), "asc");
        assert_eq!(normalize_sort_order(Some("desc".into())), "desc");
        // Legacy JSON-encoded form (tune-core save path) — the bug source.
        assert_eq!(normalize_sort_order(Some("\"asc\"".into())), "asc");
        assert_eq!(normalize_sort_order(Some("\"desc\"".into())), "desc");
        // Missing / empty -> default.
        assert_eq!(normalize_sort_order(None), "asc");
        assert_eq!(normalize_sort_order(Some(String::new())), "asc");
    }

    /// 🔴 #1231 — le compteur d'une collection compte AUSSI les favoris de
    /// service.
    ///
    /// Mesuré sur le .18 le 19/09/2026 : une collection dont la seule règle est
    /// `source = qobuz` rend ses 3 albums quand on l'ouvre, et la liste des
    /// collections annonçait `"album_count": 0`. La liste était juste, son
    /// compteur mentait — c'est le « retourne 0 album » de Bertrand.
    ///
    /// L'épreuve passe par le HANDLER, avec une vraie base : une garde
    /// textuelle laissait passer le débranchement (`en_base + en_service`
    /// remplacé par `en_base`) sans rien voir.
    #[test]
    fn le_compteur_voit_les_favoris_de_service() {
        use super::compte_albums;
        use std::sync::Arc;
        use tune_core::db::backend::ToSqlValue;
        use tune_core::db::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().expect("base");
        db.init_schema().expect("schéma");
        db.connection()
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS streaming_favorites (
                     id INTEGER PRIMARY KEY AUTOINCREMENT, profile_id INTEGER,
                     item_type TEXT, service TEXT, service_id TEXT, title TEXT,
                     artist TEXT, album TEXT, cover_url TEXT, created_at TEXT);",
            )
            .expect("table");
        let backend: Arc<dyn tune_core::db::backend::DbBackend> = Arc::new(db);
        for (t, svc) in [
            ("album", "qobuz"),
            ("album", "qobuz"),
            ("track", "qobuz"),
            ("album", "tidal"),
        ] {
            backend
                .execute(
                    "INSERT INTO streaming_favorites (profile_id, item_type, service, service_id, title) \
                     VALUES (1, ?1, ?2, 'x', 'y')",
                    &[&t as &dyn ToSqlValue, &svc],
                )
                .expect("favori");
        }

        let regles = r#"[{"field":"source","op":"=","value":"qobuz"}]"#;
        // La bibliothèque ne rend rien : aucun album local n'est « qobuz ».
        let vide = "SELECT COUNT(*) FROM albums WHERE 1 = 0";
        assert_eq!(
            compte_albums(&*backend, vide, regles, "all", 1),
            2,
            "les DEUX favoris ALBUM Qobuz doivent être comptés — ni la piste, ni l'album Tidal"
        );

        // Et le compte de la bibliothèque s'y ajoute, il ne le remplace pas.
        let un = "SELECT 5";
        assert_eq!(compte_albums(&*backend, un, regles, "all", 1), 7);

        // Sans règle de service, rien ne s'ajoute.
        let sans = r#"[{"field":"year","op":"=","value":"2025"}]"#;
        assert_eq!(compte_albums(&*backend, un, sans, "all", 1), 5);
    }

    /// 🔴 Et le handler l'APPELLE vraiment.
    ///
    /// La garde précédente éprouve `compte_albums` ; celle-ci éprouve qu'il est
    /// branché. Sans elle, remplacer l'appel par `0 * compte_albums(...)`
    /// passait au vert — le défaut « écrit mais pas branché », en plus petit.
    #[tokio::test]
    async fn la_liste_des_collections_porte_ce_compte() {
        use crate::SmartHttpState;
        use std::sync::Arc;
        use tune_core::db::backend::ToSqlValue;
        use tune_core::db::sqlite::SqliteDb;
        use tune_http_types::ActiveProfile;

        let db = SqliteDb::open_in_memory().expect("base");
        db.init_schema().expect("schéma");
        db.connection()
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS smart_collections (
                     id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, rules TEXT,
                     match_mode TEXT, sort_by TEXT, sort_order TEXT, max_limit INTEGER,
                     description TEXT, icon TEXT, color TEXT, created_at TEXT);
                 CREATE TABLE IF NOT EXISTS streaming_favorites (
                     id INTEGER PRIMARY KEY AUTOINCREMENT, profile_id INTEGER,
                     item_type TEXT, service TEXT, service_id TEXT, title TEXT,
                     artist TEXT, album TEXT, cover_url TEXT, created_at TEXT);",
            )
            .expect("tables");
        let backend: Arc<dyn tune_core::db::backend::DbBackend> = Arc::new(db);
        backend
            .execute(
                "INSERT INTO smart_collections (name, rules, match_mode, sort_by, sort_order) \
                 VALUES ('Qobuz', ?1, 'all', 'title', 'asc')",
                &[&r#"[{"field":"source","op":"=","value":"qobuz"}]"# as &dyn ToSqlValue],
            )
            .expect("collection");
        backend
            .execute(
                "INSERT INTO smart_collections (name, rules, match_mode, sort_by, sort_order) \
                 VALUES ('Z Coltrane chez Qobuz', ?1, 'all', 'title', 'asc')",
                &[&r#"[{"field":"source","op":"=","value":"catalogue:qobuz"},
                       {"field":"artist","op":"=","value":"John Coltrane"}]"#
                    as &dyn ToSqlValue],
            )
            .expect("collection catalogue");
        for t in ["album", "album", "track"] {
            backend
                .execute(
                    "INSERT INTO streaming_favorites (profile_id, item_type, service, service_id, title) \
                     VALUES (1, ?1, 'qobuz', 'x', 'y')",
                    &[&t as &dyn ToSqlValue],
                )
                .expect("favori");
        }

        let etat = SmartHttpState::new(backend);
        let Ok(reponse) =
            super::list_collections(axum::extract::State(etat), ActiveProfile(1)).await
        else {
            panic!("la liste doit répondre");
        };
        assert_eq!(
            reponse.0[0]["album_count"].as_i64(),
            Some(2),
            "la collection annonce ses favoris ALBUM : {}",
            reponse.0[0]
        );
        // Des FAVORIS, pas un catalogue : rien à annoncer en plus (#4473).
        assert_eq!(reponse.0[0]["catalogue_service"], serde_json::Value::Null);
        // Un catalogue : la liste le DIT, sans le compter (#4473, arbitrage 2).
        assert_eq!(
            reponse.0[1]["catalogue_service"], "qobuz",
            "{}",
            reponse.0[1]
        );
    }

    /// 🔴 #4466 — « 3 albums · 0 piste » : le compteur de PISTES d'une
    /// collection faite de favoris de service.
    ///
    /// `album_count` sait les additionner depuis #4470. `track_count`, lui,
    /// reste le seul `COUNT(DISTINCT t.id)` de la base — et
    /// `streaming_favorites` ne porte aucun nombre de pistes à y ajouter. Le
    /// champ n'est donc plus RENDU quand des favoris de service entrent dans
    /// la collection, au lieu d'annoncer un 0 démenti par la vue.
    ///
    /// L'épreuve passe par le HANDLER : une garde sur la seule fonction de
    /// comptage laisserait passer un débranchement de la condition.
    #[tokio::test]
    async fn le_compte_de_pistes_ne_se_rend_pas_quand_il_serait_partiel() {
        use crate::SmartHttpState;
        use std::sync::Arc;
        use tune_core::db::backend::ToSqlValue;
        use tune_core::db::sqlite::SqliteDb;
        use tune_http_types::ActiveProfile;

        let db = SqliteDb::open_in_memory().expect("base");
        db.init_schema().expect("schéma");
        db.connection()
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS smart_collections (
                     id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, rules TEXT,
                     match_mode TEXT, sort_by TEXT, sort_order TEXT, max_limit INTEGER,
                     description TEXT, icon TEXT, color TEXT, created_at TEXT);
                 CREATE TABLE IF NOT EXISTS streaming_favorites (
                     id INTEGER PRIMARY KEY AUTOINCREMENT, profile_id INTEGER,
                     item_type TEXT, service TEXT, service_id TEXT, title TEXT,
                     artist TEXT, album TEXT, cover_url TEXT, created_at TEXT);",
            )
            .expect("tables");
        let backend: Arc<dyn tune_core::db::backend::DbBackend> = Arc::new(db);
        // « A » d'abord : la liste est triée par nom.
        backend
            .execute(
                "INSERT INTO smart_collections (name, rules, match_mode, sort_by, sort_order) \
                 VALUES ('A Qobuz', ?1, 'all', 'title', 'asc')",
                &[&r#"[{"field":"source","op":"=","value":"qobuz"}]"# as &dyn ToSqlValue],
            )
            .expect("collection de service");
        backend
            .execute(
                "INSERT INTO smart_collections (name, rules, match_mode, sort_by, sort_order) \
                 VALUES ('B Jazz local', ?1, 'all', 'title', 'asc')",
                &[&r#"[{"field":"genre","op":"contains","value":"jazz"}]"# as &dyn ToSqlValue],
            )
            .expect("collection de bibliothèque");
        for t in ["album", "album", "album"] {
            backend
                .execute(
                    "INSERT INTO streaming_favorites (profile_id, item_type, service, service_id, title) \
                     VALUES (1, ?1, 'qobuz', 'x', 'y')",
                    &[&t as &dyn ToSqlValue],
                )
                .expect("favori");
        }

        let etat = SmartHttpState::new(backend);
        let Ok(reponse) =
            super::list_collections(axum::extract::State(etat), ActiveProfile(1)).await
        else {
            panic!("la liste doit répondre");
        };

        let service = &reponse.0[0];
        assert_eq!(
            service["album_count"].as_i64(),
            Some(3),
            "les 3 favoris ALBUM Qobuz sont bien là : {service}"
        );
        assert!(
            service["track_count"].is_null(),
            "un compte de pistes qui ne couvre pas les favoris de service ne doit PAS être rendu — obtenu : {service}"
        );
        assert_eq!(
            service["track_count_partiel"], true,
            "et l'absence doit se dire : {service}"
        );

        // Contre-partie indispensable : sans favori de service, le compte est
        // complet et il se rend. Sans cette moitié, supprimer purement le
        // champ passerait au vert.
        let locale = &reponse.0[1];
        assert!(
            !locale["track_count"].is_null(),
            "une collection de bibliothèque garde son compte de pistes : {locale}"
        );
        assert!(
            locale["track_count_partiel"].is_null(),
            "et ne s'annonce pas partielle : {locale}"
        );
    }

    /// 🔴 #4473 — le catalogue d'un service, et ses trois refus.
    ///
    /// Un service simulé : la garde porte sur ce que le module DÉCIDE, pas sur
    /// ce que Qobuz répond. Aucun réseau, aucune clé, et le comportement se
    /// mesure quand même.
    mod catalogue_de_service {
        use crate::SmartHttpState;
        use crate::catalogue::{AlbumDistant, CatalogueDistant, PisteDistante};
        use std::sync::Arc;
        use tune_core::db::sqlite::SqliteDb;

        struct ServiceSimule;

        #[async_trait::async_trait]
        impl CatalogueDistant for ServiceSimule {
            async fn albums_par_artiste(&self, service: &str, nom: &str) -> Vec<AlbumDistant> {
                vec![AlbumDistant {
                    service: service.into(),
                    source_id: "a1".into(),
                    title: format!("Best of {nom}"),
                    artist: nom.into(),
                    cover_url: None,
                    year: Some(1960),
                }]
            }
            async fn albums_par_titre(&self, service: &str, titre: &str) -> Vec<AlbumDistant> {
                vec![AlbumDistant {
                    service: service.into(),
                    source_id: "a2".into(),
                    title: titre.into(),
                    artist: "X".into(),
                    cover_url: None,
                    year: None,
                }]
            }
            async fn pistes_par_artiste(&self, _s: &str, _n: &str) -> Vec<PisteDistante> {
                Vec::new()
            }
            async fn pistes_par_album(&self, _s: &str, _t: &str) -> Vec<PisteDistante> {
                Vec::new()
            }
        }

        fn etat(avec_service: bool) -> SmartHttpState {
            let db = SqliteDb::open_in_memory().expect("base");
            db.init_schema().expect("schéma");
            let backend: Arc<dyn tune_core::db::backend::DbBackend> = Arc::new(db);
            let e = SmartHttpState::new(backend);
            if avec_service {
                e.avec_catalogue(Arc::new(ServiceSimule))
            } else {
                e
            }
        }

        const AVEC_ARTISTE: &str = r#"[{"field":"source","op":"=","value":"catalogue:qobuz"},
                                       {"field":"artist","op":"=","value":"John Coltrane"}]"#;

        #[tokio::test]
        async fn les_albums_du_service_rejoignent_ceux_de_la_bibliotheque() {
            let locaux = vec![serde_json::json!({"id": 1, "title": "Un album local"})];
            let r = super::super::avec_albums_de_catalogue(&etat(true), locaux, AVEC_ARTISTE, None)
                .await;
            let Ok(r) = r else {
                panic!("le catalogue doit répondre")
            };
            assert_eq!(r.len(), 2, "le local et le distant : {r:?}");
            assert_eq!(r[1]["source"], "qobuz");
            assert_eq!(r[1]["title"], "Best of John Coltrane");
            assert_eq!(
                r[1]["id"],
                serde_json::Value::Null,
                "un album distant n'a pas d'id local"
            );
        }

        #[tokio::test]
        async fn sans_regle_de_catalogue_rien_ne_change() {
            let locaux = vec![serde_json::json!({"id": 1})];
            let r = super::super::avec_albums_de_catalogue(
                &etat(true),
                locaux.clone(),
                r#"[{"field":"year","op":"=","value":"2025"}]"#,
                None,
            )
            .await;
            let Ok(r) = r else {
                panic!("aucun catalogue demandé")
            };
            assert_eq!(r, locaux);
        }

        #[tokio::test]
        async fn sans_cible_on_refuse_au_lieu_de_rendre_tout_ou_rien() {
            // « catalogue Qobuz ET année 2025 » : aucun service ne sait
            // énumérer son catalogue. Refuser est la seule réponse honnête —
            // rendre vide ferait croire à une bibliothèque sans rien, rendre
            // tout est le défaut de #4469.
            let sans = r#"[{"field":"source","op":"=","value":"catalogue:qobuz"},
                           {"field":"year","op":"=","value":"2025"}]"#;
            let r =
                super::super::avec_albums_de_catalogue(&etat(true), Vec::new(), sans, None).await;
            assert!(r.is_err(), "doit refuser");
        }

        #[tokio::test]
        async fn sans_registre_on_refuse_aussi() {
            let r = super::super::avec_albums_de_catalogue(
                &etat(false),
                Vec::new(),
                AVEC_ARTISTE,
                None,
            )
            .await;
            assert!(r.is_err(), "sans service, on ne fait pas semblant");
        }

        /// L'aperçu de l'éditeur, par la route elle-même.
        async fn apercu(
            regles: &str,
        ) -> Result<axum::Json<serde_json::Value>, tune_http_types::AppError> {
            let e = etat(true);
            e.backend
                .execute(
                    "CREATE TABLE IF NOT EXISTS streaming_favorites (
                         id INTEGER PRIMARY KEY AUTOINCREMENT, profile_id INTEGER,
                         item_type TEXT, service TEXT, service_id TEXT, title TEXT,
                         artist TEXT, album TEXT, cover_url TEXT, created_at TEXT,
                         position TEXT)",
                    &[],
                )
                .expect("favoris");
            super::super::preview_albums(
                axum::extract::State(e),
                tune_http_types::ActiveProfile(1),
                axum::Json(super::super::PreviewRequest {
                    rules: serde_json::from_str(regles).expect("json"),
                    match_mode: None,
                    sort_by: None,
                    sort_order: None,
                    max_limit: None,
                }),
            )
            .await
        }

        /// 🔴 #4473 — l'aperçu montre le catalogue que la collection rendra.
        ///
        /// Rouge avant : `preview_albums` n'appelait pas le catalogue, l'éditeur
        /// affichait zéro album pour « catalogue Qobuz + Coltrane ».
        #[tokio::test]
        async fn l_apercu_de_l_editeur_montre_le_catalogue() {
            let Ok(r) = apercu(AVEC_ARTISTE).await else {
                panic!("l'aperçu doit répondre")
            };
            assert_eq!(r.0["total"], 1, "{}", r.0);
            assert_eq!(r.0["albums"][0]["title"], "Best of John Coltrane");
        }

        /// … et il REFUSE ce que la collection refusera, au moment où on
        /// l'écrit, pas après l'enregistrement.
        #[tokio::test]
        async fn l_apercu_refuse_comme_la_collection() {
            let sans = r#"[{"field":"source","op":"=","value":"catalogue:qobuz"},
                           {"field":"year","op":"=","value":"2025"}]"#;
            assert!(apercu(sans).await.is_err(), "sans cible : refus");
        }

        /// 🔴 Une règle que le service ne sait pas filtrer est refusée — elle
        /// ne s'applique pas en silence au local seulement.
        #[tokio::test]
        async fn une_regle_de_format_a_cote_du_catalogue_est_refusee() {
            let r = r#"[{"field":"source","op":"=","value":"catalogue:qobuz"},
                        {"field":"artist","op":"=","value":"John Coltrane"},
                        {"field":"format","op":"=","value":"FLAC"}]"#;
            let Err(e) =
                super::super::avec_albums_de_catalogue(&etat(true), Vec::new(), r, None).await
            else {
                panic!("format : le service ne sait pas le filtrer")
            };
            // Refusée en la NOMMANT, jamais ignorée en silence (#4473).
            assert!(
                e.message.contains("format ="),
                "le refus doit nommer la règle : {}",
                e.message
            );
        }

        /// Artiste ET titre : le titre trie ce que la recherche par artiste rend.
        #[tokio::test]
        async fn le_titre_trie_la_discographie_de_l_artiste() {
            let r = r#"[{"field":"source","op":"=","value":"catalogue:qobuz"},
                        {"field":"artist","op":"=","value":"John Coltrane"},
                        {"field":"album","op":"=","value":"Blue Train"}]"#;
            let Ok(r) =
                super::super::avec_albums_de_catalogue(&etat(true), Vec::new(), r, None).await
            else {
                panic!("doit répondre")
            };
            assert!(
                r.is_empty(),
                "« Best of John Coltrane » n'est pas « Blue Train » : {r:?}"
            );
        }

        #[tokio::test]
        async fn la_borne_de_la_collection_s_applique_au_distant() {
            let locaux = vec![serde_json::json!({"id": 1}), serde_json::json!({"id": 2})];
            let r =
                super::super::avec_albums_de_catalogue(&etat(true), locaux, AVEC_ARTISTE, Some(2))
                    .await;
            let Ok(r) = r else {
                panic!("le catalogue doit répondre")
            };
            assert_eq!(r.len(), 2, "le plafond vaut pour tout le résultat");
        }
    }
}
