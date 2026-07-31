pub fn same_quality_tier(sr1: Option<u32>, sr2: Option<u32>) -> bool {
    match (sr1, sr2) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

pub fn quality_suffix(sample_rate: Option<u32>, bit_depth: Option<u16>) -> String {
    let hi_rate = sample_rate.is_some_and(|sr| sr > 44100);
    let hi_depth = bit_depth.is_some_and(|bd| bd > 16);

    if !hi_rate && !hi_depth {
        return String::new();
    }

    let rate_part = sample_rate.filter(|_| hi_rate).map(|sr| {
        if sr >= 1000 {
            format!("{}kHz", sr / 1000)
        } else {
            format!("{}Hz", sr)
        }
    });
    let depth_part = bit_depth
        .filter(|_| hi_depth)
        .map(|bd| format!("{}bit", bd));

    match (rate_part, depth_part) {
        (Some(r), Some(d)) => format!("{r}/{d}"),
        (Some(r), None) => r,
        (None, Some(d)) => d,
        (None, None) => String::new(),
    }
}

/// Remove a quality suffix from an album title — the inverse of
/// [`quality_suffix`].
///
/// The scanner used to append the tier to the title (`"Album (96kHz/24bit)"`) to
/// keep a hi-res copy apart from a CD rip; the album's folder does that now, and
/// clients render the real quality from `sample_rate`/`bit_depth`. This undoes
/// what was written into titles by that scheme.
///
/// Only parenthesised groups that are *entirely* a quality descriptor are
/// dropped, so "Live (Remastered)" and "Symphony No. 5 (1962)" are untouched.
pub fn strip_quality_suffix(title: &str) -> String {
    let mut result = String::with_capacity(title.len());
    let mut depth = 0i32;
    let mut paren_start = 0;
    for (i, c) in title.char_indices() {
        if c == '(' {
            if depth == 0 {
                paren_start = i;
            }
            depth += 1;
        } else if c == ')' {
            depth -= 1;
            if depth <= 0 {
                depth = 0;
                let inner = &title[paren_start + 1..i];
                if is_quality_descriptor(inner) {
                    while result.ends_with(' ') {
                        result.pop();
                    }
                } else {
                    result.push_str(&title[paren_start..=i]);
                }
            }
        } else if depth == 0 {
            result.push(c);
        }
    }
    result.trim().to_string()
}

/// Whether a parenthesised group is nothing but a rate and/or bit depth:
/// `96kHz/24bit`, `192kHz`, `24bit`, `2822kHz`, `44.1kHz 16bit`.
fn is_quality_descriptor(inner: &str) -> bool {
    let lower = inner.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    let mut saw_unit = false;
    for part in lower.split(['/', ' ', ',']).filter(|p| !p.is_empty()) {
        let Some(number) = part
            .strip_suffix("khz")
            .or_else(|| part.strip_suffix("hz"))
            .or_else(|| part.strip_suffix("bit"))
        else {
            return false;
        };
        if number.is_empty()
            || !number
                .chars()
                .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
        {
            return false;
        }
        saw_unit = true;
    }
    saw_unit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_tier_equal() {
        assert!(same_quality_tier(Some(44100), Some(44100)));
        assert!(same_quality_tier(Some(96000), Some(96000)));
    }

    #[test]
    fn same_tier_none() {
        assert!(same_quality_tier(None, Some(44100)));
        assert!(same_quality_tier(Some(44100), None));
        assert!(same_quality_tier(None, None));
    }

    #[test]
    fn different_tier() {
        assert!(!same_quality_tier(Some(44100), Some(96000)));
    }

    #[test]
    fn suffix_hires() {
        assert_eq!(quality_suffix(Some(96000), Some(24)), "96kHz/24bit");
        assert_eq!(quality_suffix(Some(192000), Some(24)), "192kHz/24bit");
    }

    #[test]
    fn suffix_cd() {
        assert_eq!(quality_suffix(Some(44100), Some(16)), "");
    }

    #[test]
    fn suffix_rate_only() {
        assert_eq!(quality_suffix(Some(96000), Some(16)), "96kHz");
    }

    #[test]
    fn suffix_depth_only() {
        assert_eq!(quality_suffix(Some(44100), Some(24)), "24bit");
    }

    #[test]
    fn strip_undoes_what_suffix_wrote() {
        // Every shape `quality_suffix` can produce, round-tripped.
        for (sr, bd) in [
            (Some(96000), Some(24)),
            (Some(192000), Some(24)),
            (Some(96000), Some(16)),
            (Some(44100), Some(24)),
        ] {
            let suffix = quality_suffix(sr, bd);
            assert!(!suffix.is_empty());
            assert_eq!(strip_quality_suffix(&format!("Album ({suffix})")), "Album");
        }
    }

    #[test]
    fn strip_handles_the_titles_found_in_the_wild() {
        assert_eq!(strip_quality_suffix("Jolene (96kHz/24bit)"), "Jolene");
        assert_eq!(
            strip_quality_suffix("Wish You Were Here (2822kHz)"),
            "Wish You Were Here"
        );
        assert_eq!(strip_quality_suffix("Consolidate (24bit)"), "Consolidate");
        // Double space before the suffix, as one library had it.
        assert_eq!(
            strip_quality_suffix("The Division Bell  (192kHz/24bit)"),
            "The Division Bell"
        );
        assert_eq!(strip_quality_suffix("American Idiot"), "American Idiot");
    }

    #[test]
    fn strip_leaves_real_parentheses_alone() {
        for title in [
            "Live (Remastered)",
            "Symphony No. 5 (1962)",
            "Tommy (Deluxe Edition)",
            "Songs (For Drella)",
            "1999 (2019 Remaster)",
        ] {
            assert_eq!(strip_quality_suffix(title), title, "{title} must survive");
        }
    }

    #[test]
    fn strip_keeps_a_quality_group_that_is_not_the_whole_parenthesis() {
        // "24bit Remaster" is a description, not a machine-written tier.
        assert_eq!(
            strip_quality_suffix("Album (24bit Remaster)"),
            "Album (24bit Remaster)"
        );
    }
}
