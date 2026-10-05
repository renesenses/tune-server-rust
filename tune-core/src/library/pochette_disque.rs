//! La pochette d'un album face au DISQUE (#5034, Didier, fil 1904).
//!
//! Une pochette tirée d'un fichier — la jaquette intégrée d'une piste, ou le
//! `cover.jpg` posé à côté des pistes — doit suivre ce fichier. Jusqu'ici,
//! rien ne la retirait jamais : quand la jaquette était ôtée dans Mp3tag, ou
//! le `cover.jpg` supprimé, l'ancienne image restait à l'écran, même après une
//! « Analyse complète ».
//!
//! Ce module porte la règle UNE fois, pour les trois passes qui relisent le
//! disque — le scan (rapide, « Répertoires » et complet), le scan de
//! démarrage, et le surveillant de fichiers :
//!
//! - la SOURCE de chaque pochette est écrite avec elle (`albums.cover_source`,
//!   migration 111) : seules [`SourcePochette::Integree`] et
//!   [`SourcePochette::Dossier`] suivent le disque. Une pochette téléversée
//!   n'est jamais touchée ; une pochette de fournisseur n'est jamais retirée ;
//! - le FICHIER d'où l'image a été tirée est écrit aussi, avec son empreinte
//!   (« mtime:taille ») : un seul `stat` dit au scan suivant que ce fichier a
//!   disparu, sans relire aucune image ;
//! - une source INCONNUE (toute ligne d'avant la migration) n'est retirée que
//!   PROUVÉE : même image relue sur le disque, ou ancienne adresse dérivée du
//!   chemin d'un fichier (`artwork_hash`, d'avant #1444). Sinon elle est
//!   gardée — la valeur par défaut prudente voulue par la décision du
//!   25/09/2026.
//!
//! L'ordre de lecture est celui de `refresh_cover_hash` : l'image du
//! DOSSIER d'abord, puis la jaquette intégrée. Décision de Bertrand du
//! 03/10/2026 (#5685, Marco Polo, fil 2118), qui renverse celle du 25/09
//! (#5035) : une image posée par l'utilisateur à côté de ses fichiers — dans
//! le dossier du disque, ou dans le dossier qui réunit les disques d'un
//! coffret ([`dossier_commun`]) — passe avant la jaquette intégrée.

use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use super::artwork::{
    EmpreinteJaquette, FOLDER_COVER_NAMES, artwork_hash, content_hash, empreinte_jaquette_flac,
    extended_path, extract_cover_art, find_cached, find_folder_cover, image_de_pochette_dans,
    save_to_cache,
};
use crate::db::album_repo::{AlbumRepo, EtatPochette};
use crate::db::backend::DbBackend;
use crate::db::models::SourcePochette;

/// Une pochette relue sur le disque, mise en cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PochetteLue {
    /// Condensat du CONTENU (#1444) — l'adresse servie par la route.
    pub condensat: String,
    pub source: SourcePochette,
    /// La piste (jaquette intégrée) ou l'image (pochette de dossier).
    pub fichier: String,
    /// « mtime:taille » du fichier au moment de la lecture.
    pub empreinte: Option<String>,
}

/// Ce que la règle décide pour un album.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Geste {
    Garder,
    Poser(PochetteLue),
    Retirer,
}

/// Le fichier existe-t-il ? Un chemin rangé dans une image ISO (#5299) n'a
/// pas d'`exists()` : il existe si l'image le contient.
fn existe(fichier: &Path) -> bool {
    if crate::audio::iso9660::est_chemin_virtuel(&fichier.to_string_lossy()) {
        return crate::audio::iso9660::taille_et_mtime(fichier).is_some();
    }
    extended_path(fichier).exists()
}

/// « mtime:taille » d'un fichier, `None` s'il n'existe plus.
///
/// Secondes entières : SMB et FAT ne portent pas mieux, et deux lectures du
/// même fichier doivent rendre la même empreinte.
pub fn empreinte_du_fichier(fichier: &Path) -> Option<String> {
    // #5299 — une pochette ou une piste rangée dans une image ISO : sa taille
    // propre, la date de l'image.
    if crate::audio::iso9660::est_chemin_virtuel(&fichier.to_string_lossy()) {
        let (taille, mtime) = crate::audio::iso9660::taille_et_mtime(fichier)?;
        return Some(format!("{}:{taille}", mtime as u64));
    }
    let meta = std::fs::metadata(&*extended_path(fichier)).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Some(format!("{mtime}:{}", meta.len()))
}

fn en_cache(octets: &[u8], ext: &str, cache_dir: &Path) -> Option<String> {
    let condensat = content_hash(octets);
    if find_cached(cache_dir, &condensat).is_some() {
        return Some(condensat);
    }
    save_to_cache(octets, cache_dir, &condensat, ext).map(|_| condensat)
}

/// La jaquette INTÉGRÉE d'une piste. `octets` : ceux lus avec les balises,
/// quand le lecteur les a gardés ; sinon la piste est relue.
pub fn lire_la_jaquette(
    piste: &Path,
    cache_dir: &Path,
    octets: Option<&(Vec<u8>, String)>,
) -> Option<PochetteLue> {
    let relus;
    let (data, mime) = match octets {
        Some((d, m)) => (d.as_slice(), m.as_str()),
        None => {
            relus = extract_cover_art(piste)?;
            (relus.0.as_slice(), relus.1.as_str())
        }
    };
    let ext = if mime.contains("png") {
        "png"
    } else if mime.contains("bmp") {
        "bmp"
    } else {
        "jpg"
    };
    let condensat = en_cache(data, ext, cache_dir)?;
    Some(PochetteLue {
        condensat,
        source: SourcePochette::Integree,
        fichier: piste.to_string_lossy().into_owned(),
        empreinte: empreinte_du_fichier(piste),
    })
}

