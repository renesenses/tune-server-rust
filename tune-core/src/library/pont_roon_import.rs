//! Appliquer un export du moissonneur Roon contre la bibliothèque — crédits
//! par piste et, depuis l'archive, images d'artistes et pochettes d'albums.
//!
//! Descendu de `tune-server` (`routes/system/import_pont_roon.rs`, #4251)
//! pour que l'extension « Pont Roon » (greffon Premium) et la route d'import
//! appliquent LA MÊME écriture : un deuxième corps divergerait au premier
//! correctif.
//!
//! Règles, toutes deux conservatrices :
//!
//! - **crédits** : on n'écrit que sur une piste qui n'en a AUCUN, rôle
//!   `composer`, provenance `track_metadata.credits_source = roon` ;
//! - **images** : on ne pose une image que là où Tune n'en a PAS — une image
//!   choisie, scannée ou enrichie n'est jamais remplacée par celle de Roon.
//!   L'image d'artiste porte `image_source = roon`.
//!
//! Ce qui vient de Roon reste local : le sync cloud ne lit ni `track_credits`,
//! ni `track_metadata`, ni `image_path` (témoin dans `tune-server`).

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use crate::db::album_repo::AlbumRepo;
use crate::db::artist_repo::ArtistRepo;
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::models::Album;
use crate::db::track_metadata_repo::TrackMetadataRepo;
use crate::db::track_repo::TrackRepo;
use crate::library::pont_roon::{
    Candidat, Classement, Decision, ExportRoon, PisteLocale, Rapport, apparier_piste,
    cle_courte_d_album, cle_d_album, credits_a_ecrire, decider, exemplaires_identiques,
    nom_generique, plier,
};

/// Marque d'origine, sur la piste : `track_metadata.credits_source = roon`.
pub const CLE_PROVENANCE: &str = "credits_source";
pub const PROVENANCE_ROON: &str = "roon";
/// `artists.image_source` d'une image venue de Roon.
pub const SOURCE_IMAGE_ROON: &str = "roon";
/// Le rôle donné aux noms que Roon ajoute à l'interprète — voir `pont_roon`.
const ROLE: &str = "composer";

/// Plafond d'une archive reçue. Elle n'est plus lue en mémoire : le serveur
/// l'écrit sur disque au fil de l'envoi et n'en extrait les images qu'une à
/// une. Le plafond ne garde donc plus la RAM, seulement le disque. L'archive
/// réelle de Fabien dépassait les 600 Mio de l'ancien plafond (#4251) : il
/// avait été calculé sur « ~1 800 images à ~80 Ko ».
pub const ARCHIVE_MAX_OCTETS: u64 = 8 * 1024 * 1024 * 1024;
/// Une entrée de l'archive ne se décompresse jamais au-delà : garde contre une
/// bombe de décompression, entrée par entrée.
pub const ENTREE_MAX_OCTETS: u64 = 256 * 1024 * 1024;

/// D'où viennent les octets des images nommées par l'export.
pub trait SourceImages {
    /// L'archive porte-t-elle cette image ? Ne lit rien.
    fn contient(&self, cle: &str) -> bool;
    /// Les octets de l'image, lus à la demande.
    fn lire(&self, cle: &str) -> Option<Vec<u8>>;
}

impl SourceImages for HashMap<String, Vec<u8>> {
    fn contient(&self, cle: &str) -> bool {
        self.contains_key(cle)
    }
    fn lire(&self, cle: &str) -> Option<Vec<u8>> {
        self.get(cle).cloned()
    }
}

/// Les octets d'image portés par une archive, et où les ranger.
pub struct ImagesRoon<'a> {
    /// Clé d'image Roon → octets.
    pub octets: &'a dyn SourceImages,
    /// Le cache d'illustrations du serveur (`artwork_cache_dir`).
    pub dossier_cache: &'a Path,
}

/// L'entrée : un texte est-il un export du moissonneur ?
pub fn est_un_export_du_pont(texte: &str) -> bool {
    texte.trim_start().starts_with('{')
        && texte.contains("\"source\"")
        && texte.contains("\"artistes\"")
}

/// Une archive du moissonneur (`--archive`) ouverte : les images restent dans
/// l'archive, seul leur index est gardé.
pub struct ArchiveRoon<R> {
    zip: std::sync::Mutex<zip::ZipArchive<R>>,
    index: HashMap<String, usize>,
}

impl<R: Read + std::io::Seek> ArchiveRoon<R> {
    /// Les clés d'image que l'archive porte.
    pub fn cles(&self) -> impl Iterator<Item = &str> {
        self.index.keys().map(String::as_str)
    }
    pub fn est_vide(&self) -> bool {
        self.index.is_empty()
    }
}

impl<R: Read + std::io::Seek> SourceImages for ArchiveRoon<R> {
    fn contient(&self, cle: &str) -> bool {
        self.index.contains_key(cle)
    }
    fn lire(&self, cle: &str) -> Option<Vec<u8>> {
        let i = *self.index.get(cle)?;
        let mut z = self.zip.lock().ok()?;
        let f = z.by_index(i).ok()?;
        lire_entree(f).ok()
    }
}

fn lire_entree(f: impl Read) -> std::io::Result<Vec<u8>> {
    let mut contenu = Vec::new();
    f.take(ENTREE_MAX_OCTETS + 1).read_to_end(&mut contenu)?;
    if contenu.len() as u64 > ENTREE_MAX_OCTETS {
        return Err(std::io::Error::other("entrée trop volumineuse"));
    }
    Ok(contenu)
}

/// Ouvre une archive du moissonneur : `export.json` + `images/<clé>.jpg`.
/// Lit l'export, indexe les images sans les décompresser.
pub fn ouvrir_archive<R: Read + std::io::Seek>(
    lecteur: R,
) -> Result<(ExportRoon, ArchiveRoon<R>), String> {
    let mut z = zip::ZipArchive::new(lecteur).map_err(|e| format!("archive illisible : {e}"))?;
    let mut export: Option<ExportRoon> = None;
    let mut index = HashMap::new();
    for i in 0..z.len() {
        let f = z
            .by_index(i)
            .map_err(|e| format!("archive illisible : {e}"))?;
        if f.is_dir() {
            continue;
        }
        let nom = f.name().to_string();
        if nom == "export.json" {
            let contenu = lire_entree(f).map_err(|e| format!("archive illisible ({nom}) : {e}"))?;
            let texte = String::from_utf8(contenu)
                .map_err(|_| "export.json n'est pas de l'UTF-8".to_string())?;
            export = Some(ExportRoon::lire(&texte)?);
        } else if let Some(cle) = nom
            .strip_prefix("images/")
            .and_then(|n| n.rsplit_once('.').map(|(c, _)| c))
            .filter(|c| {
                !c.is_empty()
                    && c.chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
            })
        {
            index.insert(cle.to_string(), i);
        }
    }
    let export = export.ok_or("archive sans export.json")?;
    Ok((
        export,
        ArchiveRoon {
            zip: std::sync::Mutex::new(z),
            index,
        },
    ))
}

