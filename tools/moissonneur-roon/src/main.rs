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
//! Deux conforts, ajoutés après la première tournée de testeurs :
//!
//! - **il trouve le Core tout seul** (SOOD, `roon_sood::SoodDiscovery`) ;
//!   `--hote=` reste l'échappatoire quand la diffusion ne traverse pas. Il ne
//!   départage JAMAIS plusieurs Cores à la place de l'utilisateur ;
//! - **l'autorisation donnée dans Roon est gardée** (`FileStateStore`), dans
//!   le dossier de configuration du compte. Elle n'est demandée qu'une fois.
//!
//! ⚠️ `browse` est un arbre à curseur : chaque descente déplace la position de
//! la session, et il faut remonter d'autant. `hierarchy` doit être passé à
//! CHAQUE appel, y compris les descentes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use roon_api::{
    BrowseItem, BrowseOptions, Core, FileStateStore, ImageOptions, LoadOptions, RoonClientBuilder,
    StateStore,
};
use serde::Serialize;

const SESSION: &str = "tune-moissonneur";
const HIER_ARTISTES: &str = "artists";
const PAGE: u32 = 100;
const PORT_ROON: u16 = 9330;

/// Le dossier de l'utilisateur où le moissonneur range ce qu'il doit retrouver
/// au lancement suivant — aujourd'hui le seul jeton d'autorisation.
const DOSSIER_CONFIG: &str = "tune-moissonneur-roon";
/// Le fichier écrit par `FileStateStore` : jeton d'autorisation par Core, et
/// l'identifiant du Core apparié. Son contenu n'est JAMAIS affiché.
const FICHIER_JETON: &str = "jeton-roon.json";
/// Combien de temps écouter les réponses SOOD avant de conclure. Un Core
/// répond en une fraction de seconde ; ce délai borne le cas où rien ne vient.
const DECOUVERTE: Duration = Duration::from_secs(5);

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
// La ligne de commande
// ---------------------------------------------------------------------------

/// Ce que la ligne de commande a dit. Les options s'écrivent **avec `=`** —
/// c'était déjà la règle, elle ne change pas.
#[derive(Debug, PartialEq, Eq)]
struct Arguments {
    /// `--hote` : donné, il court-circuite la découverte.
    hote: Option<String>,
    /// `--port` : donné, il l'emporte même sur le port annoncé par le Core.
    port: Option<u16>,
    sortie: PathBuf,
    archive: Option<PathBuf>,
    /// `--jeton` : où ranger l'autorisation, quand le chemin par défaut ne
    /// convient pas (clé USB, machine partagée, compte sans dossier maison).
    jeton: Option<PathBuf>,
    sans_pistes: bool,
    decouverte: Duration,
    aide: bool,
}

impl Default for Arguments {
    fn default() -> Self {
        Self {
            hote: None,
            port: None,
            sortie: PathBuf::from("export-roon.json"),
            archive: None,
            jeton: None,
            sans_pistes: false,
            decouverte: DECOUVERTE,
            aide: false,
        }
    }
}

