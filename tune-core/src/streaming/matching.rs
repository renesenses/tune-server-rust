//! Resolve a *known* track (title/artist, optionally ISRC + duration) onto a
//! streaming service's search results.
//!
//! Historically the playlist-transfer and playlist-manager paths matched with
//! naive string equality ("title == title") then fell back to "take the first
//! result", which happily attached live versions, covers or unrelated songs.
//! This routes both through the shared normalized+fuzzy matcher in
//! [`crate::library::track_matcher`] (accent folding, "(Remastered …)" stripping,
//! duration bonus) and refuses anything below an acceptance score, so a wrong
//! match becomes "not found" rather than a silent mismatch.

use crate::library::track_matcher::{MatchCandidate, find_best_match};
use crate::streaming::traits::StreamTrack;

/// Minimum score required to accept a match when resolving a known track onto a
/// streaming service. An exact normalized title+artist scores 0.95 and an ISRC
/// hit 1.0, so this floor keeps genuine matches while rejecting weak fuzzy hits
/// (a same-title different song, a cover, a live take).
pub const MATCH_ACCEPT_SCORE: f64 = 0.7;

/// Plancher « approximatif » : sous [`MATCH_ACCEPT_SCORE`] mais au-dessus de ce
/// seuil, le fuzzy matcher a bien trouvé quelque chose (son propre plancher
/// interne est 0.6) — c'était jusqu'ici jeté en silence, et la bande 0.6–0.7
/// devenait « not_found » pour les favoris radio (#1235) alors qu'une recherche
/// manuelle Qobuz trouvait la piste. Les appelants qui savent présenter la
/// nuance à l'utilisateur peuvent l'exploiter via [`best_stream_match_scored`].
pub const MATCH_APPROX_SCORE: f64 = 0.6;

/// Pick the streaming track that best matches a known `(title, artist)` — with an
/// optional `isrc` and `duration_ms` for extra confidence — from a service's
/// search results. Returns `None` when nothing clears [`MATCH_ACCEPT_SCORE`],
/// which callers should treat as "not found on this service" rather than forcing
/// a bad match.
///
/// `duration_ms` / `isrc` may be `0` / `""` when the source doesn't carry them
/// (e.g. radio favorites); matching then relies on the normalized title+artist.
pub fn best_stream_match<'a>(
    title: &str,
    artist: &str,
    isrc: &str,
    duration_ms: u64,
    tracks: &'a [StreamTrack],
) -> Option<&'a StreamTrack> {
    best_stream_match_scored(title, artist, isrc, duration_ms, tracks)
        .filter(|(_, score)| *score >= MATCH_ACCEPT_SCORE)
        .map(|(t, _)| t)
}