/// L'image du DOSSIER d'une piste (`cover.jpg`, `folder.jpg`…).
pub fn lire_l_image_du_dossier(piste: &Path, cache_dir: &Path) -> Option<PochetteLue> {
    lire_l_image_de_pochette(&find_folder_cover(piste)?, cache_dir)
}

/// Une image de pochette de dossier déjà trouvée, mise en cache.
fn lire_l_image_de_pochette(image: &Path, cache_dir: &Path) -> Option<PochetteLue> {
    let data = match crate::library::artwork::lire_l_image(image) {
        Ok(d) => d,
        Err(e) => {
            debug!(path = %image.display(), error = %e, "pochette_dossier_illisible");
            return None;
        }
    };
    let ext = image.extension().and_then(|e| e.to_str()).unwrap_or("jpg");
    let condensat = en_cache(&data, ext, cache_dir)?;
    Some(PochetteLue {
        condensat,
        source: SourcePochette::Dossier,
        fichier: image.to_string_lossy().into_owned(),
        empreinte: empreinte_du_fichier(image),
    })
}

/// #5685 — de combien de niveaux, au plus, on remonte au-dessus du dossier
/// d'une piste pour trouver le dossier qui réunit les disques d'un album :
/// `Coffret/CD1/` (un niveau) ou `Coffret/CD1/FLAC/` (deux).
pub const REMONTEE_MAX: usize = 2;

/// #5685 — le dossier qui RÉUNIT les dossiers de disques d'un album rangé un
/// dossier par disque (`Coffret/CD1`, `Coffret/CD2`… → `Coffret`).
///
/// `None` quand l'album tient dans un seul dossier (l'image de ce dossier est
/// celle de [`find_folder_cover`]), quand un dossier de disque est à plus de
/// [`REMONTEE_MAX`] niveaux du dossier commun, quand ce dossier serait la
/// racine du système de fichiers, ou pour une piste rangée dans une image ISO
/// (#5299, qui a sa propre règle).
pub fn dossier_commun(pistes: &[PathBuf]) -> Option<PathBuf> {
    let mut dossiers: Vec<&Path> = Vec::new();
    for p in pistes {
        if crate::audio::iso9660::est_chemin_virtuel(&p.to_string_lossy()) {
            return None;
        }
        let d = p.parent()?;
        if !dossiers.contains(&d) {
            dossiers.push(d);
        }
    }
    let (premier, autres) = dossiers.split_first()?;
    if autres.is_empty() {
        return None;
    }
    let mut commun = premier.to_path_buf();
    for d in autres {
        // `starts_with` compare par COMPOSANTS : `Coffret` n'est pas un
        // préfixe de `Coffret 2`.
        while !d.starts_with(&commun) {
            if !commun.pop() {
                return None;
            }
        }
    }
    let niveaux = commun.components().count();
    if commun.parent().is_none()
        || dossiers
            .iter()
            .any(|d| d.components().count().saturating_sub(niveaux) > REMONTEE_MAX)
    {
        return None;
    }
    Some(commun)
}

/// #5685 — l'image de pochette du dossier qui réunit les disques de l'album
/// ([`dossier_commun`]), si ce dossier n'abrite QUE cet album.
///
/// La garde est lue en base : une seule piste d'un autre album sous ce
/// dossier, et l'image n'est pas la sienne — c'est le dossier d'un artiste
/// (son `folder.jpg` est sa photo), ou la racine de la bibliothèque. C'est
/// aussi ce qui borne la remontée à la racine de la bibliothèque : aucune
/// piste n'est indexée au-dessus d'elle, mais d'autres albums vivent à côté.
fn image_commune(
    db: &std::sync::Arc<dyn DbBackend>,
    album_id: i64,
    pistes: &[PathBuf],
) -> Option<PathBuf> {
    let commun = dossier_commun(pistes)?;
    let image = image_de_pochette_dans(&commun)?;
    let sous = crate::db::track_repo::TrackRepo::with_backend(db.clone())
        .albums_sous_dossier(&commun.to_string_lossy())
        .ok()?;
    (!sous.is_empty() && sous.iter().all(|(_, a)| *a == album_id)).then_some(image)
}

/// #5685 — l'image de DOSSIER d'un album entier : celle du dossier qui réunit
/// ses disques (`commune`), sinon la première image d'un dossier de piste
/// dans l'ordre du disque. Chaque dossier n'est regardé qu'une fois.
fn lire_l_image_de_l_album(
    commune: Option<&Path>,
    pistes: &[PathBuf],
    cache_dir: &Path,
) -> Option<PochetteLue> {
    if let Some(lue) = commune.and_then(|i| lire_l_image_de_pochette(i, cache_dir)) {
        return Some(lue);
    }
    let mut vus: Vec<&Path> = Vec::new();
    pistes.iter().find_map(|p| {
        let d = p.parent()?;
        if vus.contains(&d) {
            return None;
        }
        vus.push(d);
        lire_l_image_du_dossier(p, cache_dir)
    })
}

/// Ce que l'appelant sait déjà de la jaquette intégrée d'une piste.
#[derive(Debug, Clone, Copy)]
pub enum Jaquette<'a> {
    /// Rien : elle sera relue sur le disque si la règle en a besoin.
    Inconnue,
    /// Lue, et la piste n'en porte pas : rien à relire.
    Absente,
    /// Lue : ses octets et son type.
    Lue(&'a (Vec<u8>, String)),
}

impl<'a> Jaquette<'a> {
    /// Des octets lus avec les balises : présents, ou inconnus.
    pub fn depuis(octets: Option<&'a (Vec<u8>, String)>) -> Self {
        octets.map_or(Self::Inconnue, Self::Lue)
    }

    /// Des octets relus sur la piste : présents, ou absents.
    pub fn relue(octets: Option<&'a (Vec<u8>, String)>) -> Self {
        octets.map_or(Self::Absente, Self::Lue)
    }

