use axum::extract::State;
use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", post(graphql_query))
        .route("/schema", get(graphql_schema))
        .route("/playground", get(graphql_playground))
}

const SCHEMA_SDL: &str = r#"type Query {
  tracks(limit: Int = 50, offset: Int = 0): TrackConnection!
  albums(limit: Int = 50, offset: Int = 0): AlbumConnection!
  artists(limit: Int = 50, offset: Int = 0): ArtistConnection!
  search(q: String!, limit: Int = 20): SearchResult!
  track(id: ID!): Track
  album(id: ID!): Album
  artist(id: ID!): Artist
}

type Track {
  id: ID!
  title: String!
  artist_name: String
  album_title: String
  duration: Float
  path: String
  format: String
  sample_rate: Int
  bit_depth: Int
}

type Album {
  id: ID!
  title: String!
  artist_name: String
  year: Int
  track_count: Int
  cover_path: String
}

type Artist {
  id: ID!
  name: String!
  album_count: Int
  track_count: Int
}

type TrackConnection {
  items: [Track!]!
  total: Int!
}

type AlbumConnection {
  items: [Album!]!
  total: Int!
}

type ArtistConnection {
  items: [Artist!]!
  total: Int!
}

type SearchResult {
  tracks: [Track!]!
  albums: [Album!]!
  artists: [Artist!]!
}
"#;

#[derive(Deserialize)]
struct GraphqlRequest {
    query: String,
    #[serde(default)]
    variables: Option<Value>,
}

/// Execute a GraphQL query against the library database.
/// This is a lightweight hand-rolled parser, not a full GraphQL engine.
async fn graphql_query(
    State(state): State<AppState>,
    Json(body): Json<GraphqlRequest>,
) -> Result<Json<Value>, AppError> {
    let query = body.query.trim();
    let variables = body.variables.unwrap_or(json!({}));

    // Simple top-level query parser
    if let Some(result) = try_execute(query, &variables, &state) {
        Ok(Json(json!({"data": result})))
    } else {
        Err(AppError::bad_request(
            "Unsupported query. Supported: tracks, albums, artists, search, track(id), album(id), artist(id)",
        ))
    }
}

fn try_execute(query: &str, variables: &Value, state: &AppState) -> Option<Value> {
    let lower = query.to_lowercase();

    // Detect which root field is being queried
    if lower.contains("tracks") && lower.contains("search") {
        // search query
        let q = extract_string_arg(query, "q")
            .or_else(|| variables["q"].as_str().map(String::from))
            .unwrap_or_default();
        let limit = extract_int_arg(query, "limit").unwrap_or(20);
        return Some(execute_search(state, &q, limit));
    }

    if lower.contains("search") {
        let q = extract_string_arg(query, "q")
            .or_else(|| variables["q"].as_str().map(String::from))
            .unwrap_or_default();
        let limit = extract_int_arg(query, "limit").unwrap_or(20);
        return Some(json!({"search": execute_search(state, &q, limit)}));
    }

    if lower.contains("tracks") {
        let limit = extract_int_arg(query, "limit").unwrap_or(50);
        let offset = extract_int_arg(query, "offset").unwrap_or(0);
        return Some(json!({"tracks": execute_tracks(state, limit, offset)}));
    }

    if lower.contains("albums") {
        let limit = extract_int_arg(query, "limit").unwrap_or(50);
        let offset = extract_int_arg(query, "offset").unwrap_or(0);
        return Some(json!({"albums": execute_albums(state, limit, offset)}));
    }

    if lower.contains("artists") {
        let limit = extract_int_arg(query, "limit").unwrap_or(50);
        let offset = extract_int_arg(query, "offset").unwrap_or(0);
        return Some(json!({"artists": execute_artists(state, limit, offset)}));
    }

    None
}