/// Applique (ou aperçoit) un export contre la bibliothèque. `apercu` compte
/// tout et n'écrit rien. `images` absent = export JSON seul, sans octets.
pub fn appliquer(
    backend: &Arc<dyn DbBackend>,
    export: &ExportRoon,
    apercu: bool,
    images: Option<&ImagesRoon<'_>>,
) -> Rapport {
    let artistes = ArtistRepo::with_backend(backend.clone());
    let albums = AlbumRepo::with_backend(backend.clone());
    let pistes = TrackRepo::with_backend(backend.clone());
    let meta = TrackMetadataRepo::with_backend(backend.clone());

    let locaux: Vec<(i64, String)> = artistes
        .list_all_id_name_mbid()
        .unwrap_or_default()
        .into_iter()
        .map(|(id, nom, _)| (id, nom))
        .collect();
    // TOUTES les fiches d'un nom replié : un artiste scindé en deux lignes
    // garde ses deux discographies, et le doublon est dit (#5749).
    let mut par_nom: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, (_, n)) in locaux.iter().enumerate() {
        par_nom.entry(plier(n)).or_default().push(i);
    }
    // Compter ne lit rien ; les octets ne sont extraits qu'au moment de poser.
    let porte = |cle: &Option<String>| -> bool {
        cle.as_deref()
            .is_some_and(|c| images.is_some_and(|i| i.octets.contient(c)))
    };
    let octets_de =
        |cle: &Option<String>| -> Option<Vec<u8>> { images?.octets.lire(cle.as_deref()?) };

    // Lus à la demande, une fois : les pistes d'un album, et la bibliothèque
    // entière par clé de candidat (dernier recours du niveau 2).
    let mut pistes_par_album: HashMap<i64, Vec<PisteLocale>> = HashMap::new();
    let mut pistes_de = |album_id: i64| -> Vec<PisteLocale> {
        pistes_par_album
            .entry(album_id)
            .or_insert_with(|| {
                pistes
                    .list_by_album(album_id)
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|t| {
                        Some(PisteLocale {
                            id: t.id?,
                            titre: t.title.clone(),
                            numero: (t.track_number > 0).then_some(t.track_number),
                            disque: (t.disc_number > 0).then_some(t.disc_number),
                            artiste: t.artist_name.clone(),
                            // Lu au moment d'écrire, piste par piste.
                            a_des_credits: false,
                        })
                    })
                    .collect()
            })
            .clone()
    };
    let mut bibliotheque: Option<Bibliotheque> = None;

    let mut r = Rapport {
        artistes_total: export.artistes.len(),
        ..Default::default()
    };
    for (ia, ar) in export.artistes.iter().enumerate() {
        r.albums_total += ar.albums.len();
        r.pistes_total += ar.albums.iter().map(|a| a.pistes.len()).sum::<usize>();
        if ar.image.is_some() {
            r.images_nommees += 1;
            r.images_portees += usize::from(porte(&ar.image));
        }
        let fiches: Vec<usize> = par_nom.get(&plier(&ar.nom)).cloned().unwrap_or_default();
        // « Unknown Artist », « Various Artists » : le nom ne prouve rien. Ses
        // fiches restent des candidats au niveau 2, rien de plus — ni artiste
        // apparié, ni image, ni album apparié en strict.
        let artiste_generique = nom_generique(&ar.nom);
        if fiches.is_empty() || artiste_generique {
            r.artistes_inconnus.push(ar.nom.clone());
        } else {
            r.artistes_apparies += 1;
        }
        if fiches.len() > 1 {
            let ids: Vec<String> = fiches.iter().map(|&i| locaux[i].0.to_string()).collect();
            doublon(
                &mut r,
                format!(
                    "artiste « {} » : {} fiches Tune (id {})",
                    ar.nom,
                    fiches.len(),
                    ids.join(", ")
                ),
            );
        }

        // Image d'artiste : seulement s'il n'en a pas, et seulement sur une
        // fiche UNIQUE — entre deux homonymes, on ne choisit pas.
        if let [i] = fiches.as_slice()
            && !artiste_generique
            && porte(&ar.image)
        {
            let artiste_id = locaux[*i].0;
            let sans_image = artistes
                .get(artiste_id)
                .ok()
                .flatten()
                .map(|a| a.image_path.as_deref().is_none_or(str::is_empty))
                .unwrap_or(false);
            if sans_image {
                r.images_artistes_a_poser += 1;
                if !apercu
                    && let Some(hash) = octets_de(&ar.image).and_then(|d| ranger(&d, images))
                    && artistes
                        .update_image(artiste_id, &hash, SOURCE_IMAGE_ROON)
                        .is_ok()
                {
                    r.images_artistes_posees += 1;
                }
            }
        }

        let noms_tune: Vec<String> = fiches.iter().map(|&i| locaux[i].1.clone()).collect();
        // Niveau 1 : les albums dont une de ces fiches est l'artiste.
        let mut siens: Vec<Album> = Vec::new();
        for &i in &fiches {
            ajouter_sans_double(&mut siens, albums.list_by_artist(locaux[i].0));
        }
        // Niveau 2 : … plus ceux qui portent une de ses pistes (#4767).
        let mut presents: Option<Vec<Album>> = None;

        for (ja, al) in ar.albums.iter().enumerate() {
            r.images_nommees += usize::from(al.image.is_some());
            r.images_portees += usize::from(porte(&al.image));
            let nom_roon = format!("{} — {}", ar.nom, al.titre);

            let voulu = plier(&al.titre);
            // Un nom générique d'artiste ou d'album n'est jamais une preuve :
            // l'album passe directement au niveau 2.
            let stricts: Vec<&Album> = if artiste_generique || nom_generique(&al.titre) {
                Vec::new()
            } else {
                siens.iter().filter(|a| plier(&a.title) == voulu).collect()
            };
            // (album Tune, paires (piste Roon, piste locale))
            let trouves: Vec<(Album, Vec<(usize, PisteLocale)>)> = if let [seul] =
                stricts.as_slice()
                && let Some(id) = seul.id
            {
                let locales = pistes_de(id);
                let paires = al
                    .pistes
                    .iter()
                    .enumerate()
                    .filter_map(|(i, p)| apparier_piste(p, &locales).map(|l| (i, l.clone())))
                    .collect();
                r.albums_apparies_strict += 1;
                r.albums_par_strict
                    .push(format!("{nom_roon} → [{id}] {}", seul.title));
                classer(&mut r, ia, ja, "strict", vec![id]);
                vec![((*seul).clone(), paires)]
            } else {
                let candidats: Vec<Album> = if stricts.len() > 1 {
                    let ids: Vec<String> = stricts
                        .iter()
                        .filter_map(|a| a.id.map(|i| i.to_string()))
                        .collect();
                    doublon(
                        &mut r,
                        format!(
                            "album « {nom_roon} » : {} albums Tune de même titre (id {})",
                            stricts.len(),
                            ids.join(", ")
                        ),
                    );
                    stricts.into_iter().cloned().collect()
                } else {
                    let cle = cle_d_album(&al.titre).0;
                    let pool = presents.get_or_insert_with(|| {
                        let mut v = siens.clone();
                        for &i in &fiches {
                            let id = locaux[i].0;
                            ajouter_sans_double(
                                &mut v,
                                albums.list_compilations_with_artist_track(id),
                            );
                            ajouter_sans_double(&mut v, albums.list_appearances_of_artist(id));
                        }
                        v
                    });
                    let mut c: Vec<Album> = pool
                        .iter()
                        .filter(|a| cle_d_album(&a.title).0 == cle)
                        .cloned()
                        .collect();
                    let biblio = bibliotheque.get_or_insert_with(|| Bibliotheque::lire(&albums));
                    if c.is_empty() {
                        // Dernier recours : l'artiste est inconnu ou mal
                        // orthographié d'un côté (« Holliday »). Les pistes
                        // décident seules, au même seuil.
                        c = biblio.par_cle.get(&cle).cloned().unwrap_or_default();
                    }
                    if c.is_empty() {
                        // Seconde clé, sans crochets : « Black Orpheus
                        // [Original Soundtrack] » trouve « Black Orpheus ».
                        // Mêmes étapes, mêmes pistes pour décider.
                        let courte = cle_courte_d_album(&al.titre);
                        c = pool
                            .iter()
                            .filter(|a| cle_courte_d_album(&a.title) == courte)
                            .cloned()
                            .collect();
                        if c.is_empty() {
                            c = biblio
                                .par_cle_courte
                                .get(&courte)
                                .cloned()
                                .unwrap_or_default();
                        }
                    }
                    c
                };
                let candidats: Vec<(Album, Candidat)> = candidats
                    .into_iter()
                    .filter_map(|a| {
                        let id = a.id?;
                        let c = Candidat {
                            id,
                            titre: a.title.clone(),
                            pistes: pistes_de(id),
                        };
                        Some((a, c))
                    })
                    .collect();
                let seuls: Vec<Candidat> = candidats.iter().map(|(_, c)| c.clone()).collect();
                match decider(&al.pistes, &seuls) {
                    Decision::Apparie(choix) => {
                        let titres: Vec<String> = choix
                            .iter()
                            .map(|(k, _)| format!("[{}] {}", seuls[*k].id, seuls[*k].titre))
                            .collect();
                        r.albums_par_contenu
                            .push(format!("{nom_roon} → {}", titres.join(" + ")));
                        r.albums_apparies_contenu += 1;
                        let ids = choix.iter().map(|(k, _)| seuls[*k].id).collect();
                        classer(&mut r, ia, ja, "contenu", ids);
                        choix
                            .into_iter()
                            .map(|(k, paires)| {
                                let locales = &seuls[k].pistes;
                                (
                                    candidats[k].0.clone(),
                                    paires
                                        .into_iter()
                                        .map(|(i, j)| (i, locales[j].clone()))
                                        .collect(),
                                )
                            })
                            .collect()
                    }
                    Decision::Ambigu(ks) => {
                        let en_cause: Vec<&Candidat> = ks.iter().map(|k| &seuls[*k]).collect();
                        let titres: Vec<String> = en_cause
                            .iter()
                            .map(|c| format!("[{}] {}", c.id, c.titre))
                            .collect();
                        let copies = exemplaires_identiques(&en_cause);
                        let copies = if copies >= 2 {
                            format!(" ; {copies} exemplaires identiques")
                        } else {
                            String::new()
                        };
                        let ids = en_cause.iter().map(|c| c.id).collect();
                        classer(&mut r, ia, ja, "ambigu", ids);
                        r.albums_ambigus.push(format!(
                            "{nom_roon} ({} candidats : {}{copies})",
                            ks.len(),
                            titres.join(" ; ")
                        ));
                        continue;
                    }
                    Decision::Introuvable => {
                        classer(&mut r, ia, ja, "inconnu", Vec::new());
                        r.albums_inconnus.push(nom_roon);
                        continue;
                    }
                }
            };
            r.albums_apparies += 1;

            for (album, paires) in trouves {
                let Some(album_id) = album.id else {
                    continue;
                };
                r.albums_tune_apparies += 1;

                // Pochette : seulement s'il n'en a pas — sur chaque disque
                // d'un coffret.
                if porte(&al.image) && album.cover_path.as_deref().is_none_or(str::is_empty) {
                    r.images_albums_a_poser += 1;
                    if !apercu
                        && let Some(hash) = octets_de(&al.image).and_then(|d| ranger(&d, images))
                        // `force` : une chaîne vide n'est pas remplacée par
                        // COALESCE ; on vient de vérifier qu'il n'y a rien.
                        && albums
                            .force_update_cover_path(
                                album_id,
                                &hash,
                                crate::db::models::SourcePochette::Importee,
                            )
                            .is_ok()
                    {
                        r.images_albums_posees += 1;
                    }
                }

                for (i, locale) in paires {
                    let p = &al.pistes[i];
                    r.pistes_appariees += 1;
                    let Some(ligne) = p.credits.as_deref().filter(|c| !c.trim().is_empty()) else {
                        continue;
                    };
                    let mut interpretes: Vec<&str> = vec![ar.nom.as_str()];
                    interpretes.extend(noms_tune.iter().map(String::as_str));
                    interpretes.extend(album.artist_name.as_deref());
                    interpretes.extend(locale.artiste.as_deref());
                    let noms = credits_a_ecrire(ligne, &interpretes);
                    if noms.is_empty() {
                        continue;
                    }
                    if a_des_credits(backend, locale.id) {
                        r.credits_deja_presents += 1;
                        continue;
                    }
                    r.credits_a_ecrire += 1;
                    if apercu {
                        continue;
                    }
                    let ecrits = ecrire(backend, &artistes, locale.id, &noms);
                    if ecrits > 0 {
                        r.credits_ecrits += 1;
                        let _ = meta.set(locale.id, CLE_PROVENANCE, PROVENANCE_ROON);
                    }
                }
            }
        }
    }
    r
}

