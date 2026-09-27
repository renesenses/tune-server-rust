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

/// Leading articles used to (a) undo the "sort form" `Beatles, The` → `The
/// Beatles`, and (b) keep `X & The/His/Her Y` band names together instead of
/// splitting off the ensemble. Measured on a real library, `, The` sort forms
/// were ~49% of the comma "splits" (false positives).
const ARTICLES: &[&str] = &[
    "the", "his", "her", "their", "les", "los", "las", "die", "das", "der", "la", "le", "el", "il",
    "gli", "i",
];

/// Strong separators — these essentially never occur inside a single legitimate
/// artist name, so we always split on them (case-insensitive, space-padded so
/// `ft.` doesn't match inside a word).
/// Generational and honorific suffixes that follow a comma but belong to the
/// SAME person: `"Grover Washington, Jr."` is one artist, not two. Same family
/// as the `"X, The"` sort form undone by [`reorder_sort_form`].
const NAME_SUFFIXES: &[&str] = &[
    "jr", "jr.", "sr", "sr.", "jnr", "jnr.", "snr", "snr.", "ii", "iii", "iv",
];

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

/// Case-insensitive `replace`.
///
/// #5236 — la comparaison se fait CARACTÈRE PAR CARACTÈRE, sur `haystack`
/// seul. L'ancienne version indexait `haystack.to_lowercase()` avec des
/// positions en octets de `haystack` : dès qu'une minuscule change de taille
/// (« İ » → « i̇ », 2 → 3 octets ; « ẞ » → « ß », 3 → 2), les positions
/// tombaient au milieu d'un caractère et le serveur paniquait au démarrage
/// (passe `recalculer_les_compilations`, v0.9.166).
fn replace_ci(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }
    let mut out = String::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        let rest = &haystack[i..];
        if let Some(n) = prefix_len_ci(rest, needle) {
            out.push_str(replacement);
            i += n;
        } else {
            // `i` est toujours une frontière de caractère de `haystack`.
            let Some(ch) = rest.chars().next() else { break };
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Longueur en octets, DANS `hay`, du préfixe de `hay` égal à `needle` sans
/// tenir compte de la casse (caractère par caractère), ou `None`.
fn prefix_len_ci(hay: &str, needle: &str) -> Option<usize> {
    let mut chars = hay.char_indices();
    for nc in needle.chars() {
        let (_, hc) = chars.next()?;
        if hc != nc && !hc.to_lowercase().eq(nc.to_lowercase()) {
            return None;
        }
    }
    Some(chars.next().map_or(hay.len(), |(j, _)| j))
}

/// Whether `s` begins with a leading article (`the`, `his`, `les`, …).
fn starts_with_article(s: &str) -> bool {
    let l = s.trim().to_lowercase();
    ARTICLES
        .iter()
        .any(|a| l == *a || l.starts_with(&format!("{a} ")))
}

/// Undo a trailing "sort form": `"Beatles, The"` → `"The Beatles"`. Only fires
/// when the text after the LAST comma is exactly an article.
fn reorder_sort_form(name: &str) -> String {
    if let Some((head, tail)) = name.rsplit_once(',') {
        let t = tail.trim();
        if !t.is_empty() && ARTICLES.contains(&t.to_lowercase().as_str()) {
            return format!("{} {}", t, head.trim());
        }
    }
    name.to_string()
}

/// Re-attach a bare generational suffix to the part before it, mirroring how
/// [`split_ampersand`] re-attaches an article-led ensemble to its leader.
fn reattach_name_suffixes(parts: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in parts {
        let key = p.trim().to_lowercase();
        if NAME_SUFFIXES.contains(&key.as_str())
            && let Some(last) = out.last_mut()
        {
            *last = format!("{last}, {}", p.trim());
            continue;
        }
        out.push(p);
    }
    out
}

/// Split on ` & ` / ` and `, but keep `X & The/His/Her Y` together (band name:
/// "Count Basie & His Orchestra", "Art Blakey & The Jazz Messengers"). Returns
/// `(parts, did_split)`.
fn split_ampersand(s: &str) -> (Vec<String>, bool) {
    let norm = replace_ci(s, " and ", " & ");
    if !norm.contains(" & ") {
        return (vec![s.trim().to_string()], false);
    }
    let mut out: Vec<String> = Vec::new();
    let mut did = false;
    for part in norm.split(" & ") {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        if let Some(last) = out.last_mut()
            && starts_with_article(p)
        {
            // Re-attach the ensemble to the leader: one performing entity.
            *last = format!("{last} & {p}");
            continue;
        }
        if !out.is_empty() {
            did = true;
        }
        out.push(p.to_string());
    }
    if out.is_empty() {
        out.push(s.trim().to_string());
    }
    (out, did)
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
    // Undo "Beatles, The" sort form up-front so the trailing article isn't
    // mistaken for a separate artist (the dominant comma false positive).
    let original = reorder_sort_form(raw.trim());
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

    // 2) Risky split (comma, then article-aware ampersand) on each strong piece,
    //    unless the piece itself is an allowlisted legit name.
    let mut tokens: Vec<String> = Vec::new();
    for piece in strong_pieces {
        if !split_risky || is_allowlisted(&piece, extra_allowlist) {
            tokens.push(piece);
            continue;
        }
        let comma_parts: Vec<String> = if piece.contains(',') {
            let parts = reattach_name_suffixes(
                piece
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            );
            // Une virgule qui ne separe rien (suffixe rattache) n'est pas un
            // separateur : ne pas l'inscrire, sinon le rapport ment.
            if parts.len() > 1 {
                separators.push(Separator::Comma);
            }
            parts
        } else {
            vec![piece.clone()]
        };
        for cp in comma_parts {
            if is_allowlisted(&cp, extra_allowlist) {
                tokens.push(cp);
                continue;
            }
            let (parts, did) = split_ampersand(&cp);
            if did && !separators.contains(&Separator::Ampersand) {
                separators.push(Separator::Ampersand);
            }
            for t in parts {
                // Drop empty / single-char noise tokens.
                if t.chars().count() >= 2 {
                    tokens.push(t);
                }
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

    // --- Regression tests from the real .18 library dry-run (Phase 0) ---

    #[test]
    fn sort_form_comma_the_is_reordered_not_split() {
        // "~49% of comma splits" were this pattern → junk "The" token.
        assert_eq!(split("Delfonics, The"), vec!["The Delfonics"]);
        assert_eq!(split("Grass Roots, The"), vec!["The Grass Roots"]);
        assert_eq!(split("Brothers Johnson, The"), vec!["The Brothers Johnson"]);
    }

    #[test]
    fn ampersand_ensemble_stays_together() {
        // "X & His/Her/The <ensemble>" is one performing entity, not two.
        assert_eq!(
            split("Count Basie & His Orchestra"),
            vec!["Count Basie & His Orchestra"]
        );
        assert_eq!(
            split("Art Blakey & The Jazz Messengers"),
            vec!["Art Blakey & The Jazz Messengers"]
        );
        assert_eq!(
            split("Bob Marley & The Wailers"),
            vec!["Bob Marley & The Wailers"]
        );
    }

    #[test]
    fn ampersand_two_people_still_splits() {
        assert_eq!(
            split("Ella Fitzgerald & Louis Armstrong"),
            vec!["Ella Fitzgerald", "Louis Armstrong"]
        );
        assert_eq!(
            split("Bill Evans & Jim Hall"),
            vec!["Bill Evans", "Jim Hall"]
        );
        assert_eq!(
            split("Duke Ellington & John Coltrane"),
            vec!["Duke Ellington", "John Coltrane"]
        );
    }

    #[test]
    fn mixed_comma_and_article_ampersand() {
        // Real: comma splits the leaders, ampersand-with-name splits the last two.
        assert_eq!(
            split("Jordi Savall, Les Musiciennes du Concert des Nations & Alfia Bakieva"),
            vec![
                "Jordi Savall",
                "Les Musiciennes du Concert des Nations",
                "Alfia Bakieva"
            ]
        );
    }

    /// #5236 — des caractères dont la minuscule n'a PAS la même taille en
    /// octets que la majuscule : « İ » (2 → 3), « ẞ » (3 → 2), « Ⱥ » (2 → 3),
    /// « Ω » (U+2126, 3 → 2), « K » (U+212A, 3 → 1).
    const TAILLE_CHANGEANTE: &[char] = &['İ', 'ẞ', 'Ⱥ', 'Ⱦ', '\u{2126}', '\u{212A}'];

    #[test]
    fn replace_ci_ne_panique_pas_quand_la_minuscule_change_de_taille() {
        assert_eq!(replace_ci("İlhan and Ömer", " and ", " & "), "İlhan & Ömer");
        assert_eq!(replace_ci("ẞéb AND Ⱥé", " and ", " & "), "ẞéb & Ⱥé");
        assert_eq!(
            replace_ci("İstanbul Devlet Senfoni Orkestrası", " and ", " & "),
            "İstanbul Devlet Senfoni Orkestrası"
        );
        assert_eq!(replace_ci("Ⱥé feat. İİ", " feat. ", "|"), "Ⱥé|İİ");
        // Aucun caractère n'est perdu ni dédoublé quand rien ne correspond.
        for &c in TAILLE_CHANGEANTE {
            let s = format!("{c}é{c} x {c}");
            assert_eq!(replace_ci(&s, " and ", " & "), s);
        }
    }

    #[test]
    fn le_decoupage_d_un_credit_turc_ou_allemand_ne_panique_pas() {
        assert_eq!(
            split("İstanbul Devlet Senfoni Orkestrası feat. Ⱥlpha"),
            vec!["İstanbul Devlet Senfoni Orkestrası", "Ⱥlpha"]
        );
        assert_eq!(split("İlhan and Ömer"), vec!["İlhan", "Ömer"]);
        assert_eq!(split("GROẞE Band & Ⱥnna"), vec!["GROẞE Band", "Ⱥnna"]);
    }

    /// Balayage : TOUT caractère du plan multilingue de base dont la minuscule
    /// change de taille, placé autour de chaque marqueur. Aucune panique.
    #[test]
    fn aucun_caractere_a_minuscule_de_taille_changeante_ne_fait_paniquer() {
        let mut vus = 0;
        for c in (0u32..0x1_0000).filter_map(char::from_u32) {
            let bas: usize = c.to_lowercase().map(char::len_utf8).sum();
            if bas == c.len_utf8() {
                continue;
            }
            vus += 1;
            for gabarit in [
                "{c}x and y{c}é",
                "{c}{c} feat. {c}é, {c}ö & The {c}",
                "é{c} vs {c}; {c} with {c}ä",
            ] {
                let s = gabarit.replace("{c}", &c.to_string());
                let _ = analyze_artist_credit(&s, &[], true);
                let _ = analyze_artist_credit(&s, &[], false);
            }
        }
        assert!(vus > 10, "le balayage doit trouver des cas ({vus})");
    }
}
