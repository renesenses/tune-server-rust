//! #7642 — le convolveur de `StreamingDsp` garde-t-il son état d'un tampon à
//! l'autre ?
//!
//! Le relais du bras progressif (`spawn_streaming_dsp_relay`) passe le PCM
//! tampon après tampon dans la MÊME instance. Le témoin y fait passer un
//! signal connu — des impulsions isolées — à travers une IR connue — deux
//! taps, à 300 et 1 500 trames, donc une queue plus longue que le bloc de
//! 1 024 et que la plupart des tampons — et compare la sortie à la
//! convolution calculée directement. Une impulsion dont l'écho tombe dans un
//! tampon suivant doit l'y retrouver, à sa place.
//!
//! Aucun son n'est joué.

use super::*;

const CANAUX: usize = 2;
const TRAMES: usize = 8_000;
const BLOC: usize = 1_024;
const TOLERANCE_LSB: i32 = 2;

/// L'IR : 0,5 à 300 trames, 1,0 à 1 500 trames. Identique sur les deux canaux.
fn ir() -> Vec<f32> {
    let mut h = vec![0.0f32; 2_000];
    h[300] = 0.5;
    h[1_500] = 1.0;
    h
}

/// Impulsions isolées, 16 bits stéréo : (trame, canal, valeur).
const IMPULSIONS: [(usize, usize, i16); 5] = [
    (900, 0, 8_192),
    (3_100, 0, 8_192),
    (5_000, 0, -8_192),
    (1_200, 1, 8_192),
    (4_321, 1, -8_192),
];

fn entree(trames: usize) -> Vec<i16> {
    let mut x = vec![0i16; trames * CANAUX];
    for (t, c, v) in IMPULSIONS {
        if t < trames {
            x[t * CANAUX + c] = v;
        }
    }
    x
}

/// La convolution attendue, calculée directement (f64) et arrondie.
fn attendu(x: &[i16]) -> Vec<i16> {
    let h = ir();
    let trames = x.len() / CANAUX;
    let mut y = vec![0i16; x.len()];
    for c in 0..CANAUX {
        for n in 0..trames {
            let mut acc = 0.0f64;
            for (k, &hk) in h.iter().enumerate() {
                if hk != 0.0 && k <= n {
                    acc += f64::from(hk) * f64::from(x[(n - k) * CANAUX + c]);
                }
            }
            y[n * CANAUX + c] = acc.round() as i16;
        }
    }
    y
}