fn classer(r: &mut Rapport, i: usize, j: usize, classe: &'static str, ids_tune: Vec<i64>) {
    r.classement.push(Classement {
        i,
        j,
        classe,
        ids_tune,
    });
}

/// Signale un doublon, une fois : un album Roon présent quatre fois ne
/// répète pas la ligne.
fn doublon(r: &mut Rapport, ligne: String) {
    if !r.doublons.contains(&ligne) {
        r.doublons.push(ligne);
    }
}

/// Ajoute des albums lus à une liste, sans y mettre deux fois le même.
fn ajouter_sans_double(liste: &mut Vec<Album>, lus: Result<Vec<Album>, crate::error::TuneError>) {
    for a in lus.unwrap_or_default() {
        if a.id.is_none() || !liste.iter().any(|b| b.id == a.id) {
            liste.push(a);
        }
    }
}

/// La bibliothèque visible (masqués exclus, comme `list_by_artist`), par clé
/// de candidat ([`cle_d_album`]) et par seconde clé ([`cle_courte_d_album`]).
/// Lue une fois, au premier album qui en a besoin.
struct Bibliotheque {
    par_cle: HashMap<String, Vec<Album>>,
    par_cle_courte: HashMap<String, Vec<Album>>,
}

impl Bibliotheque {
    fn lire(albums: &AlbumRepo) -> Self {
        let mut b = Bibliotheque {
            par_cle: HashMap::new(),
            par_cle_courte: HashMap::new(),
        };
        for a in albums
            .list_filtered(1_000_000, 0, "title", "asc", None, None, None, false, None)
            .unwrap_or_default()
        {
            b.par_cle_courte
                .entry(cle_courte_d_album(&a.title))
                .or_default()
                .push(a.clone());
            b.par_cle
                .entry(cle_d_album(&a.title).0)
                .or_default()
                .push(a);
        }
        b
    }
}

