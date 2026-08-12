//! Compilations éparpillées sur plusieurs dossiers.
//!
//! Un dossier est un album ([`super::album_folder`]) — sauf quand le même
//! disque a été rangé une fois par artiste. Les téléchargements Qobuz font
//! exactement ça pour les compilations :
//!
//! ```text
//! Qobuz/Corte Real/OUF L'anthologie Souterraine 2015-2017/01 - Opium.flac
//! Qobuz/Alligator/OUF L'anthologie Souterraine 2015-2017/03 - Rafale.flac
//! ```
//!
//! Une seule anthologie, quarante-et-un dossiers, donc quarante-et-une entrées
//! d'album d'une piste chacune (#1440 : 22 familles, 172 fausses entrées sur
//! une bibliothèque de 2 144 albums).
//!
//! Recoller ces dossiers demande de la prudence : « Live » et « Greatest Hits »
//! sont des titres que des dizaines d'artistes portent légitimement. Le seul
//! critère solide est la **conjonction** de trois faits — même nom de dossier,
//! dossiers parents frères, et numéros de piste qui ne se chevauchent pas.

use std::path::Path;

/// Deux dossiers d'album sont-ils les éclats possibles d'un même disque ?
///
/// Vrai quand ils portent le **même nom**, sous des parents **différents**,
/// eux-mêmes enfants d'un **même dossier**. C'est la forme que produit un
/// rangement par artiste :
///
/// ```text
/// racine/Artiste A/Le Disque/   ┐ frères éparpillés
/// racine/Artiste B/Le Disque/   ┘
/// ```
///
/// Faux dès qu'un seul de ces trois faits manque — notamment si les deux
/// dossiers ont le même parent (deux éditions côte à côte, pas une
/// compilation) ou si les noms diffèrent (`2005-Greatest Hits` contre
/// `1992-Greatest Hits`, cas réel de la bibliothèque .18).
pub fn is_scattered_sibling(a: &str, b: &str) -> bool {
    let (pa, pb) = (Path::new(a), Path::new(b));
    if pa == pb {
        return false;
    }
    let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().to_lowercase());
    if name(pa) != name(pb) || name(pa).is_none() {
        return false;
    }
    let (parent_a, parent_b) = match (pa.parent(), pb.parent()) {
        (Some(x), Some(y)) => (x, y),
        _ => return false,
    };
    // Même parent ⇒ deux dossiers voisins du même artiste, pas un éparpillement.
    if parent_a == parent_b {
        return false;
    }
    match (parent_a.parent(), parent_b.parent()) {
        // Le grand-parent doit être un VRAI dossier commun. La racine du
        // système de fichiers n'en est pas un : deux artistes posés à `/`
        // n'ont rien qui les relie, et fusionner là-dessus serait fusionner
        // sur rien. Dans le doute, on refuse.
        (Some(ga), Some(gb)) => ga == gb && !ga.as_os_str().is_empty() && ga.parent().is_some(),
        _ => false,
    }
}

/// Peut-on rattacher une piste portant `track_number` à un album qui occupe
/// déjà les numéros `taken` ?
///
/// C'est le garde-fou qui distingue une compilation éclatée d'un homonyme :
/// deux « Greatest Hits » différents commencent tous deux à la piste 1, alors
/// que les éclats d'une même anthologie se partagent la numérotation.
///
/// Une piste sans numéro ne prouve rien : on refuse, plutôt que de fusionner
/// sur une intuition.
pub fn track_number_is_free(track_number: Option<i32>, taken: &[i32]) -> bool {
    match track_number {
        Some(n) if n > 0 => !taken.contains(&n),
        _ => false,
    }
}

/// Noms de fichier sous lesquels une pochette accompagne un dossier d'album.
/// Ordre = priorité ; le premier trouvé gagne.
const COVER_FILE_NAMES: &[&str] = &[
    "cover.jpg",
    "cover.jpeg",
    "cover.png",
    "folder.jpg",
    "folder.jpeg",
    "folder.png",
    "front.jpg",
    "front.jpeg",
    "front.png",
];

/// Écart maximal, en bits, entre deux empreintes réputées porter la MÊME
/// pochette.
///
/// Mesuré sur les 32 albums que la première version de ce découpage a séparés
/// sur .18 : les paires de dossiers portant visuellement la même image tombent
/// entre **0 et 2** bits d'écart, les pochettes réellement différentes
/// commencent à **6**. Quatre coupe l'intervalle sans frôler ni l'un ni
/// l'autre bord.
pub const COVER_DISTANCE_MAX: u32 = 4;