/// Comme [`best_stream_match`], mais renvoie aussi le score et accepte dès
/// [`MATCH_APPROX_SCORE`]. À charge de l'appelant de distinguer un match sûr
/// (`score >= MATCH_ACCEPT_SCORE`) d'un match approximatif à présenter comme
/// tel — utilisé par les favoris radio, qui n'ont ni ISRC ni durée.
pub fn best_stream_match_scored<'a>(
    title: &str,
    artist: &str,
    isrc: &str,
    duration_ms: u64,
    tracks: &'a [StreamTrack],
) -> Option<(&'a StreamTrack, f64)> {
    if tracks.is_empty() {
        return None;
    }

    // Use each track's index as the candidate id so we can map the winner back to
    // the exact StreamTrack even when several results share a title.
    let candidates: Vec<MatchCandidate> = tracks
        .iter()
        .enumerate()
        .map(|(i, t)| MatchCandidate {
            title: t.title.clone(),
            artist_name: t.artist.clone(),
            album_title: t.album.clone().unwrap_or_default(),
            source_id: i.to_string(),
            duration_ms: t.duration_ms as i64,
            // When the service exposed an ISRC, feed it so the exact ISRC fast-path
            // in find_best_match can win before any fuzzy scoring.
            isrc: t.isrc.clone().unwrap_or_default(),
            score: 0.0,
            match_method: String::new(),
            confidence: String::new(),
        })
        .collect();

    let result = find_best_match(title, artist, isrc, duration_ms as i64, &candidates);
    let best = result.best_match?;
    if best.score < MATCH_APPROX_SCORE {
        return None;
    }
    let idx: usize = best.source_id.parse().ok()?;
    tracks.get(idx).map(|t| (t, best.score))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::traits::StreamTrack;

    fn track(id: &str, title: &str, artist: &str, dur_ms: u64) -> StreamTrack {
        StreamTrack {
            id: id.into(),
            title: title.into(),
            artist: artist.into(),
            album: None,
            album_id: None,
            duration_ms: dur_ms,
            cover_path: None,
            track_number: None,
            disc_number: None,
            explicit: false,
            isrc: None,
            composer: None,
            artist_id: None,
            quality: None,
        }
    }

    #[test]
    fn matches_despite_accents_and_remaster_suffix() {
        let tracks = vec![
            track("1", "Something Else", "Other Band", 200_000),
            track(
                "2",
                "La Bohème (Remastered 2014)",
                "Charles Aznavour",
                210_000,
            ),
        ];
        let m = best_stream_match("La Bohème", "Charles Aznavour", "", 0, &tracks);
        assert_eq!(m.map(|t| t.id.as_str()), Some("2"));
    }

    #[test]
    fn rejects_when_nothing_is_close() {
        let tracks = vec![track("1", "Totally Different", "Someone Else", 200_000)];
        let m = best_stream_match("La Bohème", "Charles Aznavour", "", 0, &tracks);
        assert!(m.is_none());
    }

    #[test]
    fn empty_results_return_none() {
        let m = best_stream_match("X", "Y", "", 0, &[]);
        assert!(m.is_none());
    }

    #[test]
    fn la_bande_approximative_est_accessible_via_la_variante_scoree() {
        // Titre proche mais pas identique + artiste identique : le fuzzy tombe
        // dans la bande 0.6–0.7. `best_stream_match` refuse (comportement
        // historique conservé), mais la variante scorée l'expose pour que les
        // favoris radio puissent le présenter comme « approximate » au lieu de
        // « not_found » (#1235).
        let tracks = vec![track("1", "Nightswimming (Live at the BBC)", "R.E.M.", 0)];
        let strict = best_stream_match("Nightswimming demo", "R.E.M.", "", 0, &tracks);
        let scored = best_stream_match_scored("Nightswimming demo", "R.E.M.", "", 0, &tracks);
        match scored {
            Some((t, score)) => {
                assert_eq!(t.id, "1");
                assert!(
                    score >= MATCH_APPROX_SCORE,
                    "score {score} sous le plancher approx"
                );
                // Cohérence : si le strict a refusé, c'est que le score est
                // bien dans la bande intermédiaire.
                if strict.is_none() {
                    assert!(score < MATCH_ACCEPT_SCORE);
                }
            }
            None => panic!("la variante scorée devrait au moins trouver un approximatif"),
        }
    }

    #[test]
    fn exact_title_artist_wins_over_first_result() {
        // A naive "first result" matcher would wrongly pick the cover at index 0.
        let tracks = vec![
            track("cover", "Imagine", "A Tribute Band", 190_000),
            track("real", "Imagine", "John Lennon", 187_000),
        ];
        let m = best_stream_match("Imagine", "John Lennon", "", 187_000, &tracks);
        assert_eq!(m.map(|t| t.id.as_str()), Some("real"));
    }

    #[test]
    fn isrc_match_wins_regardless_of_title() {
        // The ISRC fast-path must pick the track carrying the source ISRC even when
        // another result looks like a better title/artist match.
        let mut with_isrc = track("isrc-hit", "Weirdly Tagged Title", "V.A.", 200_000);
        with_isrc.isrc = Some("FRUM71600123".into());
        let tracks = vec![
            track("title-look-alike", "La Bohème", "Charles Aznavour", 210_000),
            with_isrc,
        ];
        let m = best_stream_match("La Bohème", "Charles Aznavour", "FRUM71600123", 0, &tracks);
        assert_eq!(m.map(|t| t.id.as_str()), Some("isrc-hit"));
    }
}
