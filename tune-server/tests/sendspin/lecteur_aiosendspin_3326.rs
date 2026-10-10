//! #3326 S2-c — interopérabilité contre le lecteur de RÉFÉRENCE `aiosendspin`.
//!
//! Optionnel : `#[ignore]`, lancé par `--ignored` avec
//! `TUNE_AIOSENDSPIN_PYTHON` désignant un interpréteur où `aiosendspin`
//! (≥ 10.0, spec 1.0) et `soundfile` sont installés ; sans lui, il échoue.
//!
//! Le pair n'est plus écrit ici : c'est l'implémentation Python de référence
//! (`banc_aiosendspin.py`), qui s'appaire par « Pairing PSK » via la route
//! opérateur, négocie, synchronise son horloge avec son propre filtre de
//! Kalman et enregistre ce qu'il reçoit. Le serveur est le VRAI routeur de
//! Tune, sur la boucle locale ; la sortie est pilotée par l'API
//! `OutputTarget`, comme l'orchestrateur.
//!
//! Scénario : sinusoïde 1 kHz 44,1 kHz/16 bits, puis enchaînement sans coupure
//! sur une sinusoïde 440 Hz 48 kHz/24 bits (changement de format en place),
//! volume, pause et reprise, préférence FLAC envoyée par l'enceinte en pleine
//! lecture, saut de piste (`stream/clear`), fin naturelle, arrêt.
//!
//! `TUNE_AIOSENDSPIN_DOSSIER` garde les journaux des deux côtés
//! (`tune.log`, `aiosendspin.log`, `journal.json`, les flux reçus).
use super::*;
use std::io::{BufRead, BufReader, Write as _};
use std::process::{Command, Stdio};

/// Signal de test connu, en WAV entier : canal gauche = sinusoïde, canal droit
/// = rampe (`i × 97` modulo la pleine échelle) qui rend chaque position unique,
/// pour aligner sans ambiguïté un tronçon reçu (reprise après pause).
fn sinus_wav(
    chemin: &std::path::Path,
    taux: u32,
    bits: u16,
    secondes: f64,
    frequence: f64,
) -> Vec<u8> {
    let trames = (f64::from(taux) * secondes) as usize;
    let octets = usize::from(bits / 8);
    let amplitude = f64::from((1i32 << (bits - 1)) - 1) * 0.8;
    let mut pcm = Vec::with_capacity(trames * 2 * octets);
    for i in 0..trames {
        let v = (amplitude
            * (2.0 * std::f64::consts::PI * frequence * i as f64 / f64::from(taux)).sin())
            as i32;
        let pleine = 1i64 << bits;
        let rampe = ((i as i64 * 97) % pleine - pleine / 2) as i32;
        for s in [v, rampe] {
            pcm.extend_from_slice(&s.to_le_bytes()[..octets]);
        }
    }
    let mut f = Vec::new();
    let taille = pcm.len() as u32;
    let bloc = 2 * bits / 8;
    f.extend_from_slice(b"RIFF");
    f.extend_from_slice(&(36 + taille).to_le_bytes());
    f.extend_from_slice(b"WAVEfmt ");
    f.extend_from_slice(&16u32.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&2u16.to_le_bytes());
    f.extend_from_slice(&taux.to_le_bytes());
    f.extend_from_slice(&(taux * u32::from(bloc)).to_le_bytes());
    f.extend_from_slice(&bloc.to_le_bytes());
    f.extend_from_slice(&bits.to_le_bytes());
    f.extend_from_slice(b"data");
    f.extend_from_slice(&taille.to_le_bytes());
    f.extend_from_slice(&pcm);
    std::fs::write(chemin, f).unwrap();
    pcm
}

/// Échantillons entiers d'un flux reçu : PCM tel quel ; FLAC décodé côté
/// Python par libsndfile, un décodeur INDÉPENDANT de Tune.
fn echantillons(flux: &Value) -> Vec<i32> {
    let fichier = flux["fichier"].as_str().unwrap();
    let bits = flux["format"]["bit_depth"].as_u64().unwrap() as usize;
    if flux["format"]["codec"] == "pcm" {
        let octets = std::fs::read(fichier).unwrap();
        return octets
            .chunks_exact(bits / 8)
            .map(|c| {
                let mut b = [0u8; 4];
                b[4 - c.len()..].copy_from_slice(c);
                i32::from_le_bytes(b) >> (32 - bits)
            })
            .collect();
    }
    std::fs::read(flux["decode"].as_str().expect("FLAC decode par libsndfile"))
        .unwrap()
        .chunks_exact(4)
        .map(|c| i32::from_le_bytes(c.try_into().unwrap()))
        .collect()
}

