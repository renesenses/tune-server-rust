//! Des feuilles CUE d'un dossier aux albums qu'elles décrivent.
//!
//! [`super::cue`] lit le TEXTE d'une feuille et s'arrête là, par construction.
//! Ce module-ci ajoute la seule chose qui manque avant de pouvoir peupler la
//! bibliothèque : **confronter la feuille au disque**. C'est là que se jouent
//! les trois pièges rapportés par les testeurs du fil forum 1495, et aucun
//! d'eux n'est visible depuis le texte seul.
//!
//! ## Les règles, et d'où elles viennent
//!
//! 1. **Une feuille dont le `FILE` n'existe pas est écartée.** Gros Bidon
//!    (Didier), fil 1495 : « il peut y avoir des fichiers CUE isolés (sans FLAC
//!    à côté) et dans ce cas il ne faut pas les exploiter. J'ai eu ce problème
//!    avec foobar2000 qui créait un album à partir du CUE sans qu'il y ait de
//!    fichier audio. » Un défaut déjà commis par un concurrent : brancher
//!    naïvement l'analyseur peuplerait la bibliothèque d'albums injouables.
//! 2. **Une feuille dont l'image n'est pas décodable est écartée.** Le cas de
//!    Rhorn (#1763) : un `.cue` posé sur un `.mpc` ne devient pas jouable parce
//!    qu'on sait le découper. Découper ce qu'on ne sait pas lire produirait
//!    exactement les mêmes albums fantômes que la règle 1 interdit.
//! 3. **Plusieurs feuilles d'un même dossier peuvent former UN album.** Didier
//!    encore : « quand on numérise un vinyle on le fait par face et on obtient
//!    donc un FLAC par face auquel on associe un fichier CUE par fichier
//!    FLAC ». Les deux feuilles portent le même `TITLE` et se partagent la
//!    numérotation (01-05 puis 06-10) ; chacune redémarre ses temps à zéro,
//!    donc les offsets ne se fusionnent PAS — c'est le fichier image qui change
//!    d'une piste à l'autre.
//!
//! ## Ce que ce module ne fait pas
//!
//! Il ne touche ni à la base, ni au décodeur : il rend un plan, pas des pistes.
//! Il n'ouvre pas non plus les fichiers audio — la décodabilité se juge sur
//! l'extension, comme le fait déjà le parcours de bibliothèque, parce qu'un
//! scan ne doit jamais ajouter une lecture bloquante par fichier sur un NAS.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::cue::{CueSheet, parse_cue_bytes};

/// Une piste virtuelle : un intervalle nommé à l'intérieur d'un fichier réel.
#[derive(Debug, Clone, PartialEq)]
pub struct PisteCue {
    /// Le fichier image sur disque, résolu — pas le nom écrit dans la feuille.
    pub media: PathBuf,
    pub numero: u32,
    pub titre: Option<String>,
    pub interprete: Option<String>,
    /// Début dans `media`, en millisecondes.
    pub debut_ms: u64,
    /// Fin dans `media`, ou `None` si la piste court jusqu'au bout du fichier.
    pub fin_ms: Option<u64>,
}

/// Un album reconstitué à partir d'une ou plusieurs feuilles du même dossier.
#[derive(Debug, Clone, PartialEq)]
pub struct AlbumCue {
    /// Les feuilles fusionnées, triées par nom — une seule dans le cas usuel.
    pub feuilles: Vec<PathBuf>,
    pub titre: Option<String>,
    pub interprete: Option<String>,
    pub genre: Option<String>,
    pub annee: Option<String>,
    pub pistes: Vec<PisteCue>,
}

/// Pourquoi une feuille n'a pas produit d'album.
///
/// Chaque variante porte une clé stable : le rapport de fin de scan compte par
/// clé, et un motif qui changerait de libellé au fil des versions rendrait ces
/// compteurs incomparables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MotifEcart {
    /// Le fichier `.cue` n'a pas pu être lu (droits, disparition en cours de
    /// scan, périphérique retiré).
    Illisible(String),
    /// Feuille lisible mais sans aucune piste : rien à découper.
    SansPiste,
    /// Feuille sans aucune ligne `FILE` : elle ne dit pas ce qu'elle découpe.
    SansFichier,
    /// Le `FILE` de la feuille ne désigne aucun fichier du dossier.
    ImageIntrouvable { fichier: String },
    /// L'image existe mais aucun décodeur livré ne sait la lire.
    ImageNonDecodable { fichier: String, extension: String },
}

