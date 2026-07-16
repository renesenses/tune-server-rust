//! Multi-artist credit splitting.
//!
//! Concatenated performer credits — common in jazz/classical, e.g.
//! `"Antonio Vivaldi; Alfia Bakieva, Les Musiciennes du Concert des Nations,
//! Jordi Savall"` — currently produce ONE junk `artists` row per credit string,
//! which no image/bio source (mozaiklabs, MusicBrainz, Fanart…) can match.
//!
//! This module splits a raw credit string into individual artist names, while
//! carefully NOT splitting legitimate names that contain a separator
//! (`Simon & Garfunkel`, `Earth, Wind & Fire`, `AC/DC`, …).
//!
//! Phase 0 wires only the read-only analysis (`analyze_artist_credit`) into a
//! dry-run preview endpoint so we can measure the real separator distribution
//! on live libraries before changing any scan/DB behaviour.

use crate::db::engine::fold_diacritics;

/// Legitimate group/artist names that contain a "risky" separator (`,`, `&`,
/// `+`) and must NOT be split. Compared case-insensitively and accent-folded.
/// Seed list; extended at runtime via the `artist_split_allowlist` setting.
pub const KNOWN_GROUP_NAMES: &[&str] = &[
    "simon & garfunkel",
    "earth, wind & fire",
    "crosby, stills & nash",
    "crosby, stills, nash & young",
    "hootie & the blowfish",
    "ac/dc",
    "hall & oates",
    "daryl hall & john oates",
    "blood, sweat & tears",
    "emerson, lake & palmer",
    "kool & the gang",
    "sly & the family stone",
    "mumford & sons",
    "florence + the machine",
    "derek & the dominos",
    "the mamas & the papas",
    "peter, paul and mary",
    "huey lewis & the news",
    "gladys knight & the pips",
    "diana ross & the supremes",
    "martha & the vandellas",
    "bob marley & the wailers",
    "tom petty & the heartbreakers",
    "nick cave & the bad seeds",
    "echo & the bunnymen",
    "booker t & the mgs",
    "prince & the revolution",
    "katrina & the waves",
    "kc & the sunshine band",
    "buddy holly & the crickets",
    "joan jett & the blackhearts",
];

/// Strong separators — these essentially never occur inside a single legitimate
/// artist name, so we always split on them (case-insensitive, space-padded so
/// `ft.` doesn't match inside a word).
const STRONG_MARKERS: &[&str] = &[
    " featuring ",
    " feat. ",
    " feat ",
    " ft. ",
    " ft ",
    " with ",
    " vs. ",
    " vs ",
];

/// The separator tier that produced a token boundary — for telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Separator {
    Semicolon,
    Feat,
    Comma,
    Ampersand,
}

impl Separator {
    pub fn as_str(self) -> &'static str {
        match self {
            Separator::Semicolon => ";",
            Separator::Feat => "feat",
            Separator::Comma => ",",
            Separator::Ampersand => "&",
        }
    }
}

/// Dry-run analysis of a credit string (read-only; no DB, no side effects).
#[derive(Debug, Clone)]
pub struct SplitAnalysis {
    pub original: String,
    /// Split result (== `[original]` when nothing splits).
    pub tokens: Vec<String>,
    /// Which separators contributed to the split.
    pub separators: Vec<Separator>,
    /// True when the whole string matched the allowlist (kept intact).
    pub allowlisted: bool,
}

impl SplitAnalysis {
    pub fn would_split(&self) -> bool {
        self.tokens.len() > 1
    }
}

/// Whether `name` is a known legitimate group name that must not be split.
pub fn is_allowlisted(name: &str, extra: &[String]) -> bool {
    let key = fold_diacritics(name.trim()).to_lowercase();
    KNOWN_GROUP_NAMES.iter().any(|g| *g == key)
        || extra
            .iter()
            .any(|g| fold_diacritics(g.trim()).to_lowercase() == key)
}

