use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

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
) -> impl IntoResponse {
    let query = body.query.trim();
    let variables = body.variables.unwrap_or(json!({}));

    // Simple top-level query parser
    if let Some(result) = try_execute(query, &variables, &state) {
        Json(json!({"data": result})).into_response()
    } else {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "errors": [{"message": "Unsupported query. Supported: tracks, albums, artists, search, track(id), album(id), artist(id)"}],
            })),
        )
            .into_response()
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
    let conn = state.db.connection().lock().unwrap();
    let items: Vec<Value> = conn
        .prepare(
            "SELECT id, title, artist_name, album_title, duration, path, format, sample_rate, bit_depth \
             FROM tracks ORDER BY title LIMIT ?1 OFFSET ?2",
        )
        .and_then(|mut stmt| {
            stmt.query_map([limit, offset], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "title": row.get::<_, Option<String>>(1)?,
                    "artist_name": row.get::<_, Option<String>>(2)?,
                    "album_title": row.get::<_, Option<String>>(3)?,
                    "duration": row.get::<_, Option<f64>>(4)?,
                    "path": row.get::<_, Option<String>>(5)?,
                    "format": row.get::<_, Option<String>>(6)?,
                    "sample_rate": row.get::<_, Option<i64>>(7)?,
                    "bit_depth": row.get::<_, Option<i64>>(8)?,
                }))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();

    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM tracks", [], |row| row.get(0))
        .unwrap_or(0);
    drop(conn);

    json!({
        "items": items,
        "total": total,
    })
}

fn execute_albums(state: &AppState, limit: i64, offset: i64) -> Value {
    let conn = state.db.connection().lock().unwrap();
    let items: Vec<Value> = conn
        .prepare(
            "SELECT id, title, artist_name, year, track_count, cover_path \
             FROM albums ORDER BY title LIMIT ?1 OFFSET ?2",
        )
        .and_then(|mut stmt| {
            stmt.query_map([limit, offset], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "title": row.get::<_, Option<String>>(1)?,
                    "artist_name": row.get::<_, Option<String>>(2)?,
                    "year": row.get::<_, Option<i64>>(3)?,
                    "track_count": row.get::<_, Option<i64>>(4)?,
                    "cover_path": row.get::<_, Option<String>>(5)?,
                }))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();

    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM albums", [], |row| row.get(0))
        .unwrap_or(0);
    drop(conn);

    json!({
        "items": items,
        "total": total,
    })
}

fn execute_artists(state: &AppState, limit: i64, offset: i64) -> Value {
    let conn = state.db.connection().lock().unwrap();
    let items: Vec<Value> = conn
        .prepare(
            "SELECT id, name, album_count, track_count \
             FROM artists ORDER BY name LIMIT ?1 OFFSET ?2",
        )
        .and_then(|mut stmt| {
            stmt.query_map([limit, offset], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "name": row.get::<_, Option<String>>(1)?,
                    "album_count": row.get::<_, Option<i64>>(2)?,
                    "track_count": row.get::<_, Option<i64>>(3)?,
                }))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();

    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM artists", [], |row| row.get(0))
        .unwrap_or(0);
    drop(conn);

    json!({
        "items": items,
        "total": total,
    })
}

fn execute_search(state: &AppState, q: &str, limit: i64) -> Value {
    let conn = state.db.connection().lock().unwrap();
    let pattern = format!("%{q}%");

    let tracks: Vec<Value> = conn
        .prepare(
            "SELECT id, title, artist_name, album_title, duration \
             FROM tracks WHERE title LIKE ?1 OR artist_name LIKE ?1 LIMIT ?2",
        )
        .and_then(|mut stmt| {
            stmt.query_map(rusqlite::params![&pattern, limit], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "title": row.get::<_, Option<String>>(1)?,
                    "artist_name": row.get::<_, Option<String>>(2)?,
                    "album_title": row.get::<_, Option<String>>(3)?,
                    "duration": row.get::<_, Option<f64>>(4)?,
                }))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();

    let albums: Vec<Value> = conn
        .prepare(
            "SELECT id, title, artist_name, year \
             FROM albums WHERE title LIKE ?1 OR artist_name LIKE ?1 LIMIT ?2",
        )
        .and_then(|mut stmt| {
            stmt.query_map(rusqlite::params![&pattern, limit], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "title": row.get::<_, Option<String>>(1)?,
                    "artist_name": row.get::<_, Option<String>>(2)?,
                    "year": row.get::<_, Option<i64>>(3)?,
                }))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();

    let artists: Vec<Value> = conn
        .prepare("SELECT id, name FROM artists WHERE name LIKE ?1 LIMIT ?2")
        .and_then(|mut stmt| {
            stmt.query_map(rusqlite::params![&pattern, limit], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "name": row.get::<_, Option<String>>(1)?,
                }))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();

    drop(conn);

    json!({
        "tracks": tracks,
        "albums": albums,
        "artists": artists,
    })
}

/// Return the GraphQL schema as SDL text.
async fn graphql_schema() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        SCHEMA_SDL,
    )
}

/// Return an HTML page for GraphiQL playground.
async fn graphql_playground() -> Html<String> {
    Html(format!(
        r#"<!DOCTYPE html>
<html>
<head>
  <title>Tune GraphQL Playground</title>
  <style>
    body {{ margin: 0; height: 100vh; }}
    #graphiql {{ height: 100vh; }}
  </style>
  <script crossorigin src="https://unpkg.com/react@18/umd/react.production.min.js"></script>
  <script crossorigin src="https://unpkg.com/react-dom@18/umd/react-dom.production.min.js"></script>
  <link rel="stylesheet" href="https://unpkg.com/graphiql/graphiql.min.css" />
  <script crossorigin src="https://unpkg.com/graphiql/graphiql.min.js"></script>
</head>
<body>
  <div id="graphiql"></div>
  <script>
    const fetcher = GraphiQL.createFetcher({{ url: '/api/v1/graphql/' }});
    ReactDOM.createRoot(document.getElementById('graphiql')).render(
      React.createElement(GraphiQL, {{ fetcher }})
    );
  </script>
</body>
</html>"#
    ))
}

/// Extract a string argument from a GraphQL-like query string.
/// e.g. `search(q: "jazz")` -> Some("jazz")
fn extract_string_arg(query: &str, arg_name: &str) -> Option<String> {
    let pattern = format!("{arg_name}:");
    let idx = query.find(&pattern)?;
    let rest = &query[idx + pattern.len()..];
    let rest = rest.trim_start();
    if rest.starts_with('"') {
        let inner = &rest[1..];
        let end = inner.find('"')?;
        Some(inner[..end].to_string())
    } else {
        // Variable reference like $q — not supported in this simple parser
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
