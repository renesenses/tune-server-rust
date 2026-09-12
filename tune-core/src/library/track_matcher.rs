use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchCandidate {
    pub title: String,
    pub artist_name: String,
    pub album_title: String,
    pub source_id: String,
    pub duration_ms: i64,
    pub isrc: String,
    pub score: f64,
    pub match_method: String,
    pub confidence: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchResult {
    pub source_title: String,
    pub source_artist: String,
    pub source_album: String,
    pub source_isrc: String,
    pub status: String,
    pub best_match: Option<MatchCandidate>,
    pub alternatives: Vec<MatchCandidate>,
}

pub fn normalize(text: &str) -> String {
    let lower = text.to_lowercase();
    let stripped = strip_suffixes(&lower);
    let no_accents = remove_accents(&stripped);
    no_accents.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn remove_accents(text: &str) -> String {
    let nfkd = unicode_normalization_simple(text);
    nfkd.chars().filter(|c| !is_combining(*c)).collect()
}

fn unicode_normalization_simple(text: &str) -> String {
    text.chars()
        .flat_map(|c| match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => vec!['a'],
            'è' | 'é' | 'ê' | 'ë' => vec!['e'],
            'ì' | 'í' | 'î' | 'ï' => vec!['i'],
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' => vec!['o'],
            'ù' | 'ú' | 'û' | 'ü' => vec!['u'],
            'ñ' => vec!['n'],
            'ç' => vec!['c'],
            'ÿ' | 'ý' => vec!['y'],
            'æ' => vec!['a', 'e'],
            'œ' => vec!['o', 'e'],
            'ß' => vec!['s', 's'],
            _ => vec![c],
        })
        .collect()
}

fn is_combining(c: char) -> bool {
    ('\u{0300}'..='\u{036F}').contains(&c)
}

fn strip_suffixes(text: &str) -> String {
    let mut result = text.to_string();
    let patterns = [
        "(remastered",
        "(remaster",
        "[remastered",
        "[remaster",
        "(deluxe",
        "[deluxe",
        "(live)",
        "[live]",
        "(bonus track)",
        "(mono)",
        "(stereo)",
        "- remastered",
        // Featured-artist markers: radios and streaming services disagree wildly
        // on these (FIP says "Title (feat. X)", Qobuz says "Title"), so a real
        // match scored too low on the title and was rejected (forum #1235). Drop
        // the marker on both sides so the core titles line up. `find` is
        // lowercased upstream, and the space/paren prefixes avoid clipping a word
        // that merely starts with "ft"/"feat".
        "(feat",
        "[feat",
        " feat.",
        " feat ",
        " featuring ",
        "(ft",
        " ft.",
        " ft ",
    ];
    for pat in patterns {
        if let Some(pos) = result.find(pat) {
            result.truncate(pos);
        }
    }
    result.trim().to_string()
}

pub fn similarity(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let len_a = a_chars.len();
    let len_b = b_chars.len();

    let mut matches = 0usize;
    let window = (len_a.max(len_b) / 2).saturating_sub(1).max(1);
    let mut b_used = vec![false; len_b];

    for (i, &ac) in a_chars.iter().enumerate() {
        let start = i.saturating_sub(window);
        let end = (i + window + 1).min(len_b);
        for j in start..end {
            if !b_used[j] && b_chars[j] == ac {
                matches += 1;
                b_used[j] = true;
                break;
            }
        }
    }

    if matches == 0 {
        return 0.0;
    }

    matches as f64 / len_a.max(len_b) as f64
}

/// Un ISRC comparable : majuscules, sans séparateur ni espace.
///
/// L'ISRC s'écrit de deux façons pour le même enregistrement : la forme
/// d'affichage `FR-Z12-88-00001`, que portent les balises de fichiers, et la
/// forme compacte `FRZ128800001`, que rendent les API de streaming. Les
/// comparer telles quelles fait de deux écritures du même enregistrement deux
/// enregistrements différents.
///
/// ## Ce qui manquait (#2264)
///
/// Le dépôt comparait les ISRC de DEUX façons :
///
/// | site | comparaison |
/// |---|---|
/// | `tune-server/src/routes/versions.rs` | pliée (cette fonction, recopiée) |
/// | `library/track_matcher.rs::match_by_isrc` | `eq_ignore_ascii_case` **brut** |
///
/// Or `match_by_isrc` est le chemin RAPIDE de [`find_best_match`] : quand il
/// échoue, on retombe sur le rapprochement par titre approché, qui peut
/// désigner un autre enregistrement. Une piste locale étiquetée
/// `FR-Z12-88-00001` ne pouvait donc pas se rattacher à sa jumelle Qobuz
/// `FRZ128800001` — et c'est ce chemin que suivent le transfert de playlist
/// (`routes/playlist_manager.rs:318`), les radios (`routes/radios.rs:1876`)
/// et `playlist_transfer.rs:96`.
///
/// L'arbitrage du 01/09/2026 sur #2264 retient l'ISRC comme clé d'identité du
/// groupe de versions. Une clé ne peut pas se comparer de deux façons : c'est
/// le socle, posé avant le groupe persistant lui-même.
pub fn normaliser_isrc(brut: &str) -> String {
    brut.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}
pub fn match_by_isrc(source_isrc: &str, candidates: &[MatchCandidate]) -> Option<MatchCandidate> {
    let source = normaliser_isrc(source_isrc);
    if source.is_empty() {
        return None;
    }
    candidates
        .iter()
        .find(|c| {
            let candidat = normaliser_isrc(&c.isrc);
            !candidat.is_empty() && candidat == source
        })
        .map(|c| {
            let mut result = c.clone();
            result.score = 1.0;
            result.match_method = "isrc".into();
            result.confidence = "high".into();
            result
        })
}

pub fn match_exact(
    title: &str,
    artist: &str,
    candidates: &[MatchCandidate],
) -> Option<MatchCandidate> {
    let norm_title = normalize(title);
    let norm_artist = normalize(artist);

    candidates.iter().find_map(|c| {
        let ct = normalize(&c.title);
        let ca = normalize(&c.artist_name);
        if ct == norm_title && ca == norm_artist {
            let mut result = c.clone();
            result.score = 0.95;
            result.match_method = "exact".into();
            result.confidence = "high".into();
            Some(result)
        } else {
            None
        }
    })
}

pub fn match_fuzzy(
    title: &str,
    artist: &str,
    duration_ms: i64,
    candidates: &[MatchCandidate],
    threshold: f64,
) -> Option<MatchCandidate> {
    let norm_title = normalize(title);
    let norm_artist = normalize(artist);

    let mut best: Option<(f64, MatchCandidate)> = None;

    for c in candidates {
        let ct = normalize(&c.title);
        let ca = normalize(&c.artist_name);

        let title_sim = similarity(&norm_title, &ct);
        let artist_sim = similarity(&norm_artist, &ca);

        // Artiste inconnu côté requête (favoris radio ICY, tags pauvres) : le
        // 0.4 d'artiste est mathématiquement perdu et le score plafonne à 0.5,
        // sous TOUT seuil utile — même un titre parfait était rejeté (forum
        // #1234). Sans artiste, le titre porte l'essentiel du score.
        let mut score = if norm_artist.is_empty() {
            title_sim * 0.9
        } else {
            title_sim * 0.5 + artist_sim * 0.4
        };

        if duration_ms > 0 && c.duration_ms > 0 {
            let dur_diff = (duration_ms - c.duration_ms).unsigned_abs() as f64;
            let dur_ratio = 1.0 - (dur_diff / duration_ms.max(1) as f64).min(1.0);
            score += dur_ratio * 0.1;
        }

        if score >= threshold && best.as_ref().is_none_or(|(bs, _)| score > *bs) {
            let mut result = c.clone();
            result.score = score;
            result.match_method = "fuzzy".into();
            result.confidence = if score >= 0.85 {
                "high"
            } else if score >= 0.7 {
                "medium"
            } else {
                "low"
            }
            .into();
            best = Some((score, result));
        }
    }

    best.map(|(_, m)| m)
}

pub fn find_best_match(
    title: &str,
    artist: &str,
    isrc: &str,
    duration_ms: i64,
    candidates: &[MatchCandidate],
) -> MatchResult {
    let mut result = MatchResult {
        source_title: title.into(),
        source_artist: artist.into(),
        source_album: String::new(),
        source_isrc: isrc.into(),
        status: "not_found".into(),
        best_match: None,
        alternatives: Vec::new(),
    };

    if let Some(m) = match_by_isrc(isrc, candidates) {
        result.status = "matched".into();
        result.best_match = Some(m);
        return result;
    }

    if let Some(m) = match_exact(title, artist, candidates) {
        result.status = "matched".into();
        result.best_match = Some(m);
        return result;
    }

    if let Some(m) = match_fuzzy(title, artist, duration_ms, candidates, 0.6) {
        result.status = if m.score >= 0.85 {
            "matched"
        } else {
            "approximate"
        }
        .into();
        result.best_match = Some(m);
        return result;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_basic() {
        assert_eq!(normalize("Hello World"), "hello world");
    }

    #[test]
    fn normalize_accents() {
        assert_eq!(normalize("Café résumé"), "cafe resume");
    }

    #[test]
    fn normalize_strips_featured_artist() {
        // forum #1235: FIP tags "Title (feat. X)", Qobuz returns "Title".
        assert_eq!(
            normalize("Under the strikes (feat. Tony Allen)"),
            "under the strikes"
        );
        assert_eq!(normalize("So What feat. Someone"), "so what");
        assert_eq!(normalize("Song ft. Guest"), "song");
    }

    #[test]
    fn fuzzy_matches_across_featured_artist_marker() {
        // Reivax's exact case (forum #1235): favorite title carries "(feat. …)",
        // Qobuz's does not; artist is present. Before stripping feat, the title
        // similarity dragged the score to ~0.63 < 0.7 and the correct track was
        // rejected. Now the core titles line up → an exact match.
        let cand = MatchCandidate {
            title: "Under The Strikes".into(),
            artist_name: "Yannis & The Yaw".into(),
            album_title: String::new(),
            source_id: "1".into(),
            duration_ms: 0,
            isrc: String::new(),
            score: 0.0,
            match_method: String::new(),
            confidence: String::new(),
        };
        let m = find_best_match(
            "Under the strikes (feat. Tony Allen)",
            "Yannis & The Yaw",
            "",
            0,
            &[cand],
        );
        assert_eq!(m.status, "matched", "score should clear the bar");
        assert!(m.best_match.is_some());
    }

    #[test]
    fn normalize_remastered() {
        assert_eq!(
            normalize("Bohemian Rhapsody (Remastered 2011)"),
            "bohemian rhapsody"
        );
    }

    #[test]
    fn similarity_identical() {
        assert_eq!(similarity("hello", "hello"), 1.0);
    }

    #[test]
    fn similarity_empty() {
        assert_eq!(similarity("", "hello"), 0.0);
    }

    #[test]
    fn similarity_similar() {
        let s = similarity("bohemian rhapsody", "bohemian rapsody");
        assert!(s > 0.8);
    }

    #[test]
    fn fuzzy_empty_artist_title_carries_the_score() {
        // Favori radio sans artiste : un titre quasi exact doit matcher (le
        // score plafonnait à 0.5 < 0.7 et TOUT était rejeté, forum #1234).
        let cand = |title: &str, artist: &str| MatchCandidate {
            title: title.into(),
            artist_name: artist.into(),
            album_title: String::new(),
            source_id: "1".into(),
            duration_ms: 200_000,
            isrc: String::new(),
            score: 0.0,
            match_method: String::new(),
            confidence: String::new(),
        };

        let good = vec![cand("Summertime", "Ella Fitzgerald")];
        let m = match_fuzzy("Summertime", "", 0, &good, 0.6).expect("titre exact doit matcher");
        assert!(m.score >= 0.85, "score = {}", m.score);

        // Un titre franchement différent reste rejeté même sans artiste.
        let bad = vec![cand("Complètement autre chose", "Ella Fitzgerald")];
        assert!(match_fuzzy("Summertime", "", 0, &bad, 0.6).is_none());
    }

    #[test]
    fn isrc_match() {
        let candidates = vec![MatchCandidate {
            title: "Song".into(),
            artist_name: "Artist".into(),
            album_title: "Album".into(),
            source_id: "123".into(),
            duration_ms: 180000,
            isrc: "USRC12345678".into(),
            score: 0.0,
            match_method: String::new(),
            confidence: String::new(),
        }];

        let m = match_by_isrc("USRC12345678", &candidates).unwrap();
        assert_eq!(m.score, 1.0);
        assert_eq!(m.match_method, "isrc");
    }

    #[test]
    fn exact_match() {
        let candidates = vec![MatchCandidate {
            title: "Bohemian Rhapsody".into(),
            artist_name: "Queen".into(),
            album_title: String::new(),
            source_id: "456".into(),
            duration_ms: 354000,
            isrc: String::new(),
            score: 0.0,
            match_method: String::new(),
            confidence: String::new(),
        }];

        let m = match_exact("Bohemian Rhapsody", "Queen", &candidates).unwrap();
        assert_eq!(m.score, 0.95);
    }

    #[test]
    fn fuzzy_match() {
        let candidates = vec![MatchCandidate {
            title: "Bohemian Rapsody".into(),
            artist_name: "Queen".into(),
            album_title: String::new(),
            source_id: "789".into(),
            duration_ms: 354000,
            isrc: String::new(),
            score: 0.0,
            match_method: String::new(),
            confidence: String::new(),
        }];

        let m = match_fuzzy("Bohemian Rhapsody", "Queen", 354000, &candidates, 0.6);
        assert!(m.is_some());
        assert!(m.unwrap().score > 0.7);
    }

    #[test]
    fn find_best_full_pipeline() {
        let candidates = vec![MatchCandidate {
            title: "Imagine".into(),
            artist_name: "John Lennon".into(),
            album_title: "Imagine".into(),
            source_id: "abc".into(),
            duration_ms: 187000,
            isrc: String::new(),
            score: 0.0,
            match_method: String::new(),
            confidence: String::new(),
        }];

        let result = find_best_match("Imagine", "John Lennon", "", 187000, &candidates);
        assert_eq!(result.status, "matched");
        assert!(result.best_match.is_some());
    }

    #[test]
    fn no_match_found() {
        let result = find_best_match("Unknown Song", "Nobody", "", 0, &[]);
        assert_eq!(result.status, "not_found");
        assert!(result.best_match.is_none());
    }

    // -----------------------------------------------------------------
    // #2264 — l'identité d'ENREGISTREMENT se compare d'une seule façon.
    //
    // Les gardes passent par `find_best_match`, l'entrée que `streaming::
    // matching::best_stream_match` appelle : un témoin qui appellerait
    // `normaliser_isrc` en direct resterait vert alors même que le chemin
    // rapide ne l'emploie pas.
    // -----------------------------------------------------------------

    fn candidat(titre: &str, isrc: &str) -> MatchCandidate {
        MatchCandidate {
            title: titre.into(),
            artist_name: "Miles Davis".into(),
            album_title: String::new(),
            source_id: "qobuz-1".into(),
            duration_ms: 0,
            isrc: isrc.into(),
            score: 0.0,
            match_method: String::new(),
            confidence: String::new(),
        }
    }

    /// La forme d'affichage (balises de fichiers) et la forme compacte (API de
    /// streaming) désignent le MÊME enregistrement.
    #[test]
    fn un_isrc_a_tirets_rejoint_sa_forme_compacte() {
        let m = find_best_match(
            "So What",
            "Miles Davis",
            "FR-Z12-88-00001",
            0,
            &[candidat("Autre chose", "FRZ128800001")],
        );
        assert_eq!(m.status, "matched", "{m:?}");
        let trouve = m.best_match.expect("un rattachement par ISRC");
        assert_eq!(
            trouve.match_method, "isrc",
            "c'est le chemin RAPIDE qui doit répondre, pas le titre approché — \
             ici les titres ne se ressemblent même pas"
        );
        assert_eq!(trouve.score, 1.0);
    }

    /// La contre-épreuve : plier l'écriture ne fait pas se rejoindre deux
    /// enregistrements DIFFÉRENTS, même quand tout le reste concorde.
    #[test]
    fn deux_isrc_differents_ne_se_rejoignent_pas_par_l_isrc() {
        let m = find_best_match(
            "So What",
            "Miles Davis",
            "FR-Z12-88-00001",
            0,
            &[candidat("So What", "US-Z99-99-99999")],
        );
        assert_ne!(
            m.best_match.as_ref().map(|c| c.match_method.as_str()),
            Some("isrc"),
            "deux ISRC distincts ne sont pas le même enregistrement : {m:?}"
        );

        // Et un ISRC réduit au vide par le pliage ne rattache rien : sinon
        // « --- » rejoindrait « /// », et toute piste sans ISRC utilisable
        // s'accrocherait à la première venue.
        let m = find_best_match(
            "So What",
            "Miles Davis",
            "---",
            0,
            &[candidat("Autre chose", "///")],
        );
        assert!(
            m.best_match.is_none(),
            "rien ne doit être rattaché sur une clé vide : {m:?}"
        );
    }
}
