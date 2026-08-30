use serde_json::{Value, json};

use crate::db::backend::{DbBackend, SqlValue, ToSqlValue};

fn rows_to_json(rows: &[Vec<SqlValue>]) -> Vec<Value> {
    rows.iter()
        .map(|r| {
            json!({
                "track_id": r[0].as_i64().unwrap_or(0),
                "title": r[1].as_string().unwrap_or_default(),
                "artist": r[2].as_string(),
                "album": r[3].as_string(),
                "duration_ms": r[4].as_i64().unwrap_or(0),
                "genre": r[5].as_string(),
                "year": r[6].as_i64(),
                "bpm": r[7].as_f64(),
            })
        })
        .collect()
}

/// Library tracks for a list of artist names (case-insensitive match), up to
/// `per_artist` tracks each and `count` total, in the given name order so the
/// most-similar artists come first. Same JSON shape as `generate_queue`.
pub fn tracks_for_artist_names(
    db: &std::sync::Arc<dyn DbBackend>,
    names: &[String],
    per_artist: usize,
    count: usize,
) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for name in names {
        if out.len() >= count {
            break;
        }
        let lname = name.to_lowercase();
        let limit = per_artist.min(count - out.len()) as i64;
        let rows = db
            .query_many(
                "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.genre, t.year, t.bpm \
                 FROM tracks t \
                 JOIN artists ar ON t.artist_id = ar.id \
                 LEFT JOIN albums al ON t.album_id = al.id \
                 WHERE LOWER(ar.name) = ?1 \
                 ORDER BY RANDOM() LIMIT ?2",
                &[&lname, &limit],
            )
            .map(|r| rows_to_json(&r))
            .unwrap_or_default();
        out.extend(rows);
    }
    out.truncate(count);
    out
}

/// « Radio artistes similaires » at queue end: the seed is an artist NAME (so
/// a streaming now-playing works too, no local track id needed). Similar
/// names come from the mozaiklabs enrichment API and are matched against the
/// local library. Returns empty when offline or nothing matches — callers
/// fall back to the genre/BPM `generate_queue` (cloud graceful degradation:
/// Tune must work fully without mozaiklabs.fr).
pub async fn generate_similar_artists_queue(
    db: &std::sync::Arc<dyn DbBackend>,
    seed_artist: &str,
    count: usize,
) -> Vec<Value> {
    let names = similar_artist_names(db, seed_artist, 20).await;
    if names.is_empty() {
        return Vec::new();
    }
    tracks_for_artist_names(db, &names, 2, count)
}

/// Similar-artist names for a seed, from the enrichment API. Shared by the
/// local and streaming radios so they agree on « qui ressemble à qui ».
pub async fn similar_artist_names(
    db: &std::sync::Arc<dyn DbBackend>,
    seed_artist: &str,
    max: usize,
) -> Vec<String> {
    // Pas de repli codé en dur ici : le client porte déjà l'adresse de
    // référence. Celui qui vivait à cette ligne pointait vers
    // `https://api.mozaiklabs.fr`, un domaine qui n'existe pas (NXDOMAIN) —
    // toutes les suggestions échouaient en silence sur chaque installation qui
    // n'avait pas surchargé le réglage (#1730).
    let api_base = crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .get("artist_enrichment_api")
        .ok()
        .flatten();
    let mut client =
        crate::metadata::artist_enrichment::ArtistEnrichmentClient::new(api_base.as_deref(), 5);
    // `get_similar` est indexée par MBID ; on lui passait le NOM de la graine.
    // L'appel ne pouvait pas aboutir — quel que soit l'hôte (#1730). On résout
    // d'abord, et on renonce proprement si l'artiste est inconnu du cloud :
    // l'appelant se rabat sur le genre et le tempo (dégradation gracieuse).
    let Some(mbid) = client.resolve_mbid(seed_artist).await else {
        return Vec::new();
    };
    let mut names: Vec<String> = client
        .get_similar(&mbid)
        .await
        .iter()
        .filter_map(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_owned))
        .filter(|n| !n.eq_ignore_ascii_case(seed_artist))
        .collect();
    names.truncate(max);
    names
}

/// Pick one track per similar artist from a streaming service's search.
///
/// The local radio can only ever queue what the library holds: it turns
/// artist names into `SELECT ... FROM tracks`. Someone who listens to Qobuz
/// without a local library therefore got an empty queue and silence at the end
/// of the album — the seed was handled, the results were not.
///
/// `search` is the service's own search, injected rather than taken from the
/// registry, so this stays testable without a network or a subscription.
///
/// A search that returns nothing for an artist is skipped, not fatal: a radio
/// of nine tracks beats no radio at all.
///
/// `exclude_ids` are source ids the radio must not propose: the track that just
/// ended, and everything already sitting in the queue. Without it the very
/// first candidate is often the seed itself — a radio that replays the song you
/// just heard.
pub async fn streaming_tracks_for_artist_names<F, Fut>(
    names: &[String],
    count: usize,
    exclude_ids: &std::collections::HashSet<String>,
    mut search: F,
) -> Vec<crate::streaming::traits::StreamTrack>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Vec<crate::streaming::traits::StreamTrack>>,
{
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<String> = exclude_ids.clone();
    for name in names {
        if out.len() >= count {
            break;
        }
        let found = search(name.clone()).await;
        // One track per artist: a radio that plays four titles by the same
        // artist in a row is a playlist, not a radio.
        if let Some(t) = found.into_iter().find(|t| seen.insert(t.id.clone())) {
            out.push(t);
        }
    }
    out
}

