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
//! L'ordre de lecture est celui de `refresh_cover_hash` : la jaquette
//! intégrée d'abord, puis l'image du dossier.

use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use super::artwork::{
    FOLDER_COVER_NAMES, artwork_hash, content_hash, extended_path, extract_cover_art, find_cached,
    find_folder_cover, save_to_cache,
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

/// « mtime:taille » d'un fichier, `None` s'il n'existe plus.
///
/// Secondes entières : SMB et FAT ne portent pas mieux, et deux lectures du
/// même fichier doivent rendre la même empreinte.
pub fn empreinte_du_fichier(fichier: &Path) -> Option<String> {
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
    let image = find_folder_cover(piste)?;
    let data = match std::fs::read(&*extended_path(&image)) {
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
        empreinte: empreinte_du_fichier(&image),
    })
}

/// UNE piste : sa jaquette intégrée d'abord, puis l'image de son dossier.
pub fn lire_depuis_la_piste(
    piste: &Path,
    cache_dir: &Path,
    octets: Option<&(Vec<u8>, String)>,
) -> Option<PochetteLue> {
    lire_la_jaquette(piste, cache_dir, octets).or_else(|| lire_l_image_du_dossier(piste, cache_dir))
}

/// L'album ENTIER, relu sur le disque : la première jaquette intégrée parmi
/// ses pistes (dans l'ordre du disque), sinon la première image de dossier.
///
/// Ne sert qu'aux reprises — un fichier source disparu ou changé — jamais au
/// fil du scan : c'est la seule lecture qui ouvre toutes les pistes.
pub fn lire_depuis_l_album(pistes: &[PathBuf], cache_dir: &Path) -> Option<PochetteLue> {
    pistes
        .iter()
        .filter(|p| extended_path(p).exists())
        .find_map(|p| lire_la_jaquette(p, cache_dir, None))
        .or_else(|| {
            pistes
                .iter()
                .find_map(|p| lire_l_image_du_dossier(p, cache_dir))
        })
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
    let mut vues = std::collections::HashSet::new();
    crate::db::track_repo::TrackRepo::with_backend(db.clone())
        .list_by_album(album_id)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|t| t.file_path.or(t.cue_media_path))
        .filter(|p| vues.insert(p.clone()))
        .map(PathBuf::from)
        .collect()
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
    let lue = lire_depuis_l_album(&pistes, cache_dir);
    let du_disque = vient_du_disque(etat, lue.as_ref(), &pistes);
    let geste = arbitrer(etat, lue.as_ref(), complet, du_disque);
    appliquer(repo, album_id, &geste);
    geste
}

/// Ce que le scan retient d'une piste pour la pochette de son album.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Suivi {
    /// L'album est tranché pour ce scan : les pistes suivantes n'y reviennent
    /// pas. Faux seulement pour un album SANS pochette dont cette piste n'a
    /// rien donné — la suivante peut en porter une.
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
/// `octets` : la jaquette déjà lue avec les balises, s'il y en a.
pub fn suivre_la_piste(
    db: &std::sync::Arc<dyn DbBackend>,
    album_id: i64,
    piste: &Path,
    octets: Option<&(Vec<u8>, String)>,
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
        let Some(lue) = lire_depuis_la_piste(piste, cache_dir, octets) else {
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
        .is_some_and(|f| !extended_path(Path::new(f)).exists());
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

    let lue = lire_depuis_la_piste(piste, cache_dir, octets);
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
pub fn suivre_les_fichiers_sources(
    db: &std::sync::Arc<dyn DbBackend>,
    cache_dir: &Path,
    portee: &[String],
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
        let actuelle = empreinte_du_fichier(Path::new(&fichier));
        if actuelle.is_some() && actuelle == empreinte {
            continue;
        }
        if reevaluer_l_album(db, album_id, cache_dir, complet, None) != Geste::Garder {
            reprises += 1;
        }
    }
    if reprises > 0 {
        info!(reprises, "pochettes_suivies_sur_le_disque");
    }
    reprises
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