// Sortir de cet intervalle ferait revenir l'un des deux défauts : sous 3, les
// ré-encodages recoupent et les albums se redécoupent ; à partir de 6, deux
// volumes distincts se confondent. Vérifié à la compilation plutôt qu'en test,
// pour que la borne arrête celui qui touche à la constante, pas celui qui
// lance la suite.
const _: () = assert!(COVER_DISTANCE_MAX > 2 && COVER_DISTANCE_MAX < 6);

/// Empreinte PERCEPTUELLE d'une pochette : ce que l'image montre, pas les
/// octets qui la codent.
///
/// La première version hachait le fichier (SHA-256). Elle a fait la preuve de
/// son insuffisance en production : sur .18, dix-sept albums légitimes ont été
/// coupés en deux parce que le même artwork y était présent deux fois avec un
/// ré-encodage différent. Cas mesuré — la pochette Stockfisch, 750×750 des deux
/// côtés, pixel pour pixel identique à l'œil, 40 562 octets d'un côté et 41 823
/// de l'autre. Deux SHA-256 sans rapport, donc deux disques pour la machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoverFingerprint(u64);

impl CoverFingerprint {
    /// Les deux empreintes désignent-elles la même pochette ?
    ///
    /// Volontairement pas `PartialEq` : une égalité stricte inviterait à s'en
    /// servir comme clé de table de hachage, ce qu'une empreinte à seuil ne
    /// permet pas (la relation n'est pas transitive). Il faut comparer deux à
    /// deux, d'où une méthode qui se voit.
    pub fn matches(&self, other: &Self) -> bool {
        (self.0 ^ other.0).count_ones() <= COVER_DISTANCE_MAX
    }
}

/// Empreinte de la pochette POSÉE DANS le dossier, `None` s'il n'y en a pas
/// ou si l'image est illisible.
///
/// À ne pas confondre avec la jaquette extraite des pistes, ré-encodée fichier
/// par fichier par certains fournisseurs (Qobuz) — celle-là diffère d'une piste
/// à l'autre et ne regroupe rien. Le `cover.jpg` déposé à côté des fichiers,
/// lui, accompagne tous les dossiers d'un même disque.
///
/// C'est ce qui permet de séparer plusieurs VOLUMES portant le même titre :
/// mesuré sur .18, les 41 dossiers « ALLOPOP » se répartissent exactement en
/// quatre pochettes distinctes — quatre volumes (idée de Bertrand, #1444).
///
/// L'empreinte est un *dHash* 8×8 : l'image est ramenée en 9×8 niveaux de gris,
/// puis chaque bit dit si un pixel est plus sombre que son voisin de droite.
/// Comparer des gradients plutôt que des valeurs absolues rend l'empreinte
/// insensible à la résolution, à la qualité JPEG et aux écarts de luminosité —
/// exactement les trois choses qui varient entre deux copies d'une pochette.
///
/// Une image qu'on n'arrive pas à décoder ne donne pas d'empreinte : sans
/// signal, l'appelant ne regroupe ni ne sépare, ce qui est le côté prudent.
pub fn folder_cover_fingerprint(folder: &str) -> Option<CoverFingerprint> {
    for name in COVER_FILE_NAMES {
        let path = Path::new(folder).join(name);
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if bytes.is_empty() {
            continue;
        }
        return cover_fingerprint_from_bytes(&bytes);
    }
    None
}

/// Répartit des éléments en groupes portant la même pochette.
///
/// Rend `(groupes, sans_pochette)` : les indices des éléments dont l'empreinte
/// est absente sortent à part, l'appelant seul sachant si les ignorer ou les
/// laisser en place.
///
/// L'appartenance se décide contre le PREMIER membre de chaque groupe, jamais
/// de proche en proche. [`CoverFingerprint::matches`] tolère un écart, donc la
/// relation n'est pas transitive : `a≈b` et `b≈c` n'entraînent pas `a≈c`, et
/// un rattachement transitif laisserait un groupe glisser d'une pochette à
/// une autre par petits pas. Avec un seuil de quatre bits sur soixante-quatre
/// la dérive resterait théorique, mais le coût de s'en prémunir est nul.
pub fn group_by_cover(empreintes: &[Option<CoverFingerprint>]) -> (Vec<Vec<usize>>, Vec<usize>) {
    let mut groupes: Vec<(CoverFingerprint, Vec<usize>)> = Vec::new();
    let mut sans_pochette = Vec::new();
    for (i, empreinte) in empreintes.iter().enumerate() {
        let Some(empreinte) = empreinte else {
            sans_pochette.push(i);
            continue;
        };
        match groupes.iter_mut().find(|(chef, _)| chef.matches(empreinte)) {
            Some((_, membres)) => membres.push(i),
            None => groupes.push((*empreinte, vec![i])),
        }
    }
    (
        groupes.into_iter().map(|(_, membres)| membres).collect(),
        sans_pochette,
    )
}