/// Among a service's search hits, the artist that IS the seed — not a tribute.
///
/// Searching Qobuz for « Pink Floyd » also returns « The Australian Pink Floyd
/// Show » and « Pink Floyd Floydhead ». Taking the first hit would build the
/// radio on a cover band, so only an exact name match (case- and
/// whitespace-insensitive) is accepted. No match is a normal outcome: the
/// caller falls back rather than guessing.
pub fn pick_seed_artist_id(
    artists: &[crate::streaming::traits::StreamArtist],
    seed_artist: &str,
) -> Option<String> {
    let seed = seed_artist.trim();
    artists
        .iter()
        .find(|a| a.name.trim().eq_ignore_ascii_case(seed))
        .map(|a| a.id.clone())
}

/// Second source of similar-artist names: the streaming service itself (#1553).
///
/// The first source — the mozaiklabs enrichment API — is keyed by MusicBrainz
/// id. A streaming now-playing carries none, and only ~10 % of artists resolve
/// to one anyway, so it answered « nobody » every single time and the autoplay
/// queue stayed empty. The service that is streaming the track knows its own
/// catalogue: ask it.
///
/// Both calls are injected rather than taken from the registry, so this is
/// testable without a network or a subscription. Exactly two network calls,
/// whatever happens — one to resolve the seed, one to list its neighbours.
/// Returns the neighbours WITH their catalogue ids, not just their names. The
/// id is what lets the radio ask for « des titres DE cet artiste » instead of
/// « des titres qui contiennent son nom » — searching Qobuz for the band
/// Caravan otherwise queues Duke Ellington's *Caravan*, and Traffic returns
/// *Traffic Lights*. Four of the first ten picks were the wrong artist before
/// the ids were carried through.
pub async fn service_similar_artists<FS, FutS, FA, FutA>(
    seed_artist: &str,
    max: usize,
    search_artists: FS,
    similar_artists: FA,
) -> Vec<crate::streaming::traits::StreamArtist>
where
    FS: FnOnce(String) -> FutS,
    FutS: std::future::Future<Output = Vec<crate::streaming::traits::StreamArtist>>,
    FA: FnOnce(String) -> FutA,
    FutA: std::future::Future<Output = Vec<crate::streaming::traits::StreamArtist>>,
{
    if seed_artist.trim().is_empty() || max == 0 {
        return Vec::new();
    }
    let hits = search_artists(seed_artist.to_string()).await;
    let Some(seed_id) = pick_seed_artist_id(&hits, seed_artist) else {
        return Vec::new();
    };
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<crate::streaming::traits::StreamArtist> = Vec::new();
    for mut artist in similar_artists(seed_id).await {
        artist.name = artist.name.trim().to_string();
        // Never seed the radio with the artist we are coming from, and never
        // twice with the same one — a duplicate name means a duplicate lookup
        // for zero extra candidates.
        if artist.name.is_empty()
            || artist.id.is_empty()
            || artist.name.eq_ignore_ascii_case(seed_artist.trim())
            || !seen.insert(artist.name.to_lowercase())
        {
            continue;
        }
        out.push(artist);
        if out.len() >= max {
            break;
        }
    }
    out
}

