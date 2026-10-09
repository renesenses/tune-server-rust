//! Le détail de la mémoire résidente, pour la ligne `memory_diagnostics`.
//!
//! Le RSS seul ne dit pas CE qui est résident. Trois parts le composent, que
//! le noyau Linux publie dans `/proc/self/status` (depuis la version 4.5) :
//!
//! - `RssAnon` : la mémoire anonyme — le tas, les piles, les arènes de
//!   l'allocateur. C'est là que vit tout ce que le serveur alloue, et aussi
//!   la mémoire libre que l'allocateur garde sans la rendre.
//! - `RssFile` : les pages adossées à un fichier — le binaire et ses
//!   bibliothèques, et surtout les fichiers projetés en mémoire (la base
//!   SQLite en `mmap`). Le noyau les reprend sans rien perdre.
//! - `RssShmem` : la mémoire partagée (`tmpfs`, segments partagés).
//!
//! Un serveur qui pèse plusieurs gigaoctets au repos ne se diagnostique pas
//! de la même façon selon la part qui domine : un `RssAnon` massif désigne le
//! tas (fuite, ou mémoire libre gardée par l'allocateur), un `RssFile` massif
//! désigne les projections de fichiers, que le système récupère dès qu'il en
//! a besoin.
//!
//! Hors Linux, ni `/proc/self/status` ni cette ventilation n'existent : la
//! lecture rend un relevé VIDE, et chaque champ absent est simplement omis de
//! la ligne de journal. Jamais de panique, jamais de zéro inventé.

/// Les trois parts du RSS, en mégaoctets. `None` = indisponible (hors Linux,
/// noyau antérieur à 4.5, ligne absente ou illisible).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DetailRss {
    pub anon_mb: Option<u64>,
    pub file_mb: Option<u64>,
    pub shmem_mb: Option<u64>,
}

/// Lit le détail du processus courant. Vide hors Linux, ou si le fichier ne
/// se lit pas.
pub async fn lire() -> DetailRss {
    #[cfg(target_os = "linux")]
    {
        match tokio::fs::read_to_string("/proc/self/status").await {
            Ok(status) => analyser_status(&status),
            Err(_) => DetailRss::default(),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        DetailRss::default()
    }
}

/// Extrait `RssAnon`, `RssFile` et `RssShmem` d'un texte au format de
/// `/proc/<pid>/status` (`RssAnon:\t  123456 kB`). Pure, donc testable sur
/// toutes les plateformes. Une ligne absente, sans unité `kB` ou au nombre
/// illisible donne `None` pour son champ, et rien d'autre.
pub fn analyser_status(status: &str) -> DetailRss {
    let mut detail = DetailRss::default();
    for ligne in status.lines() {
        let Some((cle, valeur)) = ligne.split_once(':') else {
            continue;
        };
        let champ = match cle.trim() {
            "RssAnon" => &mut detail.anon_mb,
            "RssFile" => &mut detail.file_mb,
            "RssShmem" => &mut detail.shmem_mb,
            _ => continue,
        };
        *champ = kio_en_mo(valeur);
    }
    detail
}

/// `"  123456 kB"` → `Some(120)`. Le noyau écrit toujours des kio ; une
/// autre unité n'est pas devinée.
fn kio_en_mo(valeur: &str) -> Option<u64> {
    let mut morceaux = valeur.split_whitespace();
    let nombre: u64 = morceaux.next()?.parse().ok()?;
    match morceaux.next() {
        Some(unite) if unite.eq_ignore_ascii_case("kB") => Some(nombre / 1024),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un extrait réel de `/proc/self/status` (noyau 6.x).
    const STATUS: &str = "Name:\ttune-server\n\
        VmRSS:\t 2765432 kB\n\
        RssAnon:\t 2457600 kB\n\
        RssFile:\t  296960 kB\n\
        RssShmem:\t   10872 kB\n\
        VmSwap:\t       0 kB\n";

    #[test]
    fn les_trois_parts_sont_lues_en_mo() {
        assert_eq!(
            analyser_status(STATUS),
            DetailRss {
                anon_mb: Some(2400),
                file_mb: Some(290),
                shmem_mb: Some(10),
            }
        );
    }

    #[test]
    fn un_noyau_sans_ventilation_rend_un_releve_vide() {
        // Noyau antérieur à 4.5 : VmRSS seul.
        let ancien = "Name:\ttune-server\nVmRSS:\t 123456 kB\n";
        assert_eq!(analyser_status(ancien), DetailRss::default());
        assert_eq!(analyser_status(""), DetailRss::default());
    }

    #[test]
    fn une_ligne_illisible_ne_vide_que_son_champ() {
        let abime =
            "RssAnon:\t abc kB\nRssFile:\t 2048 kB\nRssShmem:\t 1024 MB\nRssAnon2:\t 9 kB\n";
        assert_eq!(
            analyser_status(abime),
            DetailRss {
                anon_mb: None,
                file_mb: Some(2),
                shmem_mb: None,
            }
        );
    }

    #[tokio::test]
    async fn la_lecture_ne_panique_jamais() {
        let detail = lire().await;
        // Sous Linux, le noyau de la CI et de Shrek publie la ventilation :
        // le tas d'un processus de test n'est jamais vide.
        #[cfg(target_os = "linux")]
        assert!(detail.anon_mb.is_some(), "RssAnon absent : {detail:?}");
        // Ailleurs, le relevé est vide, pas inventé.
        #[cfg(not(target_os = "linux"))]
        assert_eq!(detail, DetailRss::default());
    }
}