    fn octets(self) -> Option<&'a (Vec<u8>, String)> {
        match self {
            Self::Lue(o) => Some(o),
            _ => None,
        }
    }
}

/// UNE piste : l'image de son dossier d'abord, puis sa jaquette intégrée
/// (#5685).
pub fn lire_depuis_la_piste(
    piste: &Path,
    cache_dir: &Path,
    octets: Option<&(Vec<u8>, String)>,
) -> Option<PochetteLue> {
    lire_selon(piste, cache_dir, Jaquette::depuis(octets))
}

fn lire_selon(piste: &Path, cache_dir: &Path, jaquette: Jaquette<'_>) -> Option<PochetteLue> {
    lire_l_image_du_dossier(piste, cache_dir).or_else(|| match jaquette {
        Jaquette::Absente => None,
        j => lire_la_jaquette(piste, cache_dir, j.octets()),
    })
}

/// UNE piste face à son ALBUM : l'image du dossier qui réunit ses disques
/// (#5685), puis [`lire_selon`].
fn lire_pour_l_album(
    db: &std::sync::Arc<dyn DbBackend>,
    album_id: i64,
    piste: &Path,
    cache_dir: &Path,
    jaquette: Jaquette<'_>,
) -> Option<PochetteLue> {
    let pistes = pistes_de_l_album(db, album_id);
    image_commune(db, album_id, &pistes)
        .and_then(|i| lire_l_image_de_pochette(&i, cache_dir))
        .or_else(|| lire_selon(piste, cache_dir, jaquette))
}

/// L'album ENTIER, relu sur le disque : la première image de dossier dans
/// l'ordre du disque (#5685), sinon la jaquette intégrée que porte la
/// MAJORITÉ de ses pistes (à égalité, la première dans l'ordre du disque).
/// L'image du dossier qui réunit les disques d'un coffret passe avant tout :
/// voir [`reevaluer_l_album`], qui seul peut la vérifier en base.
///
/// #5454 (Fuccaro, fil 1317) — c'était « la première jaquette intégrée dans
/// l'ordre du disque » : le single éponyme en piste 1 (*À partir de
/// maintenant*) imposait son image à tout l'album. Décision de Bertrand du
/// 29/09/2026 : l'image portée par la majorité des pistes.
///
/// Ne sert qu'aux reprises — un fichier source disparu ou changé, une
/// jaquette en désaccord — jamais à chaque piste du scan : c'est la seule
/// lecture qui ouvre toutes les pistes.
pub fn lire_depuis_l_album(pistes: &[PathBuf], cache_dir: &Path) -> Option<PochetteLue> {
    lire_l_album(None, pistes, cache_dir)
}

/// [`lire_depuis_l_album`], l'image du dossier commun (`commune`) en tête.
/// Les jaquettes ne sont comptées — toutes les pistes ouvertes — que si
/// aucune image de dossier n'existe.
fn lire_l_album(
    commune: Option<&Path>,
    pistes: &[PathBuf],
    cache_dir: &Path,
) -> Option<PochetteLue> {
    lire_l_image_de_l_album(commune, pistes, cache_dir).or_else(|| {
        compter_les_jaquettes(pistes)
            .gagnante()
            .and_then(|g| lire_la_jaquette(&g.fichier, cache_dir, None))
    })
}

/// Une image distincte parmi les jaquettes d'un album (#5454).
#[derive(Debug, Clone)]
struct Groupe {
    condensat: String,
    /// Nombre de pistes (de FICHIERS) qui la portent.
    voix: usize,
    /// La première piste, dans l'ordre du disque, qui la porte.
    fichier: PathBuf,
    empreinte: Option<EmpreinteJaquette>,
}

/// Les jaquettes intégrées d'un album, comptées piste par piste (#5454).
#[derive(Debug, Default)]
struct Decompte {
    /// Chaque fichier présent — son rang dans la liste comptée, qui suit
    /// l'ordre du disque — avec le condensat de sa jaquette (`None` : il n'en
    /// porte pas, il ne vote pas).
    pistes: Vec<(usize, Option<String>)>,
    /// Les images distinctes, dans l'ordre de leur première apparition.
    groupes: Vec<Groupe>,
}

impl Decompte {
    /// L'image portée par le plus de pistes ; à égalité, celle qui apparaît
    /// la première dans l'ordre du disque (disque, puis numéro de piste).
    fn gagnante(&self) -> Option<&Groupe> {
        // `groupes` est rangé par première apparition : garder le premier en
        // cas d'égalité, c'est garder le premier dans l'ordre du disque.
        self.groupes
            .iter()
            .fold(None, |meilleur, g| match meilleur {
                Some(m) if m.voix >= g.voix => Some(m),
                _ => Some(g),
            })
    }
}

/// Compte les jaquettes intégrées des pistes, DANS L'ORDRE DONNÉ (celui du
/// disque).
///
/// Sans charger chaque image : l'EMPREINTE d'un FLAC (longueur du bloc
/// PICTURE et trois échantillons, `artwork::empreinte_jaquette_flac`) déjà
/// vue dit de quelle image il s'agit ; seule la première piste de chaque
/// image est relue en entier, pour son condensat. Les autres formats sont
/// relus, comme au fil du scan. Aucun octet n'est gardé en mémoire.
fn compter_les_jaquettes(pistes: &[PathBuf]) -> Decompte {
    let mut connues: Vec<(EmpreinteJaquette, String)> = Vec::new();
    let mut d = Decompte::default();
    for (rang, p) in pistes.iter().enumerate().filter(|(_, p)| existe(p)) {
        let empreinte = empreinte_jaquette_flac(p);
        let connu = empreinte
            .as_ref()
            .and_then(|e| connues.iter().find(|(x, _)| x == e).map(|(_, c)| c.clone()));
        let condensat = connu.or_else(|| {
            let c = content_hash(&extract_cover_art(p)?.0);
            if let Some(e) = &empreinte {
                connues.push((e.clone(), c.clone()));
            }
            Some(c)
        });
        if let Some(c) = &condensat {
            match d.groupes.iter_mut().find(|g| &g.condensat == c) {
                Some(g) => g.voix += 1,
                None => d.groupes.push(Groupe {
                    condensat: c.clone(),
                    voix: 1,
                    fichier: p.clone(),
                    empreinte: empreinte.clone(),
                }),
            }
        }
        d.pistes.push((rang, condensat));
    }
    d
}

