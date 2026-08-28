//! Du chemin **stocké** au chemin que le **système de fichiers** accepte.
//!
//! Le scanner enregistre les chemins normalisés en **NFC**
//! (`scanner/walker.rs`, `auto_scan.rs`) : c'est ce qui donne une clé de
//! comparaison stable entre deux balayages et ce qui a réglé le « scan
//! interminable » (`routes/system/scan.rs`). Mais macOS, les partages
//! Samba/CIFS et les disques venus d'un Mac portent les noms de fichiers en
//! **NFD** (`e` + accent combinant). Les deux chaînes s'affichent à
//! l'identique et ne sont pas égales octet à octet : `open()` avec le chemin
//! de la base rend `ENOENT` alors que le fichier est là (#1865).
//!
//! # La règle, en une phrase
//!
//! **La normalisation sert à FABRIQUER DES CANDIDATS et à COMPARER ; jamais à
//! stocker, jamais à ouvrir.**
//!
//! Concrètement, [`resolve_local_path`] ne rend JAMAIS une chaîne normalisée
//! par nos soins : elle rend l'orthographe pour laquelle `exists()` a répondu
//! vrai, telle quelle. Sur un montage réseau ou un système de fichiers
//! sensible à la forme — où une seule des deux graphies désigne un fichier —
//! réécrire le chemin le rendrait introuvable. Le chemin stocké en base, lui,
//! n'est pas touché : il reste NFC, sinon la déduplication du scan repart en
//! vrille.
//!
//! # Absent n'est pas illisible
//!
//! [`LocalPath::Missing`] ne dit pas « ce fichier n'existe plus », il dit
//! « aucune graphie ne répond MAINTENANT ». Un partage démonté, un disque USB
//! débranché rendent exactement cela — et reviennent. Les passes de fond
//! doivent donc **différer** une piste absente, jamais la marquer traitée :
//! un témoin d'échec posé sur un `ENOENT` transforme une panne passagère en
//! état définitif (c'est ce qui a gelé 135 pistes sur .18). Voir
//! [`deferral_stamp`].

use unicode_normalization::UnicodeNormalization as _;

/// Graphies possibles, sur le disque, d'un chemin lu en base — dans l'ordre où
/// il faut les essayer.
///
/// 1. **Le chemin stocké tel quel** : le cas de loin le plus fréquent, et le
///    seul qui soit certainement une graphie réelle.
/// 2. **Sa forme NFD** : le fichier vient de macOS ou d'un partage SMB/CIFS.
/// 3. **Sa forme NFC** : le sens inverse — la base tient du NFD (import,
///    greffon, base écrite par une version antérieure au scanner normalisant)
///    et le disque du NFC.
///
/// Un chemin purement ASCII rend un seul candidat : les trois formes
/// coïncident et un `stat` de plus ne coûterait rien pour rien.
pub fn local_path_candidates(stored: &str) -> Vec<String> {
    let mut candidates = vec![stored.to_string()];
    for form in [
        stored.nfd().collect::<String>(),
        stored.nfc().collect::<String>(),
    ] {
        if !candidates.contains(&form) {
            candidates.push(form);
        }
    }
    candidates
}

/// Ce que le disque répond pour un chemin stocké.
///
/// Le type existe pour que l'appelant NE PUISSE PAS confondre les deux échecs
/// qui n'appellent pas le même remède : un fichier introuvable
/// ([`LocalPath::Missing`]) peut revenir, un fichier trouvé mais indécodable
/// non. Les traiter pareil est le défaut central de #1865.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalPath {
    /// Une graphie existe sur le disque. La chaîne portée est **celle que le
    /// système a reconnue**, à donner telle quelle à `open()`.
    Found(String),
    /// Aucun candidat n'existe à cet instant. Ce n'est pas une preuve de
    /// disparition : partage démonté, disque absent, permissions.
    Missing,
}

impl LocalPath {
    /// La graphie trouvée, ou `None`.
    pub fn found(self) -> Option<String> {
        match self {
            LocalPath::Found(p) => Some(p),
            LocalPath::Missing => None,
        }
    }

    /// Vrai quand aucune graphie ne répond.
    pub fn is_missing(&self) -> bool {
        matches!(self, LocalPath::Missing)
    }
}

/// Première graphie de `stored` qui existe réellement sur le disque.
///
/// Rend la chaîne **telle que le système de fichiers l'a acceptée** — pas une
/// version normalisée. C'est le point 2 de l'en-tête de module : ce que
/// l'appelant passera à `open()` a déjà été validé par un `exists()`.
pub fn resolve_local_path(stored: &str) -> LocalPath {
    match local_path_candidates(stored)
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
    {
        Some(p) => LocalPath::Found(p),
        None => LocalPath::Missing,
    }
}

