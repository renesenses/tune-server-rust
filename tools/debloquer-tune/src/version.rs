//! Classer la version annoncée par le Tune.
//!
//! L'outil n'agit QUE sur une version antérieure à la rc13. Dès la rc13, la
//! page renvoie au fil épinglé : on n'y force rien, et en particulier on
//! n'utilise aucun défaut de connexion de ces versions.

/// Ce que l'outil a le droit de faire selon la version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Famille {
    /// 1.0.0-rc12 (correctifs rc12.x compris) ou plus ancien : procédure
    /// « activer l'authentification puis mettre à jour ».
    AvantRc13,
    /// 1.0.0-rc13 ou plus récent : on ne fait rien.
    Rc13OuPlus,
    /// Version illisible : on ne fait rien non plus.
    Inconnue,
}

/// `"1.0.0-rc12"` → `([1,0,0], Some([12]))`. Tolère un `v` initial et un
/// suffixe de construction `+…`. Rend `None` si les numéros sont illisibles.
fn decouper(v: &str) -> Option<(Vec<u64>, Option<Vec<u64>>)> {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let v = v.split('+').next().unwrap_or("");
    let (rel, pre) = match v.split_once('-') {
        Some((r, p)) => (r, Some(p)),
        None => (v, None),
    };
    let nums: Vec<u64> = rel
        .split('.')
        .map(|p| p.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()?;
    if nums.is_empty() || nums.len() > 4 {
        return None;
    }
    let pre = match pre {
        None => None,
        Some(p) => {
            let n: Vec<u64> = p
                .split(|c: char| !c.is_ascii_digit())
                .filter_map(|t| t.parse().ok())
                .collect();
            if n.is_empty() {
                return None;
            }
            Some(n)
        }
    };
    Some((nums, pre))
}

pub fn famille(version: &str) -> Famille {
    let Some((nums, pre)) = decouper(version) else {
        return Famille::Inconnue;
    };
    let base = |i: usize| nums.get(i).copied().unwrap_or(0);
    let triple = (base(0), base(1), base(2));
    if triple > (1, 0, 0) {
        return Famille::Rc13OuPlus;
    }
    if triple < (1, 0, 0) {
        return Famille::AvantRc13;
    }
    match pre {
        // 1.0.0 finale : plus récente que toute rc.
        None => Famille::Rc13OuPlus,
        Some(p) if p[0] >= 13 => Famille::Rc13OuPlus,
        Some(_) => Famille::AvantRc13,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_rc12_et_ses_correctifs_sont_a_debloquer() {
        for v in [
            "1.0.0-rc12",
            "v1.0.0-rc12",
            "1.0.0-rc12.1",
            "1.0.0-rc11",
            "0.9.169",
            "1.0.0-rc12+abc",
        ] {
            assert_eq!(famille(v), Famille::AvantRc13, "{v}");
        }
    }

    #[test]
    fn la_rc13_et_au_dela_ne_sont_jamais_forcees() {
        for v in [
            "1.0.0-rc13",
            "1.0.0-rc13.1",
            "1.0.0-rc14",
            "1.0.0",
            "1.0.1",
            "1.1.0-rc1",
            "2.0.0",
        ] {
            assert_eq!(famille(v), Famille::Rc13OuPlus, "{v}");
        }
    }

    #[test]
    fn une_version_illisible_n_ouvre_rien() {
        for v in ["", "tune", "1.x.0", "debloquer-tune-v1", "1.0.0-rc"] {
            assert_eq!(famille(v), Famille::Inconnue, "{v}");
        }
    }
}