/// Les adresses qu'aurait données, AVANT #1444, une pochette tirée de ces
/// pistes : `artwork_hash` du chemin de la piste (jaquette, ou pochette de
/// dossier recopiée sous l'adresse de la piste) et du chemin de chaque image de
/// dossier possible — y compris une image qui n'existe plus. Aucune n'est
/// fabriquée par un téléversement (`album-upload-{id}`) ni par un fournisseur
/// (MBID) : les retrouver PROUVE que la pochette venait du disque.
fn adresses_heritees(pistes: &[PathBuf]) -> Vec<String> {
    let mut v = Vec::new();
    for p in pistes {
        v.push(artwork_hash(&p.to_string_lossy()));
        if let Some(dossier) = p.parent() {
            for nom in FOLDER_COVER_NAMES {
                v.push(artwork_hash(&dossier.join(nom).to_string_lossy()));
            }
        }
    }
    v
}

/// La pochette en place est-elle PROUVÉE tirée du disque ?
///
/// Oui si sa source le dit ; sinon (source inconnue), seulement si l'image
/// relue est la même, ou si l'adresse en place est une adresse héritée dérivée
/// du chemin de l'un de ces fichiers.
fn vient_du_disque(etat: &EtatPochette, lue: Option<&PochetteLue>, pistes: &[PathBuf]) -> bool {
    match etat.source {
        Some(s) => s.vient_du_disque(),
        None => {
            let Some(actuelle) = etat.cover_path.as_deref() else {
                return false;
            };
            lue.is_some_and(|l| l.condensat == actuelle)
                || adresses_heritees(pistes).iter().any(|h| h == actuelle)
        }
    }
}

/// LA règle. `complet` : « Analyse complète » (scan forcé) ; sinon passe
/// automatique — scan rapide, « Répertoires », scan de démarrage, surveillant.
///
/// | pochette en place | disque : rien | disque : même image | disque : autre image |
/// |---|---|---|---|
/// | aucune | — | — | posée |
/// | téléversée | gardée | gardée | gardée |
/// | fournisseur, import | gardée | gardée | posée si complet |
/// | du disque (prouvée) | **retirée** | confirmée | **posée** |
/// | inconnue, non prouvée | gardée | — | posée si complet |
///
/// Décision de Bertrand du 25/09/2026 (#5034, point 1) : les passes
/// automatiques SUIVENT une pochette du disque CHANGÉE — jaquette retouchée
/// dans Mp3tag, `cover.jpg` remplacé. Jusqu'ici seule l'Analyse complète le
/// faisait, et pour la seule jaquette intégrée.
pub fn arbitrer(
    etat: &EtatPochette,
    lue: Option<&PochetteLue>,
    complet: bool,
    du_disque: bool,
) -> Geste {
    let Some(actuelle) = etat.cover_path.as_deref() else {
        return lue.map_or(Geste::Garder, |l| Geste::Poser(l.clone()));
    };
    match etat.source {
        Some(SourcePochette::Televersee) => return Geste::Garder,
        Some(SourcePochette::Fournisseur | SourcePochette::Importee) => {
            return match lue {
                Some(l) if complet => Geste::Poser(l.clone()),
                _ => Geste::Garder,
            };
        }
        _ => {}
    }
    match lue {
        None if du_disque => Geste::Retirer,
        None => Geste::Garder,
        // Même image : on la CONFIRME, pour que sa source et son fichier
        // soient à jour (une ligne inconnue devient ainsi classée).
        Some(l) if l.condensat == actuelle => Geste::Poser(l.clone()),
        Some(l) if complet || du_disque => Geste::Poser(l.clone()),
        Some(_) => Geste::Garder,
    }
}

/// Écrit le geste. Rend vrai si une pochette a été posée.
fn appliquer(repo: &AlbumRepo, album_id: i64, geste: &Geste) -> bool {
    match geste {
        Geste::Garder => false,
        Geste::Poser(l) => {
            match repo.poser_pochette_du_disque(
                album_id,
                &l.condensat,
                l.source,
                &l.fichier,
                l.empreinte.as_deref(),
            ) {
                Ok(()) => true,
                Err(e) => {
                    warn!(album_id, error = %e, "cover_path_update_failed");
                    false
                }
            }
        }
        Geste::Retirer => {
            match repo.retirer_pochette(album_id) {
                Ok(()) => info!(
                    album_id,
                    "pochette_retiree — son fichier source a quitté le disque (#5034)"
                ),
                Err(e) => warn!(album_id, error = %e, "pochette_retrait_echoue"),
            }
            false
        }
    }
}

/// Les fichiers d'un album tels que la base les connaît, dans l'ordre du
/// disque.
///
/// 🔴 `file_path` ne suffit pas. Une piste découpée par une feuille CUE est une
/// tranche à l'intérieur d'un autre fichier : elle porte `file_path = NULL` par
/// construction, son support étant `cue_media_path`. Le rattrapage des
/// pochettes filtrait sur `file_path` et écartait ces pistes AVANT de chercher
/// une image (Gros Bidon, fil 1738, 09/09/2026 : « Tune ne semble pas prendre
/// le fichier cover.jpg associé au FLAC quand il est associé à un fichier
/// CUE »).
fn pistes_de_l_album(db: &std::sync::Arc<dyn DbBackend>, album_id: i64) -> Vec<PathBuf> {
    fichiers_de_l_album(db, album_id)
        .into_iter()
        .map(|(p, _)| p)
        .collect()
}