/// Variante `Option` de [`resolve_local_path`], pour les appelants qui n'ont
/// rien de différent à faire d'un fichier absent (la lecture, par exemple :
/// elle remonte l'erreur à l'utilisateur sur-le-champ).
pub fn resolve_existing_local_path(stored: &str) -> Option<String> {
    resolve_local_path(stored).found()
}

/// Délai avant de re-tenter une piste dont aucune graphie ne répondait.
///
/// Assez long pour ne pas marteler un partage mort, assez court pour qu'un
/// disque rebranché le matin soit repris dans la journée sans rien demander à
/// personne.
pub const PATH_RETRY_AFTER_SECS: i64 = 6 * 3600;

/// Largeur du tampon décimal d'un report. Douze chiffres tiennent l'époque
/// Unix jusqu'à l'an 33 658.
const DEFERRAL_STAMP_WIDTH: usize = 12;

/// Estampille de report, en décimal **rempli de zéros à gauche**.
///
/// Le rembourrage n'est pas cosmétique : il rend l'ordre lexicographique égal
/// à l'ordre numérique, ce qui permet aux requêtes de candidats de comparer
/// l'estampille **en TEXTE** (`m.value > ?`). Un `CAST(m.value AS INTEGER)`
/// aurait marché sur SQLite et fait exploser PostgreSQL le jour où une valeur
/// non numérique traîne dans `track_metadata.value` — la colonne est partagée
/// par toutes les clés.
pub fn deferral_stamp(epoch_secs: i64) -> String {
    format!(
        "{:0width$}",
        epoch_secs.max(0),
        width = DEFERRAL_STAMP_WIDTH
    )
}

