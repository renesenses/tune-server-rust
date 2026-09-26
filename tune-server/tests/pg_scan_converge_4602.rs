//! #4602 — sur PostgreSQL, un scan complet laissait la plupart des pistes
//! SANS album.
//!
//! Kimon (fil 1864) : après le passage à PostgreSQL, 6 730 albums là où Roon
//! en voit 8 193, et un total qui bouge d'un scan à l'autre.
//!
//! Cause, mesurée par ce banc : `AlbumRepo::find_scattered_compilation`
//! interroge `sql::scattered_candidates`, écrite avec `GROUP_CONCAT` — une
//! fonction que PostgreSQL ne connaît pas. La requête ne part que pour un
//! dossier qui porte une pochette et des numéros de piste, soit presque toute
//! une vraie bibliothèque ; son erreur remontait par `?`, l'album n'était pas
//! créé, et la piste entrait en base avec `album_id = NULL`. Mesuré avant le
//! correctif : 20 pistes sur 24 sans album sur PostgreSQL, 0 sur SQLite.
//!
//! Le banc pose de vrais fichiers FLAC étiquetés, chacun avec sa `cover.png`
//! (sans pochette la requête fautive ne part pas, et le banc serait vert contre
//! le défaut), lance le VRAI scan par `POST /system/scan` quatre fois (dont une
//! forcée), et relit les comptes en base.
//!
//! Doctrine du saut, reprise de `pg_3182_moteur_annonce_dans_le_rapport.rs` :
//! `TUNE_TEST_PG_URL` absente ⇒ l'épreuve PostgreSQL saute (SQLite joue
//! toujours) ; posée mais injoignable ⇒ elle ROUGIT.

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

/// Une piste à poser : (dossier relatif, fichier, balises Vorbis).
struct Piste {
    dossier: &'static str,
    fichier: &'static str,
    balises: &'static [(&'static str, &'static str)],
}