impl MotifEcart {
    /// Clé stable pour les compteurs du rapport de scan.
    pub fn cle(&self) -> &'static str {
        match self {
            MotifEcart::Illisible(_) => "cue-illisible",
            MotifEcart::SansPiste => "cue-sans-piste",
            MotifEcart::SansFichier => "cue-sans-fichier",
            MotifEcart::ImageIntrouvable { .. } => "cue-image-introuvable",
            MotifEcart::ImageNonDecodable { .. } => "cue-image-non-decodable",
        }
    }

    /// Motif destiné à l'utilisateur, pas seulement au journal.
    pub fn motif(&self) -> &'static str {
        match self {
            MotifEcart::Illisible(_) => "feuille CUE illisible",
            MotifEcart::SansPiste => "feuille CUE sans aucune piste",
            MotifEcart::SansFichier => "feuille CUE ne désignant aucun fichier audio",
            MotifEcart::ImageIntrouvable { .. } => {
                "feuille CUE sans son fichier audio : rien à découper"
            }
            MotifEcart::ImageNonDecodable { .. } => {
                "feuille CUE posée sur un format qu'aucun décodeur livré ne lit"
            }
        }
    }
}

/// Ce qu'un dossier a donné.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanCue {
    pub albums: Vec<AlbumCue>,
    /// Feuilles écartées, avec leur raison. Jamais une erreur : une feuille
    /// bancale ne doit pas emporter le dossier, encore moins le scan.
    pub ecartees: Vec<(PathBuf, MotifEcart)>,
}

impl PlanCue {
    pub fn est_vide(&self) -> bool {
        self.albums.is_empty() && self.ecartees.is_empty()
    }

    /// Compte les feuilles écartées par clé de motif, pour le rapport de scan.
    pub fn ecarts_par_cle(&self) -> HashMap<&'static str, usize> {
        let mut par_cle = HashMap::new();
        for (_, motif) in &self.ecartees {
            *par_cle.entry(motif.cle()).or_insert(0) += 1;
        }
        par_cle
    }
}

/// Le nom de fichier nu d'une référence `FILE`.
///
/// Les feuilles écrites sous Windows contiennent parfois un chemin
/// (`..\\autre\\album.flac`, `D:\\rips\\album.flac`), et `\` n'est pas un
/// séparateur sur Unix : `Path::file_name` rendrait la chaîne entière. On coupe
/// donc sur les DEUX séparateurs.
///
/// Ne garder que la dernière composante n'est pas qu'une commodité : c'est
/// aussi ce qui interdit à une feuille de faire sortir le scan de son dossier.
/// Une bibliothèque est du contenu apporté par l'utilisateur, et un `.cue`
/// contenant `../../../etc/passwd` ne doit désigner que `passwd` dans le
/// dossier de la feuille — donc rien.
fn nom_nu(reference: &str) -> Option<&str> {
    let nom = reference
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(reference)
        .trim();
    (!nom.is_empty() && nom != "." && nom != "..").then_some(nom)
}

/// Index des fichiers d'un dossier, minuscules → nom réel.
///
/// La casse compte : EAC écrit le nom tel que Windows le lui donne, et la même
/// arborescence recopiée sur un NAS Linux devient sensible à la casse. Un
/// `FILE "Album.Flac"` en face d'un `album.flac` sur disque ferait alors passer
/// pour orpheline une feuille parfaitement valide — et un album de plus
/// disparaîtrait.
fn index_du_dossier(dossier: &Path) -> HashMap<String, PathBuf> {
    let mut index = HashMap::new();
    let Ok(entrees) = std::fs::read_dir(dossier) else {
        return index;
    };
    for entree in entrees.flatten() {
        if !entree.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        if let Some(nom) = entree.file_name().to_str() {
            index.insert(nom.to_lowercase(), entree.path());
        }
    }
    index
}