/// Le cœur de [`folder_cover_fingerprint`], séparé pour être testable sans
/// toucher au disque.
fn cover_fingerprint_from_bytes(bytes: &[u8]) -> Option<CoverFingerprint> {
    use image::imageops::FilterType;
    let image = image::load_from_memory(bytes).ok()?;
    // 9 colonnes pour 8 comparaisons par ligne, 8 lignes ⇒ 64 bits.
    let petit = image.resize_exact(9, 8, FilterType::Lanczos3).to_luma8();
    let mut bits = 0u64;
    for y in 0..8u32 {
        for x in 0..8u32 {
            let gauche = petit.get_pixel(x, y).0[0];
            let droite = petit.get_pixel(x + 1, y).0[0];
            bits = (bits << 1) | u64::from(gauche < droite);
        }
    }
    Some(CoverFingerprint(bits))
}

/// Une pochette de test : un damier 8×8 de gris déterminés par `motif`,
/// agrandi à `cote` pixels puis encodé en JPEG à la qualité demandée.
///
/// Les trois paramètres reproduisent les trois façons dont deux copies d'une
/// même pochette diffèrent dans une vraie bibliothèque : la taille, la qualité
/// d'encodage, et le dessin lui-même. Partagée avec les tests de migration,
/// qui ont besoin des mêmes images.
#[cfg(test)]
pub(crate) fn pochette_de_test(motif: u32, cote: u32, qualite: u8) -> Vec<u8> {
    let mut img = image::RgbImage::new(cote, cote);
    for (x, y, pixel) in img.enumerate_pixels_mut() {
        let (bx, by) = (x * 8 / cote, y * 8 / cote);
        let v = (((bx * 37 + by * 91 + motif * 53) % 7) * 36) as u8;
        *pixel = image::Rgb([v, v, v]);
    }
    let mut octets = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut octets, qualite)
        .encode_image(&img)
        .unwrap();
    octets
}

#[cfg(test)]
mod tests {
    use super::*;

    // Chemins RÉELS relevés sur la bibliothèque de .18 (#1440).
    const OUF_CORTE_REAL: &str =
        "/mnt/recordings_usb/Qobuz/Corte Real/OUF L'anthologie Souterraine 2015-2017";
    const OUF_ALLIGATOR: &str =
        "/mnt/recordings_usb/Qobuz/Alligator/OUF L'anthologie Souterraine 2015-2017";
    const GREATEST_BENATAR: &str = "/data/music/NEW_FLAC/POP-ROCK/P/Pat Benatar/2005-Greatest Hits";
    const GREATEST_POLICE: &str = "/data/music/NEW_FLAC/POP-ROCK/P/Police/1992-Greatest Hits";

    use super::pochette_de_test as pochette;

    #[test]
    fn a_folder_cover_fingerprints_by_what_the_image_shows() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("Artiste A/ALLOPOP");
        let b = dir.path().join("Artiste B/ALLOPOP");
        let c = dir.path().join("Artiste C/ALLOPOP");
        for d in [&a, &b, &c] {
            std::fs::create_dir_all(d).unwrap();
        }
        // A et B : MÊME pochette, mais ré-encodée — taille et qualité
        // différentes, donc pas un octet en commun. C'est le cas Stockfisch
        // relevé sur .18, celui qui coupait l'album en deux.
        std::fs::write(a.join("cover.jpg"), pochette(1, 144, 92)).unwrap();
        std::fs::write(b.join("cover.jpg"), pochette(1, 96, 60)).unwrap();
        // C : autre volume, autre pochette.
        std::fs::write(c.join("cover.jpg"), pochette(4, 144, 92)).unwrap();