pub fn generate_queue(
    db: &std::sync::Arc<dyn DbBackend>,
    seed_track_id: i64,
    count: usize,
) -> Vec<Value> {
    // Load seed track metadata
    let seed = db
        .query_one(
            "SELECT t.genre, t.year, t.bpm FROM tracks t WHERE t.id = ?",
            &[&seed_track_id],
        )
        .ok()
        .flatten();

    let (genre, year, bpm) = seed
        .map(|r| {
            (
                r[0].as_string(),
                r[1].as_i64().map(|v| v as i32),
                r[2].as_f64(),
            )
        })
        .unwrap_or((None, None, None));

    // Build dynamic query based on available seed metadata.
    // We use positional params (?1, ?2, ...) and collect owned
    // SqlValue params so we can pass &dyn ToSqlValue slices.
    let mut conditions = vec!["t.id != ?1".to_string()];
    let mut owned_params: Vec<crate::db::backend::SqlValue> = vec![seed_track_id.to_sql_value()];
    let mut param_idx = 2;

    if let Some(ref g) = genre {
        conditions.push(format!("t.genre LIKE ?{param_idx}"));
        owned_params.push(format!("%{g}%").to_sql_value());
        param_idx += 1;
    }

    if let Some(y) = year {
        conditions.push(format!(
            "t.year BETWEEN ?{param_idx} AND ?{}",
            param_idx + 1
        ));
        owned_params.push((y - 5).to_sql_value());
        owned_params.push((y + 5).to_sql_value());
        param_idx += 2;
    }

    if let Some(b) = bpm {
        if b > 0.0 {
            conditions.push(format!("t.bpm BETWEEN ?{param_idx} AND ?{}", param_idx + 1));
            owned_params.push((b * 0.85).to_sql_value());
            owned_params.push((b * 1.15).to_sql_value());
        }
    }

    let where_clause = conditions.join(" AND ");
    let sql = format!(
        "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.genre, t.year, t.bpm \
         FROM tracks t \
         LEFT JOIN artists ar ON t.artist_id = ar.id \
         LEFT JOIN albums al ON t.album_id = al.id \
         WHERE {where_clause} \
         ORDER BY RANDOM() LIMIT ?",
    );

    owned_params.push((count as i64).to_sql_value());
    let param_refs: Vec<&dyn ToSqlValue> =
        owned_params.iter().map(|p| p as &dyn ToSqlValue).collect();

    let mut results = db
        .query_many(&sql, &param_refs)
        .map(|r| rows_to_json(&r))
        .unwrap_or_default();

    // Fallback to random if no matches
    if results.is_empty() && (genre.is_some() || year.is_some() || bpm.is_some()) {
        let cnt = count as i64;
        results = db
            .query_many(
                "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.genre, t.year, t.bpm \
             FROM tracks t \
             LEFT JOIN artists ar ON t.artist_id = ar.id \
             LEFT JOIN albums al ON t.album_id = al.id \
             WHERE t.id != ? \
             ORDER BY RANDOM() LIMIT ?",
                &[&seed_track_id, &cnt],
            )
            .map(|r| rows_to_json(&r))
            .unwrap_or_default();
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::SqliteDb;

    fn stream_track(id: &str, artist: &str) -> crate::streaming::traits::StreamTrack {
        crate::streaming::traits::StreamTrack {
            id: id.into(),
            title: format!("Titre {id}"),
            artist: artist.into(),
            album: None,
            album_id: None,
            duration_ms: 200_000,
            cover_path: None,
            track_number: None,
            disc_number: None,
            explicit: false,
            quality: None,
            isrc: None,
            composer: None,
            artist_id: None,
        }
    }

    #[tokio::test]
    async fn streaming_radio_takes_one_track_per_artist() {
        let names: Vec<String> = ["A", "B", "C"].iter().map(|s| s.to_string()).collect();
        let no_exclusion = std::collections::HashSet::new();
        let got = streaming_tracks_for_artist_names(&names, 10, &no_exclusion, |name| async move {
            // Chaque artiste renvoie trois titres : la radio n'en garde qu'un,
            // sinon elle enchaîne quatre morceaux du même artiste et devient
            // une playlist.
            (0..3)
                .map(|i| stream_track(&format!("{name}{i}"), &name))
                .collect()
        })
        .await;
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].artist, "A");
        assert_eq!(got[1].artist, "B");
        assert_eq!(got[2].artist, "C");
    }

    #[tokio::test]
    async fn streaming_radio_stops_at_count_and_skips_empty_results() {
        let names: Vec<String> = ["A", "B", "C", "D"].iter().map(|s| s.to_string()).collect();
        // B ne renvoie rien : on saute, on ne s'arrête pas.
        let no_exclusion = std::collections::HashSet::new();
        let got = streaming_tracks_for_artist_names(&names, 2, &no_exclusion, |name| async move {
            if name == "B" {
                Vec::new()
            } else {
                vec![stream_track(&format!("{name}0"), &name)]
            }
        })
        .await;
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].artist, "A");
        assert_eq!(got[1].artist, "C");
    }

    #[tokio::test]
    async fn streaming_radio_never_repeats_the_same_track_id() {
        // Deux artistes différents qui renvoient le MÊME enregistrement
        // (compilation, featuring) : il ne doit apparaître qu'une fois.
        let names: Vec<String> = ["A", "B"].iter().map(|s| s.to_string()).collect();
        let no_exclusion = std::collections::HashSet::new();
        let got = streaming_tracks_for_artist_names(&names, 10, &no_exclusion, |name| async move {
            vec![stream_track("meme-id", &name)]
        })
        .await;
        assert_eq!(got.len(), 1);
    }

    fn stream_artist(id: &str, name: &str) -> crate::streaming::traits::StreamArtist {
        crate::streaming::traits::StreamArtist {
            id: id.into(),
            name: name.into(),
            image_path: None,
            bio: None,
        }
    }

    // --- Garde-fous de la radio streaming (#1553) ---

    #[tokio::test]
    async fn streaming_radio_never_replays_the_track_that_just_ended() {
        // Sandro : la graine est « Money » (Qobuz 47683556). La recherche par
        // artiste la renvoie evidemment en premier — sans exclusion, la radio
        // rejoue la chanson qui vient de se terminer.
        let names: Vec<String> = vec!["Pink Floyd".into()];
        let mut exclude = std::collections::HashSet::new();
        exclude.insert("47683556".to_string());
        let got = streaming_tracks_for_artist_names(&names, 10, &exclude, |name| async move {
            vec![
                stream_track("47683556", &name),
                stream_track("47683557", &name),
            ]
        })
        .await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "47683557");
    }

    #[tokio::test]
    async fn streaming_radio_never_duplicates_what_the_queue_already_holds() {
        // Les ids deja en file sont exclus au meme titre que la graine : une
        // radio relancee deux fois de suite ne doit pas empiler les doublons.
        let names: Vec<String> = vec!["A".into(), "B".into()];
        let mut exclude = std::collections::HashSet::new();
        exclude.insert("deja-en-file".to_string());
        let got = streaming_tracks_for_artist_names(&names, 10, &exclude, |name| async move {
            if name == "A" {
                vec![stream_track("deja-en-file", &name)]
            } else {
                vec![stream_track("nouveau", &name)]
            }
        })
        .await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "nouveau");
    }

    // --- Deuxieme source de candidats : le service (#1553) ---

    #[test]
    fn seed_artist_id_never_lands_on_a_tribute_band() {
        // Qobuz, requete « Pink Floyd » : le vrai groupe, puis deux hommages.
        // Prendre le premier resultat marcherait ici par chance ; exiger le nom
        // exact protege le cas ou l'hommage sort en tete.
        let hits = vec![
            stream_artist("3778014", "The Australian Pink Floyd Show"),
            stream_artist("38324", "Pink Floyd"),
            stream_artist("5661969", "Pink Floyd Floydhead"),
        ];
        assert_eq!(
            pick_seed_artist_id(&hits, "Pink Floyd").as_deref(),
            Some("38324")
        );
    }

    #[test]
    fn seed_artist_id_tolerates_case_and_spacing() {
        let hits = vec![stream_artist("42", "Dave Brubeck")];
        assert_eq!(
            pick_seed_artist_id(&hits, "  dave brubeck ").as_deref(),
            Some("42")
        );
    }

    #[test]
    fn seed_artist_id_absent_rather_than_wrong() {
        // Aucun nom exact : on rend None et l'appelant retombe, plutot que de
        // batir la radio sur un artiste au hasard.
        let hits = vec![stream_artist("3778014", "The Australian Pink Floyd Show")];
        assert!(pick_seed_artist_id(&hits, "Pink Floyd").is_none());
    }

    #[tokio::test]
    async fn service_similar_names_follow_the_catalogue() {
        // Reponse reelle de /artist/getSimilarArtists pour Pink Floyd (38324).
        let got = service_similar_artists(
            "Pink Floyd",
            20,
            |q| async move {
                assert_eq!(q, "Pink Floyd");
                vec![stream_artist("38324", "Pink Floyd")]
            },
            |id| async move {
                assert_eq!(id, "38324");
                vec![
                    stream_artist("1191678", "King Crimson"),
                    stream_artist("26718", "Yes"),
                    stream_artist("43821", "Queen"),
                ]
            },
        )
        .await;
        // Les identifiants voyagent avec les noms : c'est eux qui permettront
        // de demander les titres DE l'artiste, pas une recherche par nom.
        assert_eq!(
            got.iter()
                .map(|a| (a.id.as_str(), a.name.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("1191678", "King Crimson"),
                ("26718", "Yes"),
                ("43821", "Queen")
            ]
        );
    }

    #[tokio::test]
    async fn service_similar_names_drop_the_seed_and_the_duplicates() {
        let got = service_similar_artists(
            "Pink Floyd",
            20,
            |_| async { vec![stream_artist("38324", "Pink Floyd")] },
            |_| async {
                vec![
                    // Le service se cite lui-meme : on ne relance pas la radio
                    // sur l'artiste dont on sort.
                    stream_artist("38324", "PINK FLOYD"),
                    stream_artist("26718", "Yes"),
                    // Meme artiste, deux entrees : une seule recherche.
                    stream_artist("26719", "yes"),
                    stream_artist("0", "   "),
                    // Sans identifiant de catalogue, on ne peut rien demander.
                    stream_artist("", "Sans Identifiant"),
                ]
            },
        )
        .await;
        assert_eq!(
            got.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            vec!["Yes"]
        );
    }

    #[tokio::test]
    async fn service_similar_names_bounded_by_max() {
        let names = service_similar_artists(
            "A",
            2,
            |_| async { vec![stream_artist("1", "A")] },
            |_| async {
                (0..50)
                    .map(|i| stream_artist(&format!("{i}"), &format!("Artiste {i}")))
                    .collect()
            },
        )
        .await;
        assert_eq!(names.len(), 2);
    }

    #[tokio::test]
    async fn service_similar_names_stop_before_the_second_call_when_seed_unresolved() {
        // Le service ne connait pas l'artiste : un seul appel reseau, pas deux.
        let called = std::cell::Cell::new(false);
        let names = service_similar_artists(
            "Artiste Inconnu",
            20,
            |_| async { Vec::new() },
            |_| {
                called.set(true);
                async { vec![stream_artist("1", "Quelqu'un")] }
            },
        )
        .await;
        assert!(names.is_empty());
        assert!(!called.get(), "aucun appel similaires sans graine resolue");
    }

    #[tokio::test]
    async fn service_similar_names_empty_seed_asks_nothing() {
        let called = std::cell::Cell::new(false);
        let names = service_similar_artists(
            "   ",
            20,
            |_| {
                called.set(true);
                async { vec![stream_artist("1", "X")] }
            },
            |_| async { Vec::new() },
        )
        .await;
        assert!(names.is_empty());
        assert!(!called.get());
    }

    fn test_db() -> std::sync::Arc<dyn crate::db::backend::DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        std::sync::Arc::new(db)
    }

    // --- La graine quand la file est vide dès le départ ---

    fn ecoute(
        db: &std::sync::Arc<dyn crate::db::backend::DbBackend>,
        zone_id: Option<i64>,
        artiste: Option<&str>,
        track_id: Option<i64>,
        quand: &str,
    ) {
        // `listen_history` référence `zones` et `tracks` : sans les parents,
        // l'insertion se heurte à la clé étrangère et le test ne dit plus rien
        // de la requête qu'il prétend éprouver.
        if let Some(z) = zone_id {
            db.execute(
                "INSERT OR IGNORE INTO zones (id, name) VALUES (?, 'test')",
                &[&z],
            )
            .unwrap();
        }
        if let Some(t) = track_id {
            db.execute(
                "INSERT OR IGNORE INTO tracks (id, title, duration_ms) VALUES (?, 'x', 200000)",
                &[&t],
            )
            .unwrap();
        }
        db.execute(
            "INSERT INTO listen_history (track_id, title, artist_name, listened_at, zone_id) \
             VALUES (?, 'x', ?, ?, ?)",
            &[&track_id, &artiste, &quand, &zone_id],
        )
        .unwrap();
    }

    // --- Les derniers titres, pas le dernier titre ---

    #[test]
    fn les_artistes_recents_viennent_du_plus_recent_au_plus_ancien() {
        let db = test_db();
        ecoute(&db, Some(1), Some("Bowie"), None, "2026-08-20T10:00:00Z");
        ecoute(
            &db,
            Some(1),
            Some("Nina Simone"),
            None,
            "2026-08-21T10:00:00Z",
        );
        ecoute(
            &db,
            Some(1),
            Some("Miles Davis"),
            None,
            "2026-08-22T10:00:00Z",
        );
        assert_eq!(
            artistes_recents(&db, 1, 5),
            vec![
                "Miles Davis".to_string(),
                "Nina Simone".to_string(),
                "Bowie".to_string()
            ]
        );
    }

    /// Trois albums de suite du meme artiste ne doivent pas occuper les trois
    /// places de tete : la radio se reduirait a un seul gout, ce qui est
    /// exactement ce qu'on cherche a depasser.
    #[test]
    fn un_meme_artiste_ne_prend_qu_une_place() {
        let db = test_db();
        for h in ["08", "09", "10"] {
            ecoute(
                &db,
                Some(1),
                Some("Bowie"),
                None,
                &format!("2026-08-22T{h}:00:00Z"),
            );
        }
        ecoute(
            &db,
            Some(1),
            Some("Nina Simone"),
            None,
            "2026-08-22T07:00:00Z",
        );
        assert_eq!(
            artistes_recents(&db, 1, 5),
            vec!["Bowie".to_string(), "Nina Simone".to_string()]
        );
    }

    #[test]
    fn la_casse_ne_cree_pas_deux_artistes() {
        let db = test_db();
        ecoute(&db, Some(1), Some("BOWIE"), None, "2026-08-22T10:00:00Z");
        ecoute(&db, Some(1), Some("bowie"), None, "2026-08-22T09:00:00Z");
        assert_eq!(artistes_recents(&db, 1, 5), vec!["BOWIE".to_string()]);
    }

    #[test]
    fn la_liste_est_bornee_a_ce_qu_on_demande() {
        let db = test_db();
        for (i, nom) in ["A", "B", "C", "D", "E", "F"].iter().enumerate() {
            ecoute(
                &db,
                Some(1),
                Some(nom),
                None,
                &format!("2026-08-2{i}T10:00:00Z"),
            );
        }
        assert_eq!(artistes_recents(&db, 1, 3).len(), 3);
    }

    #[test]
    fn la_zone_prime_puis_la_maison_complete() {
        // La zone n'a qu'un artiste : on complete avec la maison plutot que de
        // rendre une radio d'un seul nom.
        let db = test_db();
        ecoute(&db, Some(1), Some("Chopin"), None, "2026-08-22T10:00:00Z");
        ecoute(
            &db,
            Some(2),
            Some("Motorhead"),
            None,
            "2026-08-22T11:00:00Z",
        );
        let r = artistes_recents(&db, 1, 5);
        assert_eq!(r.first().map(String::as_str), Some("Chopin"));
        assert!(r.contains(&"Motorhead".to_string()), "{r:?}");
    }

    #[test]
    fn sans_historique_la_liste_est_vide() {
        let db = test_db();
        assert!(artistes_recents(&db, 1, 5).is_empty());
    }

    #[test]
    fn sans_historique_il_n_y_a_pas_de_graine() {
        // Installation neuve : on ne fabrique rien a partir de rien.
        let db = test_db();
        assert_eq!(graine_recente(&db, 1), None);
    }

    #[test]
    fn la_graine_est_la_derniere_ecoute_de_la_zone() {
        let db = test_db();
        ecoute(
            &db,
            Some(1),
            Some("Bowie"),
            Some(10),
            "2026-08-20T10:00:00Z",
        );
        ecoute(
            &db,
            Some(1),
            Some("Nina Simone"),
            Some(11),
            "2026-08-21T10:00:00Z",
        );
        let g = graine_recente(&db, 1).unwrap();
        assert_eq!(g.artist_name.as_deref(), Some("Nina Simone"));
        assert_eq!(g.track_id, Some(11));
    }

    /// La cuisine et le salon n'ecoutent pas la meme chose : repartir sur le
    /// dernier morceau joue N'IMPORTE OU serait plus faux que juste.
    #[test]
    fn la_zone_prime_sur_la_maison() {
        let db = test_db();
        ecoute(
            &db,
            Some(2),
            Some("Motorhead"),
            Some(20),
            "2026-08-21T23:00:00Z",
        );
        ecoute(
            &db,
            Some(1),
            Some("Chopin"),
            Some(21),
            "2026-08-20T09:00:00Z",
        );
        assert_eq!(
            graine_recente(&db, 1).unwrap().artist_name.as_deref(),
            Some("Chopin")
        );
    }

    #[test]
    fn une_zone_vierge_se_rabat_sur_la_maison() {
        let db = test_db();
        ecoute(
            &db,
            Some(2),
            Some("Miles Davis"),
            Some(30),
            "2026-08-21T08:00:00Z",
        );
        assert_eq!(
            graine_recente(&db, 9).unwrap().artist_name.as_deref(),
            Some("Miles Davis")
        );
    }

    /// C'est le NOM D'ARTISTE qui alimente la radio « artistes similaires ».
    /// Une ecoute sans artiste ne mene nulle part : on l'ignore au lieu de
    /// semer du vide.
    #[test]
    fn une_ecoute_sans_artiste_ne_sert_pas_de_graine() {
        let db = test_db();
        ecoute(
            &db,
            Some(1),
            Some("Ella Fitzgerald"),
            Some(40),
            "2026-08-20T10:00:00Z",
        );
        ecoute(&db, Some(1), None, Some(41), "2026-08-21T10:00:00Z");
        ecoute(&db, Some(1), Some("   "), Some(42), "2026-08-21T11:00:00Z");
        assert_eq!(
            graine_recente(&db, 1).unwrap().artist_name.as_deref(),
            Some("Ella Fitzgerald")
        );
    }

    #[test]
    fn une_ecoute_de_service_sert_de_graine_sans_track_id() {
        // Qobuz sans bibliotheque locale : pas d'identifiant de piste, mais un
        // artiste — et c'est lui qui compte pour la radio.
        let db = test_db();
        ecoute(
            &db,
            Some(1),
            Some("Kings Of Leon"),
            None,
            "2026-08-21T10:00:00Z",
        );
        let g = graine_recente(&db, 1).unwrap();
        assert_eq!(g.artist_name.as_deref(), Some("Kings Of Leon"));
        assert_eq!(g.track_id, None);
    }

    #[test]
    fn empty_library_returns_empty() {
        let db = test_db();
        let result = generate_queue(&db, 1, 10);
        assert!(result.is_empty());
    }

    #[test]
    fn generates_queue_from_seed() {
        let db = test_db();
        db.execute("INSERT INTO artists (id, name) VALUES (1, 'Artist')", &[])
            .unwrap();
        db.execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Album', 1)",
            &[],
        )
        .unwrap();
        for i in 1..=10i64 {
            let title = format!("Track {i}");
            db.execute(
                "INSERT INTO tracks (id, title, artist_id, album_id, genre, year, duration_ms) VALUES (?, ?, 1, 1, 'Jazz', 2000, 240000)",
                &[&i, &title.as_str()],
            ).unwrap();
        }

        let result = generate_queue(&db, 1, 5);
        assert_eq!(result.len(), 5);
        assert!(result.iter().all(|t| t["track_id"].as_i64().unwrap() != 1));
    }

    #[test]
    fn tracks_for_artist_names_matches_case_insensitive_in_order() {
        let db = test_db();
        db.execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Miles Davis'), (2, 'John Coltrane'), (3, 'Someone Else')",
            &[],
        )
        .unwrap();
        for (id, artist) in [(1i64, 1i64), (2, 1), (3, 2), (4, 2), (5, 3)] {
            let title = format!("T{id}");
            db.execute(
                "INSERT INTO tracks (id, title, artist_id, duration_ms) VALUES (?, ?, ?, 200000)",
                &[&id, &title.as_str(), &artist],
            )
            .unwrap();
        }

        let names = vec![
            "john coltrane".to_string(),
            "MILES DAVIS".to_string(),
            "Unknown Guy".to_string(),
        ];
        let result = tracks_for_artist_names(&db, &names, 2, 10);
        // Coltrane (2 tracks) first — similarity order preserved — then Davis (2).
        assert_eq!(result.len(), 4);
        assert_eq!(result[0]["artist"].as_str(), Some("John Coltrane"));
        assert_eq!(result[1]["artist"].as_str(), Some("John Coltrane"));
        assert_eq!(result[2]["artist"].as_str(), Some("Miles Davis"));

        // per_artist and count caps hold.
        let capped = tracks_for_artist_names(&db, &names, 1, 1);
        assert_eq!(capped.len(), 1);
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mood {
    Chill,
    Party,
    Focus,
    Energetic,
}

