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
use crate::db::track_metadata_repo::TrackMetadataRepo;
use crate::db::track_repo::TrackRepo;
use crate::library::pont_roon::{
    ExportRoon, PisteLocale, Rapport, apparier_piste, credits_a_ecrire, index_par_titre, plier,
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
    let par_nom: HashMap<String, usize> = index_par_titre(&locaux, |(_, n)| n.as_str());
    // Compter ne lit rien ; les octets ne sont extraits qu'au moment de poser.
    let porte = |cle: &Option<String>| -> bool {
        cle.as_deref()
            .is_some_and(|c| images.is_some_and(|i| i.octets.contient(c)))
    };
    let octets_de =
        |cle: &Option<String>| -> Option<Vec<u8>> { images?.octets.lire(cle.as_deref()?) };

    let mut r = Rapport {
        artistes_total: export.artistes.len(),
        ..Default::default()
    };
    for ar in &export.artistes {
        r.albums_total += ar.albums.len();
        r.pistes_total += ar.albums.iter().map(|a| a.pistes.len()).sum::<usize>();
        if ar.image.is_some() {
            r.images_nommees += 1;
            r.images_portees += usize::from(porte(&ar.image));
        }
        let Some(&i) = par_nom.get(&plier(&ar.nom)) else {
            r.artistes_inconnus.push(ar.nom.clone());
            for al in &ar.albums {
                r.images_nommees += usize::from(al.image.is_some());
                r.images_portees += usize::from(porte(&al.image));
            }
            continue;
        };
        r.artistes_apparies += 1;
        let (artiste_id, artiste_nom) = &locaux[i];

        // Image d'artiste : seulement s'il n'en a pas.
        if porte(&ar.image) {
            let sans_image = artistes
                .get(*artiste_id)
                .ok()
                .flatten()
                .map(|a| a.image_path.as_deref().is_none_or(str::is_empty))
                .unwrap_or(false);
            if sans_image {
                r.images_artistes_a_poser += 1;
                if !apercu {
                    if let Some(hash) = octets_de(&ar.image).and_then(|d| ranger(&d, images)) {
                        if artistes
                            .update_image(*artiste_id, &hash, SOURCE_IMAGE_ROON)
                            .is_ok()
                        {
                            r.images_artistes_posees += 1;
                        }
                    }
                }
            }
        }

        let siens = albums.list_by_artist(*artiste_id).unwrap_or_default();
        let par_titre = index_par_titre(&siens, |a| a.title.as_str());
        for al in &ar.albums {
            r.images_nommees += usize::from(al.image.is_some());
            r.images_portees += usize::from(porte(&al.image));
            let Some(&j) = par_titre.get(&plier(&al.titre)) else {
                r.albums_inconnus.push(format!("{} — {}", ar.nom, al.titre));
                continue;
            };
            r.albums_apparies += 1;
            let Some(album_id) = siens[j].id else {
                continue;
            };

            // Pochette : seulement s'il n'en a pas.
            if porte(&al.image) {
                let pochette = siens[j].cover_path.as_deref();
                if pochette.is_none_or(str::is_empty) {
                    r.images_albums_a_poser += 1;
                    if !apercu {
                        if let Some(hash) = octets_de(&al.image).and_then(|d| ranger(&d, images)) {
                            // `force` : une chaîne vide n'est pas remplacée par
                            // COALESCE ; on vient de vérifier qu'il n'y a rien.
                            if albums.force_update_cover_path(album_id, &hash).is_ok() {
                                r.images_albums_posees += 1;
                            }
                        }
                    }
                }
            }

            let locales: Vec<PisteLocale> = pistes
                .list_by_album(album_id)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|t| {
                    let id = t.id?;
                    Some(PisteLocale {
                        id,
                        titre: t.title.clone(),
                        numero: (t.track_number > 0).then_some(t.track_number),
                        disque: (t.disc_number > 0).then_some(t.disc_number),
                        artiste: t.artist_name.clone(),
                        a_des_credits: a_des_credits(backend, id),
                    })
                })
                .collect();
            for p in &al.pistes {
                let Some(locale) = apparier_piste(p, &locales) else {
                    continue;
                };
                r.pistes_appariees += 1;
                let Some(ligne) = p.credits.as_deref().filter(|c| !c.trim().is_empty()) else {
                    continue;
                };
                let mut interpretes: Vec<&str> = vec![artiste_nom.as_str(), ar.nom.as_str()];
                if let Some(a) = locale.artiste.as_deref() {
                    interpretes.push(a);
                }
                let noms = credits_a_ecrire(ligne, &interpretes);
                if noms.is_empty() {
                    continue;
                }
                if locale.a_des_credits {
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
    r
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

    #[test]
    fn la_porte_ne_prend_que_l_export_du_moissonneur() {
        assert!(est_un_export_du_pont(EXPORT));
        assert!(!est_un_export_du_pont("Title,Artist\nA,B"));
        assert!(!est_un_export_du_pont(r#"{"data":[]}"#));
    }
}