/// Résout une référence `FILE` dans le dossier de la feuille.
fn resoudre_image(index: &HashMap<String, PathBuf>, reference: &str) -> Option<PathBuf> {
    let nom = nom_nu(reference)?;
    index.get(&nom.to_lowercase()).cloned()
}

/// Clé de regroupement d'un album : le titre, réduit à ce qui se compare.
fn cle_album(titre: &str) -> String {
    titre
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Une feuille lue, résolue sur disque, prête à être regroupée.
struct FeuilleResolue {
    chemin: PathBuf,
    feuille: CueSheet,
    /// Référence `FILE` → fichier réel. Toutes résolues et décodables.
    images: HashMap<String, PathBuf>,
}

impl FeuilleResolue {
    /// L'intervalle de numéros de pistes couvert par cette feuille.
    fn plage(&self) -> Option<(u32, u32)> {
        let mut nums = self.feuille.tracks.iter().map(|t| t.number);
        let premier = nums.next()?;
        Some(nums.fold((premier, premier), |(lo, hi), n| (lo.min(n), hi.max(n))))
    }
}

/// Lit toutes les feuilles CUE d'un dossier et rend les albums qu'elles
/// décrivent réellement, plus la liste motivée de celles qui n'en décrivent
/// aucun.
///
/// **Ne rend jamais d'erreur.** Un dossier illisible rend un plan vide, une
/// feuille bancale est écartée avec son motif, et les autres feuilles du même
/// dossier sont traitées normalement : une ligne parasite dans un `.cue` ne
/// doit pas faire disparaître le dossier, ni a fortiori interrompre le scan.
pub fn planifier_dossier(dossier: &Path) -> PlanCue {
    let index = index_du_dossier(dossier);

    // Tri par nom : les feuilles d'un vinyle s'appellent « … Side A.cue » et
    // « … Side B.cue », et l'ordre de `read_dir` n'est pas défini. Sans ce tri,
    // deux scans du même dossier pourraient rendre les faces dans un ordre
    // différent.
    let mut feuilles: Vec<PathBuf> = index
        .values()
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("cue"))
        })
        .cloned()
        .collect();
    feuilles.sort();

    let mut plan = PlanCue::default();
    let mut resolues: Vec<FeuilleResolue> = Vec::new();

    for chemin in feuilles {
        let octets = match std::fs::read(&chemin) {
            Ok(o) => o,
            Err(e) => {
                plan.ecartees
                    .push((chemin, MotifEcart::Illisible(e.to_string())));
                continue;
            }
        };
        let feuille = parse_cue_bytes(&octets);

        if feuille.tracks.is_empty() {
            plan.ecartees.push((chemin, MotifEcart::SansPiste));
            continue;
        }
        if feuille.audio_files.is_empty() {
            plan.ecartees.push((chemin, MotifEcart::SansFichier));
            continue;
        }

        // Une feuille ne vaut que si TOUTES ses images sont là et lisibles.
        // Accepter une feuille à moitié résolue livrerait un album amputé sans
        // rien dire — pire qu'un album absent, qu'au moins le rapport explique.
        let mut images = HashMap::new();
        let mut ecart = None;
        for reference in &feuille.audio_files {
            let Some(image) = resoudre_image(&index, reference) else {
                ecart = Some(MotifEcart::ImageIntrouvable {
                    fichier: reference.clone(),
                });
                break;
            };
            if !crate::audio::support::native_decoder_supports(&image) {
                ecart = Some(MotifEcart::ImageNonDecodable {
                    extension: image
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or_default()
                        .to_lowercase(),
                    fichier: reference.clone(),
                });
                break;
            }
            images.insert(reference.clone(), image);
        }

        match ecart {
            Some(motif) => plan.ecartees.push((chemin, motif)),
            None => resolues.push(FeuilleResolue {
                chemin,
                feuille,
                images,
            }),
        }
    }

    plan.albums = regrouper(resolues);
    plan
}

