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
//! # Trois graphies ne suffisent pas : la normalisation est PAR COMPOSANT
//!
//! Normaliser le chemin **entier** suppose que le disque tient tout le chemin
//! dans une seule forme. C'est faux — et le repli global de #1865 laisse donc
//! un reliquat. Mesure sur `.18` le 29/08/2026, sur les **12** pistes qui
//! portent le témoin de report `audio_embed_path_unresolved` (celui que #1865
//! pose au lieu de marquer « analysée ») : **les 12 fichiers sont bel et bien
//! sur le disque**, **aucun** n'est retrouvé par l'une des trois graphies
//! globales, et **3** portent un nom qui n'est ni NFC ni NFD. Reportées à
//! chaque passe, elles ne seraient jamais analysées. La normalisation change
//! d'un composant à l'autre, voire à l'intérieur d'un composant :
//!
//! ```text
//! stocké : …/Adrian Quesada/Boleros Psicodélicos/03 - Ídolo.flac
//! disque :   répertoire en NFC          fichier en NFD
//!   → NFD global décompose AUSSI le répertoire → ENOENT sur le répertoire
//!   → NFC global recompose AUSSI le fichier    → ENOENT sur le fichier
//!
//! stocké : …/Aşk/09 - Güzelliğin On Para Etmez.flac
//! disque : ü précomposé (U+00FC) MAIS ğ décomposé (g + U+0306)
//!   → le nom sur le disque n'est NI NFC NI NFD : aucune normalisation
//!     globale de la chaîne stockée ne peut le produire.
//! ```
//!
//! D'où [`resolve_local_path`] et son **dernier recours** : descendre le
//! chemin composant par composant et, quand un composant manque, lire le
//! répertoire parent et comparer les noms **repliés en NFC** — la comparaison
//! est normalisée, la chaîne rendue reste celle du disque. Ce recours ne
//! s'engage qu'après l'échec des trois candidats bon marché ET seulement sur
//! un chemin non-ASCII : un chemin ASCII n'a qu'une graphie, un `read_dir` n'y
//! trouverait jamais rien de plus qu'un `exists()` (#1837).
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
    if let Some(p) = local_path_candidates(stored)
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
    {
        return LocalPath::Found(p);
    }
    // Dernier recours : les trois graphies globales ne couvrent pas une
    // normalisation qui change d'un composant à l'autre (#1837).
    if merite_un_parcours(stored) {
        if let Some(p) = resolve_par_composant(stored) {
            return LocalPath::Found(p);
        }
    }
    LocalPath::Missing
}

/// Le parcours du disque vaut-il d'être payé pour ce chemin ?
///
/// Fonction à part, et pure, pour que le garde-fou soit **vérifiable** : sans
/// elle on ne pourrait tester que le résultat (`Missing` dans les deux cas) et
/// jamais le coût, qui est tout l'enjeu. Un `read_dir` par composant, sur
/// chaque fichier d'un partage démonté, à chaque passe, se paierait cher pour
/// rien.
///
/// Deux refus :
/// - **chemin ASCII** — une seule graphie Unicode possible, `exists()` a déjà
///   répondu, un `read_dir` ne trouverait rien de plus ;
/// - **chemin relatif** — la descente part de la racine, elle n'a pas de point
///   de départ ici (et les passes de fond ne manipulent que de l'absolu).
fn merite_un_parcours(stored: &str) -> bool {
    !stored.is_ascii() && stored.starts_with('/')
}

/// Nombre d'entrées lues au plus dans un répertoire pendant la descente.
///
/// Un plafond, pas une heuristique : sans lui, un répertoire de cent mille
/// fichiers se paierait entièrement à chaque piste introuvable. Au-delà, on
/// abandonne la descente plutôt que de rendre un résultat tiré d'une lecture
/// tronquée — [`LocalPath::Missing`] fait différer la piste, il ne la fige pas.
const MAX_ENTREES_PARCOURUES: usize = 50_000;