impl Mood {
    pub fn bpm_range(&self) -> (f64, f64) {
        match self {
            Mood::Chill => (60.0, 100.0),
            Mood::Party => (110.0, 140.0),
            Mood::Focus => (80.0, 120.0),
            Mood::Energetic => (130.0, 180.0),
        }
    }

    pub fn genres(&self) -> &[&str] {
        match self {
            Mood::Chill => &[
                "jazz",
                "ambient",
                "classical",
                "folk",
                "bossa",
                "soul",
                "downtempo",
                "trip-hop",
            ],
            Mood::Party => &[
                "electronic",
                "dance",
                "pop",
                "hip-hop",
                "house",
                "techno",
                "disco",
                "funk",
            ],
            Mood::Focus => &[
                "classical",
                "ambient",
                "instrumental",
                "minimal",
                "piano",
                "soundtrack",
            ],
            Mood::Energetic => &[
                "rock",
                "metal",
                "punk",
                "electronic",
                "drum and bass",
                "hardcore",
                "garage",
            ],
        }
    }
}

/// Ce que la zone écoutait en dernier, quand elle n'a plus rien sous la main.
#[derive(Debug, Clone, PartialEq)]
pub struct GraineRecente {
    pub track_id: Option<i64>,
    pub artist_name: Option<String>,
}