/// Regroupe les feuilles résolues en albums.
///
/// Deux feuilles du même dossier ne forment un album que si elles portent le
/// même `TITLE` **et** que leurs numéros de pistes ne se recouvrent pas : c'est
/// la signature d'un enregistrement coupé en morceaux (les deux faces d'un
/// vinyle numérotées 01-05 puis 06-10). Deux feuilles qui recommencent toutes
/// deux à 01 sont deux disques distincts qui partagent un titre — les fusionner
/// entrelacerait leurs pistes. Dans le doute, on sépare : un album de trop se
/// voit et se corrige, une pochette de dix pistes mélangées ne se comprend pas.
fn regrouper(resolues: Vec<FeuilleResolue>) -> Vec<AlbumCue> {
    // Un groupe = (clé de titre, feuilles). La clé est `None` pour une feuille
    // sans titre : elle ne se regroupe avec rien, faute de quoi toutes les
    // feuilles anonymes d'un dossier fusionneraient en un seul album.
    let mut groupes: Vec<(Option<String>, Vec<FeuilleResolue>)> = Vec::new();

    for feuille in resolues {
        let cle = feuille.feuille.album_title.as_deref().map(cle_album);
        let position = cle.as_ref().and_then(|c| {
            groupes.iter().position(|(autre, membres)| {
                autre.as_deref() == Some(c.as_str())
                    && membres.iter().all(|deja| plages_disjointes(deja, &feuille))
            })
        });
        match position {
            Some(i) => groupes[i].1.push(feuille),
            None => groupes.push((cle, vec![feuille])),
        }
    }

    groupes
        .into_iter()
        .map(|(_, membres)| album_depuis(membres))
        .collect()
}

fn plages_disjointes(a: &FeuilleResolue, b: &FeuilleResolue) -> bool {
    match (a.plage(), b.plage()) {
        (Some((a_lo, a_hi)), Some((b_lo, b_hi))) => a_hi < b_lo || b_hi < a_lo,
        _ => false,
    }
}