/// Comme [`pistes_de_l_album`], avec les identifiants des pistes que porte
/// chaque fichier (plusieurs pour une image découpée par une feuille CUE).
fn fichiers_de_l_album(
    db: &std::sync::Arc<dyn DbBackend>,
    album_id: i64,
) -> Vec<(PathBuf, Vec<i64>)> {
    let mut fichiers: Vec<(PathBuf, Vec<i64>)> = Vec::new();
    let mut rang: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for t in crate::db::track_repo::TrackRepo::with_backend(db.clone())
        .list_by_album(album_id)
        .unwrap_or_default()
    {
        let Some(p) = t.file_path.or(t.cue_media_path) else {
            continue;
        };
        let i = *rang.entry(p.clone()).or_insert_with(|| {
            fichiers.push((PathBuf::from(&p), Vec::new()));
            fichiers.len() - 1
        });
        if let Some(id) = t.id {
            fichiers[i].1.push(id);
        }
    }
    fichiers
}

/// L'album entier, relu sur le disque, puis la règle. Pour les reprises :
/// fichier source disparu (fin de scan), image de dossier touchée
/// (surveillant), jaquette perdue par la piste qui la portait.
///
/// `en_plus` : une piste que la base ne rattache pas encore à l'album (celle
/// que le scan est en train d'écrire).
pub fn reevaluer_l_album(
    db: &std::sync::Arc<dyn DbBackend>,
    album_id: i64,
    cache_dir: &Path,
    complet: bool,
    en_plus: Option<&Path>,
) -> Geste {
    let repo = AlbumRepo::with_backend(db.clone());
    let Ok(Some(etat)) = repo.etat_pochette(album_id) else {
        return Geste::Garder;
    };
    reevaluer_avec(db, &repo, album_id, &etat, cache_dir, complet, en_plus)
}

#[allow(clippy::too_many_arguments)]
fn reevaluer_avec(
    db: &std::sync::Arc<dyn DbBackend>,
    repo: &AlbumRepo,
    album_id: i64,
    etat: &EtatPochette,
    cache_dir: &Path,
    complet: bool,
    en_plus: Option<&Path>,
) -> Geste {
    // Rien à relire pour une pochette que le disque ne peut pas toucher.
    if etat.cover_path.is_some() && matches!(etat.source, Some(SourcePochette::Televersee)) {
        return Geste::Garder;
    }
    let mut pistes = pistes_de_l_album(db, album_id);
    // La piste en cours d'écriture passe APRÈS celles que la base connaît :
    // son rang dans le disque n'est pas encore écrit, et elle n'est relue ici
    // que quand la pochette ne vient pas d'elle — une autre piste source a
    // changé, ou elle a perdu sa jaquette. La mettre en tête laissait un
    // single, relu le premier, imposer sa jaquette à l'album (#4650).
    if let Some(p) = en_plus
        && !pistes.iter().any(|q| q == p)
    {
        pistes.push(p.to_path_buf());
    }
    // #5682 (fil 2115) — AUCUNE piste de l'album n'est joignable : c'est le
    // SUPPORT qui manque (partage du NAS pas encore monté, disque débranché),
    // pas la pochette qui a été ôtée. Conclure « rien sur le disque » retirait
    // la pochette de chaque album à chaque démarrage où le NAS arrivait en
    // retard, alors que les pistes, elles, étaient conservées. Une piste
    // vraiment supprimée quitte la base par la purge d'un scan sain, et
    // l'album vidé avec elle : la règle de #5034 ne perd rien.
    if !pistes.is_empty() && !pistes.iter().any(|p| existe(p)) {
        debug!(album_id, "pochette_gardee — aucune piste joignable (#5682)");
        return Geste::Garder;
    }
    let commune = image_commune(db, album_id, &pistes);
    let lue = lire_l_album(commune.as_deref(), &pistes, cache_dir);
    let du_disque = vient_du_disque(etat, lue.as_ref(), &pistes);
    let geste = arbitrer(etat, lue.as_ref(), complet, du_disque);
    appliquer(repo, album_id, &geste);
    geste
}

/// Ce que [`trancher_par_la_majorite`] a décidé pour un album (#5454).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tranche {
    pub geste: Geste,
    /// L'image qui fait RÉFÉRENCE pour les pistes de l'album : une piste qui
    /// porte une autre jaquette garde la sienne, les autres retombent sur la
    /// pochette de l'album. C'est la pochette de l'album quand elle vient du
    /// disque ; sinon (téléversée, fournisseur) la jaquette majoritaire.
    /// `None` : aucune piste ne porte de jaquette, rien n'a été tranché.
    pub reference: Option<String>,
    /// L'empreinte FLAC de la référence, quand elle est connue.
    pub empreinte: Option<EmpreinteJaquette>,
    /// Pistes qui portent une pochette propre après la décision.
    pub propres: usize,
}

