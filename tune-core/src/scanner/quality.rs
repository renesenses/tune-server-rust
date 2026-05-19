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
        if sr >= 1000 { format!("{}kHz", sr / 1000) } else { format!("{}Hz", sr) }
    });
    let depth_part = bit_depth.filter(|_| hi_depth).map(|bd| format!("{}bit", bd));

    match (rate_part, depth_part) {
        (Some(r), Some(d)) => format!("{r}/{d}"),
        (Some(r), None) => r,
        (None, Some(d)) => d,
        (None, None) => String::new(),
    }
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
}