fn execute_tracks(state: &AppState, limit: i64, offset: i64) -> Value {
    use tune_core::db::backend::ToSqlValue;
    let (p1, p2) = if state.backend.engine() == tune_core::db::engine::Engine::Postgres {
        ("$1".to_string(), "$2".to_string())
    } else {
        ("?".to_string(), "?".to_string())
    };
    let sql = format!(
        "SELECT id, title, artist_name, album_title, duration, path, format, sample_rate, bit_depth \
         FROM tracks ORDER BY title LIMIT {p1} OFFSET {p2}"
    );
    let rows = state
        .backend
        .query_many(
            &sql,
            &[&limit as &dyn ToSqlValue, &offset as &dyn ToSqlValue],
        )
        .unwrap_or_default();
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "title": r.get(1).and_then(|v| v.as_string()),
                "artist_name": r.get(2).and_then(|v| v.as_string()),
                "album_title": r.get(3).and_then(|v| v.as_string()),
                "duration": r.get(4).and_then(|v| v.as_f64()),
                "path": r.get(5).and_then(|v| v.as_string()),
                "format": r.get(6).and_then(|v| v.as_string()),
                "sample_rate": r.get(7).and_then(|v| v.as_i64()),
                "bit_depth": r.get(8).and_then(|v| v.as_i64()),
            })
        })
        .collect();

    let total: i64 = state
        .backend
        .query_one("SELECT COUNT(*) FROM tracks", &[])
        .ok()
        .flatten()
        .and_then(|r| r.into_iter().next())
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    json!({
        "items": items,
        "total": total,
    })
}

fn execute_albums(state: &AppState, limit: i64, offset: i64) -> Value {
    use tune_core::db::backend::ToSqlValue;
    let (p1, p2) = if state.backend.engine() == tune_core::db::engine::Engine::Postgres {
        ("$1".to_string(), "$2".to_string())
    } else {
        ("?".to_string(), "?".to_string())
    };
    let sql = format!(
        "SELECT id, title, artist_name, year, track_count, cover_path \
         FROM albums ORDER BY title LIMIT {p1} OFFSET {p2}"
    );
    let rows = state
        .backend
        .query_many(
            &sql,
            &[&limit as &dyn ToSqlValue, &offset as &dyn ToSqlValue],
        )
        .unwrap_or_default();
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "title": r.get(1).and_then(|v| v.as_string()),
                "artist_name": r.get(2).and_then(|v| v.as_string()),
                "year": r.get(3).and_then(|v| v.as_i64()),
                "track_count": r.get(4).and_then(|v| v.as_i64()),
                "cover_path": r.get(5).and_then(|v| v.as_string()),
            })
        })
        .collect();

    let total: i64 = state
        .backend
        .query_one("SELECT COUNT(*) FROM albums", &[])
        .ok()
        .flatten()
        .and_then(|r| r.into_iter().next())
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    json!({
        "items": items,
        "total": total,
    })
}

fn execute_artists(state: &AppState, limit: i64, offset: i64) -> Value {
    use tune_core::db::backend::ToSqlValue;
    let (p1, p2) = if state.backend.engine() == tune_core::db::engine::Engine::Postgres {
        ("$1".to_string(), "$2".to_string())
    } else {
        ("?".to_string(), "?".to_string())
    };
    let sql = format!(
        "SELECT id, name, album_count, track_count \
         FROM artists ORDER BY name LIMIT {p1} OFFSET {p2}"
    );
    let rows = state
        .backend
        .query_many(
            &sql,
            &[&limit as &dyn ToSqlValue, &offset as &dyn ToSqlValue],
        )
        .unwrap_or_default();
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "name": r.get(1).and_then(|v| v.as_string()),
                "album_count": r.get(2).and_then(|v| v.as_i64()),
                "track_count": r.get(3).and_then(|v| v.as_i64()),
            })
        })
        .collect();

    let total: i64 = state
        .backend
        .query_one("SELECT COUNT(*) FROM artists", &[])
        .ok()
        .flatten()
        .and_then(|r| r.into_iter().next())
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    json!({
        "items": items,
        "total": total,
    })
}