fn album_depuis(mut groupe: Vec<FeuilleResolue>) -> AlbumCue {
    groupe.sort_by(|a, b| a.plage().cmp(&b.plage()).then(a.chemin.cmp(&b.chemin)));

    let premiere = &groupe[0].feuille;
    let titre = premiere.album_title.clone();
    let interprete = premiere.album_performer.clone();
    // Le genre et l'année peuvent n'être écrits que sur l'une des feuilles :
    // on prend la première qui en porte, plutôt que d'imposer la face A.
    let genre = groupe.iter().find_map(|f| f.feuille.album_genre.clone());
    let annee = groupe.iter().find_map(|f| f.feuille.album_date.clone());

    let mut pistes = Vec::new();
    let mut feuilles = Vec::new();
    for resolue in &groupe {
        feuilles.push(resolue.chemin.clone());
        for piste in &resolue.feuille.tracks {
            // Une piste sans `FILE` au-dessus d'elle se rattache au premier
            // fichier de la feuille : c'est l'ordre normal d'un `.cue`, et une
            // feuille qui déclare ses pistes avant son `FILE` reste lisible.
            let media = piste
                .audio_file
                .as_ref()
                .and_then(|r| resolue.images.get(r))
                .or_else(|| {
                    resolue
                        .feuille
                        .premier_fichier()
                        .and_then(|r| resolue.images.get(r))
                });
            let Some(media) = media else { continue };
            pistes.push(PisteCue {
                media: media.clone(),
                numero: piste.number,
                titre: piste.title.clone(),
                interprete: piste.performer.clone(),
                debut_ms: piste.start_ms,
                fin_ms: piste.end_ms,
            });
        }
    }

    AlbumCue {
        feuilles,
        titre,
        interprete,
        genre,
        annee,
        pistes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Un vrai fichier WAV court : en-tête RIFF complet et quelques échantillons
    /// PCM. Les fixtures de ce module doivent être des fichiers que le décodeur
    /// livré sait réellement ouvrir — un fichier vide prouverait seulement que
    /// l'extension a été lue.
    fn ecrire_wav(chemin: &Path, millisecondes: u32) {
        const TAUX: u32 = 44_100;
        let echantillons = TAUX * millisecondes / 1000;
        let octets_data = echantillons * 4; // 2 canaux × 16 bits
        let mut f = Vec::new();
        f.extend_from_slice(b"RIFF");
        f.extend_from_slice(&(36 + octets_data).to_le_bytes());
        f.extend_from_slice(b"WAVEfmt ");
        f.extend_from_slice(&16u32.to_le_bytes());
        f.extend_from_slice(&1u16.to_le_bytes()); // PCM
        f.extend_from_slice(&2u16.to_le_bytes()); // stéréo
        f.extend_from_slice(&TAUX.to_le_bytes());
        f.extend_from_slice(&(TAUX * 4).to_le_bytes());
        f.extend_from_slice(&4u16.to_le_bytes());
        f.extend_from_slice(&16u16.to_le_bytes());
        f.extend_from_slice(b"data");
        f.extend_from_slice(&octets_data.to_le_bytes());
        for n in 0..echantillons {
            // Une sinusoïde grossière : du signal, pas du silence.
            let v = ((n as f32 / 40.0).sin() * 8000.0) as i16;
            f.extend_from_slice(&v.to_le_bytes());
            f.extend_from_slice(&v.to_le_bytes());
        }
        fs::write(chemin, f).unwrap();
    }

    const FEUILLE: &str = "REM GENRE \"Classical\"\nREM DATE 1981\nPERFORMER \"Glenn Gould\"\nTITLE \"Goldberg Variations\"\nFILE \"image.wav\" WAVE\n  TRACK 01 AUDIO\n    TITLE \"Aria\"\n    INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    TITLE \"Variatio 1\"\n    INDEX 01 00:01:00\n";

    #[test]
    fn decoupe_un_album_a_partir_dune_feuille_et_de_son_image() {
        let d = tempfile::TempDir::new().unwrap();
        ecrire_wav(&d.path().join("image.wav"), 200);
        fs::write(d.path().join("album.cue"), FEUILLE).unwrap();

        let plan = planifier_dossier(d.path());
        assert!(plan.ecartees.is_empty(), "{:?}", plan.ecartees);
        assert_eq!(plan.albums.len(), 1);
        let a = &plan.albums[0];
        assert_eq!(a.titre.as_deref(), Some("Goldberg Variations"));
        assert_eq!(a.interprete.as_deref(), Some("Glenn Gould"));
        assert_eq!(a.genre.as_deref(), Some("Classical"));
        assert_eq!(a.annee.as_deref(), Some("1981"));
        assert_eq!(a.pistes.len(), 2);
        assert_eq!(a.pistes[0].media, d.path().join("image.wav"));
        assert_eq!(a.pistes[0].debut_ms, 0);
        assert_eq!(a.pistes[0].fin_ms, Some(1_000));
        assert_eq!(a.pistes[1].debut_ms, 1_000);
        assert_eq!(a.pistes[1].fin_ms, None);
    }

    /// La règle nº1 de Gros Bidon : foobar2000 créait un album à partir d'un
    /// `.cue` sans fichier audio, et il a dû configurer une exception.
    #[test]
    fn une_feuille_orpheline_ne_cree_aucun_album() {
        let d = tempfile::TempDir::new().unwrap();
        fs::write(d.path().join("album.cue"), FEUILLE).unwrap(); // pas de .wav

        let plan = planifier_dossier(d.path());
        assert!(plan.albums.is_empty(), "album fantôme créé");
        assert_eq!(plan.ecartees.len(), 1);
        assert_eq!(
            plan.ecartees[0].1,
            MotifEcart::ImageIntrouvable {
                fichier: "image.wav".into()
            }
        );
        assert_eq!(plan.ecartees[0].1.cle(), "cue-image-introuvable");
    }

    /// Découper ce qu'on ne sait pas décoder produit les mêmes albums
    /// injouables qu'une feuille orpheline. Le cas de Rhorn : `.cue` + `.mpc`.
    #[test]
    fn une_image_sans_decodeur_est_ecartee() {
        let d = tempfile::TempDir::new().unwrap();
        fs::write(d.path().join("image.mpc"), b"pas un decodeur connu").unwrap();
        fs::write(
            d.path().join("album.cue"),
            FEUILLE.replace("image.wav", "image.mpc"),
        )
        .unwrap();

        let plan = planifier_dossier(d.path());
        assert!(plan.albums.is_empty());
        assert_eq!(
            plan.ecartees[0].1,
            MotifEcart::ImageNonDecodable {
                fichier: "image.mpc".into(),
                extension: "mpc".into()
            }
        );
    }

    /// La casse diffère entre le Windows qui a écrit la feuille et le NAS Linux
    /// qui la relit. Sans repli insensible à la casse, l'album disparaît.
    #[test]
    fn resout_l_image_malgre_la_casse() {
        let d = tempfile::TempDir::new().unwrap();
        ecrire_wav(&d.path().join("image.wav"), 50);
        fs::write(
            d.path().join("album.cue"),
            FEUILLE.replace("image.wav", "IMAGE.WAV"),
        )
        .unwrap();

        let plan = planifier_dossier(d.path());
        assert_eq!(plan.albums.len(), 1, "{:?}", plan.ecartees);
        assert_eq!(plan.albums[0].pistes[0].media, d.path().join("image.wav"));
    }

    /// Une feuille ne doit pas pouvoir désigner un fichier hors de son dossier.
    #[test]
    fn une_reference_avec_chemin_reste_dans_le_dossier() {
        let d = tempfile::TempDir::new().unwrap();
        let dedans = d.path().join("dedans");
        fs::create_dir(&dedans).unwrap();
        ecrire_wav(&d.path().join("image.wav"), 50); // hors du dossier scanné
        fs::write(
            dedans.join("album.cue"),
            FEUILLE.replace("image.wav", "..\\image.wav"),
        )
        .unwrap();

        let plan = planifier_dossier(&dedans);
        assert!(plan.albums.is_empty(), "la feuille est sortie du dossier");
        assert_eq!(plan.ecartees.len(), 1);
    }

    /// Le multi-CUE de Gros Bidon : un vinyle numérisé face par face, deux
    /// `.cue` de même titre, numérotation continue, temps repartant de zéro.
    #[test]
    fn deux_feuilles_de_meme_titre_et_de_numeros_suivis_font_un_album() {
        let d = tempfile::TempDir::new().unwrap();
        ecrire_wav(&d.path().join("Side A.wav"), 50);
        ecrire_wav(&d.path().join("Side B.wav"), 50);
        fs::write(
            d.path().join("A.cue"),
            "TITLE \"Stationary Traveller\"\nPERFORMER \"Camel\"\nREM DATE 1984\nFILE \"Side A.wav\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:04:00\n",
        )
        .unwrap();
        fs::write(
            d.path().join("B.cue"),
            "TITLE \"Stationary Traveller\"\nPERFORMER \"Camel\"\nREM GENRE \"Rock\"\nFILE \"Side B.wav\" WAVE\nTRACK 06 AUDIO\nINDEX 01 00:00:00\nTRACK 07 AUDIO\nINDEX 01 00:03:00\n",
        )
        .unwrap();

        let plan = planifier_dossier(d.path());
        assert_eq!(plan.albums.len(), 1, "les deux faces font deux albums");
        let a = &plan.albums[0];
        assert_eq!(a.feuilles.len(), 2);
        let numeros: Vec<u32> = a.pistes.iter().map(|p| p.numero).collect();
        assert_eq!(numeros, vec![1, 2, 6, 7]);
        // Chaque face garde SON fichier et SES temps : la piste 06 redémarre à
        // zéro dans la face B, elle ne se superpose pas à la piste 01.
        assert_eq!(a.pistes[2].media, d.path().join("Side B.wav"));
        assert_eq!(a.pistes[2].debut_ms, 0);
        assert_eq!(a.pistes[0].media, d.path().join("Side A.wav"));
        assert_eq!(a.pistes[0].debut_ms, 0);
        // Genre et année sont pris là où ils sont écrits, pas seulement en A.
        assert_eq!(a.annee.as_deref(), Some("1984"));
        assert_eq!(a.genre.as_deref(), Some("Rock"));
    }

    /// Deux disques qui partagent un titre et recommencent tous deux à 01 ne
    /// sont pas un album coupé en deux : les fusionner entrelacerait les pistes.
    #[test]
    fn deux_feuilles_aux_numeros_qui_se_recouvrent_restent_deux_albums() {
        let d = tempfile::TempDir::new().unwrap();
        ecrire_wav(&d.path().join("cd1.wav"), 50);
        ecrire_wav(&d.path().join("cd2.wav"), 50);
        for (nom, image) in [("cd1.cue", "cd1.wav"), ("cd2.cue", "cd2.wav")] {
            fs::write(
                d.path().join(nom),
                format!(
                    "TITLE \"Integrale\"\nFILE \"{image}\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:02:00\n"
                ),
            )
            .unwrap();
        }

        let plan = planifier_dossier(d.path());
        assert_eq!(plan.albums.len(), 2, "deux disques ont été entrelacés");
    }

    /// Une feuille bancale ne doit emporter ni le dossier, ni le scan.
    ///
    /// L'analyseur est tolérant par construction : il ne rend jamais d'erreur,
    /// il rend ce qu'il a su lire. Des octets qui ne sont d'aucun encodage
    /// connu ne produisent donc pas `Illisible` — qui est réservé à un échec
    /// d'entrée-sortie — mais une feuille sans aucune piste. Ce qui compte, et
    /// ce que ce test épingle, est que la bonne feuille du même dossier
    /// survive.
    #[test]
    fn une_feuille_bancale_ne_fait_pas_tomber_le_dossier() {
        let d = tempfile::TempDir::new().unwrap();
        ecrire_wav(&d.path().join("image.wav"), 50);
        fs::write(d.path().join("bon.cue"), FEUILLE).unwrap();
        // Des octets qui ne sont ni de l'UTF-8, ni de l'UTF-16, ni du CUE.
        fs::write(
            d.path().join("casse.cue"),
            b"\x00\xff\xfe\x01TRACK\nINDEX 01 pas-une-heure\n\xc3\x28",
        )
        .unwrap();
        // Et une feuille syntaxiquement lisible mais vide de pistes.
        fs::write(d.path().join("vide.cue"), "TITLE \"Rien\"\n").unwrap();

        let plan = planifier_dossier(d.path());
        assert_eq!(plan.albums.len(), 1, "la bonne feuille a été perdue");
        assert_eq!(plan.albums[0].titre.as_deref(), Some("Goldberg Variations"));
        assert_eq!(plan.ecartees.len(), 2);
        let cles = plan.ecarts_par_cle();
        assert_eq!(cles.get("cue-sans-piste"), Some(&2), "{cles:?}");
    }

    #[test]
    fn un_dossier_sans_feuille_rend_un_plan_vide() {
        let d = tempfile::TempDir::new().unwrap();
        ecrire_wav(&d.path().join("piste.wav"), 50);
        assert!(planifier_dossier(d.path()).est_vide());
    }

    /// Un dossier inexistant ne doit pas paniquer : pendant un scan, un
    /// répertoire peut disparaître entre l'énumération et la lecture.
    #[test]
    fn un_dossier_absent_ne_panique_pas() {
        let d = tempfile::TempDir::new().unwrap();
        assert!(planifier_dossier(&d.path().join("nexiste-pas")).est_vide());
    }

    #[test]
    fn nom_nu_coupe_les_deux_separateurs() {
        assert_eq!(nom_nu("album.flac"), Some("album.flac"));
        assert_eq!(nom_nu("..\\autre\\album.flac"), Some("album.flac"));
        assert_eq!(nom_nu("D:\\rips\\album.flac"), Some("album.flac"));
        assert_eq!(nom_nu("sous/dossier/album.flac"), Some("album.flac"));
        assert_eq!(nom_nu(".."), None);
        assert_eq!(nom_nu("   "), None);
    }
}