fn reference(pcm: &[u8], bits: usize) -> Vec<i32> {
    pcm.chunks_exact(bits / 8)
        .map(|c| {
            let mut b = [0u8; 4];
            b[4 - c.len()..].copy_from_slice(c);
            i32::from_le_bytes(b) >> (32 - bits)
        })
        .collect()
}

/// Position (en échantillons entrelacés) où `recu` s'aligne exactement dans
/// `reference`, s'il s'y trouve en entier.
fn aligner(recu: &[i32], reference: &[i32]) -> Option<usize> {
    if recu.is_empty() || recu.len() > reference.len() {
        return None;
    }
    (0..=reference.len() - recu.len())
        .step_by(2)
        .find(|&i| reference[i..i + recu.len()] == *recu)
}

/// L'interpréteur du banc : exigé, jamais deviné (pas de saut silencieux).
fn interpreteur_aiosendspin() -> std::ffi::OsString {
    std::env::var_os("TUNE_AIOSENDSPIN_PYTHON")
        .expect("TUNE_AIOSENDSPIN_PYTHON : interpreteur ou aiosendspin et soundfile sont installes")
}

/// Où garder les journaux des deux côtés ; un dossier temporaire sinon.
fn dossier_des_journaux() -> Option<std::path::PathBuf> {
    std::env::var_os("TUNE_AIOSENDSPIN_DOSSIER").map(std::path::PathBuf::from)
}

fn jouer(chemin: &str, taux: u32, ms: u64) -> PlayMedia<'_> {
    PlayMedia {
        url: chemin,
        file_path: Some(chemin),
        duration_ms: Some(ms),
        sample_rate: Some(taux),
        ..Default::default()
    }
}

struct Lecteur {
    enfant: std::process::Child,
    sortie: BufReader<std::process::ChildStdout>,
}

impl Lecteur {
    fn ligne(&mut self, prefixe: &str) -> String {
        loop {
            let mut l = String::new();
            assert!(
                self.sortie.read_line(&mut l).unwrap() > 0,
                "aiosendspin s'est arrete avant {prefixe}"
            );
            if let Some(v) = l.trim().strip_prefix(prefixe) {
                return v.to_owned();
            }
        }
    }

    fn commande(&mut self, c: &str) {
        let entree = self.enfant.stdin.as_mut().unwrap();
        writeln!(entree, "{c}").unwrap();
        entree.flush().unwrap();
    }
}