/// #5454 — la pochette d'un album dont les pistes portent des jaquettes
/// DIFFÉRENTES est celle que porte la MAJORITÉ des pistes ; à égalité, celle
/// de la première piste dans l'ordre du disque. Décision de Bertrand du
/// 29/09/2026 (Fuccaro, fil 1317 : le single éponyme *À partir de
/// maintenant*, lu le premier, imposait son image à tout l'album de
/// Hallyday).
///
/// Relit l'album entier ([`compter_les_jaquettes`]), puis :
/// - la pochette de l'album suit [`arbitrer`], comme toute reprise : une
///   pochette TÉLÉVERSÉE n'est jamais touchée ; celle d'un fournisseur ne
///   cède qu'à une Analyse complète (`complet`) ;
/// - chaque piste dont la jaquette diffère de la référence reçoit sa
///   pochette propre (#4650) — le single garde la sienne ;
/// - une pochette propre ÉGALE à la référence est retirée : la piste retombe
///   sur celle de l'album.
///
/// Et la retouche d'une jaquette (#5034, point 1) ? Suivie seulement si elle
/// devient majoritaire : retoucher la seule piste qui avait donné la pochette
/// ne la change plus, la piste garde sa nouvelle image pour elle.
///
/// Appelée par le scan pour un album en DÉSACCORD — une piste relue dont la
/// jaquette s'écarte de la référence, ou une pochette changée par une piste —
/// jamais pour un album à jaquette unique : celui-là ne coûte rien de plus.
pub fn trancher_par_la_majorite(
    db: &std::sync::Arc<dyn DbBackend>,
    album_id: i64,
    cache_dir: &Path,
    complet: bool,
) -> Tranche {
    let rien = Tranche {
        geste: Geste::Garder,
        reference: None,
        empreinte: None,
        propres: 0,
    };
    let repo = AlbumRepo::with_backend(db.clone());
    let Ok(Some(etat)) = repo.etat_pochette(album_id) else {
        return rien;
    };
    let fichiers = fichiers_de_l_album(db, album_id);
    let pistes: Vec<PathBuf> = fichiers.iter().map(|(p, _)| p.clone()).collect();
    let decompte = compter_les_jaquettes(&pistes);
    let Some(gagnante) = decompte.gagnante().cloned() else {
        // Aucune jaquette : l'image du dossier, s'il y en a une, reste
        // l'affaire des règles de #5034/#5035.
        return rien;
    };

    let televersee =
        etat.cover_path.is_some() && matches!(etat.source, Some(SourcePochette::Televersee));
    let geste = if televersee {
        Geste::Garder
    } else {
        // #5685 — une image de dossier passe avant la jaquette majoritaire :
        // la majorité ne tranche alors que les pochettes PROPRES des pistes.
        let commune = image_commune(db, album_id, &pistes);
        match lire_l_image_de_l_album(commune.as_deref(), &pistes, cache_dir)
            .or_else(|| lire_la_jaquette(&gagnante.fichier, cache_dir, None))
        {
            Some(lue) => {
                let du_disque = vient_du_disque(&etat, Some(&lue), &pistes);
                let g = arbitrer(&etat, Some(&lue), complet, du_disque);
                appliquer(&repo, album_id, &g);
                g
            }
            // Relue entre-temps sans jaquette : la prochaine passe tranchera.
            None => return rien,
        }
    };

    // La référence des pistes est la pochette de l'album quand elle EST une
    // jaquette intégrée ; sinon (image de dossier — #5685 —, téléversée,
    // fournisseur) la jaquette majoritaire : seules les pistes qui s'en
    // écartent gardent une pochette propre, les autres retombent sur celle
    // de l'album.
    let apres = repo.etat_pochette(album_id).ok().flatten();
    let reference = match apres {
        Some(EtatPochette {
            cover_path: Some(c),
            source: Some(SourcePochette::Integree),
            ..
        }) => c,
        _ => gagnante.condensat.clone(),
    };
    let empreinte = (reference == gagnante.condensat)
        .then(|| gagnante.empreinte.clone())
        .flatten();

    // Les pochettes propres : chaque image distincte de la référence est mise
    // en cache UNE fois (relue sur la première piste qui la porte).
    let mut en_cache: std::collections::HashMap<&str, bool> = std::collections::HashMap::new();
    let mut a_poser: Vec<crate::db::models::Track> = Vec::new();
    for (rang, c) in &decompte.pistes {
        let Some(c) = c.as_deref().filter(|c| *c != reference) else {
            continue;
        };
        let ids = &fichiers[*rang].1;
        let pret = *en_cache.entry(c).or_insert_with(|| {
            decompte
                .groupes
                .iter()
                .find(|g| g.condensat == c)
                .and_then(|g| lire_la_jaquette(&g.fichier, cache_dir, None))
                .is_some_and(|l| l.condensat == c)
        });
        if !pret {
            continue;
        }
        for id in ids {
            let mut t = crate::db::models::Track::new(String::new());
            t.id = Some(*id);
            t.cover_path = Some(c.to_string());
            a_poser.push(t);
        }
    }
    let pistes_repo = crate::db::track_repo::TrackRepo::with_backend(db.clone());
    let propres = match pistes_repo.appliquer_pochettes_de_piste(&a_poser) {
        Ok(_) => a_poser.len(),
        Err(e) => {
            warn!(album_id, error = %e, "pochettes_de_piste_majorite_echec");
            0
        }
    };
    if let Err(e) = pistes_repo.retirer_pochettes_de_piste_egales(album_id, &reference) {
        warn!(album_id, error = %e, "pochettes_de_piste_redondantes_non_retirees");
    }
    debug!(
        album_id,
        voix = gagnante.voix,
        images = decompte.groupes.len(),
        propres,
        ?geste,
        "pochette_album_tranchee_par_la_majorite"
    );
    Tranche {
        geste,
        reference: Some(reference),
        empreinte,
        propres,
    }
}

/// Ce que le scan retient d'une piste pour la pochette de son album.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Suivi {
    /// L'album est tranché pour ce scan : les pistes suivantes n'y reviennent
    /// pas. Faux pour un album SANS pochette dont cette piste n'a rien donné.
    pub tranche: bool,
    /// Une pochette a été posée (compteur du rapport de scan).
    pub posee: bool,
}

