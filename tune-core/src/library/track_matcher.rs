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

pub fn match_by_isrc(source_isrc: &str, candidates: &[MatchCandidate]) -> Option<MatchCandidate> {
    if source_isrc.is_empty() {
        return None;
    }
    candidates
        .iter()
        .find(|c| !c.isrc.is_empty() && c.isrc.eq_ignore_ascii_case(source_isrc))
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

        let mut score = title_sim * 0.5 + artist_sim * 0.4;

        if duration_ms > 0 && c.duration_ms > 0 {
            let dur_diff = (duration_ms - c.duration_ms).unsigned_abs() as f64;
            let dur_ratio = 1.0 - (dur_diff / duration_ms.max(1) as f64).min(1.0);
            score += dur_ratio * 0.1;
        }

        if score >= threshold
            && best.as_ref().is_none_or(|(bs, _)| score > *bs) {
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
}