/// Descente composant par composant, comparaison repliée en NFC.
///
/// Rend l'orthographe **du disque**, jamais une normalisation fabriquée : à
/// chaque niveau, la chaîne retenue est celle que `read_dir` a rendue.
///
/// Rend `None` dès qu'un niveau est ambigu — deux entrées distinctes octet à
/// octet peuvent très bien se replier sur le même NFC (un disque peut porter
/// `Ídolo` en NFC *et* en NFD côte à côte). Deviner laquelle des deux porte le
/// bon contenu serait pire que de différer.
fn resolve_par_composant(stored: &str) -> Option<String> {
    // Chemins absolus seulement : la descente part d'une racine connue, et les
    // passes de fond ne manipulent que des chemins absolus.
    let reste = stored.strip_prefix('/')?;
    let mut courant = String::from("/");
    for composant in reste.split('/') {
        if composant.is_empty() {
            continue;
        }
        let tel_quel = joindre(&courant, composant);
        if std::path::Path::new(&tel_quel).exists() {
            courant = tel_quel;
            continue;
        }
        courant = joindre(&courant, &entree_equivalente(&courant, composant)?);
    }
    Some(courant)
}

/// Concaténation sans doubler le séparateur quand le préfixe est la racine.
fn joindre(prefixe: &str, composant: &str) -> String {
    if prefixe.ends_with('/') {
        format!("{prefixe}{composant}")
    } else {
        format!("{prefixe}/{composant}")
    }
}

/// L'unique entrée de `repertoire` dont le nom se replie sur le même NFC que
/// `cherche`. `None` s'il n'y en a aucune, plusieurs, ou si le répertoire
/// dépasse [`MAX_ENTREES_PARCOURUES`].
fn entree_equivalente(repertoire: &str, cherche: &str) -> Option<String> {
    entree_equivalente_plafonnee(repertoire, cherche, MAX_ENTREES_PARCOURUES)
}

