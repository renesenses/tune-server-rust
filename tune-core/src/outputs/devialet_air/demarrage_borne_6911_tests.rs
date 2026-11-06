//! #6911 — une source distante ne tient plus la sortie Devialet AIR.
//!
//! Sur un vrai Expert D400 (rc9), une radio ne jouait pas et la zone ne
//! s'arrêtait plus : `lancer` téléchargeait TOUT le corps de
//! `/stream/<id>.wav` avant de décoder. Une radio n'a pas de fin : la sortie,
//! et la zone qui attend son verrou, restaient prises jusqu'au délai de 600 s
//! du client HTTP, sans qu'aucun paquet ne parte vers l'ampli.
//!
//! Les témoins jouent face au simulateur d'ampli et à une fausse source HTTP
//! locale qui ignore `Range` (comme une session WAV à la volée). Aucun son
//! n'est produit : le PCM est du silence ou un sinus lu par le simulateur.

use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;

/// Ce que la fausse source envoie après ses en-têtes HTTP.
#[derive(Clone)]
enum Corps {
    /// Une radio : un en-tête WAV « à la volée », puis du silence sans fin,
    /// au rythme de la lecture.
    SansFin,
    /// Rien du tout : la connexion reste ouverte, muette.
    Muet,
    /// Un fichier fini, envoyé d'un bloc.
    Fini(Arc<Vec<u8>>),
}

/// Un serveur HTTP local qui ne connaît pas `Range` : toute requête reçoit
/// `200` et le même corps.
struct FausseSource {
    adresse: SocketAddr,
    tache: tokio::task::JoinHandle<()>,
}

impl Drop for FausseSource {
    fn drop(&mut self) {
        self.tache.abort();
    }
}

impl FausseSource {
    async fn demarrer(corps: Corps) -> Self {
        let ecoute = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let adresse = ecoute.local_addr().unwrap();
        let tache = tokio::spawn(async move {
            loop {
                let Ok((mut connexion, _)) = ecoute.accept().await else {
                    return;
                };
                let corps = corps.clone();
                tokio::spawn(async move {
                    let mut requete = Vec::new();
                    let mut tampon = [0u8; 1024];
                    while !requete.windows(4).any(|w| w == b"\r\n\r\n") {
                        match connexion.read(&mut tampon).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => requete.extend_from_slice(&tampon[..n]),
                        }
                    }
                    let entetes = "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\n\
                                   Connection: close\r\n\r\n";
                    if connexion.write_all(entetes.as_bytes()).await.is_err() {
                        return;
                    }
                    match corps {
                        Corps::SansFin => {
                            let entete =
                                crate::audio::wav::build_wav_header_streaming(2, 44_100, 16);
                            if connexion.write_all(&entete).await.is_err() {
                                return;
                            }
                            // 50 ms de silence toutes les 50 ms.
                            let silence = vec![0u8; 2_205 * 4];
                            loop {
                                if connexion.write_all(&silence).await.is_err() {
                                    return;
                                }
                                tokio::time::sleep(Duration::from_millis(50)).await;
                            }
                        }
                        Corps::Muet => {
                            // Assez longtemps pour dépasser l'attente bornée,
                            // puis la connexion se ferme : le fil du décodeur
                            // ne reste pas bloqué après le test.
                            tokio::time::sleep(Duration::from_secs(4)).await;
                        }
                        Corps::Fini(octets) => {
                            let _ = connexion.write_all(&octets).await;
                        }
                    }
                });
            }
        });
        Self { adresse, tache }
    }

    fn url(&self, chemin: &str) -> String {
        format!("http://{}{chemin}", self.adresse)
    }
}

/// Une piste distante comme l'orchestrateur la donne à la sortie : une URL,
/// pas de fichier local.
fn media_distant(url: &str, duree_ms: Option<u64>) -> PlayMedia<'_> {
    PlayMedia {
        url,
        file_path: None,
        mime_type: "audio/wav",
        duration_ms: duree_ms,
        ..Default::default()
    }
}