/// Une graine tirée de l'écoute passée, pour une file **vide dès le départ**.
///
/// L'autoplay ne se déclenchait qu'en FIN de file, et prenait sa graine dans
/// le morceau qui venait de jouer. Une file vide au démarrage — un serveur
/// qu'on rallume, une file qu'on vient d'effacer — n'avait donc aucune graine,
/// et la seule trace en était un `autoplay_skipped_no_seed` en DEBUG. Le
/// réglage « lecture automatique » était activé, et il ne se passait rien.
///
/// On regarde d'abord l'historique de LA ZONE, puis celui de la maison. Ce
/// n'est pas la même chose : la cuisine et le salon n'écoutent pas la même
/// musique, et repartir sur le dernier morceau joué n'importe où serait plus
/// faux que juste.
///
/// On ne retient que les écoutes qui portent un nom d'artiste : c'est lui qui
/// alimente la radio « artistes similaires », locale comme de service.
/// Les derniers artistes écoutés par une zone, du plus récent au plus ancien.
///
/// Distincts, et dans l'ordre d'écoute : c'est ce qui fait la différence entre
/// « prolonger le dernier morceau » et « une radio à partir de ce que vous
/// venez d'écouter ». Trois albums de suite du même artiste ne doivent pas
/// occuper les trois places de tête et réduire la radio à un seul goût.
///
/// La zone prime sur la maison, pour la même raison que [`graine_recente`] :
/// la cuisine et le salon n'écoutent pas la même chose. On complète toutefois
/// avec la maison si la zone n'a pas de quoi remplir la liste — mieux vaut une
/// radio un peu plus large que pas de radio.
pub fn artistes_recents(
    db: &std::sync::Arc<dyn DbBackend>,
    zone_id: i64,
    combien: usize,
) -> Vec<String> {
    const FILTRE: &str = "artist_name IS NOT NULL AND TRIM(artist_name) <> ''";
    // On lit large et on déduplique ensuite : `DISTINCT` sur une colonne
    // ordonnée par une autre n'a pas le même sens d'un moteur à l'autre, et ce
    // qu'on veut est l'ordre d'ÉCOUTE, pas l'ordre alphabétique.
    let large = (combien * 12).max(60) as i64;

    let mut noms: Vec<String> = Vec::new();
    let mut vus: std::collections::HashSet<String> = std::collections::HashSet::new();

    // La zone d'abord, la maison ensuite si la zone ne suffit pas à remplir.
    let requetes: [(String, Vec<&dyn crate::db::backend::ToSqlValue>); 2] = [
        (
            format!(
                "SELECT artist_name FROM listen_history \
                 WHERE zone_id = ? AND {FILTRE} ORDER BY listened_at DESC LIMIT ?"
            ),
            vec![&zone_id, &large],
        ),
        (
            format!(
                "SELECT artist_name FROM listen_history \
                 WHERE {FILTRE} ORDER BY listened_at DESC LIMIT ?"
            ),
            vec![&large],
        ),
    ];

    for (sql, params) in &requetes {
        if noms.len() >= combien {
            break;
        }
        let Ok(rows) = db.query_many(sql, params) else {
            continue;
        };
        for r in rows {
            if noms.len() >= combien {
                break;
            }
            let Some(nom) = r[0].as_string() else {
                continue;
            };
            let nom = nom.trim().to_string();
            if !nom.is_empty() && vus.insert(nom.to_lowercase()) {
                noms.push(nom);
            }
        }
    }
    noms
}