fn octets(x: &[i16]) -> Vec<u8> {
    x.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn echantillons(b: &[u8]) -> Vec<i16> {
    b.chunks_exact(2)
        .map(|p| i16::from_le_bytes([p[0], p[1]]))
        .collect()
}

fn dsp_convolveur() -> StreamingDsp {
    let h = ir();
    StreamingDsp {
        convolver: Some(crate::audio::convolver::Convolver::new(
            &[h.clone(), h],
            BLOC,
        )),
        channels: CANAUX as u16,
        sample_rate: 44_100,
        ..Default::default()
    }
}

fn comparer(obtenu: &[i16], attendu: &[i16], quoi: &str) {
    assert_eq!(
        obtenu.len(),
        attendu.len(),
        "{quoi} : le flux doit rendre exactement autant d'échantillons qu'il en reçoit"
    );
    let ecarts: Vec<(usize, i16, i16)> = obtenu
        .iter()
        .zip(attendu)
        .enumerate()
        .filter(|(_, (o, a))| (i32::from(**o) - i32::from(**a)).abs() > TOLERANCE_LSB)
        .map(|(i, (o, a))| (i / CANAUX, *o, *a))
        .take(8)
        .collect();
    assert!(
        ecarts.is_empty(),
        "#7642 — {quoi} : l'état du convolveur est corrompu entre deux tampons, \
         la sortie diverge de la convolution directe (trame, obtenu, attendu) : {ecarts:?}"
    );
}

/// Fait passer `x` dans le relais réel, découpé en tampons de `tailles`
/// trames (le reste en dernier), derrière l'en-tête WAV.
async fn par_le_relais(x: &[i16], tailles: &[usize]) -> Vec<i16> {
    let (aval_tx, mut aval_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let amont = spawn_streaming_dsp_relay(dsp_convolveur(), 16, true, aval_tx);
    let entete = crate::audio::wav::build_wav_header(CANAUX as u16, 44_100, 16).to_vec();
    let lecteur = tokio::spawn(async move {
        let mut tout = Vec::new();
        let premier = aval_rx.recv().await.expect("en-tête attendu");
        assert_eq!(&premier[0..4], b"RIFF", "l'en-tête doit passer intact");
        while let Some(t) = aval_rx.recv().await {
            tout.extend_from_slice(&t);
        }
        tout
    });
    amont.send(entete).await.unwrap();
    let mut debut = 0usize;
    let trames = x.len() / CANAUX;
    for &t in tailles.iter().chain(std::iter::once(&usize::MAX)) {
        if debut >= trames {
            break;
        }
        let fin = trames.min(debut.saturating_add(t));
        amont
            .send(octets(&x[debut * CANAUX..fin * CANAUX]))
            .await
            .unwrap();
        debut = fin;
    }
    drop(amont);
    echantillons(&lecteur.await.unwrap())
}

/// LE témoin : des tampons de tailles irrégulières, plus courts et plus longs
/// que le bloc et que la queue de l'IR.
#[tokio::test]
async fn le_convolveur_du_relais_garde_son_etat_entre_les_tampons_7642() {
    let x = entree(TRAMES);
    let obtenu = par_le_relais(&x, &[700, 1_500, 333, 2_048, 1_000, 64]).await;
    comparer(&obtenu, &attendu(&x), "relais en tampons successifs");
}

/// Le même flux en tampons de taille fixe, plus petits que le bloc.
#[tokio::test]
async fn le_convolveur_du_relais_supporte_des_tampons_plus_petits_que_le_bloc_7642() {
    let x = entree(TRAMES);
    let obtenu = par_le_relais(&x, &[256; 64]).await;
    comparer(&obtenu, &attendu(&x), "relais en tampons de 256 trames");
}

/// Un flux plus court que la latence du convolveur : tout sort au drainage
/// final, et rien ne manque.
#[tokio::test]
async fn un_flux_plus_court_que_la_latence_sort_en_entier_7642() {
    let mut x = vec![0i16; 600 * CANAUX];
    x[100 * CANAUX] = 8_192;
    let obtenu = par_le_relais(&x, &[250, 250]).await;
    comparer(&obtenu, &attendu(&x), "flux court");
    assert_eq!(
        obtenu[400 * CANAUX],
        4_096,
        "l'écho à 300 trames doit sortir"
    );
}

/// Contre-épreuve : un appel UNIQUE sur la piste entière (bras DASH et
/// HTTPS, `StreamingDsp::process`) rend déjà la convolution exacte — c'est le
/// chaînage des tampons, et lui seul, que le témoin met en cause.
#[test]
fn un_appel_unique_sur_la_piste_entiere_est_exact_7642() {
    let x = entree(TRAMES);
    let mut pcm = octets(&x);
    dsp_convolveur().process(&mut pcm, 16);
    comparer(&echantillons(&pcm), &attendu(&x), "appel unique");
}

/// Contre-épreuve : sans convolveur, le relais ne change pas le découpage —
/// chaque tampon ressort tel qu'il est entré (ici, traité par un ReplayGain
/// neutre).
#[tokio::test]
async fn sans_convolveur_le_relais_garde_le_decoupage_7642() {
    let (aval_tx, mut aval_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let dsp = StreamingDsp {
        replaygain: Some(1.0),
        ..Default::default()
    };
    let amont = spawn_streaming_dsp_relay(dsp, 16, false, aval_tx);
    let a = octets(&entree(300));
    let b = octets(&entree(500)[..200 * CANAUX]);
    amont.send(a.clone()).await.unwrap();
    amont.send(b.clone()).await.unwrap();
    drop(amont);
    assert_eq!(aval_rx.recv().await.unwrap().len(), a.len());
    assert_eq!(aval_rx.recv().await.unwrap().len(), b.len());
    assert!(
        aval_rx.recv().await.is_none(),
        "aucun tampon de drainage sans convolveur"
    );
}