/// Estampille en deçà de laquelle un report est périmé : tout report
/// **strictement plus récent** que le seuil tient encore la piste à l'écart.
pub fn deferral_threshold(now_epoch_secs: i64) -> String {
    deferral_stamp(now_epoch_secs - PATH_RETRY_AFTER_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Couples réels NFC/NFD. Écrits en échappements explicites : un fichier
    /// source peut être re-normalisé par un éditeur ou un outil de `git`, et
    /// le test se mettrait alors à comparer deux fois la même chaîne sans le
    /// dire (#1865 se cache exactement dans ce genre d'égalité muette).
    const COUPLES: &[(&str, &str)] = &[
        // Björk — o + tréma
        ("Bj\u{00f6}rk", "Bjo\u{0308}rk"),
        // Étienne — E accent aigu, en tête de chaîne
        ("\u{00c9}tienne", "E\u{0301}tienne"),
        // Núñez — deux marques combinantes dans le même mot
        ("N\u{00fa}\u{00f1}ez", "Nu\u{0301}n\u{0303}ez"),
        // Décollage — le cas mesuré sur .18
        ("D\u{00e9}collage", "De\u{0301}collage"),
    ];

    #[test]
    fn les_couples_du_test_sont_bien_distincts_octet_a_octet() {
        // Contre-épreuve du test lui-même : si l'outillage avait recomposé les
        // littéraux, tous les tests de ce module passeraient en ne prouvant
        // rien.
        for (nfc, nfd) in COUPLES {
            assert_ne!(
                nfc, nfd,
                "le couple NFC/NFD doit differer octet a octet: {nfc:?}"
            );
            assert_eq!(nfc.nfc().collect::<String>(), *nfc, "forme gauche = NFC");
            assert_eq!(nfd.nfd().collect::<String>(), *nfd, "forme droite = NFD");
        }
    }

    #[test]
    fn un_chemin_ascii_ne_produit_quun_candidat() {
        assert_eq!(
            local_path_candidates("/music/Gramophone/01.flac"),
            vec!["/music/Gramophone/01.flac".to_string()]
        );
    }

    #[test]
    fn chaque_forme_propose_lautre_comme_candidat() {
        for (nfc, nfd) in COUPLES {
            let depuis_nfc = local_path_candidates(&format!("/music/{nfc}/01.flac"));
            assert_eq!(depuis_nfc[0], format!("/music/{nfc}/01.flac"));
            assert!(
                depuis_nfc.contains(&format!("/music/{nfd}/01.flac")),
                "un chemin NFC doit proposer sa forme NFD: {nfc:?}"
            );

            // Et l'inverse : une base qui tiendrait du NFD doit retrouver un
            // disque en NFC.
            let depuis_nfd = local_path_candidates(&format!("/music/{nfd}/01.flac"));
            assert_eq!(depuis_nfd[0], format!("/music/{nfd}/01.flac"));
            assert!(
                depuis_nfd.contains(&format!("/music/{nfc}/01.flac")),
                "un chemin NFD doit proposer sa forme NFC: {nfd:?}"
            );
        }
    }

    #[test]
    fn les_candidats_sont_sans_doublon_et_le_stocke_dabord() {
        for (nfc, _) in COUPLES {
            let c = local_path_candidates(&format!("/music/{nfc}/01.flac"));
            let mut trie = c.clone();
            trie.sort();
            trie.dedup();
            assert_eq!(trie.len(), c.len(), "candidats en double: {c:?}");
            assert_eq!(c[0], format!("/music/{nfc}/01.flac"));
        }
    }

    /// Le cœur du piège : ce qu'on rend est la graphie du DISQUE, pas une
    /// normalisation. On écrit le fichier sous UNE forme, on interroge avec
    /// l'AUTRE, et on exige que la chaîne rendue soit octet pour octet celle
    /// qui est sur le disque.
    ///
    /// (Sur macOS APFS les deux graphies désignent le même fichier : l'égalité
    /// avec la graphie écrite reste vraie, puisqu'on rend le premier candidat
    /// qui existe et que le stocké est essayé en premier.)
    #[test]
    fn la_graphie_rendue_est_celle_du_disque_jamais_une_normalisation() {
        let tmp = tempfile::TempDir::new().unwrap();
        for (nfc, nfd) in COUPLES {
            for (ecrit, cherche) in [(nfc, nfd), (nfd, nfc)] {
                let sur_disque = tmp.path().join(format!("{ecrit}.flac"));
                std::fs::write(&sur_disque, b"x").unwrap();
                let vrai = sur_disque.to_string_lossy().to_string();

                let demande = tmp.path().join(format!("{cherche}.flac"));
                let demande = demande.to_string_lossy().to_string();

                match resolve_local_path(&demande) {
                    LocalPath::Found(trouve) => {
                        assert!(
                            std::path::Path::new(&trouve).exists(),
                            "la graphie rendue doit exister telle quelle: {trouve:?}"
                        );
                        // Ou bien le système accepte les deux graphies (APFS) et
                        // c'est le stocké qui sort ; ou bien il est sensible
                        // (ext4, la plupart des montages) et c'est la graphie
                        // reelle du disque. Jamais autre chose.
                        assert!(
                            trouve == demande || trouve == vrai,
                            "graphie inattendue {trouve:?} (demande {demande:?}, disque {vrai:?})"
                        );
                    }
                    LocalPath::Missing => panic!(
                        "le fichier existe sous {ecrit:?}, la recherche sous {cherche:?} doit le trouver"
                    ),
                }
                std::fs::remove_file(&sur_disque).unwrap();
            }
        }
    }

    #[test]
    fn un_fichier_reellement_absent_rend_missing_et_pas_un_chemin_invente() {
        let tmp = tempfile::TempDir::new().unwrap();
        let absent = tmp
            .path()
            .join("Bj\u{00f6}rk - Jo\u{0301}ga.flac")
            .to_string_lossy()
            .to_string();
        let r = resolve_local_path(&absent);
        assert!(r.is_missing(), "aucune graphie ne doit etre inventee");
        assert_eq!(resolve_existing_local_path(&absent), None);
    }

    #[test]
    fn lestampille_de_report_se_compare_en_texte_comme_en_nombre() {
        // C'est la propriété dont dépend le prédicat SQL portable.
        let mut secondes = vec![
            0i64,
            1,
            9,
            10,
            99,
            1_000_000_000,
            1_756_000_000,
            9_999_999_999,
        ];
        secondes.sort();
        let estampilles: Vec<String> = secondes.iter().map(|s| deferral_stamp(*s)).collect();
        let mut triees = estampilles.clone();
        triees.sort();
        assert_eq!(
            estampilles, triees,
            "l'ordre lexicographique doit suivre l'ordre numerique"
        );
        for e in &estampilles {
            assert_eq!(e.len(), DEFERRAL_STAMP_WIDTH);
        }
    }

    #[test]
    fn le_seuil_de_report_laisse_repasser_apres_la_fenetre() {
        let maintenant = 1_756_000_000i64;
        let seuil = deferral_threshold(maintenant);
        // Un report tout frais est STRICTEMENT au-dessus du seuil : la piste
        // reste ecartee.
        assert!(deferral_stamp(maintenant) > seuil);
        // Un report plus vieux que la fenetre repasse candidat.
        assert!(deferral_stamp(maintenant - PATH_RETRY_AFTER_SECS - 1) < seuil);
    }
}