/// Un FLAC minimal mais valide pour `lofty` : `fLaC`, STREAMINFO, puis un
/// bloc VORBIS_COMMENT (dernier bloc), puis des octets de « trames » propres à
/// chaque fichier (le hachage de doublon échantillonne à 25 %).
fn flac(balises: &[(&str, &str)], graine: u32) -> Vec<u8> {
    let mut out = b"fLaC".to_vec();
    // STREAMINFO (type 0), 34 octets.
    out.push(0x00);
    out.extend_from_slice(&[0, 0, 34]);
    out.extend_from_slice(&4096u16.to_be_bytes());
    out.extend_from_slice(&4096u16.to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    let sr: u64 = 44_100;
    let canaux: u64 = 2 - 1;
    let bps: u64 = 16 - 1;
    let total: u64 = 44_100 * 180;
    let packed: u64 = (sr << 44) | (canaux << 41) | (bps << 36) | total;
    out.extend_from_slice(&packed.to_be_bytes());
    out.extend_from_slice(&[0u8; 16]);
    // VORBIS_COMMENT (type 4), dernier bloc.
    let mut vc = Vec::new();
    let vendeur = b"banc-4602";
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
    for _ in 0..(96 * 1024) {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        out.push(x as u8);
    }
    out
}

const BIBLIOTHEQUE: &[Piste] = &[
    // 1. Album ordinaire.
    Piste {
        dossier: "Willy DeVille/Live In Paris And New York DR 12",
        fichier: "01. Lilly's Daddy's Cadillac.flac",
        balises: &[
            ("TITLE", "Lilly's Daddy's Cadillac"),
            ("ARTIST", "Willy DeVille"),
            ("ALBUM", "Live in Paris and New York"),
            ("TRACKNUMBER", "1"),
            ("DATE", "1993"),
        ],
    },
    Piste {
        dossier: "Willy DeVille/Live In Paris And New York DR 12",
        fichier: "02. This Must Be the Night.flac",
        balises: &[
            ("TITLE", "This Must Be the Night"),
            ("ARTIST", "Willy DeVille"),
            ("ALBUM", "Live in Paris and New York"),
            ("TRACKNUMBER", "2"),
            ("DATE", "1993"),
        ],
    },
    // 2. Casse de l'album qui varie d'une piste à l'autre, même dossier.
    Piste {
        dossier: "Willy DeVille/In Berlin (CD1) DR 9",
        fichier: "01.flac",
        balises: &[
            ("TITLE", "Spanish Stroll"),
            ("ARTIST", "The Willy DeVille Acoustic Trio"),
            ("ALBUM", "In Berlin"),
            ("DISCNUMBER", "1"),
            ("TRACKNUMBER", "1"),
        ],
    },
    Piste {
        dossier: "Willy DeVille/In Berlin (CD1) DR 9",
        fichier: "02.flac",
        balises: &[
            ("TITLE", "Heaven Stood Still"),
            ("ARTIST", "The Willy DeVille Acoustic Trio"),
            ("ALBUM", "In berlin"),
            ("DISCNUMBER", "1"),
            ("TRACKNUMBER", "2"),
        ],
    },
    Piste {
        dossier: "Willy DeVille/In Berlin (CD2)",
        fichier: "01.flac",
        balises: &[
            ("TITLE", "Storybook Love"),
            ("ARTIST", "The Willy DeVille Acoustic Trio"),
            ("ALBUM", "In Berlin"),
            ("DISCNUMBER", "2"),
            ("TRACKNUMBER", "1"),
        ],
    },
    // 3. Artiste dont la casse varie entre deux albums.
    Piste {
        dossier: "The Beatles/Abbey Road",
        fichier: "01.flac",
        balises: &[
            ("TITLE", "Come Together"),
            ("ARTIST", "The Beatles"),
            ("ALBUMARTIST", "The Beatles"),
            ("ALBUM", "Abbey Road"),
            ("TRACKNUMBER", "1"),
            ("DATE", "1969"),
        ],
    },
    Piste {
        dossier: "The Beatles/Let It Be",
        fichier: "01.flac",
        balises: &[
            ("TITLE", "Two of Us"),
            ("ARTIST", "the beatles"),
            ("ALBUMARTIST", "the beatles"),
            ("ALBUM", "Let It Be"),
            ("TRACKNUMBER", "1"),
            ("DATE", "1970"),
        ],
    },
    // 4. Accents, et un nom de dossier en NFD.
    Piste {
        dossier: "Ce\u{301}line Dion/D'eux",
        fichier: "01.flac",
        balises: &[
            ("TITLE", "Pour que tu m'aimes encore"),
            ("ARTIST", "Céline Dion"),
            ("ALBUM", "D'eux"),
            ("TRACKNUMBER", "1"),
        ],
    },
    Piste {
        dossier: "Ce\u{301}line Dion/D'eux",
        fichier: "02.flac",
        balises: &[
            ("TITLE", "Le ballet"),
            ("ARTIST", "Ce\u{301}line Dion"),
            ("ALBUM", "D'eux"),
            ("TRACKNUMBER", "2"),
        ],
    },
    // 5. Compilation étiquetée, artistes différents.
    Piste {
        dossier: "Compilations/Woodstock",
        fichier: "01.flac",
        balises: &[
            ("TITLE", "Freedom"),
            ("ARTIST", "Richie Havens"),
            ("ALBUM", "Woodstock"),
            ("COMPILATION", "1"),
            ("TRACKNUMBER", "1"),
        ],
    },
    Piste {
        dossier: "Compilations/Woodstock",
        fichier: "02.flac",
        balises: &[
            ("TITLE", "Coming Into Los Angeles"),
            ("ARTIST", "Arlo Guthrie"),
            ("ALBUM", "Woodstock"),
            ("COMPILATION", "1"),
            ("TRACKNUMBER", "2"),
        ],
    },
    // 6. Compilation SANS balise, artistes différents (forme des dossiers).
    Piste {
        dossier: "Compilations/Nuggets",
        fichier: "01.flac",
        balises: &[
            ("TITLE", "I Had Too Much to Dream"),
            ("ARTIST", "The Electric Prunes"),
            ("ALBUM", "Nuggets"),
            ("TRACKNUMBER", "1"),
        ],
    },
    Piste {
        dossier: "Compilations/Nuggets",
        fichier: "02.flac",
        balises: &[
            ("TITLE", "Dirty Water"),
            ("ARTIST", "The Standells"),
            ("ALBUM", "Nuggets"),
            ("TRACKNUMBER", "2"),
        ],
    },
    // 7. Même titre, même artiste, deux dossiers (deux éditions).
    Piste {
        dossier: "Miles Davis/Kind of Blue",
        fichier: "01.flac",
        balises: &[
            ("TITLE", "So What"),
            ("ARTIST", "Miles Davis"),
            ("ALBUM", "Kind of Blue"),
            ("TRACKNUMBER", "1"),
            ("DATE", "1959"),
        ],
    },
    Piste {
        dossier: "Miles Davis/Kind of Blue (Legacy)",
        fichier: "01.flac",
        balises: &[
            ("TITLE", "So What"),
            ("ARTIST", "Miles Davis"),
            ("ALBUM", "Kind of Blue"),
            ("TRACKNUMBER", "1"),
            ("DATE", "1959"),
        ],
    },
    // 8. Piste sans aucune balise d'album.
    Piste {
        dossier: "Divers/Sans album",
        fichier: "01 - Inconnu.flac",
        balises: &[("TITLE", "Inconnu"), ("ARTIST", "Quelqu'un")],
    },
    // 9. Coffret CD01/CD02.
    Piste {
        dossier: "Karajan/Beethoven Symphonies/CD01",
        fichier: "01.flac",
        balises: &[
            ("TITLE", "Symphony No. 1 - I"),
            ("ARTIST", "Herbert von Karajan"),
            ("ALBUMARTIST", "Herbert von Karajan"),
            ("ALBUM", "Beethoven: The Symphonies"),
            ("DISCNUMBER", "1"),
            ("TRACKNUMBER", "1"),
        ],
    },
    Piste {
        dossier: "Karajan/Beethoven Symphonies/CD02",
        fichier: "01.flac",
        balises: &[
            ("TITLE", "Symphony No. 3 - I"),
            ("ARTIST", "Herbert von Karajan"),
            ("ALBUMARTIST", "Herbert von Karajan"),
            ("ALBUM", "Beethoven: The Symphonies"),
            ("DISCNUMBER", "2"),
            ("TRACKNUMBER", "1"),
        ],
    },
    // 10. Année qui diffère d'une piste à l'autre du même album.
    Piste {
        dossier: "Bowie/Heroes",
        fichier: "01.flac",
        balises: &[
            ("TITLE", "Beauty and the Beast"),
            ("ARTIST", "David Bowie"),
            ("ALBUM", "\"Heroes\""),
            ("TRACKNUMBER", "1"),
            ("DATE", "1977"),
        ],
    },
    Piste {
        dossier: "Bowie/Heroes",
        fichier: "02.flac",
        balises: &[
            ("TITLE", "Joe the Lion"),
            ("ARTIST", "David Bowie"),
            ("ALBUM", "\"Heroes\""),
            ("TRACKNUMBER", "2"),
            ("DATE", "1999"),
        ],
    },
    // 11. MBID de release.
    Piste {
        dossier: "Radiohead/OK Computer",
        fichier: "01.flac",
        balises: &[
            ("TITLE", "Airbag"),
            ("ARTIST", "Radiohead"),
            ("ALBUM", "OK Computer"),
            (
                "MUSICBRAINZ_ALBUMID",
                "0b6b4ba0-d36f-47bd-b4ea-6a5b91842d29",
            ),
            ("TRACKNUMBER", "1"),
        ],
    },
    Piste {
        dossier: "Radiohead/OK Computer",
        fichier: "02.flac",
        balises: &[
            ("TITLE", "Paranoid Android"),
            ("ARTIST", "Radiohead"),
            ("ALBUM", "OK Computer"),
            (
                "MUSICBRAINZ_ALBUMID",
                "0b6b4ba0-d36f-47bd-b4ea-6a5b91842d29",
            ),
            ("TRACKNUMBER", "2"),
        ],
    },
    // 12. Artiste avec espace final.
    Piste {
        dossier: "Nina Simone/Pastel Blues",
        fichier: "01.flac",
        balises: &[
            ("TITLE", "Be My Husband"),
            ("ARTIST", "Nina Simone "),
            ("ALBUM", "Pastel Blues"),
            ("TRACKNUMBER", "1"),
        ],
    },
    Piste {
        dossier: "Nina Simone/Pastel Blues",
        fichier: "02.flac",
        balises: &[
            ("TITLE", "Nobody's Fault But Mine"),
            ("ARTIST", "Nina Simone"),
            ("ALBUM", "Pastel Blues"),
            ("TRACKNUMBER", "2"),
        ],
    },
];

/// Une pochette `cover.png` propre au dossier : c'est elle qui arme la
/// recherche des compilations éparpillées (`find_scattered_compilation`), qui
/// renonce sans pochette. Une vraie bibliothèque en a presque partout.
fn pochette(graine: u32) -> Vec<u8> {
    let graine = graine % 251;
    let img = image::RgbImage::from_fn(32, 32, |x, y| {
        let v = (x * 7 + y * 13 + graine * 37) % 256;
        image::Rgb([
            v as u8,
            ((v * 3) % 256) as u8,
            ((x * y + graine) % 256) as u8,
        ])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

fn poser(racine: &std::path::Path) {
    for (i, p) in BIBLIOTHEQUE.iter().enumerate() {
        let d = racine.join(p.dossier);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(p.fichier), flac(p.balises, i as u32 + 1)).unwrap();
        let graine = p
            .dossier
            .bytes()
            .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
        std::fs::write(d.join("cover.png"), pochette(graine)).unwrap();
    }
}

async fn requete(state: &AppState, methode: &str, route: &str) -> Value {
    let app: Router = tune_server::routes::router(state.clone());
    let r = app
        .oneshot(
            Request::builder()
                .method(methode)
                .uri(route)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = r.status();
    let o = axum::body::to_bytes(r.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    let t = String::from_utf8_lossy(&o).into_owned();
    assert!(statut.is_success(), "{methode} {route} → {statut} : {t}");
    serde_json::from_str(&t).unwrap_or(Value::Null)
}

fn compte(db: &dyn DbBackend, sql: &str) -> i64 {
    db.query_one(sql, &[])
        .unwrap()
        .and_then(|r| r.first()?.as_i64())
        .unwrap_or(-1)
}

#[derive(Debug, PartialEq, Eq, Clone)]
struct Comptes {
    pistes: i64,
    albums: i64,
    albums_peuples: i64,
    sans_album: i64,
    artistes: i64,
}

async fn scanner(state: &AppState, force: bool) -> (Comptes, Value) {
    let route = if force {
        "/api/v1/system/scan?full=true"
    } else {
        "/api/v1/system/scan"
    };
    requete(state, "POST", route).await;
    let mut dernier = Value::Null;
    for _ in 0..600 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        dernier = requete(state, "GET", "/api/v1/system/scan/status").await;
        if dernier["status"] != "scanning" {
            break;
        }
    }
    assert_ne!(dernier["status"], "scanning", "scan jamais terminé");
    let db = state.backend.as_ref();
    let c = Comptes {
        pistes: compte(db, "SELECT COUNT(*) FROM tracks"),
        albums: compte(db, "SELECT COUNT(*) FROM albums"),
        albums_peuples: compte(
            db,
            "SELECT COUNT(DISTINCT album_id) FROM tracks WHERE album_id IS NOT NULL",
        ),
        sans_album: compte(db, "SELECT COUNT(*) FROM tracks WHERE album_id IS NULL"),
        artistes: compte(db, "SELECT COUNT(*) FROM artists"),
    };
    (c, dernier["result"].clone())
}

async fn jouer(state: &AppState, etiquette: &str) -> Vec<Comptes> {
    // Hors de `temp_dir()` : le scan y écarte tout (`is_tune_temp_file`).
    let racine = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("banc-4602-{etiquette}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&racine);
    poser(&racine);
    SettingsRepo::with_backend(state.backend.clone())
        .set(
            "music_dirs",
            &format!("[{}]", serde_json::json!(racine.to_string_lossy())),
        )
        .unwrap();
    let mut passes = Vec::new();
    for (i, force) in [false, false, true, false].into_iter().enumerate() {
        let (c, rapport) = scanner(state, force).await;
        eprintln!("[{etiquette}] passe {} (force={force}) : {c:?}", i + 1);
        if i == 0 {
            eprintln!("[{etiquette}] rapport : {rapport}");
        }
        passes.push(c);
    }

    // Une base abîmée par l'ancien code garde ses pistes sans album : un scan
    // ordinaire saute les fichiers inchangés et ne les relit pas. C'est le
    // « Scan complet » (`?full=true`, le bouton des clients) qui les
    // re-résout. On le vérifie en rejouant l'état d'avant le correctif.
    state
        .backend
        .execute_batch("UPDATE tracks SET album_id = NULL")
        .expect("simuler une base abîmée");
    let (repare, _) = scanner(state, true).await;
    eprintln!("[{etiquette}] après « Scan complet » sur base abîmée : {repare:?}");
    assert_eq!(
        repare.sans_album, 0,
        "{etiquette} : le « Scan complet » ne rend pas leur album aux pistes \
         qu'une version antérieure a laissées sans album ({repare:?})"
    );

    let _ = std::fs::remove_dir_all(&racine);
    passes
}

/// Ce qu'un moteur doit tenir, passe après passe : toutes les pistes posées
/// sont en base, AUCUNE n'est sans album, et rien ne bouge d'une passe à
/// l'autre alors que rien ne bouge sur le disque.
fn verifier(moteur: &str, passes: &[Comptes]) {
    for (i, c) in passes.iter().enumerate() {
        assert_eq!(
            c.pistes,
            BIBLIOTHEQUE.len() as i64,
            "{moteur}, passe {} : des pistes manquent en base ({c:?})",
            i + 1
        );
        assert_eq!(
            c.sans_album,
            0,
            "{moteur}, passe {} : {} pistes indexées SANS album — la résolution d'album a \
             échoué (BUG_album_create_failed) ; {c:?}",
            i + 1,
            c.sans_album
        );
    }
    assert!(
        passes.iter().all(|c| *c == passes[0]),
        "{moteur} : les comptes changent d'une passe à l'autre sans que le disque change : {passes:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn le_scan_range_chaque_piste_dans_un_album_et_rend_les_memes_comptes_a_chaque_passe() {
    // Un seul test, les deux moteurs l'un après l'autre : le bail de scan est
    // global au processus (`try_begin_scan`), deux scans ne se chevauchent pas.
    let sqlite = AppState::new(":memory:", 0, Default::default()).expect("SQLite");
    let s = jouer(&sqlite, "sqlite").await;
    verifier("SQLite", &s);

    #[cfg(feature = "postgres")]
    if let Ok(url) = std::env::var("TUNE_TEST_PG_URL") {
        let config = tune_server::config::TuneConfig {
            database_url: Some(url),
            ..Default::default()
        };
        // Pas de `ok()?` : une base posée mais injoignable doit ROUGIR.
        let pg = AppState::new("", 0, config).expect("AppState sur PostgreSQL");
        // La base de la CI sert à plusieurs étapes à la suite : partir d'une
        // bibliothèque vide, sans quoi les comptes absolus ne veulent rien dire.
        pg.backend
            .execute_batch("TRUNCATE tracks, albums, artists RESTART IDENTITY CASCADE")
            .expect("vider la bibliothèque PostgreSQL");
        let p = jouer(&pg, "postgres").await;
        verifier("PostgreSQL", &p);
        // ⚠️ Le NOMBRE d'albums n'est pas comparé entre moteurs. La fusion
        // post-scan des albums homonymes (#593), longtemps morte sur
        // PostgreSQL (`GROUP_CONCAT` en dur), tourne sur les deux moteurs
        // depuis le reste de #5005 — elle a son propre banc,
        // `pg_fusions_auto_albums.rs`.
        return;
    }
    eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL de #4602 SAUTÉE");
}