/// La borne au-delà de laquelle un démarrage tient la zone : bien plus que
/// l'attente du premier son du banc (1 s), bien moins que les 600 s d'avant.
const BORNE: Duration = Duration::from_secs(5);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn air_6911_une_radio_sans_fin_demarre_sans_tenir_la_sortie() {
    let b = banc(annonce_de_test(), true).await;
    let radio = FausseSource::demarrer(Corps::SansFin).await;
    let url = radio.url("/stream/radio.wav");

    let debut = Instant::now();
    let lancement = tokio::time::timeout(BORNE, b.sortie.play_media(&media_distant(&url, None)))
        .await
        .unwrap_or_else(|_| {
            panic!(
                "#6911 : play_media tient la sortie plus de {BORNE:?} sur une radio sans fin \
                 (la source est téléchargée en entier avant le décodage)"
            )
        });
    lancement.expect("la radio doit démarrer");
    assert!(debut.elapsed() < BORNE);
    assert_eq!(
        b.sortie.get_status().await.unwrap().state,
        TransportState::Playing
    );

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !b.sim.etat().paquets.is_empty(),
        "des paquets audio doivent partir vers l'ampli"
    );

    let arret = tokio::time::timeout(Duration::from_secs(2), b.sortie.stop()).await;
    assert!(
        arret.is_ok(),
        "#6911 : l'arrêt d'une radio doit rendre la main"
    );
    assert_eq!(
        b.sortie.get_status().await.unwrap().state,
        TransportState::Stopped
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn air_6911_une_source_muette_echoue_vite_et_le_dit() {
    let b = banc(annonce_de_test(), true).await;
    let source = FausseSource::demarrer(Corps::Muet).await;
    let url = source.url("/stream/muette.wav");

    let refus = tokio::time::timeout(BORNE, b.sortie.play_media(&media_distant(&url, None)))
        .await
        .unwrap_or_else(|_| {
            panic!(
                "#6911 : play_media tient la sortie plus de {BORNE:?} sur une source qui ne \
                 livre rien (aucune attente bornée)"
            )
        })
        .expect_err("une source muette ne doit pas passer pour une lecture");
    assert!(
        refus.contains("EXPERT-TEST") && refus.contains("ne livre aucun son"),
        "l'erreur doit nommer l'ampli et dire ce qui manque : {refus}"
    );
    assert!(
        !refus.contains("127.0.0.1"),
        "l'adresse de la source ne doit pas finir dans le message : {refus}"
    );
    assert_eq!(
        b.sortie.get_status().await.unwrap().state,
        TransportState::Stopped
    );
    assert!(b.sim.etat().paquets.is_empty(), "rien ne part vers l'ampli");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn air_6911_un_ampli_muet_sur_une_radio_arrete_la_zone_en_le_disant() {
    let b = banc(annonce_de_test(), true).await;
    b.sim.etat().muet = true;
    let radio = FausseSource::demarrer(Corps::SansFin).await;
    let url = radio.url("/stream/radio.wav");

    tokio::time::timeout(BORNE, b.sortie.play_media(&media_distant(&url, None)))
        .await
        .expect("#6911 : play_media ne doit pas attendre la réponse de l'ampli")
        .expect("le flux part ; c'est l'ampli qui ne répond pas");
    let st = attendre_l_arret(&b.sortie).await;
    assert!(
        !st.ended_naturally,
        "un ampli muet n'est pas une fin de piste"
    );
    let motif = b
        .sortie
        .take_output_failure()
        .expect("le silence de l'ampli doit remonter au sondeur, donc à l'interface");
    assert!(motif.contains("l'ampli ne répond pas"), "{motif}");
}

/// Le décodage au fil de l'eau ne doit pas perdre la reprise à la position
/// de la pause quand le serveur ignore `Range` : le PCM d'avant est écarté.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn air_6911_la_reprise_sur_une_source_sans_range_repart_de_la_pause() {
    let b = banc(annonce_de_test(), true).await;
    let dossier = crate::test_scratch::scratch_dir("devialet-air-6911-reprise");
    let (chemin, attendu) = wav_16_bits(&dossier, 44_100, 66_150);
    let octets = Arc::new(std::fs::read(&chemin).unwrap());
    let source = FausseSource::demarrer(Corps::Fini(octets)).await;
    let url = source.url("/stream/piste.wav");

    b.sortie
        .play_media(&media_distant(&url, Some(1_500)))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    b.sortie.pause().await.unwrap();
    let position_ms = b.sortie.get_status().await.unwrap().position_ms;
    assert!(position_ms > 0, "position mémorisée");
    b.sortie.resume().await.unwrap();
    let st = attendre_l_arret(&b.sortie).await;
    assert!(st.ended_naturally, "la reprise va jusqu'au bout");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let s = b.sim.etat();
    assert_eq!(s.paquets.len(), 2, "un flux neuf à la reprise");
    let second = *s.paquets.keys().nth(1).unwrap();
    let pcm = s.pcm(second).expect("second flux complet");
    let tete = trames_arrondies(44_100, CADENCE_DE_TEST.silence_de_tete) as usize;
    let voulu = (position_ms * 44_100 / 1_000) as usize;
    let echantillon = &pcm[tete..tete + 200];
    let trouve = (voulu.saturating_sub(4)..=voulu + 4)
        .find(|&k| attendu.get(k..k + 200) == Some(echantillon));
    assert!(
        trouve.is_some(),
        "la reprise doit partir de la trame {voulu} (±4) de la piste, pas du début"
    );
}
