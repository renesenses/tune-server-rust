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

use crate::library::track_matcher::{MatchCandidate, classer_candidats, find_best_match};
use crate::streaming::traits::{StreamTrack, StreamingService};

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

    let candidates = candidats(tracks);
    let result = find_best_match(title, artist, isrc, duration_ms as i64, &candidates);
    let best = result.best_match?;
    if best.score < MATCH_APPROX_SCORE {
        return None;
    }
    let idx: usize = best.source_id.parse().ok()?;
    tracks.get(idx).map(|t| (t, best.score))
}

/// Les résultats d'un service, traduits en candidats d'appariement.
///
/// L'INDICE sert d'identifiant de candidat : deux résultats peuvent partager
/// un titre, et c'est la position qui ramène au bon [`StreamTrack`].
fn candidats(tracks: &[StreamTrack]) -> Vec<MatchCandidate> {
    tracks
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
        .collect()
}

/// Plafond du nombre de candidats rendus par un appariement CLASSÉ (#4716).
///
/// Le greffon a besoin d'un recours quand le vainqueur rate sa propre règle
/// (la tolérance de durée), pas de la page de résultats entière : une
/// poignée suffit, et la mémoire linéaire du bac à sable n'est pas gratuite.
pub const MAX_CANDIDATS_APPARIEMENT: usize = 5;

/// Comme [`best_stream_match_scored`], mais rend PLUSIEURS candidats classés,
/// le verdict d'abord (#4716).
///
/// Un seul candidat ne suffit pas à l'appelant qui applique ensuite sa propre
/// règle : le greffon « Playlists converter » refuse un appariement dont la
/// durée s'écarte de plus de 3 s de la piste d'origine (règle tranchée le
/// 22/09/2026), et le titre ressortait alors « introuvable » alors qu'un autre
/// résultat du même service aurait convenu. Le classement lui laisse
/// redescendre d'un cran.
///
/// Rien n'est rescoré : [`classer_candidats`] appelle le MÊME
/// `find_best_match`, et sa tête est le verdict que [`best_stream_match_scored`]
/// aurait rendu — l'écran et le greffon désignent toujours la même piste.
pub fn best_stream_matches_scored<'a>(
    title: &str,
    artist: &str,
    isrc: &str,
    duration_ms: u64,
    tracks: &'a [StreamTrack],
    max: usize,
) -> Vec<(&'a StreamTrack, f64)> {
    if tracks.is_empty() {
        return Vec::new();
    }
    let candidates = candidats(tracks);
    classer_candidats(
        title,
        artist,
        isrc,
        duration_ms as i64,
        &candidates,
        MATCH_APPROX_SCORE,
        max,
    )
    .into_iter()
    .filter_map(|(i, score)| tracks.get(i).map(|t| (t, score)))
    .collect()
}

/// Nombre de résultats demandés au service avant l'appariement.
///
/// C'est la valeur que la route de transfert (`routes/playlist_manager.rs`)
/// utilisait déjà en dur : l'extraction ci-dessous la NOMME au lieu de la
/// recopier chez le deuxième appelant.
pub const LIMITE_RECHERCHE_APPARIEMENT: usize = 10;

/// Chercher un titre connu chez un service, puis l'apparier — les DEUX gestes
/// que tout transfert de playlist enchaîne.
///
/// Extrait de `transfer_playlist` (`tune-server/src/routes/playlist_manager.rs`)
/// pour l'ouverture de l'interface hôte WASM (#4716, épique #4715) : la capacité
/// `host_streaming_match_track` doit apparier EXACTEMENT comme la route, sans
/// quoi le greffon « Playlists converter » et l'écran de fusion donneraient deux
/// verdicts différents sur le même titre. Rien n'est réécrit ici : la requête
/// est la même (`"{titre} {artiste}"`, l'artiste seul étant facultatif), et le
/// verdict reste celui de [`best_stream_match_scored`].
///
/// Rend le score avec la piste : l'appelant décide s'il exige
/// [`MATCH_ACCEPT_SCORE`] (comme le transfert) ou s'il sait présenter la bande
/// approximative ([`MATCH_APPROX_SCORE`]) à l'utilisateur. `isrc` et
/// `duration_ms` peuvent être vides/`0` quand la source ne les porte pas.
pub async fn apparier_chez_le_service(
    svc: &dyn StreamingService,
    title: &str,
    artist: &str,
    isrc: &str,
    duration_ms: u64,
) -> Result<Option<(StreamTrack, f64)>, String> {
    // Un seul chemin de code avec la variante classée : le verdict rendu ici
    // est, par construction, la TÊTE de ce classement.
    Ok(
        apparier_chez_le_service_classe(svc, title, artist, isrc, duration_ms, 1)
            .await?
            .into_iter()
            .next(),
    )
}