/// La piste que le scan (ou le surveillant) vient de relire, face à la
/// pochette de son album.
///
/// Ne relit que ce qu'il faut :
/// - album sans pochette : cette piste (jaquette, puis dossier) ;
/// - pochette tirée d'une AUTRE piste ou du dossier, dont le fichier est
///   toujours là : rien, un `stat` au plus ;
/// - pochette tirée de CETTE piste, ou fichier source disparu : cette piste,
///   puis — si elle ne porte plus d'image — l'album entier.
///
/// `jaquette` : ce que l'appelant sait déjà de la jaquette de la piste.
pub fn suivre_la_piste(
    db: &std::sync::Arc<dyn DbBackend>,
    album_id: i64,
    piste: &Path,
    jaquette: Jaquette<'_>,
    cache_dir: &Path,
    complet: bool,
) -> Suivi {
    // #5685 — l'image du dossier passe avant la jaquette intégrée : un album
    // illustré par son image de DOSSIER ne reste plus ouvert (#5035 le
    // gardait ouvert pour une jaquette). Une image d'un AUTRE dossier de
    // disque, ou du dossier qui réunit les disques d'un coffret, est vue en
    // fin de scan (`suivre_les_fichiers_sources`), album par album.
    suivre(db, album_id, piste, jaquette, cache_dir, complet)
}

fn suivre(
    db: &std::sync::Arc<dyn DbBackend>,
    album_id: i64,
    piste: &Path,
    jaquette: Jaquette<'_>,
    cache_dir: &Path,
    complet: bool,
) -> Suivi {
    let repo = AlbumRepo::with_backend(db.clone());
    let etat = match repo.etat_pochette(album_id) {
        Ok(Some(e)) => e,
        Ok(None) => {
            return Suivi {
                tranche: true,
                posee: false,
            };
        }
        Err(e) => {
            warn!(album_id, error = %e, "pochette_etat_illisible");
            return Suivi {
                tranche: false,
                posee: false,
            };
        }
    };

    // Album sans pochette : cette piste la donne, ou la suivante.
    if etat.cover_path.is_none() {
        let Some(lue) = lire_pour_l_album(db, album_id, piste, cache_dir, jaquette) else {
            return Suivi {
                tranche: false,
                posee: false,
            };
        };
        let posee = appliquer(&repo, album_id, &Geste::Poser(lue));
        return Suivi {
            tranche: posee,
            posee,
        };
    }

    let chemin = piste.to_string_lossy();
    let piste_source = etat.fichier.as_deref() == Some(chemin.as_ref());
    let source_disparue = etat
        .fichier
        .as_deref()
        .is_some_and(|f| !existe(Path::new(f)));
    // Disparu, ou réécrit depuis la lecture : l'empreinte (« mtime:taille »)
    // ne correspond plus.
    let source_changee = source_disparue
        || etat.fichier.as_deref().is_some_and(|f| {
            empreinte_du_fichier(Path::new(f)).as_deref() != etat.empreinte.as_deref()
        });

    match etat.source {
        Some(SourcePochette::Televersee) => {
            return Suivi {
                tranche: true,
                posee: false,
            };
        }
        Some(SourcePochette::Fournisseur | SourcePochette::Importee) if !complet => {
            return Suivi {
                tranche: true,
                posee: false,
            };
        }
        // Pochette du disque tirée d'un autre fichier, toujours présent : la
        // relecture de cette piste ne la concerne pas (un single à la jaquette
        // propre, rangé dans l'album, ne la remplace pas — #4650).
        Some(s) if s.vient_du_disque() && !complet && !piste_source && !source_changee => {
            return Suivi {
                tranche: true,
                posee: false,
            };
        }
        // Tirée d'un AUTRE fichier, qui a changé : c'est LUI qu'il faut
        // relire, pas cette piste — l'album entier tranche. Sans quoi un
        // single à la jaquette propre, relu le premier après une retouche de
        // tout l'album, imposerait sa pochette au disque (#4650).
        Some(s) if s.vient_du_disque() && !complet && !piste_source => {
            let geste = reevaluer_avec(db, &repo, album_id, &etat, cache_dir, complet, Some(piste));
            return Suivi {
                tranche: true,
                posee: matches!(geste, Geste::Poser(_)),
            };
        }
        _ => {}
    }

    let lue = lire_pour_l_album(db, album_id, piste, cache_dir, jaquette);
    let pistes = [piste.to_path_buf()];
    let du_disque = vient_du_disque(&etat, lue.as_ref(), &pistes);
    // La piste ne porte plus d'image — ou plus la sienne : l'album entier
    // tranche (une autre piste peut encore porter la jaquette).
    let perdue = match &lue {
        None => true,
        Some(l) => {
            etat.source == Some(SourcePochette::Integree)
                && piste_source
                && l.source != SourcePochette::Integree
        }
    };
    let geste = if du_disque && (perdue || source_disparue) {
        reevaluer_avec(db, &repo, album_id, &etat, cache_dir, complet, Some(piste))
    } else {
        let g = arbitrer(&etat, lue.as_ref(), complet, du_disque);
        appliquer(&repo, album_id, &g);
        g
    };
    Suivi {
        tranche: true,
        posee: matches!(geste, Geste::Poser(_)),
    }
}