        assert_ne!(
            std::fs::read(a.join("cover.jpg")).unwrap(),
            std::fs::read(b.join("cover.jpg")).unwrap(),
            "les deux fichiers diffèrent bien octet pour octet"
        );

        let fa = folder_cover_fingerprint(a.to_str().unwrap()).expect("pochette A lisible");
        let fb = folder_cover_fingerprint(b.to_str().unwrap()).expect("pochette B lisible");
        let fc = folder_cover_fingerprint(c.to_str().unwrap()).expect("pochette C lisible");
        assert!(fa.matches(&fb), "un même volume ré-encodé reste un volume");
        assert!(!fa.matches(&fc), "deux volumes se séparent");
    }

    #[test]
    fn a_folder_without_a_cover_has_no_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        assert!(folder_cover_fingerprint(dir.path().to_str().unwrap()).is_none());
    }

    #[test]
    fn an_unreadable_cover_gives_no_fingerprint() {
        // Fichier présent mais indécodable : pas d'empreinte, donc l'appelant
        // ne regroupe ni ne sépare. Le silence vaut mieux qu'une devinette.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cover.jpg"), b"CECI N'EST PAS UNE IMAGE").unwrap();
        assert!(folder_cover_fingerprint(dir.path().to_str().unwrap()).is_none());
    }

    #[test]
    fn the_real_scattered_anthology_is_recognised() {
        // Le cas signalé : une anthologie rangée par artiste de piste.
        assert!(is_scattered_sibling(OUF_CORTE_REAL, OUF_ALLIGATOR));
        assert!(is_scattered_sibling(OUF_ALLIGATOR, OUF_CORTE_REAL));
    }

    #[test]
    fn two_real_greatest_hits_are_never_merged() {
        // Pat Benatar (20 pistes, n° 1..20) et Police (16 pistes, n° 1..16) :
        // même titre d'album, artistes différents, et DEUX raisons de refuser —
        // les noms de dossier portent l'année, et les numéros se chevauchent.
        assert!(!is_scattered_sibling(GREATEST_BENATAR, GREATEST_POLICE));
        assert!(!track_number_is_free(Some(1), &[1, 2, 3]));
    }

    #[test]
    fn same_named_folders_under_one_parent_would_still_be_refused_on_numbers() {
        // Si les deux dossiers portaient le MÊME nom (« Greatest Hits » sans
        // année), la parenté ne suffirait pas : c'est le chevauchement des
        // numéros qui tranche.
        let a = "/data/music/P/Pat Benatar/Greatest Hits";
        let b = "/data/music/P/Police/Greatest Hits";
        assert!(is_scattered_sibling(a, b), "la forme est bien fraternelle");
        assert!(
            !track_number_is_free(Some(1), &[1, 2, 3, 4]),
            "mais la piste 1 est déjà prise : refus"
        );
    }

    #[test]
    fn tracks_of_the_real_anthology_do_not_collide() {
        // Corte Real occupe la piste 1, Alligator la 3 : elles se complètent.
        assert!(track_number_is_free(Some(3), &[1]));
        assert!(track_number_is_free(Some(1), &[3, 7, 12]));
    }

    #[test]
    fn a_track_without_a_number_never_merges() {
        assert!(!track_number_is_free(None, &[]));
        assert!(!track_number_is_free(Some(0), &[]));
    }

    #[test]
    fn siblings_under_the_same_parent_are_not_scattered() {
        // Deux éditions côte à côte chez le même artiste : même parent.
        let a = "/music/Pink Floyd/Animals";
        let b = "/music/Pink Floyd/Animals";
        assert!(!is_scattered_sibling(a, b), "identiques");
        assert!(!is_scattered_sibling(
            "/music/Pink Floyd/CD1",
            "/music/Pink Floyd/CD2"
        ));
    }

    #[test]
    fn different_roots_are_not_scattered() {
        // Même nom, même profondeur, mais racines distinctes : deux
        // bibliothèques, deux disques.
        assert!(!is_scattered_sibling(
            "/mnt/a/Artiste/Le Disque",
            "/mnt/b/Artiste/Le Disque"
        ));
    }

    #[test]
    fn a_folder_at_the_root_has_no_grandparent() {
        assert!(!is_scattered_sibling("/A/Disque", "/B/Disque"));
    }

    #[test]
    fn folder_name_comparison_ignores_case() {
        assert!(is_scattered_sibling(
            "/r/Artiste A/Le Disque",
            "/r/Artiste B/LE DISQUE"
        ));
    }
}
