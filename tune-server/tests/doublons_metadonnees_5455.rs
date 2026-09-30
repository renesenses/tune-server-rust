//! #5455 — Métadonnées, onglet « Doublons » : « la fonction tourne et
//! cherche… et ne s'arrête jamais, ne donnant aucun résultat » (Fuccaro,
//! Windows 11, 0.9.167 ou 0.9.168, 29/09/2026).
//!
//! L'onglet attend trois routes : `GET /library/albums/eclates`,
//! `GET /library/artists/doublons` et `GET /library/duplicates`. La troisième
//! regroupe les pistes par CONTENU (`audio::empreinte::grouper_par_contenu`),
//! et ce regroupement comparait chaque piste empreintée à toutes les autres :
//! sa borne « durées à une seconde près » portait sur la longueur de
//! l'EMPREINTE, qui s'arrête à une minute — toute piste de plus d'une minute
//! et demie en a exactement 600 trames. Depuis #5246 (0.9.167), les empreintes
//! se calculent même ReplayGain coupé : une bibliothèque s'en remplit en
//! tâche de fond, et la route devient quadratique. Mesuré sur ce banc avant le
//! correctif (Shrek, profil de test) : 347 s pour 2 000 pistes empreintées.
//!
//! Le banc : une base SQLite FICHIER de [`PISTES`] pistes toutes empreintées,
//! dont [`COPIES`] copies du même contenu plantées, le VRAI routeur sur une
//! VRAIE socket. Il cloue :
//!
//!  (a) le résultat : les copies plantées sont toutes rendues comme « même
//!      enregistrement » ;
//!  (b) le temps : les trois routes répondent, `/library/duplicates` en moins
//!      de [`DELAI`].
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! par sa cible `[[test]]` dans `Cargo.toml`.
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use serde_json::Value;

use tune_server::state::AppState;

/// Pistes du banc, toutes empreintées : de quoi rendre la double boucle
/// d'origine interminable (≈ 18 millions de paires), sans ralentir le banc
/// corrigé.
const PISTES: i64 = 6_000;
/// Copies du même enregistrement plantées dans la bibliothèque (une par
/// piste d'origine, 200 ms plus longue, empreinte légèrement bruitée).
const COPIES: i64 = 25;
/// Au-delà, l'onglet « ne s'arrête jamais » pour l'utilisateur : c'est aussi
/// le délai d'abandon du client web (tune-web-client#1788).
const DELAI: Duration = Duration::from_secs(60);

/// Un générateur déterministe : le banc est le même à chaque passage.
struct Alea(u64);
impl Alea {
    fn tirer(&mut self, borne: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % borne
    }
}

/// L'empreinte d'un « morceau » : une enveloppe lisse de 600 trames
/// (énergie haute, passages par zéro moyens), comme une piste de plus d'une
/// minute et demie. Deux morceaux différents passent souvent le préfiltre
/// grossier : c'est ce qui rendait chaque paire coûteuse.
fn trames_du_morceau(alea: &mut Alea) -> Vec<(i64, i64)> {
    // Période et phase propres à chaque morceau, sur les deux octets.
    let (f1, f2) = (
        4.0 + alea.tirer(1_000) as f64 / 40.0,
        4.0 + alea.tirer(1_000) as f64 / 40.0,
    );
    let (p1, p2) = (
        alea.tirer(1_000) as f64 / 159.0,
        alea.tirer(1_000) as f64 / 159.0,
    );
    (0..600u64)
        .map(|k| {
            let e = (200.0 + 40.0 * ((k as f64) / f1 + p1).sin()) as i64 + alea.tirer(12) as i64;
            let z = (60.0 + 30.0 * ((k as f64) / f2 + p2).sin()) as i64 + alea.tirer(12) as i64;
            (e, z)
        })
        .collect()
}

fn serialiser(trames: &[(i64, i64)]) -> String {
    let mut hex = String::from("env100ms-v1:");
    for &(e, z) in trames {
        hex.push_str(&format!("{:02x}{:02x}", e.clamp(0, 255), z.clamp(0, 255)));
    }
    hex
}