async fn attendre<F, Fut>(quoi: &str, secondes: u64, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    tokio::time::timeout(Duration::from_secs(secondes), async {
        while !condition().await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("delai depasse : {quoi}"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "interop aiosendspin : exige TUNE_AIOSENDSPIN_PYTHON (aiosendspin + soundfile)"]
async fn i3326_s2c_interop_aiosendspin_lecteur_de_reference() {
    // Ignoré par défaut, comme les autres bancs d'interopérabilité ; lancé
    // avec `--ignored`, il EXIGE son interpréteur au lieu de sortir vert.
    let python = interpreteur_aiosendspin();
    let temporaire = tempfile::tempdir().unwrap();
    let dossier = dossier_des_journaux().unwrap_or(temporaire.path().to_path_buf());
    std::fs::create_dir_all(&dossier).unwrap();
    let journal_tune = std::fs::File::create(dossier.join("tune.log")).unwrap();
    let _ = tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(journal_tune))
        .with_ansi(false)
        .with_env_filter("tune_core::outputs::sendspin=debug,tune_core::sendspin=debug,tune_server::routes::sendspin=debug")
        .try_init();

    let b = Banc::nouveau(&[]).await;
    let a_wav = dossier.join("a-1khz-44100-16.wav");
    let b_wav = dossier.join("b-440hz-48000-24.wav");
    let c_wav = dossier.join("c-660hz-48000-24.wav");
    let pcm_a = sinus_wav(&a_wav, 44_100, 16, 2.0, 1_000.0);
    let pcm_b = sinus_wav(&b_wav, 48_000, 24, 3.0, 440.0);
    let pcm_c = sinus_wav(&c_wav, 48_000, 24, 1.0, 660.0);

    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/sendspin/banc_aiosendspin.py"
    );
    let mut enfant = Command::new(&python)
        .arg("-I")
        .arg(script)
        .args(["--url", &format!("ws://{}/sendspin", b.adresse)])
        .args(["--dossier", dossier.to_str().unwrap()])
        .args([
            "--formats",
            "pcm:44100:16:2,pcm:48000:24:2,flac:48000:24:2,flac:44100:16:2",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("lancer aiosendspin");
    let sortie = BufReader::new(enfant.stdout.take().unwrap());
    let mut lecteur = Lecteur { enfant, sortie };
    let client_id = lecteur.ligne("CLIENT_ID=");
    let jeton = lecteur.ligne("TOKEN=");
    lecteur.ligne("CONNECTE");

    // Appairage « Pairing PSK » par la route opérateur, comme un administrateur.
    let http = reqwest::Client::new();
    let url = format!(
        "http://{}/api/v1/devices/sendspin/{client_id}/pair",
        b.adresse
    );
    let r = http
        .post(&url)
        .json(&json!({"method": "pairing_psk", "token": jeton}))
        .send()
        .await
        .unwrap();
    assert!(
        r.status().is_success(),
        "appairage refuse : {}",
        r.text().await.unwrap()
    );
    let sortie = tokio::time::timeout(Duration::from_secs(20), b.sortie(&client_id))
        .await
        .expect("la zone apparait apres l'appairage et le premier client/state");
    attendre("enceinte disponible", 10, || {
        let s = sortie.clone();
        async move { s.lock().await.is_available().await }
    })
    .await;

    // A puis B, enchaînées sans coupure (B préparée comme le fait le poller).
    let (a, bb, c) = (
        a_wav.to_str().unwrap().to_owned(),
        b_wav.to_str().unwrap().to_owned(),
        c_wav.to_str().unwrap().to_owned(),
    );
    sortie
        .lock()
        .await
        .play_media(&jouer(&a, 44_100, 2_000))
        .await
        .unwrap();
    sortie
        .lock()
        .await
        .set_next_media(&jouer(&bb, 48_000, 3_000))
        .await
        .unwrap();
    attendre("B devient la piste courante", 10, || {
        let s = sortie.clone();
        let bb = bb.clone();
        async move {
            s.lock()
                .await
                .get_status()
                .await
                .unwrap()
                .current_uri
                .as_deref()
                == Some(bb.as_str())
        }
    })
    .await;

    // Volume : server/command, puis l'état renvoyé par l'enceinte remonte.
    sortie.lock().await.set_volume(0.4).await.unwrap();
    attendre("volume 40 reporte par l'enceinte", 5, || {
        let s = sortie.clone();
        async move { (s.lock().await.get_status().await.unwrap().volume - 0.4).abs() < 1e-9 }
    })
    .await;

    // Pause, reprise.
    tokio::time::sleep(Duration::from_millis(300)).await;
    sortie.lock().await.pause().await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    sortie.lock().await.resume().await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;

    // L'enceinte préfère FLAC 48 kHz/24 en pleine lecture.
    lecteur.commande("FORMAT flac:48000:24:2");
    lecteur.ligne("FORMAT_ENVOYE");
    tokio::time::sleep(Duration::from_millis(2_000)).await;

    // Saut de piste vers C (même format préféré : FLAC 48 kHz/24, sans perte).
    sortie
        .lock()
        .await
        .play_media(&jouer(&c, 48_000, 1_000))
        .await
        .unwrap();
    attendre("fin naturelle de C", 15, || {
        let s = sortie.clone();
        async move { s.lock().await.get_status().await.unwrap().ended_naturally }
    })
    .await;
    sortie.lock().await.stop().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    drop(lecteur.enfant.stdin.take());
    lecteur.ligne("JOURNAL_ECRIT");
    assert!(lecteur.enfant.wait().unwrap().success());
    let journal: Value =
        serde_json::from_slice(&std::fs::read(dossier.join("journal.json")).unwrap()).unwrap();

    // --- Ce que le lecteur de référence a reçu ---------------------------
    let evenements: Vec<String> = journal["evenements"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] != "commande_format")
        .map(|e| match e["type"].as_str().unwrap() {
            "group/update" => format!("group/{}", e["playback_state"].as_str().unwrap()),
            "server/command" => format!("command/{}", e["command"].as_str().unwrap()),
            "stream/start" => format!(
                "start/{}{}/{}",
                e["format"]["codec"].as_str().unwrap(),
                e["format"]["sample_rate"],
                e["format"]["bit_depth"]
            ),
            t => t.to_owned(),
        })
        .collect();
    eprintln!("evenements recus par aiosendspin : {evenements:?}");
    let attendus = [
        "group/stopped",
        "group/playing",
        "start/pcm44100/16",
        "start/pcm48000/24",
        "command/volume",
        "stream/end",
        "group/stopped",
        "group/playing",
        "start/pcm48000/24",
        "start/flac48000/24",
        "stream/clear",
        "stream/end",
        "group/stopped",
    ];
    assert_eq!(
        evenements, attendus,
        "sequence de controle vue par aiosendspin"
    );

    // Un fichier par stream/start et par stream/clear : A, B (PCM), B reprise
    // (PCM), B (FLAC, en place), C (FLAC, après le saut de piste).
    let flux = journal["flux"].as_array().unwrap();
    assert_eq!(flux.len(), 5, "quatre stream/start et un stream/clear");
    // Ligne de temps : continue dans chaque flux ET à travers les deux
    // stream/start en place (A → B, PCM → FLAC).
    let mut dernier_fin: Option<i64> = None;
    for (n, f) in flux.iter().enumerate() {
        let taux = f["format"]["sample_rate"].as_i64().unwrap();
        let morceaux = f["morceaux"].as_array().unwrap();
        assert!(!morceaux.is_empty(), "flux {n} vide");
        let t0 = morceaux[0]["ts"].as_i64().unwrap();
        if matches!(n, 1 | 3) {
            assert_eq!(
                Some(t0),
                dernier_fin,
                "flux {n} : premier morceau du nouveau format a la suite du dernier de l'ancien"
            );
        }
        let mut cumul = 0i64;
        for (k, m) in morceaux.iter().enumerate() {
            let ts = m["ts"].as_i64().unwrap();
            assert_eq!(
                ts,
                t0 + cumul * 1_000_000 / taux,
                "flux {n} morceau {k} : ligne de temps"
            );
            let trames = m["trames"].as_i64().unwrap();
            assert!(
                trames * 1_000_000 / taux <= 150_000,
                "flux {n} morceau {k} > 150 ms"
            );
            if m["synchro"].as_bool().unwrap() {
                let avance =
                    m["lecture_predite"].as_i64().unwrap() - m["arrivee"].as_i64().unwrap();
                assert!(
                    avance > 0,
                    "flux {n} morceau {k} arrive en retard de {} us",
                    -avance
                );
            }
            cumul += trames;
        }
        dernier_fin = Some(t0 + cumul * 1_000_000 / taux);
    }

    // Bit-exact : A entière ; B en trois tronçons (avant pause, reprise PCM,
    // puis FLAC), chacun aligné exactement dans B ; C entière en FLAC.
    let ref_a = reference(&pcm_a, 16);
    let ref_b = reference(&pcm_b, 24);
    let ref_c = reference(&pcm_c, 24);
    assert!(
        echantillons(&flux[0]) == ref_a,
        "A (PCM 16) recue octet pour octet"
    );
    let b1 = echantillons(&flux[1]);
    let b2 = echantillons(&flux[2]);
    let b3 = echantillons(&flux[3]);
    assert!(
        echantillons(&flux[4]) == ref_c,
        "C (FLAC 24) recue sans perte apres le saut de piste"
    );
    let o1 = aligner(&b1, &ref_b).expect("B avant pause : une tranche exacte de B");
    assert_eq!(o1, 0, "B commence au debut");
    let o2 = aligner(&b2, &ref_b).expect("B reprise : une tranche exacte de B");
    let o3 = aligner(&b3, &ref_b).expect("B en FLAC : une tranche exacte de B");
    assert_eq!(
        o3,
        o2 + b2.len(),
        "le FLAC reprend a l'echantillon qui suit le dernier PCM"
    );

    // Horloge : le filtre de temps d'aiosendspin a convergé, erreur < 1 ms.
    let horloge = journal["horloge"].as_array().unwrap();
    let derniere = horloge.last().expect("mesures d'horloge");
    let erreur = derniere["erreur_us"].as_f64().expect("filtre converge");
    eprintln!(
        "horloge : {} mesures, erreur {erreur} us, derive {}",
        derniere["count"], derniere["derive"]
    );
    assert!(
        erreur < 1_000.0,
        "erreur du filtre de temps {erreur} us >= 1 ms"
    );
}