/// La radio par défaut : construite sur les DERNIERS TITRES écoutés, et non
/// sur le seul dernier artiste.
///
/// C'est la différence entre prolonger un morceau et proposer une radio. On
/// part de plusieurs artistes récents, on demande à chacun ses semblables, et
/// on choisit dans la bibliothèque parmi tout ce pool.
///
/// Deux replis, dans cet ordre, et aucun n'est silencieux :
///  1. le cloud d'enrichissement est injoignable ou ne connaît personne — on
///     rejoue alors les artistes récents eux-mêmes, ce qui reste une radio
///     fidèle à l'écoute ;
///  2. la bibliothèque ne contient rien de tout cela — on rend vide, et
///     l'appelant garde ses autres cartes (radio du service, genre/BPM).
pub async fn radio_depuis_l_historique(
    db: &std::sync::Arc<dyn DbBackend>,
    zone_id: i64,
    count: usize,
) -> Vec<Value> {
    let recents = artistes_recents(db, zone_id, 5);
    if recents.is_empty() {
        return Vec::new();
    }

    let mut pool: Vec<String> = Vec::new();
    let mut vus: std::collections::HashSet<String> = std::collections::HashSet::new();
    for graine in &recents {
        for nom in similar_artist_names(db, graine, 8).await {
            if vus.insert(nom.to_lowercase()) {
                pool.push(nom);
            }
        }
    }

    // Les artistes récents ferment la marche plutôt que d'ouvrir : une radio
    // qui commence par ce qu'on vient d'écouter donne l'impression de tourner
    // en rond. Ils restent là comme filet, pour le cas où les semblables ne
    // sont pas dans la bibliothèque.
    for nom in &recents {
        if vus.insert(nom.to_lowercase()) {
            pool.push(nom.clone());
        }
    }

    tracks_for_artist_names(db, &pool, 2, count)
}