/// Comme [`apparier_chez_le_service`], mais rend jusqu'à `max` candidats
/// CLASSÉS, le verdict en tête (#4716).
///
/// La recherche est la même (une seule requête chez le service) ; seul le
/// dépouillement change : [`best_stream_matches_scored`] au lieu de
/// [`best_stream_match_scored`]. L'appelant qui applique ensuite sa propre
/// règle — la tolérance de durée du greffon « Playlists converter » — peut
/// ainsi redescendre d'un cran au lieu de conclure « introuvable ».
pub async fn apparier_chez_le_service_classe(
    svc: &dyn StreamingService,
    title: &str,
    artist: &str,
    isrc: &str,
    duration_ms: u64,
    max: usize,
) -> Result<Vec<(StreamTrack, f64)>, String> {
    let query = if artist.is_empty() {
        title.to_string()
    } else {
        format!("{title} {artist}")
    };
    let results = svc
        .search(&query, LIMITE_RECHERCHE_APPARIEMENT)
        .await
        .map_err(|e| e.to_string())?;
    Ok(
        best_stream_matches_scored(title, artist, isrc, duration_ms, &results.tracks, max)
            .into_iter()
            .map(|(t, score)| (t.clone(), score))
            .collect(),
    )
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
            disponible: None,
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

    // -----------------------------------------------------------------------
    // #4716 — plusieurs candidats classés
    // -----------------------------------------------------------------------

    /// 🔴 Le manque que cette tranche répare : le MEILLEUR candidat rate la
    /// tolérance de durée que le greffon applique ensuite (±3 s, règle du
    /// 22/09/2026), et le titre ressortait « introuvable ».
    ///
    /// Les deux résultats portent le même titre et le même artiste — l'un est
    /// une autre prise, plus courte de 60 s. L'appariement désigne le premier
    /// (palier exact, qui ne regarde pas la durée) ; c'est le SECOND qui tient
    /// la tolérance. Un seul candidat rendu, et la piste est perdue.
    #[test]
    fn un_second_candidat_sauve_le_titre_quand_le_premier_rate_la_duree() {
        let source_ms: u64 = 150_000;
        let tracks = vec![
            track("autre-prise", "La Bohème", "Charles Aznavour", 210_000),
            track("la-bonne", "La Bohème", "Charles Aznavour", 150_500),
        ];

        // Ce que l'ancienne forme rendait : UN candidat, celui qui rate.
        let seul =
            best_stream_match_scored("La Bohème", "Charles Aznavour", "", source_ms, &tracks)
                .expect("un verdict");
        assert_eq!(seul.0.id, "autre-prise");
        assert!(
            seul.0.duration_ms.abs_diff(source_ms) > 3_000,
            "le vainqueur doit bien rater la tolérance, sinon le test ne prouve rien"
        );

        // La forme classée rend les deux, le verdict en tête.
        let classes = best_stream_matches_scored(
            "La Bohème",
            "Charles Aznavour",
            "",
            source_ms,
            &tracks,
            MAX_CANDIDATS_APPARIEMENT,
        );
        assert_eq!(classes.len(), 2, "{classes:?}");
        assert_eq!(
            classes[0].0.id, seul.0.id,
            "la tête du classement est le verdict d'avant, inchangé"
        );

        // Et le greffon, en appliquant SA règle, retrouve la piste.
        let retenu = classes
            .iter()
            .find(|(t, _)| t.duration_ms.abs_diff(source_ms) <= 3_000)
            .expect("un candidat doit tenir la tolérance de durée");
        assert_eq!(retenu.0.id, "la-bonne");
    }

    /// Le plafond s'applique, et l'absence de résultats rend une liste vide.
    #[test]
    fn le_classement_est_plafonne_et_vide_quand_rien_ne_correspond() {
        let tracks: Vec<StreamTrack> = (0..8)
            .map(|i| track(&format!("t{i}"), "La Bohème", "Charles Aznavour", 200_000))
            .collect();
        assert_eq!(
            best_stream_matches_scored("La Bohème", "Charles Aznavour", "", 0, &tracks, 3).len(),
            3
        );
        assert!(
            best_stream_matches_scored("La Bohème", "Charles Aznavour", "", 0, &[], 5).is_empty()
        );
        let ailleurs = vec![track("x", "Totalement autre chose", "Quelqu'un", 200_000)];
        assert!(
            best_stream_matches_scored("La Bohème", "Charles Aznavour", "", 0, &ailleurs, 5)
                .is_empty(),
            "rien de plausible : aucun candidat de dépit"
        );
    }
}
