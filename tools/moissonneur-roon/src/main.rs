//! Moissonneur Roon → Tune.
//!
//! Parcourt un Core Roon et écrit un export au format de Tune. **Lecture
//! seule** : aucune lecture n'est lancée, aucun réglage modifié.
//!
//! Ce qu'il récolte, et pourquoi — mesuré en phase 0 du chantier :
//!
//! 1. **la consolidation** : Roon voit 597 artistes là où Tune en voit ~1 183
//!    pour la même bibliothèque. La jointure se fera sur les ALBUMS, jamais sur
//!    les noms — ce sont eux qui divergent ;
//! 2. **les crédits par piste** : le sous-titre d'une piste porte compositeurs
//!    et auteurs (18 pistes sur 21 observées) ;
//! 3. **les images** : 99 artistes sur 100 en ont une.
//!
//! Avec `--archive=export-roon.zip`, il **télécharge aussi les octets** des
//! images (`GET /api/image/<clé>` du Core, JPEG 1 200 px) et range tout dans
//! une seule archive : `export.json` + `images/<clé>.jpg`. C'est ce fichier
//! que l'extension « Pont Roon » de Tune importe. Les clés d'image de Roon ne
//! servent qu'au Core qui les a émises : sans les octets, un export ne porte
//! aucune image utilisable ailleurs.
//!
//! Ce qu'il ne récolte PAS, faute d'exister dans l'API : biographies,
//! artistes similaires, suggestions de découverte, identifiants externes.
//!
//! ⚠️ `browse` est un arbre à curseur : chaque descente déplace la position de
//! la session, et il faut remonter d'autant. `hierarchy` doit être passé à
//! CHAQUE appel, y compris les descentes.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use roon_api::{BrowseItem, BrowseOptions, Core, ImageOptions, LoadOptions, RoonClientBuilder};
use serde::Serialize;

const SESSION: &str = "tune-moissonneur";
const HIER_ARTISTES: &str = "artists";
const PAGE: u32 = 100;

// ---------------------------------------------------------------------------
// Le format d'export : le NÔTRE, pas celui de Roon.
// ---------------------------------------------------------------------------

#[derive(Serialize, Default)]
struct Export {
    source: String,
    releve: String,
    core: String,
    artistes: Vec<Artiste>,
    /// Ce que le moissonneur n'a PAS pu obtenir, nommé explicitement pour que
    /// personne ne croie à un oubli.
    absent_de_l_api: Vec<String>,
    /// Images : clés demandées, octets reçus, échecs — `None` sans `--archive`.
    images: Option<BilanImages>,
}

#[derive(Serialize, Default)]
struct BilanImages {
    cles: usize,
    recues: usize,
    echecs: usize,
    octets: u64,
}

#[derive(Serialize)]
struct Artiste {
    nom: String,
    image: Option<String>,
    albums: Vec<Album>,
}

#[derive(Serialize)]
struct Album {
    titre: String,
    /// Tel que Roon l'affiche sous le titre — souvent l'artiste d'album.
    sous_titre: Option<String>,
    image: Option<String>,
    pistes: Vec<Piste>,
}

#[derive(Serialize)]
struct Piste {
    /// « 1. Hutterite Mile » tel que rendu, numéro compris.
    titre: String,
    /// Le gisement : « 16 Horsepower, Ian Curtis, Bernard Sumner… »
    credits: Option<String>,
}

// ---------------------------------------------------------------------------
// Parcours
// ---------------------------------------------------------------------------

/// Un item de liste qui porte une action (`Play Artist`, `Play Album`, `Shuffle`)
/// n'est pas une donnée : c'est un bouton. On ne descend jamais dedans.
fn est_une_action(it: &BrowseItem) -> bool {
    matches!(it.hint.as_deref(), Some("action") | Some("action_list"))
        && !it.title.chars().next().is_some_and(|c| c.is_ascii_digit())
}