/// [`entree_equivalente`] avec le plafond en paramètre.
///
/// Le plafond est un paramètre — et non la constante lue sur place — pour que
/// l'abandon soit **vérifiable sans fabriquer cinquante mille fichiers**. Sans
/// cela on ne pourrait tester que le cas nominal, et le seul comportement qui
/// compte ici — abandonner plutôt que répondre sur une lecture tronquée —
/// resterait sur parole.
fn entree_equivalente_plafonnee(repertoire: &str, cherche: &str, plafond: usize) -> Option<String> {
    let cible = cherche.nfc().collect::<String>();
    let mut trouve: Option<String> = None;
    let mut lues = 0usize;
    for entree in std::fs::read_dir(repertoire).ok()? {
        lues += 1;
        if lues > plafond {
            return None;
        }
        let Ok(entree) = entree else { continue };
        let nom = entree.file_name();
        let Some(nom) = nom.to_str() else { continue };
        if nom.nfc().collect::<String>() != cible {
            continue;
        }
        if trouve.is_some() {
            // Ambiguïté : deux graphies coexistent. On préfère différer.
            return None;
        }
        trouve = Some(nom.to_string());
    }
    trouve
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

    // ------------------------------------------------------------------
    // #1837 — normalisation MIXTE : les trois graphies globales ne suffisent
    // pas. Les deux cas ci-dessous sont copiés des 12 pistes de .18 qui, le
    // 29/08/2026, existaient bel et bien sur le disque et qu'AUCUN des trois
    // candidats ne retrouvait.
    // ------------------------------------------------------------------

    /// Lit le fichier rendu et vérifie qu'il porte bien le contenu attendu.
    ///
    /// Passer par le CONTENU et pas par l'égalité de chaînes est délibéré :
    /// sur un système qui normalise de lui-même (APFS) les deux graphies
    /// désignent le même inœud, sur ext4 non. Le contenu tranche dans les deux
    /// cas, et c'est bien le fichier — pas l'orthographe — qui nous intéresse.
    fn doit_ouvrir(stored: &str, attendu: &[u8]) {
        match resolve_local_path(stored) {
            LocalPath::Found(trouve) => {
                let lu = std::fs::read(&trouve)
                    .unwrap_or_else(|e| panic!("graphie rendue inouvrable {trouve:?}: {e}"));
                assert_eq!(lu, attendu, "graphie rendue {trouve:?} : mauvais fichier");
            }
            LocalPath::Missing => {
                panic!("le fichier existe sur le disque, {stored:?} doit le retrouver")
            }
        }
    }

    #[test]
    fn un_repertoire_nfc_et_un_fichier_nfd_sont_retrouves() {
        // Le cas « Boleros Psicodélicos / Ídolo » de .18 : le répertoire est
        // resté en NFC, le fichier est en NFD. La normalisation GLOBALE échoue
        // des deux côtés — NFD casse le répertoire, NFC casse le fichier.
        let tmp = tempfile::TempDir::new().unwrap();
        let dossier_nfc = "Boleros Psicod\u{00e9}licos";
        let fichier_nfd = "03 - I\u{0301}dolo.flac";
        let fichier_nfc = "03 - \u{00cd}dolo.flac";

        let dossier = tmp.path().join(dossier_nfc);
        std::fs::create_dir(&dossier).unwrap();
        std::fs::write(dossier.join(fichier_nfd), b"idolo").unwrap();

        let stored = dossier.join(fichier_nfc).to_string_lossy().to_string();

        // Contre-épreuve du test : aucun des trois candidats ne doit exister,
        // sans quoi le test passerait sans jamais emprunter le nouveau chemin.
        assert!(
            !local_path_candidates(&stored)
                .iter()
                .any(|c| std::path::Path::new(c).exists()),
            "les trois graphies globales ne doivent RIEN trouver ici"
        );

        doit_ouvrir(&stored, b"idolo");
    }

    #[test]
    fn un_composant_ni_nfc_ni_nfd_est_retrouve() {
        // Le cas « Güzelliğin » de .18 : dans le MÊME nom, le tréma est
        // précomposé (U+00FC) et la brève est décomposée (g + U+0306). Ce nom
        // n'est ni NFC ni NFD : aucune normalisation de la chaîne stockée ne
        // peut le produire, seule une comparaison repliée le reconnaît.
        let tmp = tempfile::TempDir::new().unwrap();
        let sur_disque = "09 - G\u{00fc}zelli\u{0067}\u{0306}in.flac";
        let stocke = "09 - G\u{00fc}zelli\u{011f}in.flac";

        // Le disque porte bien une forme bâtarde, sinon le test ne prouve rien.
        assert_ne!(sur_disque.nfc().collect::<String>(), sur_disque);
        assert_ne!(sur_disque.nfd().collect::<String>(), sur_disque);

        std::fs::write(tmp.path().join(sur_disque), b"guzel").unwrap();
        let stored = tmp.path().join(stocke).to_string_lossy().to_string();

        assert!(
            !local_path_candidates(&stored)
                .iter()
                .any(|c| std::path::Path::new(c).exists()),
            "les trois graphies globales ne doivent RIEN trouver ici"
        );

        doit_ouvrir(&stored, b"guzel");
    }

    /// Les huit graphies Unicode de `Núñéz` : chaque marque peut être
    /// précomposée ou décomposée, indépendamment des autres. Elles se replient
    /// toutes sur le même NFC et sont toutes distinctes octet à octet.
    fn graphies_de_nunez() -> Vec<String> {
        let mut out = Vec::new();
        for u in ["\u{00fa}", "u\u{0301}"] {
            for n in ["\u{00f1}", "n\u{0303}"] {
                for e in ["\u{00e9}", "e\u{0301}"] {
                    out.push(format!("N{u}{n}{e}z.flac"));
                }
            }
        }
        out
    }

    #[test]
    fn les_huit_graphies_sont_distinctes_et_equivalentes() {
        // Contre-épreuve du matériel de test : si l'outillage avait recomposé
        // les littéraux, le test d'ambiguïté ci-dessous ne prouverait rien.
        let g = graphies_de_nunez();
        assert_eq!(g.len(), 8);
        let uniques: std::collections::BTreeSet<_> = g.iter().collect();
        assert_eq!(uniques.len(), 8, "les huit graphies doivent differer");
        for x in &g {
            assert_eq!(
                x.nfc().collect::<String>(),
                g[0],
                "toutes doivent se replier sur le meme NFC"
            );
        }
    }

    #[test]
    fn deux_graphies_equivalentes_cote_a_cote_rendent_missing_plutot_quun_choix() {
        // Un disque peut porter DEUX orthographes du même nom. Rendre l'une au
        // hasard, c'est ouvrir peut-être le mauvais fichier. `Missing` fait
        // différer la piste — elle repassera — au lieu de la figer sur une
        // supposition.
        let tmp = tempfile::TempDir::new().unwrap();
        let g = graphies_de_nunez();
        let stocke = &g[3];
        let nfc = stocke.nfc().collect::<String>();
        let nfd = stocke.nfd().collect::<String>();

        // Les deux entrées écrites doivent être invisibles aux TROIS candidats
        // bon marché, sinon le parcours ne serait jamais engagé et l'ambiguïté
        // jamais atteinte.
        let paire: Vec<&String> = g
            .iter()
            .filter(|x| **x != *stocke && **x != nfc && **x != nfd)
            .take(2)
            .collect();
        assert_eq!(paire.len(), 2);
        for (i, nom) in paire.iter().enumerate() {
            std::fs::write(tmp.path().join(nom), format!("contenu{i}")).unwrap();
        }

        let stored = tmp.path().join(stocke).to_string_lossy().to_string();
        assert!(
            !local_path_candidates(&stored)
                .iter()
                .any(|c| std::path::Path::new(c).exists()),
            "les trois graphies globales ne doivent RIEN trouver ici"
        );
        // Le parcours, lui, trouve DEUX candidates : il doit refuser de choisir.
        assert_eq!(
            resolve_local_path(&stored),
            LocalPath::Missing,
            "deux graphies equivalentes : aucune ne doit etre choisie au hasard"
        );

        // Et la preuve que le refus vient bien de l'ambiguïté, pas d'un
        // parcours qui ne trouverait jamais rien : une seule entrée suffit.
        std::fs::remove_file(tmp.path().join(paire[1])).unwrap();
        match resolve_local_path(&stored) {
            LocalPath::Found(p) => assert_eq!(std::fs::read(p).unwrap(), b"contenu0"),
            LocalPath::Missing => panic!("une seule graphie reste : elle doit etre rendue"),
        }
    }

    #[test]
    fn le_parcours_nest_engage_que_sur_un_chemin_absolu_et_accentue() {
        // Le garde-fou de coût, vérifié sur la décision elle-même et pas sur
        // son résultat : `Missing` sortirait de toute façon, ce qu'on veut
        // prouver c'est qu'aucun `read_dir` n'est payé.
        assert!(merite_un_parcours("/music/Bj\u{00f6}rk/01.flac"));
        assert!(
            !merite_un_parcours("/music/Gramophone/01.flac"),
            "ASCII : une seule graphie possible, le disque n'a rien de plus a dire"
        );
        assert!(
            !merite_un_parcours("Bj\u{00f6}rk/01.flac"),
            "relatif : la descente n'a pas de racine d'ou partir"
        );
    }

    #[test]
    fn un_chemin_ascii_absent_reste_missing() {
        // Le garde-fou de coût : un chemin ASCII n'a qu'une seule graphie
        // Unicode, un `read_dir` n'y trouverait jamais ce qu'`exists()` n'a pas
        // trouvé. Il ne doit donc pas être payé — et surtout pas à chaque
        // passe, sur chaque fichier d'un partage démonté.
        let tmp = tempfile::TempDir::new().unwrap();
        let absent = tmp
            .path()
            .join("Gramophone")
            .join("01.flac")
            .to_string_lossy()
            .to_string();
        assert!(absent.is_ascii(), "le cas testé doit rester ASCII");
        assert_eq!(resolve_local_path(&absent), LocalPath::Missing);
    }

    #[test]
    fn un_chemin_relatif_reste_missing() {
        // `resolve_par_composant` descend depuis la racine ; un chemin relatif
        // n'a pas de point de départ et doit sortir sans toucher au disque.
        assert_eq!(
            resolve_local_path("Bj\u{00f6}rk/01.flac"),
            LocalPath::Missing
        );
    }

    #[test]
    fn un_repertoire_plus_grand_que_le_plafond_fait_abandonner() {
        // Le plafond n'est pas décoratif : au-delà, on ABANDONNE la descente
        // au lieu de répondre sur une lecture tronquée. Le vérifier suppose de
        // pouvoir choisir le plafond — d'où le paramètre.
        let tmp = tempfile::TempDir::new().unwrap();
        let racine = tmp.path().to_string_lossy().to_string();
        let sur_disque = "Bjo\u{0308}rk.flac";
        let cherche = "Bj\u{00f6}rk.flac";
        for i in 0..5 {
            std::fs::write(tmp.path().join(format!("bourrage{i}.flac")), b"x").unwrap();
        }
        std::fs::write(tmp.path().join(sur_disque), b"bjork").unwrap();

        // Plafond large : l'entrée équivalente est trouvée.
        assert_eq!(
            entree_equivalente_plafonnee(&racine, cherche, 50),
            Some(sur_disque.to_string())
        );
        // Plafond dépassé : abandon, quelle que soit la place de l'entrée dans
        // l'ordre — non déterministe — rendu par `read_dir`.
        assert_eq!(entree_equivalente_plafonnee(&racine, cherche, 3), None);
    }
}