pub fn graine_recente(db: &std::sync::Arc<dyn DbBackend>, zone_id: i64) -> Option<GraineRecente> {
    const CHAMPS: &str = "track_id, artist_name";
    const FILTRE: &str = "artist_name IS NOT NULL AND TRIM(artist_name) <> ''";

    let ligne = db
        .query_one(
            &format!(
                "SELECT {CHAMPS} FROM listen_history \
                 WHERE zone_id = ? AND {FILTRE} ORDER BY listened_at DESC LIMIT 1"
            ),
            &[&zone_id],
        )
        .ok()
        .flatten()
        .or_else(|| {
            // La zone n'a jamais rien joué : on se rabat sur la maison entière.
            db.query_one(
                &format!(
                    "SELECT {CHAMPS} FROM listen_history \
                     WHERE {FILTRE} ORDER BY listened_at DESC LIMIT 1"
                ),
                &[],
            )
            .ok()
            .flatten()
        })?;

    let artiste = ligne[1].as_string().filter(|a| !a.trim().is_empty())?;
    Some(GraineRecente {
        track_id: ligne[0].as_i64(),
        artist_name: Some(artiste),
    })
}

pub fn generate_mood_queue(
    db: &std::sync::Arc<dyn DbBackend>,
    mood: Mood,
    count: usize,
) -> Vec<Value> {
    let (bpm_min, bpm_max) = mood.bpm_range();
    let genres = mood.genres();

    let genre_conditions: Vec<String> = genres
        .iter()
        .map(|g| format!("t.genre LIKE '%{g}%'"))
        .collect();
    let genre_clause = if genre_conditions.is_empty() {
        "1=1".to_string()
    } else {
        format!("({})", genre_conditions.join(" OR "))
    };

    let sql = format!(
        "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.genre, t.year, t.bpm \
         FROM tracks t \
         LEFT JOIN artists ar ON t.artist_id = ar.id \
         LEFT JOIN albums al ON t.album_id = al.id \
         WHERE ({genre_clause}) \
         AND (t.bpm IS NULL OR t.bpm BETWEEN ? AND ?) \
         ORDER BY RANDOM() LIMIT ?",
    );

    let cnt = count as i64;
    let mut results = db
        .query_many(&sql, &[&bpm_min, &bpm_max, &cnt])
        .map(|r| rows_to_json(&r))
        .unwrap_or_default();

    // Fallback to random if mood filter too restrictive
    if results.is_empty() {
        results = db
            .query_many(
                "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.genre, t.year, t.bpm \
             FROM tracks t \
             LEFT JOIN artists ar ON t.artist_id = ar.id \
             LEFT JOIN albums al ON t.album_id = al.id \
             ORDER BY RANDOM() LIMIT ?",
                &[&cnt],
            )
            .map(|r| rows_to_json(&r))
            .unwrap_or_default();
    }

    results
}

#[cfg(test)]
mod mood_tests {
    use super::*;

    #[test]
    fn mood_bpm_ranges() {
        assert_eq!(Mood::Chill.bpm_range(), (60.0, 100.0));
        assert_eq!(Mood::Party.bpm_range(), (110.0, 140.0));
        assert_eq!(Mood::Focus.bpm_range(), (80.0, 120.0));
        assert_eq!(Mood::Energetic.bpm_range(), (130.0, 180.0));
    }

    #[test]
    fn mood_genres_not_empty() {
        assert!(!Mood::Chill.genres().is_empty());
        assert!(!Mood::Party.genres().is_empty());
        assert!(!Mood::Focus.genres().is_empty());
        assert!(!Mood::Energetic.genres().is_empty());
    }

    #[test]
    fn mood_serialization() {
        let json = serde_json::to_string(&Mood::Party).unwrap();
        assert_eq!(json, "\"party\"");
        let parsed: Mood = serde_json::from_str("\"chill\"").unwrap();
        assert!(matches!(parsed, Mood::Chill));
    }
}