/// Fin de scan : chaque album dont la pochette sort d'un fichier du disque
/// est confronté à ce fichier, d'un `stat`. Disparu ou réécrit (empreinte
/// « mtime:taille » différente), l'album est relu en entier et la règle
/// s'applique. C'est ce qui rattrape le `cover.jpg` supprimé ou remplacé
/// d'un album dont aucune piste n'a changé — le scan rapide ne relit que les
/// pistes modifiées.
///
/// `portee` : les dossiers scannés (« Répertoires ») ; vide = tous.
/// `exclus` : les dossiers dont le parcours a échoué (#5682) — ce qu'ils
/// contiennent n'a pas été vu, rien n'y est conclu.
///
/// À n'appeler qu'après un scan dont les racines ont répondu : voir
/// [`le_suivi_peut_conclure`].
pub fn suivre_les_fichiers_sources(
    db: &std::sync::Arc<dyn DbBackend>,
    cache_dir: &Path,
    portee: &[String],
    exclus: &[String],
    complet: bool,
) -> usize {
    let repo = AlbumRepo::with_backend(db.clone());
    let sources = match repo.pochettes_tirees_du_disque() {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "pochettes_sources_illisibles");
            return 0;
        }
    };
    let mut reprises = 0usize;
    for (album_id, fichier, empreinte) in sources {
        if !portee.is_empty() && !portee.iter().any(|d| sous_le_dossier(&fichier, d)) {
            continue;
        }
        if exclus.iter().any(|d| sous_le_dossier(&fichier, d)) {
            continue;
        }
        let actuelle = empreinte_du_fichier(Path::new(&fichier));
        let a_relire = actuelle.is_none()
            || actuelle != empreinte
            || une_image_de_dossier_l_emporte(db, album_id, &fichier);
        if a_relire && reevaluer_l_album(db, album_id, cache_dir, complet, None) != Geste::Garder {
            reprises += 1;
        }
    }
    if reprises > 0 {
        info!(reprises, "pochettes_suivies_sur_le_disque");
    }
    reprises
}

/// #5682 (fil 2115) — la passe [`suivre_les_fichiers_sources`] a-t-elle le
/// droit de conclure « fichier source disparu » au sortir d'un scan ?
///
/// Non quand le scan a été annulé, qu'une racine manquait (partage pas
/// encore monté) ou qu'une racine s'est vidée (montage absent, point de
/// montage vide) : la même garde que celle qui CONSERVE les pistes
/// (`auto_scan_root_went_empty`). Les sous-dossiers en erreur de parcours ne
/// bloquent pas toute la passe — un dossier durablement illisible
/// l'éteindrait pour toujours — ils sont exclus de sa portée (`exclus`).
pub fn le_suivi_peut_conclure(
    annule: bool,
    racines_absentes: &[String],
    racines_videes: &[String],
) -> bool {
    !annule && racines_absentes.is_empty() && racines_videes.is_empty()
}

/// #5685 — une image de DOSSIER que la règle préfère à la pochette en place
/// (tirée de `fichier`) existe-t-elle ? C'est ce qui fait prendre, au scan
/// suivant, la bonne image à un album déjà en base dont aucun fichier n'a
/// bougé :
///
/// - un album illustré par une jaquette intégrée, à côté de laquelle une
///   image de dossier attend (l'ordre d'avant #5685 la laissait passer
///   après) ;
/// - un coffret rangé un dossier par disque, illustré par l'image d'UN disque
///   alors que le dossier qui les réunit porte la sienne — y compris un
///   coffret réuni après le scan (`coffrets_auto`), dont les disques
///   absorbés n'ont rien relu.
///
/// Ne lit rien pour un album d'un seul dossier déjà illustré par l'image de
/// ce dossier. Ailleurs, un `read_dir` par dossier écarte d'emblée ceux qui
/// n'ont aucune image : on ne sonde pas trente noms dans chacun.
fn une_image_de_dossier_l_emporte(
    db: &std::sync::Arc<dyn DbBackend>,
    album_id: i64,
    fichier: &str,
) -> bool {
    let pistes = pistes_de_l_album(db, album_id);
    let source = Path::new(fichier);
    let mut dossiers: Vec<&Path> = Vec::new();
    for d in pistes.iter().filter_map(|p| p.parent()) {
        if !dossiers.contains(&d) {
            dossiers.push(d);
        }
    }
    let integree = pistes.iter().any(|p| p == source);
    if !integree && dossiers.len() == 1 && source.parent() == dossiers.first().copied() {
        return false;
    }
    let preferee = image_commune(db, album_id, &pistes).or_else(|| {
        let mut vus: Vec<&Path> = Vec::new();
        pistes.iter().find_map(|p| {
            let d = p.parent()?;
            if vus.contains(&d) {
                return None;
            }
            vus.push(d);
            if !peut_porter_une_image(p) {
                return None;
            }
            find_folder_cover(p)
        })
    });
    preferee.is_some_and(|i| i != source)
}

/// Le dossier de cette piste peut-il porter une image de pochette ? Un seul
/// `read_dir`, noms comparés sans casse ; vrai quand on ne peut pas le lister
/// (image ISO, dossier illisible) : [`find_folder_cover`] tranchera.
fn peut_porter_une_image(piste: &Path) -> bool {
    if crate::audio::iso9660::est_chemin_virtuel(&piste.to_string_lossy()) {
        return true;
    }
    let Some(dossier) = piste.parent() else {
        return false;
    };
    match std::fs::read_dir(&*extended_path(dossier)) {
        Ok(entrees) => entrees
            .flatten()
            .any(|e| est_une_image_de_pochette(&e.path())),
        Err(_) => true,
    }
}

/// `chemin` est-il sous `dossier` ? Comparaison par composants, séparateurs
/// des deux systèmes admis.
fn sous_le_dossier(chemin: &str, dossier: &str) -> bool {
    let norm = |s: &str| s.replace('\\', "/");
    let d = norm(dossier);
    let d = d.trim_end_matches('/');
    let c = norm(chemin);
    c.len() > d.len() && c.starts_with(d) && c.as_bytes()[d.len()] == b'/'
}

/// Le nom est-il celui d'une image de pochette de dossier (`cover.jpg`,
/// `Folder.png`…) ? Pour le surveillant, qui ne relayait que l'audio.
pub fn est_une_image_de_pochette(chemin: &Path) -> bool {
    chemin
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| FOLDER_COVER_NAMES.iter().any(|c| c.eq_ignore_ascii_case(n)))
}

#[cfg(test)]
#[path = "pochette_disque_tests_5034.rs"]
mod tests;

#[cfg(test)]
#[path = "pochette_disque_tests_5682.rs"]
mod tests_5682;

#[cfg(test)]
#[path = "pochette_disque_tests_5685.rs"]
mod tests_5685;