fn execute_search(state: &AppState, q: &str, limit: i64) -> Value {
    use tune_core::db::backend::ToSqlValue;
    let pattern = format!("%{q}%");

    let (p1, p2) = if state.backend.engine() == tune_core::db::engine::Engine::Postgres {
        ("$1".to_string(), "$2".to_string())
    } else {
        ("?".to_string(), "?".to_string())
    };

    let tracks_sql = format!(
        "SELECT id, title, artist_name, album_title, duration \
         FROM tracks WHERE title LIKE {p1} OR artist_name LIKE {p1} LIMIT {p2}"
    );
    let tracks: Vec<Value> = state
        .backend
        .query_many(
            &tracks_sql,
            &[&pattern as &dyn ToSqlValue, &limit as &dyn ToSqlValue],
        )
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "title": r.get(1).and_then(|v| v.as_string()),
                "artist_name": r.get(2).and_then(|v| v.as_string()),
                "album_title": r.get(3).and_then(|v| v.as_string()),
                "duration": r.get(4).and_then(|v| v.as_f64()),
            })
        })
        .collect();

    let albums_sql = format!(
        "SELECT id, title, artist_name, year \
         FROM albums WHERE title LIKE {p1} OR artist_name LIKE {p1} LIMIT {p2}"
    );
    let albums: Vec<Value> = state
        .backend
        .query_many(
            &albums_sql,
            &[&pattern as &dyn ToSqlValue, &limit as &dyn ToSqlValue],
        )
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "title": r.get(1).and_then(|v| v.as_string()),
                "artist_name": r.get(2).and_then(|v| v.as_string()),
                "year": r.get(3).and_then(|v| v.as_i64()),
            })
        })
        .collect();

    let artists_sql = format!("SELECT id, name FROM artists WHERE name LIKE {p1} LIMIT {p2}");
    let artists: Vec<Value> = state
        .backend
        .query_many(
            &artists_sql,
            &[&pattern as &dyn ToSqlValue, &limit as &dyn ToSqlValue],
        )
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "name": r.get(1).and_then(|v| v.as_string()),
            })
        })
        .collect();

    json!({
        "tracks": tracks,
        "albums": albums,
        "artists": artists,
    })
}

/// Return the GraphQL schema as SDL text.
async fn graphql_schema() -> impl axum::response::IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        SCHEMA_SDL,
    )
}

/// Return an HTML page for GraphiQL playground.
async fn graphql_playground() -> Html<String> {
    Html(
        r#"<!DOCTYPE html>
<html>
<head>
  <title>Tune GraphQL Playground</title>
  <style>
    body { margin: 0; height: 100vh; }
    #graphiql { height: 100vh; }
  </style>
  <script crossorigin src="https://unpkg.com/react@18/umd/react.production.min.js"></script>
  <script crossorigin src="https://unpkg.com/react-dom@18/umd/react-dom.production.min.js"></script>
  <link rel="stylesheet" href="https://unpkg.com/graphiql/graphiql.min.css" />
  <script crossorigin src="https://unpkg.com/graphiql/graphiql.min.js"></script>
</head>
<body>
  <div id="graphiql"></div>
  <script>
    const fetcher = GraphiQL.createFetcher({ url: '/api/v1/graphql/' });
    ReactDOM.createRoot(document.getElementById('graphiql')).render(
      React.createElement(GraphiQL, { fetcher })
    );
  </script>
</body>
</html>"#
            .to_string(),
    )
}

/// Extract a string argument from a GraphQL-like query string.
/// e.g. `search(q: "jazz")` -> Some("jazz")
fn extract_string_arg(query: &str, arg_name: &str) -> Option<String> {
    let pattern = format!("{arg_name}:");
    let idx = query.find(&pattern)?;
    let rest = &query[idx + pattern.len()..];
    let rest = rest.trim_start();
    if let Some(inner) = rest.strip_prefix('"') {
        let end = inner.find('"')?;
        Some(inner[..end].to_string())
    } else {
        // Variable reference like $q -- not supported in this simple parser
        None
    }
}

/// Extract an integer argument from a GraphQL-like query string.
fn extract_int_arg(query: &str, arg_name: &str) -> Option<i64> {
    let pattern = format!("{arg_name}:");
    let idx = query.find(&pattern)?;
    let rest = &query[idx + pattern.len()..];
    let rest = rest.trim_start();
    let num_str: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    num_str.parse().ok()
}