fn analyser_arguments<I: IntoIterator<Item = String>>(brut: I) -> Arguments {
    let mut a = Arguments::default();
    for mot in brut {
        match mot.as_str() {
            "--sans-pistes" => a.sans_pistes = true,
            "--aide" | "--help" | "-h" => a.aide = true,
            _ => {
                let Some((cle, valeur)) = mot.split_once('=') else {
                    continue;
                };
                match cle {
                    // Une adresse vide (`--hote=`) vaut « pas d'adresse » :
                    // elle ne doit pas éteindre la découverte.
                    "--hote" if !valeur.is_empty() => a.hote = Some(valeur.to_string()),
                    "--port" => a.port = valeur.parse().ok(),
                    "--sortie" if !valeur.is_empty() => a.sortie = PathBuf::from(valeur),
                    "--archive" if !valeur.is_empty() => a.archive = Some(PathBuf::from(valeur)),
                    "--jeton" if !valeur.is_empty() => a.jeton = Some(PathBuf::from(valeur)),
                    "--decouverte" => {
                        if let Ok(s) = valeur.parse::<u64>() {
                            a.decouverte = Duration::from_secs(s);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    a
}

const USAGE: &str = "\
usage : moissonneur-roon [--hote=<ip>] [--port=9330] [--archive=export-roon.zip]
                        [--sortie=f.json] [--sans-pistes] [--jeton=<fichier>]
                        [--decouverte=<secondes>]

Sans --hote, le Core Roon est cherché tout seul sur le réseau.
L'autorisation donnée dans Roon est gardée : elle n'est demandée qu'une fois.
(lecture seule : rien n'est joué, rien n'est modifié)";

// ---------------------------------------------------------------------------
// Le jeton d'autorisation, gardé d'un lancement à l'autre
// ---------------------------------------------------------------------------

/// Les trois plateformes ne rangent pas la configuration au même endroit, et
/// la variable d'environnement qui la nomme n'est pas la même. La plateforme
/// est un paramètre, pas un `cfg!` enfoui : c'est ce qui rend la règle
/// vérifiable ailleurs que sur la machine qui la compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plateforme {
    Windows,
    MacOs,
    Autre,
}

fn plateforme_courante() -> Plateforme {
    if cfg!(target_os = "windows") {
        Plateforme::Windows
    } else if cfg!(target_os = "macos") {
        Plateforme::MacOs
    } else {
        Plateforme::Autre
    }
}

/// Le dossier de configuration de l'utilisateur.
///
/// Le moissonneur n'avait aucune convention à suivre : ses deux sorties
/// (`export-roon.json`, `export-roon.zip`) s'écrivent dans le dossier courant,
/// parce que ce sont des fichiers que l'on manipule. Le jeton, lui, ne se
/// manipule pas et doit survivre au dossier d'où l'on a lancé la commande :
/// il va donc à l'emplacement de configuration de la plateforme, par
/// utilisateur.
fn dossier_config(
    plateforme: Plateforme,
    appdata: Option<&str>,
    xdg: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    let non_vide = |v: Option<&str>| v.filter(|s| !s.is_empty()).map(PathBuf::from);
    match plateforme {
        Plateforme::Windows => Some(non_vide(appdata)?.join(DOSSIER_CONFIG)),
        Plateforme::MacOs => Some(
            non_vide(home)?
                .join("Library")
                .join("Application Support")
                .join(DOSSIER_CONFIG),
        ),
        Plateforme::Autre => match non_vide(xdg) {
            Some(x) => Some(x.join(DOSSIER_CONFIG)),
            None => Some(non_vide(home)?.join(".config").join(DOSSIER_CONFIG)),
        },
    }
}

/// Le chemin du fichier de jeton : `--jeton` d'abord, sinon le dossier de
/// configuration de la plateforme.
fn chemin_du_jeton(
    explicite: Option<&Path>,
    plateforme: Plateforme,
    appdata: Option<&str>,
    xdg: Option<&str>,
    home: Option<&str>,
) -> Result<PathBuf> {
    if let Some(p) = explicite {
        return Ok(p.to_path_buf());
    }
    dossier_config(plateforme, appdata, xdg, home)
        .map(|d| d.join(FICHIER_JETON))
        .ok_or_else(|| {
            anyhow!(
                "impossible de situer le dossier de configuration de cet utilisateur : \
                 donnez un chemin avec --jeton=<fichier>"
            )
        })
}

/// Ce fichier autorise à LIRE le Core Roon de quelqu'un : il n'est lisible que
/// par son propriétaire. Créé ici, vide, avant que la bibliothèque n'écrive
/// dedans — `std::fs::write` sur un fichier existant garde ses droits.
fn securiser_le_jeton(chemin: &Path) -> std::io::Result<()> {
    if let Some(parent) = chemin.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            }
        }
    }
    if !chemin.exists() {
        std::fs::write(chemin, b"")?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(chemin, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Trouver le Core tout seul
// ---------------------------------------------------------------------------

/// Un Core aperçu sur le réseau, réduit à ce qui sert ici.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CoreVu {
    nom: Option<String>,
    version: Option<String>,
    hote: String,
    port: u16,
}

impl CoreVu {
    fn etiquette(&self) -> String {
        let nom = self.nom.as_deref().unwrap_or("Core Roon");
        match &self.version {
            Some(v) => format!("« {nom} » — {}:{} (Roon {v})", self.hote, self.port),
            None => format!("« {nom} » — {}:{}", self.hote, self.port),
        }
    }
}

/// À quel Core parler — et, quand la réponse n'est pas unique, pourquoi.
#[derive(Debug, PartialEq, Eq)]
enum Choix {
    /// `--hote` a tranché : on ne cherche rien.
    Donnee { hote: String, port: u16 },
    /// Un seul Core sur le réseau : c'est lui.
    Unique { hote: String, port: u16, vu: CoreVu },
    /// Rien trouvé.
    Aucun,
    /// Plusieurs : on ne choisit PAS à la place de l'utilisateur.
    Plusieurs(Vec<CoreVu>),
}

/// La règle, en un seul endroit : l'adresse donnée l'emporte toujours ; sinon
/// un seul Core vu est retenu, zéro ou plusieurs sont rendus à l'utilisateur.
/// `--port` reste un forçage : il l'emporte même sur le port annoncé par le
/// Core, pour les redirections de port.
fn choisir_le_core(hote: Option<&str>, port: Option<u16>, vus: &[CoreVu]) -> Choix {
    if let Some(h) = hote.filter(|h| !h.is_empty()) {
        return Choix::Donnee {
            hote: h.to_string(),
            port: port.unwrap_or(PORT_ROON),
        };
    }
    match vus {
        [] => Choix::Aucun,
        [seul] => Choix::Unique {
            hote: seul.hote.clone(),
            port: port.unwrap_or(seul.port),
            vu: seul.clone(),
        },
        plusieurs => Choix::Plusieurs(plusieurs.to_vec()),
    }
}

/// Écoute les réponses SOOD pendant `duree`, puis rend ce qui a répondu, sans
/// doublon et dans un ordre stable. Ne se connecte à rien : la découverte
/// n'est qu'un carnet d'adresses.
async fn decouvrir(duree: Duration) -> Result<Vec<CoreVu>> {
    use tokio::sync::broadcast::error::RecvError;

    let (sood, mut recu) = roon_sood::SoodDiscovery::start()
        .await
        .map_err(|e| anyhow!("découverte impossible ({e}) — donnez l'adresse avec --hote=<ip>"))?;

    let fin = tokio::time::Instant::now() + duree;
    let mut vus: BTreeMap<String, CoreVu> = BTreeMap::new();
    loop {
        let reste = fin.saturating_duration_since(tokio::time::Instant::now());
        if reste.is_zero() {
            break;
        }
        match tokio::time::timeout(reste, recu.recv()).await {
            Ok(Ok(c)) => {
                vus.insert(
                    c.core_id.clone(),
                    CoreVu {
                        nom: c.name,
                        version: c.display_version,
                        hote: c.host.to_string(),
                        port: c.http_port,
                    },
                );
            }
            // Trop de réponses d'un coup : on garde ce qu'on a et on continue.
            Ok(Err(RecvError::Lagged(_))) => continue,
            Ok(Err(RecvError::Closed)) => break,
            // Délai écoulé : c'est la borne, pas une erreur.
            Err(_) => break,
        }
    }
    sood.stop().await;
    Ok(vus.into_values().collect())
}

fn rien_trouve(duree: Duration) -> String {
    format!(
        "aucun Core Roon trouvé sur le réseau (recherche de {} s).\n\
         À essayer, dans cet ordre :\n\
         \x20 • le Core est-il allumé, et sur le MÊME réseau que cette machine ?\n\
         \x20 • Wi-Fi avec « isolation des clients », VLAN, Docker sans --network=host :\n\
         \x20   la diffusion ne traverse pas. Donnez alors l'adresse à la main :\n\
         \x20       moissonneur-roon --hote=192.168.1.20 --archive=export-roon.zip\n\
         \x20 • réseau lent ou Core qui démarre : allongez la recherche,\n\
         \x20       moissonneur-roon --decouverte=20",
        duree.as_secs()
    )
}

fn plusieurs_trouves(vus: &[CoreVu]) -> String {
    let liste: Vec<String> = vus
        .iter()
        .map(|v| format!("\x20 • {}", v.etiquette()))
        .collect();
    format!(
        "{} Cores Roon trouvés sur le réseau — dites lequel avec --hote :\n{}",
        vus.len(),
        liste.join("\n")
    )
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
        let Some(k) = it.item_key.clone() else {
            continue;
        };
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
    let args = analyser_arguments(std::env::args().skip(1));
    if args.aide {
        println!("{USAGE}");
        return Ok(());
    }
    let sortie = args.sortie.clone();
    let archive = args.archive.clone();
    let sans_pistes = args.sans_pistes;

    // La précédence se lit ici : la découverte ne tourne QUE si aucune adresse
    // n'a été donnée. `--hote` n'attend donc jamais le délai de recherche.
    let vus = if args.hote.is_some() {
        Vec::new()
    } else {
        eprintln!(
            "recherche du Core Roon sur le réseau ({} s)…",
            args.decouverte.as_secs()
        );
        decouvrir(args.decouverte).await?
    };
    let (hote, port) = match choisir_le_core(args.hote.as_deref(), args.port, &vus) {
        Choix::Donnee { hote, port } => (hote, port),
        Choix::Unique { hote, port, vu } => {
            eprintln!("Core trouvé : {}", vu.etiquette());
            (hote, port)
        }
        Choix::Aucun => {
            eprintln!("{}", rien_trouve(args.decouverte));
            std::process::exit(2);
        }
        Choix::Plusieurs(vus) => {
            eprintln!("{}", plusieurs_trouves(&vus));
            std::process::exit(2);
        }
    };

    // Le jeton d'autorisation, gardé d'un lancement à l'autre. On n'affiche
    // que le CHEMIN : le contenu autorise à lire le Core.
    let jeton = chemin_du_jeton(
        args.jeton.as_deref(),
        plateforme_courante(),
        std::env::var("APPDATA").ok().as_deref(),
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )?;
    securiser_le_jeton(&jeton).with_context(|| format!("jeton {}", jeton.display()))?;
    let deja_autorise = FileStateStore::new(&jeton).load_paired_core_id().is_some();

    let client = RoonClientBuilder::new(
        "fr.mozaiklabs.tune.moissonneur",
        "Tune — moissonneur",
        env!("CARGO_PKG_VERSION"),
        "Mozaik Labs",
        "contact@mozaiklabs.fr",
    )
    .require_browse()
    .token_store(FileStateStore::new(&jeton))
    .build()?;

    if deja_autorise {
        eprintln!(
            "connexion à {hote}:{port} — autorisation déjà donnée (jeton : {})",
            jeton.display()
        );
    } else {
        eprintln!("connexion à {hote}:{port} — autorisez « Tune — moissonneur » dans Roon (Réglages → Extensions)");
        eprintln!(
            "(une seule fois : l'autorisation est gardée dans {})",
            jeton.display()
        );
    }
    let core = client.connect(&hote, port).await.context("connexion")?;
    // La bibliothèque vient peut-être de créer le fichier elle-même.
    let _ = securiser_le_jeton(&jeton);
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
            eprintln!(
                "  {}/{} — {nom} ({} albums)",
                i + 1,
                cles.len(),
                albums.len()
            );
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
    !cle.is_empty()
        && cle.len() <= 128
        && cle
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
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
    let mut bilan = BilanImages {
        cles: cles.len(),
        ..Default::default()
    };
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
    let f =
        std::fs::File::create(chemin).with_context(|| format!("archive {}", chemin.display()))?;
    ecrire_zip(f, export, fichiers)
}

/// Le contenu de l'archive, dans n'importe quel flux : un fichier en vrai, un
/// tampon mémoire dans le test — aucun temporaire à nettoyer.
fn ecrire_zip<W: std::io::Write + std::io::Seek>(
    w: W,
    export: &Export,
    fichiers: &[(String, Vec<u8>)],
) -> Result<()> {
    use std::io::Write;
    let mut z = zip::ZipWriter::new(w);
    let json = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let brut =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
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

    fn mots(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn core(nom: &str, hote: &str, port: u16) -> CoreVu {
        CoreVu {
            nom: Some(nom.into()),
            version: None,
            hote: hote.into(),
            port,
        }
    }

    // -- la ligne de commande ------------------------------------------------

    #[test]
    fn les_arguments_se_lisent_avec_un_egal() {
        let a = analyser_arguments(mots(&[
            "--hote=192.168.1.20",
            "--port=9331",
            "--archive=a.zip",
            "--sortie=s.json",
            "--jeton=/tmp/j.json",
            "--decouverte=12",
            "--sans-pistes",
        ]));
        assert_eq!(a.hote.as_deref(), Some("192.168.1.20"));
        assert_eq!(a.port, Some(9331));
        assert_eq!(a.archive, Some(PathBuf::from("a.zip")));
        assert_eq!(a.sortie, PathBuf::from("s.json"));
        assert_eq!(a.jeton, Some(PathBuf::from("/tmp/j.json")));
        assert_eq!(a.decouverte, Duration::from_secs(12));
        assert!(a.sans_pistes);
    }

    #[test]
    fn sans_argument_tout_reste_au_defaut() {
        let a = analyser_arguments(mots(&[]));
        assert_eq!(
            a.hote, None,
            "sans --hote, la découverte doit pouvoir tourner"
        );
        assert_eq!(a.port, None);
        assert_eq!(a.sortie, PathBuf::from("export-roon.json"));
        assert_eq!(a.archive, None);
        assert_eq!(a.jeton, None);
        assert_eq!(a.decouverte, DECOUVERTE);
        assert!(!a.sans_pistes);
        assert!(!a.aide);
    }

    #[test]
    fn une_option_sans_egal_ou_vide_ne_vaut_pas_une_valeur() {
        // La forme séparée par une espace n'a jamais été gérée : elle ne doit
        // surtout pas passer pour une adresse donnée.
        let a = analyser_arguments(mots(&["--hote", "192.168.1.20"]));
        assert_eq!(a.hote, None);
        // `--hote=` vide non plus : sinon la découverte serait éteinte sans
        // qu'aucune adresse ne la remplace.
        assert_eq!(analyser_arguments(mots(&["--hote="])).hote, None);
        assert_eq!(analyser_arguments(mots(&["--port=abc"])).port, None);
        assert_eq!(
            analyser_arguments(mots(&["--decouverte=bientot"])).decouverte,
            DECOUVERTE
        );
    }

    #[test]
    fn l_aide_se_demande_de_trois_facons() {
        for mot in ["--aide", "--help", "-h"] {
            assert!(analyser_arguments(mots(&[mot])).aide, "{mot}");
        }
    }

    // -- à quel Core parler --------------------------------------------------

    #[test]
    fn l_adresse_donnee_l_emporte_sur_tout_ce_qui_a_repondu() {
        let vus = vec![
            core("Salon", "192.168.1.20", 9330),
            core("Bureau", "192.168.1.30", 9330),
        ];
        assert_eq!(
            choisir_le_core(Some("10.0.0.5"), None, &vus),
            Choix::Donnee {
                hote: "10.0.0.5".into(),
                port: PORT_ROON
            }
        );
    }

    #[test]
    fn un_seul_core_est_retenu_avec_le_port_qu_il_annonce() {
        let vus = vec![core("Salon", "192.168.1.20", 9331)];
        assert_eq!(
            choisir_le_core(None, None, &vus),
            Choix::Unique {
                hote: "192.168.1.20".into(),
                port: 9331,
                vu: vus[0].clone()
            }
        );
    }

    #[test]
    fn un_port_donne_force_meme_celui_annonce_par_le_core() {
        let vus = vec![core("Salon", "192.168.1.20", 9331)];
        let Choix::Unique { port, .. } = choisir_le_core(None, Some(9440), &vus) else {
            panic!("un seul Core vu doit donner Unique");
        };
        assert_eq!(port, 9440);
    }

    #[test]
    fn aucun_core_ne_se_devine_pas() {
        assert_eq!(choisir_le_core(None, None, &[]), Choix::Aucun);
        let texte = rien_trouve(Duration::from_secs(5));
        assert!(
            texte.contains("--hote="),
            "le message doit donner la sortie de secours"
        );
        assert!(
            texte.contains("--decouverte"),
            "et comment allonger la recherche"
        );
    }

    #[test]
    fn plusieurs_cores_ne_sont_jamais_departages_en_silence() {
        let vus = vec![
            core("Salon", "192.168.1.20", 9330),
            core("Bureau", "192.168.1.30", 9330),
        ];
        let choix = choisir_le_core(None, None, &vus);
        assert_eq!(choix, Choix::Plusieurs(vus.clone()));
        let texte = plusieurs_trouves(&vus);
        // Les DEUX sont nommés, avec leur adresse, et le drapeau est demandé.
        assert!(texte.contains("192.168.1.20"), "{texte}");
        assert!(texte.contains("192.168.1.30"), "{texte}");
        assert!(
            texte.contains("Salon") && texte.contains("Bureau"),
            "{texte}"
        );
        assert!(texte.contains("--hote"), "{texte}");
    }

    // -- où va le jeton ------------------------------------------------------

    #[test]
    fn le_jeton_va_dans_le_dossier_de_configuration_de_la_plateforme() {
        assert_eq!(
            dossier_config(
                Plateforme::Windows,
                Some("C:\\Users\\f\\AppData\\Roaming"),
                None,
                None
            ),
            Some(PathBuf::from("C:\\Users\\f\\AppData\\Roaming").join(DOSSIER_CONFIG))
        );
        assert_eq!(
            dossier_config(Plateforme::MacOs, None, Some("/ignore"), Some("/Users/f")),
            Some(PathBuf::from("/Users/f/Library/Application Support").join(DOSSIER_CONFIG))
        );
        assert_eq!(
            dossier_config(
                Plateforme::Autre,
                None,
                Some("/home/f/.cfg"),
                Some("/home/f")
            ),
            Some(PathBuf::from("/home/f/.cfg").join(DOSSIER_CONFIG))
        );
        assert_eq!(
            dossier_config(Plateforme::Autre, None, None, Some("/home/f")),
            Some(PathBuf::from("/home/f/.config").join(DOSSIER_CONFIG))
        );
        // Une variable vide ne vaut pas une variable posée.
        assert_eq!(
            dossier_config(Plateforme::Autre, None, Some(""), None),
            None
        );
        assert_eq!(
            dossier_config(Plateforme::Windows, None, None, Some("/home/f")),
            None
        );
    }

    #[test]
    fn le_chemin_donne_a_la_main_passe_avant_la_convention() {
        let choisi = chemin_du_jeton(
            Some(Path::new("/media/cle/j.json")),
            Plateforme::Autre,
            None,
            None,
            Some("/home/f"),
        )
        .unwrap();
        assert_eq!(choisi, PathBuf::from("/media/cle/j.json"));

        let defaut = chemin_du_jeton(None, Plateforme::Autre, None, None, Some("/home/f")).unwrap();
        assert_eq!(
            defaut,
            PathBuf::from("/home/f/.config")
                .join(DOSSIER_CONFIG)
                .join(FICHIER_JETON)
        );

        // Sans dossier maison ET sans --jeton, on le DIT au lieu d'écrire au hasard.
        let erreur = chemin_du_jeton(None, Plateforme::Autre, None, None, None).unwrap_err();
        assert!(erreur.to_string().contains("--jeton"), "{erreur}");
    }

    #[cfg(unix)]
    #[test]
    fn le_fichier_de_jeton_n_est_lisible_que_par_son_proprietaire() {
        use std::os::unix::fs::PermissionsExt;

        let base = std::env::temp_dir().join(format!("moissonneur-jeton-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let chemin = base.join("sous").join(FICHIER_JETON);

        securiser_le_jeton(&chemin).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode(&chemin),
            0o600,
            "le jeton autorise à lire le Core de quelqu'un"
        );
        assert_eq!(mode(chemin.parent().unwrap()), 0o700);

        // Un fichier déjà là, trop ouvert, doit être resserré — pas ignoré.
        std::fs::write(&chemin, b"{}").unwrap();
        std::fs::set_permissions(&chemin, std::fs::Permissions::from_mode(0o644)).unwrap();
        securiser_le_jeton(&chemin).unwrap();
        assert_eq!(mode(&chemin), 0o600);
        // …sans écraser ce qu'il contenait.
        assert_eq!(std::fs::read(&chemin).unwrap(), b"{}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn les_cles_sont_dedoublonnees_et_sures() {
        let export = Export {
            artistes: vec![Artiste {
                nom: "A".into(),
                image: Some("abc123".into()),
                albums: vec![
                    Album {
                        titre: "x".into(),
                        sous_titre: None,
                        image: Some("abc123".into()),
                        pistes: vec![],
                    },
                    Album {
                        titre: "y".into(),
                        sous_titre: None,
                        image: Some("def456".into()),
                        pistes: vec![],
                    },
                    Album {
                        titre: "z".into(),
                        sous_titre: None,
                        image: None,
                        pistes: vec![],
                    },
                ],
            }],
            ..Default::default()
        };
        assert_eq!(
            cles_d_image(&export),
            vec!["abc123".to_string(), "def456".to_string()]
        );
        assert!(cle_sure("ce1dc744f91d70375d7f3b2ae1f94f38"));
        assert!(!cle_sure("../etc/passwd"));
        assert!(!cle_sure(""));
    }

    #[test]
    fn l_archive_porte_l_export_et_les_images() {
        let export = Export {
            source: "roon".into(),
            ..Default::default()
        };
        let mut tampon = std::io::Cursor::new(Vec::new());
        ecrire_zip(
            &mut tampon,
            &export,
            &[("abc".into(), vec![0xFF, 0xD8, 0xFF])],
        )
        .unwrap();
        tampon.set_position(0);
        let mut z = zip::ZipArchive::new(tampon).unwrap();
        let noms: Vec<String> = (0..z.len())
            .map(|i| z.by_index(i).unwrap().name().to_string())
            .collect();
        assert_eq!(
            noms,
            vec!["export.json".to_string(), "images/abc.jpg".to_string()]
        );
    }
}