/// Une piste se reconnaît à son numéro de tête : « 1. Hutterite Mile ».
fn est_une_piste(it: &BrowseItem) -> bool {
    it.title.chars().next().is_some_and(|c| c.is_ascii_digit())
}

async fn charger(core: &Core, offset: u32, count: u32) -> Result<Vec<BrowseItem>> {
    let r = core
        .browse()
        .load(LoadOptions {
            multi_session_key: Some(SESSION.to_string()),
            hierarchy: Some(HIER_ARTISTES.to_string()),
            offset: Some(offset),
            count: Some(count),
            ..Default::default()
        })
        .await
        .context("load")?;
    Ok(r.items)
}

/// Pose le curseur à la racine de la hiérarchie des artistes.
async fn racine(core: &Core) -> Result<()> {
    core.browse()
        .browse(BrowseOptions {
            hierarchy: Some(HIER_ARTISTES.to_string()),
            multi_session_key: Some(SESSION.to_string()),
            pop_all: Some(true),
            ..Default::default()
        })
        .await
        .context("retour à la racine")?;
    Ok(())
}

/// Descend d'un cran dans `item_key`.
async fn descendre(core: &Core, item_key: &str) -> Result<()> {
    core.browse()
        .browse(BrowseOptions {
            hierarchy: Some(HIER_ARTISTES.to_string()),
            multi_session_key: Some(SESSION.to_string()),
            item_key: Some(item_key.to_string()),
            ..Default::default()
        })
        .await
        .context("descente")?;
    Ok(())
}

/// Remonte de `n` crans. Sans ça, la descente suivante part du mauvais endroit.
async fn remonter(core: &Core, n: u32) -> Result<()> {
    core.browse()
        .browse(BrowseOptions {
            hierarchy: Some(HIER_ARTISTES.to_string()),
            multi_session_key: Some(SESSION.to_string()),
            pop_levels: Some(n),
            ..Default::default()
        })
        .await
        .context("remontée")?;
    Ok(())
}

async fn pistes_de_l_album(core: &Core, cle: &str) -> Result<Vec<Piste>> {
    descendre(core, cle).await?;
    let items = charger(core, 0, 500).await?;
    remonter(core, 1).await?;
    Ok(items
        .into_iter()
        .filter(est_une_piste)
        .map(|it| Piste {
            titre: it.title,
            credits: it.subtitle,
        })
        .collect())
}

async fn albums_de_l_artiste(core: &Core, cle: &str, avec_pistes: bool) -> Result<Vec<Album>> {
    descendre(core, cle).await?;
    let items = charger(core, 0, 500).await?;

    let mut albums = Vec::new();
    for it in items {
        if est_une_action(&it) {
            continue;
        }
        let Some(k) = it.item_key.clone() else { continue };
        let pistes = if avec_pistes {
            pistes_de_l_album(core, &k).await.unwrap_or_default()
        } else {
            Vec::new()
        };
        albums.push(Album {
            titre: it.title,
            sous_titre: it.subtitle,
            image: it.image_key,
            pistes,
        });
    }
    remonter(core, 1).await?;
    Ok(albums)
}

// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let args: BTreeMap<String, String> = std::env::args()
        .skip(1)
        .filter_map(|a| a.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect();
    let hote = args.get("--hote").cloned().unwrap_or_default();
    let port: u16 = args
        .get("--port")
        .and_then(|p| p.parse().ok())
        .unwrap_or(9330);
    let sortie = PathBuf::from(
        args.get("--sortie")
            .cloned()
            .unwrap_or_else(|| "export-roon.json".into()),
    );
    let sans_pistes = std::env::args().any(|a| a == "--sans-pistes");
    let archive = args.get("--archive").map(PathBuf::from);

    if hote.is_empty() {
        eprintln!("usage : moissonneur-roon --hote=<ip> [--port=9330] [--sortie=f.json] [--archive=export-roon.zip] [--sans-pistes]");
        eprintln!("        (lecture seule : rien n'est joué, rien n'est modifié)");
        std::process::exit(2);
    }

    let client = RoonClientBuilder::new(
        "fr.mozaiklabs.tune.moissonneur",
        "Tune — moissonneur",
        env!("CARGO_PKG_VERSION"),
        "Mozaik Labs",
        "contact@mozaiklabs.fr",
    )
    .require_browse()
    .build()?;

    eprintln!("connexion à {hote}:{port} — autorisez « Tune — moissonneur » dans Roon (Réglages → Extensions)");
    let core = client.connect(&hote, port).await.context("connexion")?;
    eprintln!("connecté.");

    let t0 = Instant::now();
    racine(&core).await?;
    let premiere = charger(&core, 0, PAGE).await?;
    let mut cles: Vec<(String, String, Option<String>)> = Vec::new();
    let pousser = |items: Vec<BrowseItem>, cles: &mut Vec<_>| {
        for it in items {
            if let Some(k) = it.item_key.clone() {
                cles.push((k, it.title, it.image_key));
            }
        }
    };
    let n0 = premiere.len();
    pousser(premiere, &mut cles);
    let mut offset = PAGE;
    if n0 as u32 == PAGE {
        loop {
            let page = charger(&core, offset, PAGE).await?;
            let n = page.len();
            pousser(page, &mut cles);
            eprintln!("  artistes : {} (offset {offset})", cles.len());
            if (n as u32) < PAGE {
                break;
            }
            offset += PAGE;
        }
    }
    eprintln!("{} artistes listés en {:?}", cles.len(), t0.elapsed());

    let mut export = Export {
        source: "roon".into(),
        releve: format!("{:?}", std::time::SystemTime::now()),
        core: format!("{hote}:{port}"),
        absent_de_l_api: vec![
            "biographies".into(),
            "artistes similaires".into(),
            "suggestions de decouverte".into(),
            "identifiants externes (MBID, UPC, ISRC)".into(),
        ],
        ..Default::default()
    };

    for (i, (cle, nom, image)) in cles.iter().enumerate() {
        let albums = albums_de_l_artiste(&core, cle, !sans_pistes)
            .await
            .unwrap_or_default();
        if i % 25 == 0 {
            eprintln!("  {}/{} — {nom} ({} albums)", i + 1, cles.len(), albums.len());
        }
        export.artistes.push(Artiste {
            nom: nom.clone(),
            image: image.clone(),
            albums,
        });
    }

    if let Some(archive) = &archive {
        let (bilan, fichiers) = telecharger_images(&core, &export).await;
        eprintln!(
            "images : {} reçues sur {} clés ({} échecs, {} Mo)",
            bilan.recues,
            bilan.cles,
            bilan.echecs,
            bilan.octets / 1_048_576
        );
        export.images = Some(bilan);
        ecrire_archive(archive, &export, &fichiers)?;
        eprintln!("ARCHIVE {}", archive.display());
    }
    std::fs::write(&sortie, serde_json::to_string_pretty(&export)?)?;
    let n_alb: usize = export.artistes.iter().map(|a| a.albums.len()).sum();
    let n_pis: usize = export
        .artistes
        .iter()
        .flat_map(|a| a.albums.iter())
        .map(|b| b.pistes.len())
        .sum();
    eprintln!(
        "\nÉCRIT {} — {} artistes, {} albums, {} pistes, en {:?}",
        sortie.display(),
        export.artistes.len(),
        n_alb,
        n_pis,
        t0.elapsed()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Images : les octets, pas seulement les clés
// ---------------------------------------------------------------------------

/// Toutes les clés d'image de l'export, sans doublon, dans l'ordre.
fn cles_d_image(export: &Export) -> Vec<String> {
    let mut vues = std::collections::BTreeSet::new();
    let mut cles = Vec::new();
    let mut pousser = |k: &Option<String>| {
        if let Some(k) = k {
            if !k.is_empty() && vues.insert(k.clone()) {
                cles.push(k.clone());
            }
        }
    };
    for a in &export.artistes {
        pousser(&a.image);
        for b in &a.albums {
            pousser(&b.image);
        }
    }
    cles
}

/// Une clé sûre comme nom de fichier : Roon rend de l'hexadécimal, mais on
/// ne l'écrit sur disque qu'après l'avoir vérifié.
fn cle_sure(cle: &str) -> bool {
    !cle.is_empty() && cle.len() <= 128 && cle.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

async fn telecharger_images(core: &Core, export: &Export) -> (BilanImages, Vec<(String, Vec<u8>)>) {
    let service = core.image();
    let opts = ImageOptions {
        scale: Some("fit".into()),
        width: Some(1200),
        height: Some(1200),
        format: Some("image/jpeg".into()),
    };
    let cles = cles_d_image(export);
    let mut bilan = BilanImages { cles: cles.len(), ..Default::default() };
    let mut fichiers = Vec::new();
    for (i, cle) in cles.iter().enumerate() {
        if !cle_sure(cle) {
            bilan.echecs += 1;
            continue;
        }
        match service.get_image(cle, &opts).await {
            Ok(octets) if !octets.is_empty() => {
                bilan.recues += 1;
                bilan.octets += octets.len() as u64;
                fichiers.push((cle.clone(), octets));
            }
            Ok(_) => bilan.echecs += 1,
            Err(e) => {
                bilan.echecs += 1;
                if bilan.echecs <= 5 {
                    eprintln!("  image {cle} : {e}");
                }
            }
        }
        if i % 100 == 0 {
            eprintln!("  images : {}/{}", i + 1, cles.len());
        }
    }
    (bilan, fichiers)
}

/// `export.json` + `images/<clé>.jpg`, en une archive. Les JPEG sont rangés
/// sans recompression : ils sont déjà compressés.
fn ecrire_archive(chemin: &PathBuf, export: &Export, fichiers: &[(String, Vec<u8>)]) -> Result<()> {
    use std::io::Write;
    let f = std::fs::File::create(chemin).with_context(|| format!("archive {}", chemin.display()))?;
    let mut z = zip::ZipWriter::new(f);
    let json = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let brut = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    z.start_file("export.json", json)?;
    z.write_all(serde_json::to_string_pretty(export)?.as_bytes())?;
    for (cle, octets) in fichiers {
        z.start_file(format!("images/{cle}.jpg"), brut)?;
        z.write_all(octets)?;
    }
    z.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn les_cles_sont_dedoublonnees_et_sures() {
        let export = Export {
            artistes: vec![Artiste {
                nom: "A".into(),
                image: Some("abc123".into()),
                albums: vec![
                    Album { titre: "x".into(), sous_titre: None, image: Some("abc123".into()), pistes: vec![] },
                    Album { titre: "y".into(), sous_titre: None, image: Some("def456".into()), pistes: vec![] },
                    Album { titre: "z".into(), sous_titre: None, image: None, pistes: vec![] },
                ],
            }],
            ..Default::default()
        };
        assert_eq!(cles_d_image(&export), vec!["abc123".to_string(), "def456".to_string()]);
        assert!(cle_sure("ce1dc744f91d70375d7f3b2ae1f94f38"));
        assert!(!cle_sure("../etc/passwd"));
        assert!(!cle_sure(""));
    }

    #[test]
    fn l_archive_porte_l_export_et_les_images() {
        let dir = std::env::temp_dir().join(format!("moissonneur-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let chemin = dir.join("a.zip");
        let export = Export { source: "roon".into(), ..Default::default() };
        ecrire_archive(&chemin, &export, &[("abc".into(), vec![0xFF, 0xD8, 0xFF])]).unwrap();
        let mut z = zip::ZipArchive::new(std::fs::File::open(&chemin).unwrap()).unwrap();
        let noms: Vec<String> = (0..z.len()).map(|i| z.by_index(i).unwrap().name().to_string()).collect();
        assert_eq!(noms, vec!["export.json".to_string(), "images/abc.jpg".to_string()]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
