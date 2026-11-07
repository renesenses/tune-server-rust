//! #8106 — une compilation personnelle (un dossier, un titre d'album, des
//! artistes de piste différents) reste UN album, par le VRAI scan.
//!
//! Régression vue en rc10 sous Windows (fil 2697) : chaque morceau d'une
//! compilation faite main devenait un album, avec sa propre pochette. Les
//! fichiers sont de vrais FLAC et MP3 posés sur le disque, le banc lance
//! `POST /system/scan` et relit la base.
//!
//! Un seul test dans ce binaire : le droit de scanner est global au
//! processus, deux scans ne se chevauchent pas.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

/// Un FLAC minimal valide pour `lofty` (même fabrique que le banc #4907).
fn flac(balises: &[(&str, String)], graine: u32) -> Vec<u8> {
    let (sr, bits): (u64, u64) = (44_100, 16);
    let mut out = b"fLaC".to_vec();
    out.push(0x00);
    out.extend_from_slice(&[0, 0, 34]);
    out.extend_from_slice(&4096u16.to_be_bytes());
    out.extend_from_slice(&4096u16.to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    let canaux: u64 = 2 - 1;
    let total: u64 = sr * 180;
    let packed: u64 = (sr << 44) | (canaux << 41) | ((bits - 1) << 36) | total;
    out.extend_from_slice(&packed.to_be_bytes());
    out.extend_from_slice(&[0u8; 16]);
    let mut vc = Vec::new();
    let vendeur = b"banc-8106";
    vc.extend_from_slice(&(vendeur.len() as u32).to_le_bytes());
    vc.extend_from_slice(vendeur);
    vc.extend_from_slice(&(balises.len() as u32).to_le_bytes());
    for (k, v) in balises {
        let c = format!("{k}={v}");
        vc.extend_from_slice(&(c.len() as u32).to_le_bytes());
        vc.extend_from_slice(c.as_bytes());
    }
    out.push(0x80 | 0x04);
    let l = vc.len() as u32;
    out.extend_from_slice(&[(l >> 16) as u8, (l >> 8) as u8, l as u8]);
    out.extend_from_slice(&vc);
    let mut x = graine.wrapping_mul(2_654_435_761).wrapping_add(1);
    for _ in 0..(64 * 1024) {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        out.push(x as u8);
    }
    out
}

/// Taille d'une ID3v2.4 : entier « synchsafe » sur quatre octets.
fn synchsafe(n: usize) -> [u8; 4] {
    [
        ((n >> 21) & 0x7f) as u8,
        ((n >> 14) & 0x7f) as u8,
        ((n >> 7) & 0x7f) as u8,
        (n & 0x7f) as u8,
    ]
}

/// Un MP3 minimal valide : une étiquette ID3v2.4 (cadres texte UTF-8) puis
/// des trames MPEG-1 couche III à 128 kbit/s, 44,1 kHz.
fn mp3(balises: &[(&str, String)], graine: u32) -> Vec<u8> {
    let cadre = |id: &str| -> Option<&'static str> {
        Some(match id {
            "TITLE" => "TIT2",
            "ARTIST" => "TPE1",
            "ALBUM" => "TALB",
            "ALBUMARTIST" => "TPE2",
            "TRACKNUMBER" => "TRCK",
            "DATE" => "TDRC",
            "COMPILATION" => "TCMP",
            _ => return None,
        })
    };
    let mut cadres = Vec::new();
    for (k, v) in balises {
        let Some(id) = cadre(k) else { continue };
        let mut corps = vec![0x03u8];
        corps.extend_from_slice(v.as_bytes());
        cadres.extend_from_slice(id.as_bytes());
        cadres.extend_from_slice(&synchsafe(corps.len()));
        cadres.extend_from_slice(&[0, 0]);
        cadres.extend_from_slice(&corps);
    }
    let mut out = b"ID3".to_vec();
    out.extend_from_slice(&[4, 0, 0]);
    out.extend_from_slice(&synchsafe(cadres.len()));
    out.extend_from_slice(&cadres);
    let mut x = graine.wrapping_mul(2_654_435_761).wrapping_add(7);
    for _ in 0..200 {
        out.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
        for _ in 0..(417 - 4) {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            out.push(x as u8);
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Format {
    Flac,
    Mp3,
}

/// Une piste à poser : son dossier (relatif à la racine), son fichier et ses
/// balises.
struct Piste {
    dossier: String,
    fichier: String,
    balises: Vec<(&'static str, String)>,
    format: Format,
}

fn poser(racine: &std::path::Path, pistes: &[Piste]) {
    for (i, p) in pistes.iter().enumerate() {
        let d = racine.join(&p.dossier);
        std::fs::create_dir_all(&d).unwrap();
        let octets = match p.format {
            Format::Flac => flac(&p.balises, i as u32 + 1),
            Format::Mp3 => mp3(&p.balises, i as u32 + 1),
        };
        std::fs::write(d.join(&p.fichier), octets).unwrap();
    }
}

const ARTISTES: [&str; 5] = [
    "Nina Simone",
    "Otis Redding",
    "Aretha Franklin",
    "Sam Cooke",
    "Etta James",
];

/// La compilation faite main de l'issue : cinq pistes de cinq artistes dans
/// un même dossier, un même titre d'album, AUCUN `ALBUMARTIST`. Chaque piste
/// garde l'année et le numéro de son disque d'origine, comme le laisse un
/// étiqueteur qui n'a touché qu'au champ ALBUM.
fn compilation(format: Format, compilation: Option<&str>, dossier: &str) -> Vec<Piste> {
    let ext = match format {
        Format::Flac => "flac",
        Format::Mp3 => "mp3",
    };
    ARTISTES
        .iter()
        .enumerate()
        .map(|(i, artiste)| {
            let mut balises = vec![
                ("TITLE", format!("Titre {}", i + 1)),
                ("ARTIST", artiste.to_string()),
                ("ALBUM", "Ma soul à moi".to_string()),
                ("TRACKNUMBER", (i + 1).to_string()),
                ("DATE", (1960 + i).to_string()),
            ];
            if let Some(c) = compilation {
                balises.push(("COMPILATION", c.to_string()));
            }
            Piste {
                dossier: dossier.to_string(),
                fichier: format!("{:02} - {artiste}.{ext}", i + 1),
                balises,
                format,
            }
        })
        .collect()
}

async fn requete(state: &AppState, methode: &str, route: &str) -> (StatusCode, Value) {
    let app: Router = tune_server::routes::router(state.clone());
    let r = Request::builder().method(methode).uri(route);
    let rep = app.oneshot(r.body(Body::empty()).unwrap()).await.unwrap();
    let statut = rep.status();
    let o = axum::body::to_bytes(rep.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    (statut, serde_json::from_slice(&o).unwrap_or(Value::Null))
}

async fn scanner(state: &AppState) {
    let (s, v) = requete(state, "POST", "/api/v1/system/scan").await;
    assert!(s.is_success(), "POST /system/scan → {s} : {v}");
    let mut dernier = Value::Null;
    for _ in 0..600 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (_, v) = requete(state, "GET", "/api/v1/system/scan/status").await;
        dernier = v;
        if dernier["status"] != "scanning" {
            break;
        }
    }
    assert_ne!(dernier["status"], "scanning", "scan jamais terminé");
}

/// Les albums en base : `(titre, artiste d'album, compilation, pistes)`.
fn albums(db: &dyn DbBackend) -> Vec<(String, String, bool, i64)> {
    db.query_many(
        "SELECT a.title, COALESCE(ar.name, ''), COALESCE(a.is_compilation, 0), \
                (SELECT COUNT(*) FROM tracks t WHERE t.album_id = a.id) \
         FROM albums a LEFT JOIN artists ar ON ar.id = a.artist_id \
         WHERE EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id) \
         ORDER BY a.title, ar.name",
        &[],
    )
    .unwrap()
    .into_iter()
    .map(|r| {
        (
            r[0].as_string().unwrap_or_default(),
            r[1].as_string().unwrap_or_default(),
            r[2].as_i64().unwrap_or(0) != 0,
            r[3].as_i64().unwrap_or(0),
        )
    })
    .collect()
}

/// L'artiste de chaque piste, dans l'ordre des titres.
fn artistes_des_pistes(db: &dyn DbBackend) -> Vec<String> {
    db.query_many(
        "SELECT COALESCE(ar.name, '') FROM tracks t LEFT JOIN artists ar ON ar.id = t.artist_id \
         ORDER BY t.title",
        &[],
    )
    .unwrap()
    .into_iter()
    .map(|r| r[0].as_string().unwrap_or_default())
    .collect()
}

/// Une bibliothèque neuve (base sur disque, pas `:memory:`), ses pistes
/// posées sous `racine`, un scan.
async fn scanner_une_bibliotheque(
    base: &std::path::Path,
    nom: &str,
    pistes: &[Piste],
) -> (AppState, std::path::PathBuf) {
    let dossier = base.join(nom);
    let _ = std::fs::remove_dir_all(&dossier);
    let racine = dossier.join("musique");
    std::fs::create_dir_all(&racine).unwrap();
    poser(&racine, pistes);
    let chemin_base = dossier.join("tune.db");
    let state = AppState::new(chemin_base.to_str().unwrap(), 0, Default::default())
        .expect("AppState SQLite");
    SettingsRepo::with_backend(state.backend.clone())
        .set(
            "music_dirs",
            &serde_json::to_string(&[racine.to_string_lossy()]).unwrap(),
        )
        .unwrap();
    scanner(&state).await;
    (state, racine)
}

fn verifier_une_compilation(cas: &str, db: &dyn DbBackend) {
    let a = albums(db);
    eprintln!("[{cas}] albums : {a:?}");
    assert_eq!(
        a.len(),
        1,
        "{cas} : la compilation faite main doit rester UN album (#8106), obtenu {a:?}"
    );
    let (titre, artiste, est_compilation, pistes) = &a[0];
    assert_eq!(titre, "Ma soul à moi", "{cas}");
    assert_eq!(pistes, &5, "{cas}");
    assert_eq!(
        artiste, "Various Artists",
        "{cas} : l'artiste d'album d'une compilation sans ALBUMARTIST"
    );
    assert!(est_compilation, "{cas} : l'album doit être une compilation");
    let mut attendus: Vec<String> = ARTISTES.iter().map(|s| s.to_string()).collect();
    let obtenus = artistes_des_pistes(db);
    attendus.sort();
    let mut tries = obtenus.clone();
    tries.sort();
    assert_eq!(
        tries, attendus,
        "{cas} : chaque piste garde son ARTIST (#3179), obtenu {obtenus:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn une_compilation_faite_main_reste_un_seul_album_8106() {
    let base = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("banc-8106-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);

    // 1. FLAC, sans ALBUMARTIST, sans COMPILATION.
    let (s, _) = scanner_une_bibliotheque(
        &base,
        "flac-sans-balise",
        &compilation(Format::Flac, None, "Compilations/Ma soul"),
    )
    .await;
    verifier_une_compilation("FLAC sans COMPILATION", s.backend.as_ref());

    // 2. FLAC, sans ALBUMARTIST, COMPILATION=1.
    let (s, _) = scanner_une_bibliotheque(
        &base,
        "flac-compilation-1",
        &compilation(Format::Flac, Some("1"), "Compilations/Ma soul"),
    )
    .await;
    verifier_une_compilation("FLAC COMPILATION=1", s.backend.as_ref());

    // 3. MP3, sans ALBUMARTIST, sans COMPILATION.
    let (s, _) = scanner_une_bibliotheque(
        &base,
        "mp3-sans-balise",
        &compilation(Format::Mp3, None, "Compilations/Ma soul"),
    )
    .await;
    verifier_une_compilation("MP3 sans COMPILATION", s.backend.as_ref());

    // 4. MP3, sans ALBUMARTIST, TCMP=1.
    let (s, _) = scanner_une_bibliotheque(
        &base,
        "mp3-compilation-1",
        &compilation(Format::Mp3, Some("1"), "Compilations/Ma soul"),
    )
    .await;
    verifier_une_compilation("MP3 TCMP=1", s.backend.as_ref());

    // 5. Deux albums homonymes de deux artistes d'album, deux dossiers :
    //    deux albums, chacun sous son artiste.
    let homonyme = |artiste: &str, dossier: &str| -> Vec<Piste> {
        (1..=3)
            .map(|n| Piste {
                dossier: dossier.to_string(),
                fichier: format!("{n:02}.flac"),
                balises: vec![
                    ("TITLE", format!("{artiste} {n}")),
                    ("ARTIST", artiste.to_string()),
                    ("ALBUMARTIST", artiste.to_string()),
                    ("ALBUM", "Greatest Hits".to_string()),
                    ("TRACKNUMBER", n.to_string()),
                    ("DATE", "1990".to_string()),
                ],
                format: Format::Flac,
            })
            .collect()
    };
    let mut deux = homonyme("Queen", "Queen/Greatest Hits");
    deux.extend(homonyme("ABBA", "ABBA/Greatest Hits"));
    let (s, _) = scanner_une_bibliotheque(&base, "homonymes", &deux).await;
    let a = albums(s.backend.as_ref());
    eprintln!("[homonymes] albums : {a:?}");
    assert_eq!(
        a,
        vec![
            ("Greatest Hits".to_string(), "ABBA".to_string(), false, 3),
            ("Greatest Hits".to_string(), "Queen".to_string(), false, 3),
        ],
        "deux albums homonymes de deux artistes restent deux albums"
    );

    let _ = std::fs::remove_dir_all(&base);
}