/// Case-insensitive `replace` (ASCII-fold on the needle boundaries is enough
/// here since all markers are ASCII).
fn replace_ci(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }
    let hay_l = haystack.to_lowercase();
    let need_l = needle.to_lowercase();
    let mut out = String::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if hay_l[i..].starts_with(&need_l) {
            out.push_str(replacement);
            i += needle.len();
        } else {
            // advance one char (respecting UTF-8 boundaries)
            let ch = haystack[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Split a raw artist credit string into individual artist names, in credit
/// order. Returns `[raw]` (unchanged) when it must not split. `extra_allowlist`
/// comes from the `artist_split_allowlist` setting.
///
/// `split_risky` gates the `,` / `&` separators (Phase 0 uses `true` for the
/// dry-run so the preview shows their full effect; scan integration will gate
/// them behind a setting once the telemetry is understood).
pub fn analyze_artist_credit(
    raw: &str,
    extra_allowlist: &[String],
    split_risky: bool,
) -> SplitAnalysis {
    let original = raw.trim().to_string();
    let mut separators: Vec<Separator> = Vec::new();

    if original.is_empty() {
        return SplitAnalysis {
            original: raw.to_string(),
            tokens: vec![],
            separators,
            allowlisted: false,
        };
    }

    // Never split the compilation sentinel or an allowlisted legit name.
    if original.eq_ignore_ascii_case("various artists") || original.eq_ignore_ascii_case("va") {
        return SplitAnalysis {
            original: original.clone(),
            tokens: vec![original],
            separators,
            allowlisted: false,
        };
    }
    if is_allowlisted(&original, extra_allowlist) {
        return SplitAnalysis {
            original: original.clone(),
            tokens: vec![original],
            separators,
            allowlisted: true,
        };
    }

    // 1) Strong split: normalise `;` and the feat-markers to a sentinel.
    let mut work = original.replace(';', "\u{1}");
    if work.contains('\u{1}') {
        separators.push(Separator::Semicolon);
    }
    for m in STRONG_MARKERS {
        let replaced = replace_ci(&work, m, "\u{1}");
        if replaced != work {
            if !separators.contains(&Separator::Feat) {
                separators.push(Separator::Feat);
            }
            work = replaced;
        }
    }

    let strong_pieces: Vec<String> = work
        .split('\u{1}')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();

    // 2) Risky split (comma / ampersand) on each piece, unless the piece itself
    //    is an allowlisted legit name.
    let mut tokens: Vec<String> = Vec::new();
    for piece in strong_pieces {
        if !split_risky || is_allowlisted(&piece, extra_allowlist) {
            tokens.push(piece);
            continue;
        }
        let mut w = piece.clone();
        if w.contains(',') {
            separators.push(Separator::Comma);
            w = w.replace(',', "\u{1}");
        }
        for amp in [" & ", " and "] {
            let replaced = replace_ci(&w, amp, "\u{1}");
            if replaced != w {
                if !separators.contains(&Separator::Ampersand) {
                    separators.push(Separator::Ampersand);
                }
                w = replaced;
            }
        }
        for tok in w.split('\u{1}') {
            let t = tok.trim();
            // Drop empty / single-char noise tokens.
            if t.chars().count() >= 2 {
                tokens.push(t.to_string());
            }
        }
    }

    // De-dup while preserving order (case/accent-insensitive).
    let mut seen: Vec<String> = Vec::new();
    let mut deduped: Vec<String> = Vec::new();
    for t in tokens {
        let key = fold_diacritics(&t).to_lowercase();
        if !seen.contains(&key) {
            seen.push(key);
            deduped.push(t);
        }
    }

    if deduped.is_empty() {
        deduped.push(original.clone());
    }

    SplitAnalysis {
        original,
        tokens: deduped,
        separators,
        allowlisted: false,
    }
}

/// Individual artist names from a credit string (see [`analyze_artist_credit`]).
pub fn split_artist_credit(
    raw: &str,
    extra_allowlist: &[String],
    split_risky: bool,
) -> Vec<String> {
    analyze_artist_credit(raw, extra_allowlist, split_risky).tokens
}

/// The single artist name used for album grouping: the first token after a
/// split, or the whole string when nothing splits. Kept stable so album
/// grouping does not drift for legitimate single-artist tags.
pub fn primary_artist(raw: &str, extra_allowlist: &[String], split_risky: bool) -> String {
    analyze_artist_credit(raw, extra_allowlist, split_risky)
        .tokens
        .into_iter()
        .next()
        .unwrap_or_else(|| raw.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(s: &str) -> Vec<String> {
        split_artist_credit(s, &[], true)
    }

    #[test]
    fn splits_classical_concatenation() {
        assert_eq!(
            split(
                "Antonio Vivaldi; Alfia Bakieva, Les Musiciennes du Concert des Nations, Jordi Savall"
            ),
            vec![
                "Antonio Vivaldi",
                "Alfia Bakieva",
                "Les Musiciennes du Concert des Nations",
                "Jordi Savall"
            ]
        );
    }

    #[test]
    fn splits_ampersand_and_comma_mix() {
        assert_eq!(
            split("Jordi Savall, Les Musiciennes du Concert des Nations & Alfia Bakieva"),
            vec![
                "Jordi Savall",
                "Les Musiciennes du Concert des Nations",
                "Alfia Bakieva"
            ]
        );
    }

    #[test]
    fn splits_feat_variants() {
        assert_eq!(
            split("Miles Davis feat. John Coltrane"),
            vec!["Miles Davis", "John Coltrane"]
        );
        assert_eq!(
            split("Kendrick Lamar ft. SZA"),
            vec!["Kendrick Lamar", "SZA"]
        );
        assert_eq!(
            split("Santana featuring Rob Thomas"),
            vec!["Santana", "Rob Thomas"]
        );
    }

    #[test]
    fn keeps_allowlisted_group_names() {
        assert_eq!(split("Simon & Garfunkel"), vec!["Simon & Garfunkel"]);
        assert_eq!(split("Earth, Wind & Fire"), vec!["Earth, Wind & Fire"]);
        assert_eq!(
            split("Crosby, Stills & Nash"),
            vec!["Crosby, Stills & Nash"]
        );
        assert_eq!(
            split("Hootie & the Blowfish"),
            vec!["Hootie & the Blowfish"]
        );
    }

    #[test]
    fn never_splits_slash_or_various() {
        assert_eq!(split("AC/DC"), vec!["AC/DC"]);
        assert_eq!(split("Various Artists"), vec!["Various Artists"]);
    }

    #[test]
    fn single_artist_unchanged() {
        assert_eq!(split("Beatles"), vec!["Beatles"]);
        assert_eq!(split("Keith Jarrett"), vec!["Keith Jarrett"]);
    }

    #[test]
    fn empty_or_whitespace() {
        assert!(split("").is_empty());
        assert!(split("   ").is_empty());
    }

    #[test]
    fn primary_is_first_token() {
        assert_eq!(
            primary_artist("Miles Davis feat. John Coltrane", &[], true),
            "Miles Davis"
        );
        assert_eq!(primary_artist("Keith Jarrett", &[], true), "Keith Jarrett");
        // Allowlisted stays whole.
        assert_eq!(
            primary_artist("Simon & Garfunkel", &[], true),
            "Simon & Garfunkel"
        );
    }

    #[test]
    fn risky_disabled_keeps_comma_groups() {
        // With risky splitting off, only strong separators split.
        assert_eq!(
            split_artist_credit("Herbie Hancock, Wayne Shorter", &[], false),
            vec!["Herbie Hancock, Wayne Shorter"]
        );
        assert_eq!(
            split_artist_credit("Miles Davis feat. John Coltrane", &[], false),
            vec!["Miles Davis", "John Coltrane"]
        );
    }

    #[test]
    fn extra_allowlist_is_honoured() {
        // A runtime-allowlisted name with a comma is kept whole.
        let extra = vec!["Sonny, Cher".to_string()];
        assert_eq!(
            split_artist_credit("Sonny, Cher", &extra, true),
            vec!["Sonny, Cher"]
        );
        // An unrelated name still splits on a strong separator.
        assert_eq!(
            split_artist_credit("Aaa feat. Bbb", &extra, true),
            vec!["Aaa", "Bbb"]
        );
    }

    #[test]
    fn tokens_are_trimmed_and_deduped() {
        assert_eq!(split("Bill Evans; Bill Evans"), vec!["Bill Evans"]);
    }
}