/// Range des octets d'image dans le cache ; le condensat, ou `None` si le
/// format n'est pas une image que le serveur sait resservir.
fn ranger(data: &[u8], images: Option<&ImagesRoon<'_>>) -> Option<String> {
    let dossier = images?.dossier_cache;
    let ext = crate::library::artwork::sniff_image_ext(data)?;
    crate::library::artwork::cache_fetched_image(data, dossier, ext)
}

fn a_des_credits(backend: &Arc<dyn DbBackend>, track_id: i64) -> bool {
    // #4984 — l'entier : `track_id` est BIGINT sur PostgreSQL.
    backend
        .query_one(
            "SELECT 1 FROM track_credits WHERE track_id = ? LIMIT 1",
            &[&track_id as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .is_some()
}

/// Même écriture que `credits::ecrire_credits` — identifiants en CHAÎNE pour
/// le miroir PostgreSQL, fiche artiste LIÉE si elle existe, jamais créée.
fn ecrire(
    backend: &Arc<dyn DbBackend>,
    artistes: &ArtistRepo,
    track_id: i64,
    noms: &[String],
) -> usize {
    let id = track_id.to_string();
    let mut n = 0;
    for (pos, nom) in noms.iter().enumerate() {
        let artist_id: Option<String> = artistes
            .get_by_name(nom)
            .ok()
            .flatten()
            .and_then(|a| a.id)
            .map(|i| i.to_string());
        let pos = pos as i32;
        if backend
            .execute(
                "INSERT INTO track_credits (track_id, artist_id, artist_name, role, instrument, position) \
                 VALUES (?, ?, ?, ?, NULL, ?)",
                &[
                    &id as &dyn ToSqlValue,
                    &artist_id as &dyn ToSqlValue,
                    nom as &dyn ToSqlValue,
                    &ROLE as &dyn ToSqlValue,
                    &pos as &dyn ToSqlValue,
                ],
            )
            .is_ok()
        {
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::SqliteDb;

    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    /// Deux artistes, un album, deux pistes.
    fn bibliotheque(b: &Arc<dyn DbBackend>) -> (i64, i64) {
        b.execute(
            "INSERT INTO artists (id, name) VALUES (1, '16 Horsepower'), (2, 'Hank Williams')",
            &[],
        )
        .unwrap();
        b.execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Folklore', 1)",
            &[],
        )
        .unwrap();
        b.execute(
            "INSERT INTO tracks (id, title, album_id, artist_id, track_number, file_path, source) VALUES \
             (10, 'Hutterite Mile', 1, 1, 1, '/m/1.flac', 'local'), \
             (11, 'Alone and Forsaken', 1, 1, 4, '/m/4.flac', 'local')",
            &[],
        )
        .unwrap();
        (10, 11)
    }

    const EXPORT: &str = r#"{"source":"roon","core":"x","artistes":[
      {"nom":"16 horsepower","image":"ce1d","albums":[{"titre":"FOLKLORE","image":"5c46","pistes":[
        {"titre":"1. Hutterite Mile","credits":"16 Horsepower, David Eugene Edwards"},
        {"titre":"4. Alone and Forsaken","credits":"16 Horsepower, Hank Williams"},
        {"titre":"7. Inconnue","credits":"16 Horsepower, X"}]}]},
      {"nom":"Nick Drake","albums":[{"titre":"Pink Moon","pistes":[]}]}
    ],"absent_de_l_api":["biographies"]}"#;

    fn credits_de(b: &Arc<dyn DbBackend>, id: i64) -> Vec<(String, String, Option<i64>)> {
        b.query_many(
            "SELECT artist_name, role, artist_id FROM track_credits WHERE track_id = ? ORDER BY position",
            &[&id.to_string() as &dyn ToSqlValue],
        )
        .unwrap()
        .iter()
        .map(|r| {
            (
                r.first().and_then(|v| v.as_string()).unwrap_or_default(),
                r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                r.get(2).and_then(|v| v.as_i64()),
            )
        })
        .collect()
    }

    /// L'aperçu compte tout et n'écrit rien ; l'import écrit ce que l'aperçu
    /// a compté, marque la provenance, lie la fiche artiste quand elle existe.
    #[test]
    fn apercu_puis_import_sur_l_export_de_fabien() {
        let b = base();
        let (p1, p4) = bibliotheque(&b);
        let export = ExportRoon::lire(EXPORT).unwrap();

        let r = appliquer(&b, &export, true, None);
        assert_eq!((r.artistes_total, r.artistes_apparies), (2, 1), "{r:?}");
        assert_eq!(r.artistes_inconnus, vec!["Nick Drake"]);
        assert_eq!((r.albums_total, r.albums_apparies), (2, 1));
        assert_eq!(r.albums_apparies_strict, 1);
        assert_eq!(
            r.albums_par_strict,
            vec!["16 horsepower — FOLKLORE → [1] Folklore"]
        );
        // L'album d'un artiste inconnu est COMPTÉ, pas passé sous silence.
        assert_eq!(r.albums_inconnus, vec!["Nick Drake — Pink Moon"]);
        // La place de chaque album dans l'export, pour comparer deux
        // appariements ligne à ligne.
        let place = |c: &Classement| (c.i, c.j, c.classe, c.ids_tune.clone());
        assert_eq!(
            r.classement.iter().map(place).collect::<Vec<_>>(),
            vec![(0, 0, "strict", vec![1]), (1, 0, "inconnu", vec![])]
        );
        assert_eq!((r.pistes_total, r.pistes_appariees), (3, 2));
        assert_eq!(
            (r.credits_a_ecrire, r.credits_ecrits),
            (2, 0),
            "aperçu : rien d'écrit"
        );
        assert_eq!(
            (r.images_nommees, r.images_portees),
            (2, 0),
            "l'export nomme, ne porte pas"
        );
        assert!(credits_de(&b, p1).is_empty());

        let r = appliquer(&b, &export, false, None);
        assert_eq!(r.credits_ecrits, 2, "{r:?}");
        assert_eq!(
            credits_de(&b, p1),
            vec![(
                "David Eugene Edwards".to_string(),
                "composer".to_string(),
                None
            )]
        );
        assert_eq!(
            credits_de(&b, p4),
            vec![("Hank Williams".to_string(), "composer".to_string(), Some(2))]
        );
        let m = TrackMetadataRepo::with_backend(b.clone());
        assert_eq!(
            m.get_all(p1)
                .unwrap()
                .get(CLE_PROVENANCE)
                .map(String::as_str),
            Some("roon")
        );

        let r = appliquer(&b, &export, false, None);
        assert_eq!((r.credits_deja_presents, r.credits_ecrits), (2, 0), "{r:?}");
        assert_eq!(credits_de(&b, p1).len(), 1);
    }

    fn jpeg(octet: u8) -> Vec<u8> {
        vec![0xFF, 0xD8, 0xFF, 0xE0, octet, octet, octet]
    }

    /// Les images ne se posent que là où Tune n'en a pas, et jamais en aperçu.
    #[test]
    fn les_images_ne_remplacent_jamais_celles_de_tune() {
        let b = base();
        bibliotheque(&b);
        // Un deuxième album, qui a DÉJÀ sa pochette.
        b.execute("INSERT INTO albums (id, title, artist_id, cover_path) VALUES (2, 'Low Estate', 1, 'deja')", &[])
            .unwrap();
        let export = ExportRoon::lire(
            r#"{"source":"roon","artistes":[{"nom":"16 Horsepower","image":"ar1","albums":[
              {"titre":"Folklore","image":"al1","pistes":[]},
              {"titre":"Low Estate","image":"al2","pistes":[]}]}]}"#,
        )
        .unwrap();
        let octets: HashMap<String, Vec<u8>> =
            [("ar1", jpeg(1)), ("al1", jpeg(2)), ("al2", jpeg(3))]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect();
        let cache = crate::test_scratch::scratch_dir("pont_roon_images");
        let images = ImagesRoon {
            octets: &octets,
            dossier_cache: cache.path(),
        };

        let r = appliquer(&b, &export, true, Some(&images));
        assert_eq!((r.images_nommees, r.images_portees), (3, 3), "{r:?}");
        assert_eq!(
            (r.images_artistes_a_poser, r.images_artistes_posees),
            (1, 0)
        );
        assert_eq!(
            (r.images_albums_a_poser, r.images_albums_posees),
            (1, 0),
            "Low Estate a déjà la sienne"
        );
        let artistes = ArtistRepo::with_backend(b.clone());
        assert_eq!(
            artistes.get(1).unwrap().unwrap().image_path,
            None,
            "aperçu : rien d'écrit"
        );

        let r = appliquer(&b, &export, false, Some(&images));
        assert_eq!(
            (r.images_artistes_posees, r.images_albums_posees),
            (1, 1),
            "{r:?}"
        );
        let a = artistes.get(1).unwrap().unwrap();
        let hash = a.image_path.clone().expect("image d'artiste posée");
        assert_eq!(a.image_source.as_deref(), Some("roon"));
        assert!(
            crate::library::artwork::find_cached(cache.path(), &hash).is_some(),
            "octets dans le cache"
        );
        let albums = AlbumRepo::with_backend(b.clone());
        assert!(
            albums
                .get(1)
                .unwrap()
                .unwrap()
                .cover_path
                .is_some_and(|c| !c.is_empty())
        );
        assert_eq!(
            albums.get(2).unwrap().unwrap().cover_path.as_deref(),
            Some("deja"),
            "jamais remplacée"
        );

        // Second passage : plus rien à poser.
        let r = appliquer(&b, &export, false, Some(&images));
        assert_eq!(
            (r.images_artistes_a_poser, r.images_albums_a_poser),
            (0, 0),
            "{r:?}"
        );
    }

    #[test]
    fn l_archive_du_moissonneur_se_lit_et_refuse_les_noms_douteux() {
        use std::io::Write;
        let mut tampon = std::io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut tampon);
            let o = zip::write::SimpleFileOptions::default();
            z.start_file("export.json", o).unwrap();
            z.write_all(br#"{"source":"roon","artistes":[]}"#).unwrap();
            z.start_file("images/abc123.jpg", o).unwrap();
            z.write_all(&jpeg(9)).unwrap();
            z.start_file("images/../evil.jpg", o).unwrap();
            z.write_all(b"x").unwrap();
            z.finish().unwrap();
        }
        let (export, images) = ouvrir_archive(tampon).unwrap();
        assert_eq!(export.source, "roon");
        assert_eq!(images.cles().collect::<Vec<_>>(), vec!["abc123"]);
        // Indexée à l'ouverture, extraite seulement à la demande.
        assert_eq!(images.lire("abc123"), Some(jpeg(9)));
        assert_eq!(images.lire("evil"), None);

        let mut sans = std::io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut sans);
            z.start_file("images/a.jpg", zip::write::SimpleFileOptions::default())
                .unwrap();
            z.write_all(&jpeg(1)).unwrap();
            z.finish().unwrap();
        }
        assert!(ouvrir_archive(sans).err().unwrap().contains("export.json"));
        assert!(ouvrir_archive(std::io::Cursor::new(b"pas un zip")).is_err());
    }

    // ── Niveau 2 (#5749, fil 2140) : fixtures synthétiques, un cas chacune ──

    fn artiste(b: &Arc<dyn DbBackend>, id: i64, nom: &str) {
        b.execute(
            "INSERT INTO artists (id, name) VALUES (?, ?)",
            &[&id as &dyn ToSqlValue, &nom as &dyn ToSqlValue],
        )
        .unwrap();
    }

    fn album(b: &Arc<dyn DbBackend>, id: i64, titre: &str, artiste: i64, compilation: bool) {
        let c = i64::from(compilation);
        b.execute(
            "INSERT INTO albums (id, title, artist_id, is_compilation) VALUES (?, ?, ?, ?)",
            &[
                &id as &dyn ToSqlValue,
                &titre as &dyn ToSqlValue,
                &artiste as &dyn ToSqlValue,
                &c as &dyn ToSqlValue,
            ],
        )
        .unwrap();
    }

    /// Les pistes d'un album : (artiste, disque, numéro, titre) ; l'id de
    /// piste vaut `album * 100 + rang`.
    fn pistes_de(b: &Arc<dyn DbBackend>, album: i64, pistes: &[(i64, i32, i32, &str)]) {
        for (rang, (artiste, disque, numero, titre)) in pistes.iter().enumerate() {
            let id = album * 100 + rang as i64;
            let chemin = format!("/m/{id}.flac");
            b.execute(
                "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, file_path, source) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, 'local')",
                &[
                    &id as &dyn ToSqlValue,
                    titre as &dyn ToSqlValue,
                    &album as &dyn ToSqlValue,
                    artiste as &dyn ToSqlValue,
                    disque as &dyn ToSqlValue,
                    numero as &dyn ToSqlValue,
                    &chemin as &dyn ToSqlValue,
                ],
            )
            .unwrap();
        }
    }

    /// Un export d'un artiste et d'un album, chaque piste créditée d'un
    /// auteur en plus de l'interprète.
    fn export_un_album(artiste: &str, titre: &str, pistes: &[&str]) -> ExportRoon {
        let pistes: Vec<serde_json::Value> = pistes
            .iter()
            .map(|t| serde_json::json!({"titre": t, "credits": format!("{artiste}, Auteur X")}))
            .collect();
        let v = serde_json::json!({"source": "roon", "artistes": [
            {"nom": artiste, "albums": [{"titre": titre, "pistes": pistes}]}
        ]});
        ExportRoon::lire(&v.to_string()).unwrap()
    }

    fn total_classe(r: &Rapport) -> usize {
        r.albums_apparies_strict
            + r.albums_apparies_contenu
            + r.albums_ambigus.len()
            + r.albums_inconnus.len()
    }

    /// Le cas phare : « Dresden (Live-2007) » chez Roon, « … (CD 1/2) » chez
    /// Tune, mêmes pistes. L'égalité stricte échoue, le contenu apparie.
    #[test]
    fn le_suffixe_cd_1_2_s_apparie_par_le_contenu() {
        let b = base();
        artiste(&b, 1, "Jan Garbarek Group");
        album(&b, 1, "Dresden (Live-2007) (CD 1/2)", 1, false);
        pistes_de(
            &b,
            1,
            &[
                (1, 1, 1, "Paper Nut"),
                (1, 1, 2, "The Tall Tear Trees"),
                (1, 1, 3, "Heitor"),
            ],
        );
        let e = export_un_album(
            "Jan Garbarek Group",
            "Dresden (Live-2007)",
            &["1. Paper Nut", "2. The Tall Tear Trees", "3. Heitor"],
        );

        let r = appliquer(&b, &e, true, None);
        assert_eq!(
            (
                r.albums_apparies,
                r.albums_apparies_strict,
                r.albums_apparies_contenu
            ),
            (1, 0, 1),
            "{r:?}"
        );
        assert_eq!(
            r.albums_par_contenu,
            vec!["Jan Garbarek Group — Dresden (Live-2007) → [1] Dresden (Live-2007) (CD 1/2)"]
        );
        assert_eq!(
            (r.pistes_appariees, r.credits_a_ecrire, r.credits_ecrits),
            (3, 3, 0)
        );
        assert!(credits_de(&b, 100).is_empty(), "aperçu : rien d'écrit");
        assert_eq!(total_classe(&r), r.albums_total);

        let r = appliquer(&b, &e, false, None);
        assert_eq!(r.credits_ecrits, 3, "{r:?}");
        assert_eq!(credits_de(&b, 100)[0].0, "Auteur X");
    }

    /// « Billie Holiday » chez Roon, « Billie Holliday » chez Tune : l'artiste
    /// reste inconnu, mais l'album se retrouve par son titre et ses pistes.
    #[test]
    fn une_faute_d_artiste_ne_fait_plus_tomber_ses_albums() {
        let b = base();
        artiste(&b, 1, "Billie Holliday");
        album(&b, 1, "Lady in Satin", 1, false);
        pistes_de(
            &b,
            1,
            &[
                (1, 0, 1, "I'm a Fool to Want You"),
                (1, 0, 2, "For Heaven's Sake"),
            ],
        );
        let e = export_un_album(
            "Billie Holiday",
            "Lady in Satin",
            &["1. I'm a Fool to Want You", "2. For Heaven's Sake"],
        );
        let r = appliquer(&b, &e, true, None);
        assert_eq!(r.artistes_inconnus, vec!["Billie Holiday"], "{r:?}");
        assert_eq!((r.albums_apparies_contenu, r.pistes_appariees), (1, 2));
        assert!(r.albums_inconnus.is_empty());
    }

    /// Une compilation rangée sous « Various Artists » où l'artiste Roon
    /// signe une piste : candidate par le prédicat #4767.
    #[test]
    fn une_compilation_d_un_autre_artiste_d_album_est_candidate() {
        let b = base();
        artiste(&b, 1, "Various Artists");
        artiste(&b, 2, "Nick Drake");
        artiste(&b, 3, "John Martyn");
        album(&b, 1, "Island Folk", 1, true);
        pistes_de(&b, 1, &[(2, 1, 1, "Pink Moon"), (3, 1, 2, "May You Never")]);
        let e = export_un_album(
            "Nick Drake",
            "Island Folk",
            &["1. Pink Moon", "2. May You Never"],
        );
        let r = appliquer(&b, &e, true, None);
        assert_eq!(r.artistes_apparies, 1, "{r:?}");
        assert_eq!(
            (
                r.albums_apparies_strict,
                r.albums_apparies_contenu,
                r.pistes_appariees
            ),
            (0, 1, 2)
        );
    }

    /// Un coffret que Tune range un album par disque : l'album Roon enrichit
    /// les deux.
    #[test]
    fn un_coffret_roon_enrichit_chaque_disque_de_tune() {
        let b = base();
        artiste(&b, 1, "Keith Jarrett");
        album(&b, 1, "Sun Bear Concerts (CD 1/2)", 1, false);
        album(&b, 2, "Sun Bear Concerts (CD 2/2)", 1, false);
        pistes_de(
            &b,
            1,
            &[(1, 1, 1, "Kyoto Part 1"), (1, 1, 2, "Kyoto Part 2")],
        );
        pistes_de(
            &b,
            2,
            &[(1, 1, 1, "Osaka Part 1"), (1, 1, 2, "Osaka Part 2")],
        );
        let e = export_un_album(
            "Keith Jarrett",
            "Sun Bear Concerts",
            &[
                "1-1 Kyoto Part 1",
                "1-2 Kyoto Part 2",
                "2-1 Osaka Part 1",
                "2-2 Osaka Part 2",
            ],
        );
        let r = appliquer(&b, &e, false, None);
        assert_eq!(
            (
                r.albums_apparies,
                r.albums_apparies_contenu,
                r.albums_tune_apparies
            ),
            (1, 1, 2),
            "{r:?}"
        );
        assert_eq!((r.pistes_appariees, r.credits_ecrits), (4, 4));
        assert_eq!(credits_de(&b, 201).len(), 1, "le disque 2 aussi");
    }

    /// Deux albums Tune portent les mêmes pistes sous des titres de même clé :
    /// rien n'est écrit, l'album est classé « ambigu ».
    #[test]
    fn deux_candidats_egaux_rendent_l_album_ambigu_et_rien_n_est_ecrit() {
        let b = base();
        artiste(&b, 1, "Arthur H");
        album(&b, 1, "Mystic Rumba", 1, false);
        album(&b, 2, "Mystic Rumba !", 1, false);
        let p = [(1, 0, 1, "Mystic Rumba"), (1, 0, 2, "Lily Dale")];
        pistes_de(&b, 1, &p);
        pistes_de(&b, 2, &p);
        let e = export_un_album(
            "Arthur H",
            "Mystic Rumba.",
            &["1. Mystic Rumba", "2. Lily Dale"],
        );
        let r = appliquer(&b, &e, false, None);
        assert_eq!(r.albums_apparies, 0, "{r:?}");
        assert_eq!(
            r.albums_ambigus,
            vec![
                "Arthur H — Mystic Rumba. (2 candidats : [1] Mystic Rumba ; [2] Mystic Rumba ! ; \
                 2 exemplaires identiques)"
            ]
        );
        assert_eq!((r.pistes_appariees, r.credits_ecrits), (0, 0));
        assert!(credits_de(&b, 100).is_empty() && credits_de(&b, 200).is_empty());
        assert_eq!(total_classe(&r), r.albums_total);
    }

    /// Deux fiches d'artiste de même nom replié, et deux albums de même titre
    /// sous elles : les doublons sont DITS, et le contenu choisit au lieu de
    /// la première ligne.
    #[test]
    fn les_doublons_sont_signales_et_le_contenu_tranche() {
        let b = base();
        artiste(&b, 1, "The Beatles");
        artiste(&b, 2, "Beatles");
        album(&b, 1, "Rubber Soul", 1, false);
        album(&b, 2, "Rubber Soul", 2, false);
        pistes_de(&b, 1, &[(1, 1, 1, "Autre A"), (1, 1, 2, "Autre B")]);
        pistes_de(
            &b,
            2,
            &[(2, 1, 1, "Drive My Car"), (2, 1, 2, "Norwegian Wood")],
        );
        let e = export_un_album(
            "The Beatles",
            "Rubber Soul",
            &["1. Drive My Car", "2. Norwegian Wood"],
        );
        let r = appliquer(&b, &e, false, None);
        assert_eq!(
            r.doublons,
            vec![
                "artiste « The Beatles » : 2 fiches Tune (id 1, 2)",
                "album « The Beatles — Rubber Soul » : 2 albums Tune de même titre (id 1, 2)",
            ],
            "{r:?}"
        );
        assert_eq!(
            (r.albums_apparies_strict, r.albums_apparies_contenu),
            (0, 1)
        );
        assert_eq!(
            r.albums_par_contenu,
            vec!["The Beatles — Rubber Soul → [2] Rubber Soul"]
        );
        assert!(
            credits_de(&b, 100).is_empty(),
            "la première ligne n'a rien reçu"
        );
        assert_eq!(credits_de(&b, 200).len(), 1);
    }

    /// « Unknown Album », pistes « Track 01… » : la recherche globale trouve
    /// un album de même clé, aux mêmes numéros, mais des titres génériques
    /// ne prouvent rien. Il ne doit PAS s'apparier.
    #[test]
    fn un_album_aux_titres_generiques_ne_s_apparie_pas() {
        let b = base();
        artiste(&b, 1, "Divers");
        album(&b, 1, "Unknown Album", 1, false);
        pistes_de(
            &b,
            1,
            &[
                (1, 1, 1, "Track 01"),
                (1, 1, 2, "Track 02"),
                (1, 1, 3, "Track 03"),
            ],
        );
        let e = export_un_album(
            "Unknown Artist",
            "Unknown Album",
            &["1. Track 01", "2. Track 02", "3. Track 03"],
        );
        let r = appliquer(&b, &e, false, None);
        assert_eq!(
            (r.albums_apparies, r.pistes_appariees, r.credits_ecrits),
            (0, 0, 0),
            "{r:?}"
        );
        assert_eq!(r.albums_inconnus, vec!["Unknown Artist — Unknown Album"]);
        assert!(credits_de(&b, 100).is_empty());
    }

    /// « Unknown Artist — Unknown Album » des deux côtés, pistes « Track
    /// 01… » : avant, le niveau strict l'appariait et les crédits s'écrivaient
    /// par numéro. Un nom générique ne prouve rien : sans preuve par le
    /// contenu, rien n'est écrit.
    #[test]
    fn un_nom_generique_ne_s_apparie_jamais_en_strict() {
        let b = base();
        artiste(&b, 1, "Unknown Artist");
        album(&b, 1, "Unknown Album", 1, false);
        pistes_de(
            &b,
            1,
            &[
                (1, 1, 1, "Track 01"),
                (1, 1, 2, "Track 02"),
                (1, 1, 3, "Track 03"),
            ],
        );
        let e = export_un_album(
            "Unknown Artist",
            "Unknown Album",
            &["1. Track 01", "2. Track 02", "3. Track 03"],
        );
        let r = appliquer(&b, &e, false, None);
        assert_eq!(
            (r.albums_apparies_strict, r.albums_apparies),
            (0, 0),
            "{r:?}"
        );
        assert_eq!((r.pistes_appariees, r.credits_ecrits), (0, 0));
        assert_eq!(r.artistes_apparies, 0);
        assert!(credits_de(&b, 100).is_empty(), "aucun crédit sans preuve");
        assert_eq!(r.albums_inconnus, vec!["Unknown Artist — Unknown Album"]);

        // Les mêmes noms avec de VRAIES pistes : le contenu apparie.
        let b = base();
        artiste(&b, 1, "Unknown Artist");
        album(&b, 1, "Unknown Album", 1, false);
        pistes_de(&b, 1, &[(1, 1, 1, "Paper Nut"), (1, 1, 2, "Heitor")]);
        let e = export_un_album(
            "Unknown Artist",
            "Unknown Album",
            &["1. Paper Nut", "2. Heitor"],
        );
        let r = appliquer(&b, &e, false, None);
        assert_eq!(
            (r.albums_apparies_strict, r.albums_apparies_contenu),
            (0, 1),
            "{r:?}"
        );
        assert_eq!(r.credits_ecrits, 2);
    }

    /// La seconde clé, sans crochets : « Black Orpheus [Original Soundtrack] »
    /// chez Roon trouve « Black Orpheus » chez Tune, et les pistes décident.
    #[test]
    fn la_seconde_cle_sans_crochets_trouve_black_orpheus() {
        let b = base();
        artiste(&b, 1, "Antônio Carlos Jobim");
        album(&b, 1, "Black Orpheus", 1, false);
        pistes_de(
            &b,
            1,
            &[
                (1, 1, 1, "A Felicidade"),
                (1, 1, 2, "Frevo"),
                (1, 1, 3, "O Nosso Amor"),
            ],
        );
        let e = export_un_album(
            "Antônio Carlos Jobim",
            "Black Orpheus [Original Soundtrack]",
            &["1. A Felicidade", "2. Frevo", "3. O Nosso Amor"],
        );
        let r = appliquer(&b, &e, true, None);
        assert_eq!(
            r.albums_par_contenu,
            vec!["Antônio Carlos Jobim — Black Orpheus [Original Soundtrack] → [1] Black Orpheus"],
            "{r:?}"
        );
    }

    /// Trois copies du même album sous le même titre : un doublon, et un
    /// album ambigu qui le dit.
    #[test]
    fn des_copies_identiques_sont_ambigues_et_comptees() {
        let b = base();
        artiste(&b, 1, "Arthur H");
        let p = [(1, 1, 1, "Lor Deros"), (1, 1, 2, "Le Paradis")];
        for id in 1..=3 {
            album(&b, id, "Lor Deros", 1, false);
            pistes_de(&b, id, &p);
        }
        let e = export_un_album("Arthur H", "Lor Deros", &["1. Lor Deros", "2. Le Paradis"]);
        let r = appliquer(&b, &e, false, None);
        assert_eq!(
            r.doublons,
            vec!["album « Arthur H — Lor Deros » : 3 albums Tune de même titre (id 1, 2, 3)"],
            "{r:?}"
        );
        assert_eq!(
            r.albums_ambigus,
            vec![
                "Arthur H — Lor Deros (3 candidats : [1] Lor Deros ; [2] Lor Deros ; [3] Lor Deros ; \
                 3 exemplaires identiques)"
            ]
        );
        assert_eq!(r.credits_ecrits, 0);
        assert_eq!(r.classement[0].ids_tune, vec![1, 2, 3]);
        assert_eq!(r.classement[0].classe, "ambigu");
    }

    #[test]
    fn la_porte_ne_prend_que_l_export_du_moissonneur() {
        assert!(est_un_export_du_pont(EXPORT));
        assert!(!est_un_export_du_pont("Title,Artist\nA,B"));
        assert!(!est_un_export_du_pont(r#"{"data":[]}"#));
    }
}