/// Rend les paires (origine, copie) plantées.
fn remplir(state: &AppState) -> Vec<(i64, i64)> {
    let artistes = PISTES / 20;
    let par_album = 12;
    let albums = (PISTES + COPIES + par_album - 1) / par_album;
    let mut alea = Alea(0x5455);
    let mut sql = String::from("BEGIN;\n");
    for a in 1..=artistes {
        sql.push_str(&format!(
            "INSERT INTO artists (id, name) VALUES ({a}, 'Artiste {a}');\n"
        ));
    }
    for al in 1..=albums {
        sql.push_str(&format!(
            "INSERT INTO albums (id, title, artist_id, source) VALUES ({al}, 'Album {al}', {}, 'local');\n",
            al % artistes + 1
        ));
    }
    let piste = |sql: &mut String, id: i64, titre: &str, duree: u64, empreinte: &str| {
        let al = (id - 1) / par_album + 1;
        let n = (id - 1) % par_album + 1;
        sql.push_str(&format!(
            "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, \
             file_path, format, sample_rate, bit_depth, source, duration_ms, audio_hash, \
             audio_fingerprint) \
             VALUES ({id}, '{titre}', {al}, {artiste}, 1, {n}, \
             '/musique/Artiste {artiste}/Album {al}/{n:02}.flac', 'flac', 44100, 16, \
             'local', {duree}, 'sample64k-v2:{id:032x}', '{empreinte}');\n",
            artiste = al % artistes + 1,
        ));
    };
    let mut plantees = Vec::new();
    let mut origines = Vec::new();
    for id in 1..=PISTES {
        let trames = trames_du_morceau(&mut alea);
        let duree = 90_000 + alea.tirer(390_000);
        piste(
            &mut sql,
            id,
            &format!("Titre {}", alea.tirer(3_000)),
            duree,
            &serialiser(&trames),
        );
        if id % (PISTES / COPIES) == 0 && (origines.len() as i64) < COPIES {
            origines.push((id, trames, duree));
        }
    }
    for (k, (origine, trames, duree)) in origines.into_iter().enumerate() {
        let id = PISTES + 1 + k as i64;
        // La même piste réencodée : un peu de bruit, un rembourrage d'encodeur.
        let copie: Vec<(i64, i64)> = trames
            .iter()
            .map(|(e, z)| (e + alea.tirer(5) as i64 - 2, z + alea.tirer(5) as i64 - 2))
            .collect();
        piste(&mut sql, id, "Copie", duree + 200, &serialiser(&copie));
        plantees.push((origine, id));
    }
    sql.push_str("COMMIT;");
    state.backend.execute_batch(&sql).unwrap();
    plantees
}

/// Un GET HTTP/1.1 bloquant, `Connection: close`, borné par [`DELAI`] :
/// `None` si la route n'a pas répondu à temps.
fn obtenir(addr: SocketAddr, chemin: &str) -> Option<(u16, Value)> {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(DELAI)).unwrap();
    write!(
        s,
        "GET {chemin} HTTP/1.1\r\nHost: banc\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut brut = Vec::new();
    if s.read_to_end(&mut brut).is_err() {
        return None;
    }
    let fin_entete = brut.windows(4).position(|w| w == b"\r\n\r\n")?;
    let entete = String::from_utf8_lossy(&brut[..fin_entete]).to_ascii_lowercase();
    let statut: u16 = entete.split_whitespace().nth(1)?.parse().ok()?;
    let corps = serde_json::from_slice(&brut[fin_entete + 4..]).unwrap_or(Value::Null);
    Some((statut, corps))
}

#[test]
fn l_onglet_doublons_repond_sur_une_grande_bibliotheque_empreintee() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("tune.db");
    let (addr, plantees) = rt.block_on(async {
        let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();
        let plantees = remplir(&state);
        let app = tune_server::routes::router(state);
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = ecoute.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
        (addr, plantees)
    });

    // Les trois requêtes de l'onglet, lancées ensemble comme le client web.
    let chemins = [
        "/api/v1/library/albums/eclates",
        "/api/v1/library/artists/doublons",
        "/api/v1/library/duplicates?critere=contenu_identique&limit=1000",
    ];
    let requetes: Vec<_> = chemins
        .iter()
        .map(|c| {
            let c = c.to_string();
            std::thread::spawn(move || {
                let t0 = Instant::now();
                let r = obtenir(addr, &c);
                (c, r, t0.elapsed())
            })
        })
        .collect();
    let reponses: Vec<_> = requetes.into_iter().map(|r| r.join().unwrap()).collect();
    for (c, r, d) in &reponses {
        eprintln!(
            "#5455 : {c} → {} en {:.0} ms",
            r.as_ref()
                .map(|(s, _)| s.to_string())
                .unwrap_or_else(|| "PAS DE RÉPONSE".into()),
            d.as_secs_f64() * 1e3
        );
    }
    // Une route restée bloquée ne doit pas retenir la fin du banc.
    rt.shutdown_background();

    // (b) le temps.
    for (c, r, _) in &reponses {
        assert!(
            r.is_some(),
            "{c} n'a pas répondu en {} s : l'onglet Doublons « ne s'arrête jamais » (#5455)",
            DELAI.as_secs()
        );
    }
    // (a) le résultat.
    for (c, r, _) in &reponses {
        assert_eq!(r.as_ref().unwrap().0, 200, "{c}");
    }
    let doublons = &reponses[2].1.as_ref().unwrap().1;
    let groupes = doublons["duplicates"]["by_content"].as_array().unwrap();
    eprintln!("#5455 : {} groupes « même enregistrement »", groupes.len());
    let ensemble = |id: i64| {
        groupes.iter().find(|g| {
            g["tracks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["id"].as_i64() == Some(id))
        })
    };
    for (origine, copie) in &plantees {
        let groupe = ensemble(*origine)
            .unwrap_or_else(|| panic!("la piste {origine} et sa copie {copie} : aucun groupe"));
        assert!(
            groupe["tracks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["id"].as_i64() == Some(*copie)),
            "la piste {origine} et sa copie {copie} ne sont pas dans le même groupe : {groupe}"
        );
    }
    let paires_contenu = doublons["paires"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["critere"] == "contenu_identique")
        .count();
    assert!(paires_contenu >= COPIES as usize, "{paires_contenu} paires");
}
