//! #7148 — la sentinelle doit savoir ROUGIR : une URL cassée (`amp;` laissé
//! dans la requête, le défaut de #7106), un flux qui ne se décode pas, une
//! page HTML, un service déconnecté. Et verdir sur un flux sain.
//!
//! Le petit serveur HTTP ci-dessous n'imite AUCUN service réel : il ne sert
//! qu'à montrer que la sentinelle distingue un 200 décodable d'un refus ou
//! d'octets illisibles. La preuve sur les vrais services, c'est la sentinelle
//! elle-même, sur le .18.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::TuneError;
use crate::streaming::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamQuality,
    StreamTrack, StreamingService,
};

/// Un WAV PCM 16 bits stéréo de `secondes`, sinusoïde à 440 Hz.
fn wav(secondes: u32) -> Vec<u8> {
    wav_de(44_100 * secondes, 8000.0)
}

/// Un WAV PCM 16 bits stéréo à 44,1 kHz de `trames` trames, sinusoïde à
/// 440 Hz d'amplitude `amplitude` (0 : silence numérique).
fn wav_de(trames: u32, amplitude: f32) -> Vec<u8> {
    let cadence = 44_100u32;
    let donnees = trames * 4;
    let mut v = Vec::with_capacity(44 + donnees as usize);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + donnees).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&cadence.to_le_bytes());
    v.extend_from_slice(&(cadence * 4).to_le_bytes());
    v.extend_from_slice(&4u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&donnees.to_le_bytes());
    for i in 0..trames {
        let s =
            ((i as f32 * 440.0 * std::f32::consts::TAU / cadence as f32).sin() * amplitude) as i16;
        v.extend_from_slice(&s.to_le_bytes());
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

/// Un serveur HTTP d'essai : `/sain.wav` sert un WAV, SAUF si la requête
/// contient `amp;` (403, comme le CDN de TIDAL le 26/10) ; `/illisible.flac`
/// sert des octets qui ne sont pas un FLAC ; `/page.flac` une page HTML ;
/// `/tete-html.flac` une page HTML annoncée `audio/flac` (Qobuz, #7805) ;
/// `/instable.wav` des octets illisibles à la PREMIÈRE requête, puis le WAV.
async fn serveur() -> String {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port d'essai");
    let adresse = ecoute.local_addr().expect("adresse");
    let son = Arc::new(wav(4));
    let instables = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        loop {
            let Ok((mut flux, _)) = ecoute.accept().await else {
                return;
            };
            let son = son.clone();
            let instables = instables.clone();
            tokio::spawn(async move {
                let mut tampon = vec![0u8; 8192];
                let mut lu = 0;
                loop {
                    let Ok(n) = flux.read(&mut tampon[lu..]).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    lu += n;
                    if tampon[..lu].windows(4).any(|w| w == b"\r\n\r\n") || lu == tampon.len() {
                        break;
                    }
                }
                let requete = String::from_utf8_lossy(&tampon[..lu]).to_string();
                let cible = requete.split(' ').nth(1).unwrap_or("/").to_string();
                let (statut, type_, corps): (&str, &str, Vec<u8>) = if cible.contains("amp;") {
                    ("403 Forbidden", "text/plain", b"forbidden".to_vec())
                } else if cible.starts_with("/sain.wav") {
                    ("200 OK", "audio/wav", (*son).clone())
                } else if cible.starts_with("/illisible.flac") {
                    ("200 OK", "audio/flac", vec![0x5Au8; 64 * 1024])
                } else if cible.starts_with("/instable.wav") {
                    if instables.fetch_add(1, Ordering::SeqCst) == 0 {
                        ("200 OK", "audio/wav", vec![0x5Au8; 64 * 1024])
                    } else {
                        ("200 OK", "audio/wav", (*son).clone())
                    }
                } else if cible.starts_with("/tete-html.flac") {
                    let mut page = b"<!DOCTYPE html><html><body>".to_vec();
                    page.resize(4096, b' ');
                    ("200 OK", "audio/flac", page)
                } else if cible.starts_with("/page.flac") {
                    (
                        "200 OK",
                        "text/html; charset=utf-8",
                        b"<html><body>Bienvenue</body></html>".to_vec(),
                    )
                } else {
                    ("404 Not Found", "text/plain", b"absent".to_vec())
                };
                let entete = format!(
                    "HTTP/1.1 {statut}\r\nContent-Type: {type_}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    corps.len()
                );
                let _ = flux.write_all(entete.as_bytes()).await;
                let _ = flux.write_all(&corps).await;
                let _ = flux.shutdown().await;
            });
        }
    });
    format!("http://{adresse}")
}

fn flux(url: String, codec: &str) -> StreamUrl {
    StreamUrl {
        url,
        mime_type: String::new(),
        quality: StreamQuality {
            codec: codec.into(),
            sample_rate: 44_100,
            bit_depth: 16,
            bitrate: None,
            channels: 2,
        },
        expires_at: None,
        headers: Vec::new(),
    }
}

#[tokio::test]
async fn un_flux_sain_se_decode_et_verdit() {
    let base = serveur().await;
    let d = sonder_un_flux(&flux(format!("{base}/sain.wav?sig=abc&exp=1"), "WAV"))
        .await
        .expect("un WAV sain se décode");
    assert_eq!(d.sample_rate, 44_100);
    assert_eq!(d.channels, 2);
    assert!(d.echantillons > 0);
    assert!(d.octets_lus > 0);
}

#[tokio::test]
async fn une_url_avec_amp_laisse_dans_la_requete_rougit_en_http() {
    // Le défaut exact de #7106 en réel : `&amp;` désechappé d'un niveau
    // seulement, le CDN refuse.
    let base = serveur().await;
    let echec = sonder_un_flux(&flux(
        format!("{base}/sain.wav?token=SECRET123&amp;info=1"),
        "WAV",
    ))
    .await
    .expect_err("une requête avec amp; doit rougir");
    assert_eq!(echec.cause, Cause::Http, "{echec:?}");
    assert!(echec.detail.contains("403"), "{}", echec.detail);
    assert!(
        echec.detail.contains("[requête avec amp;]"),
        "le défaut doit être NOMMÉ : {}",
        echec.detail
    );
    assert!(
        !echec.detail.contains("SECRET123"),
        "une signature ne part jamais dans le détail : {}",
        echec.detail
    );
}

#[tokio::test]
async fn des_octets_illisibles_rougissent_en_decodage() {
    let base = serveur().await;
    let echec = sonder_un_flux(&flux(format!("{base}/illisible.flac"), "FLAC"))
        .await
        .expect_err("des octets qui ne sont pas un FLAC doivent rougir");
    assert_eq!(echec.cause, Cause::Decodage, "{echec:?}");
}

#[tokio::test]
async fn une_page_html_rougit_en_http() {
    let base = serveur().await;
    let echec = sonder_un_flux(&flux(format!("{base}/page.flac"), "FLAC"))
        .await
        .expect_err("une page HTML n'est pas un flux");
    assert_eq!(echec.cause, Cause::Http, "{echec:?}");
}

#[tokio::test]
async fn un_dash_assemble_sur_disque_se_decode_et_un_fichier_vide_rougit() {
    let sain = crate::test_scratch::scratch_file("tune-sentinelle-dash", ".wav");
    std::fs::write(sain.path(), wav(2)).expect("écriture");
    let d = sonder_un_flux(&flux(format!("file://{}", sain.as_str()), "WAV"))
        .await
        .expect("un fichier sain se décode");
    assert!(d.echantillons > 0);

    let vide = crate::test_scratch::scratch_file("tune-sentinelle-dash-vide", ".mp4");
    std::fs::write(vide.path(), b"").expect("écriture");
    let echec = sonder_un_flux(&flux(format!("file://{}", vide.as_str()), "FLAC"))
        .await
        .expect_err("un DASH vide doit rougir");
    assert_eq!(echec.cause, Cause::Resolution);
}

/// Le fichier MP4 fragmenté d'essai : un `ftyp` + `moov` de 725 octets, puis
/// six segments `moof` + `mdat` (3 s d'AAC stéréo à 44,1 kHz) et un `mfra`.
fn mp4_fragmente() -> Vec<u8> {
    std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/aac320_frag.mp4"
    ))
    .expect("tests/fixtures/aac320_frag.mp4")
}
/// La fin de l'en-tête, puis la fin de chaque segment (le dernier avec le `mfra`).
const FIN_DE_L_EN_TETE: usize = 725;
const FINS_DES_SEGMENTS: [usize; 6] = [36_521, 77_835, 122_248, 160_292, 198_340, 238_382];

#[tokio::test]
async fn un_mp4_reduit_a_son_en_tete_rougit_en_decodage() {
    let entete = crate::test_scratch::scratch_file("tune-sentinelle-entete-7805", ".mp4");
    std::fs::write(entete.path(), &mp4_fragmente()[..FIN_DE_L_EN_TETE]).expect("écriture");
    let echec = sonder_un_flux(&flux(format!("file://{}", entete.as_str()), "AAC"))
        .await
        .expect_err("#7805 : un en-tête MP4 seul n'est pas un flux qui se lit");
    assert_eq!(echec.cause, Cause::Decodage, "{echec:?}");
}

#[tokio::test]
async fn un_dash_qui_grandit_pendant_la_sonde_compte_sa_taille_finale() {
    // #7805 : avec TUNE_DASH_STREAM_DECODE, TIDAL rend le chemin dès l'en-tête
    // écrit, enregistre une poignée de croissance, et ajoute les segments
    // pendant que le décodeur les attend. Ici, le premier segment arrive seul
    // (1,02 s, moins que les 3 s décodées), puis tout le reste d'un bloc : le
    // décodeur attend donc forcément la fin de l'écriture.
    let complet = mp4_fragmente();
    assert_eq!(complet.len(), FINS_DES_SEGMENTS[5]);
    let dash = crate::test_scratch::scratch_file("tune-dash-7805", ".mp4");
    std::fs::write(dash.path(), &complet[..FIN_DE_L_EN_TETE]).expect("écriture");
    let chemin = dash.as_str().into_owned();
    let croissance = crate::audio::dash_growth::DashGrowth::new(FIN_DE_L_EN_TETE as u64);
    crate::audio::dash_growth::register(&chemin, croissance.clone());
    let redacteur = {
        let chemin = chemin.clone();
        std::thread::spawn(move || {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&chemin)
                .expect("ouverture en ajout");
            let mut debut = FIN_DE_L_EN_TETE;
            for fin in [FINS_DES_SEGMENTS[0], FINS_DES_SEGMENTS[5]] {
                std::thread::sleep(Duration::from_millis(40));
                f.write_all(&complet[debut..fin])
                    .expect("ajout d'un segment");
                f.flush().expect("flush");
                croissance.advance(fin as u64);
                debut = fin;
            }
            croissance.finish();
        })
    };
    let d = sonder_un_flux(&flux(format!("file://{chemin}"), "AAC"))
        .await
        .expect("un DASH avec ses segments se décode");
    redacteur.join().expect("rédacteur");
    assert_eq!(d.sample_rate, 44_100);
    assert_eq!(
        d.octets_lus, FINS_DES_SEGMENTS[5] as u64,
        "#7805 : octets_lus doit être la taille après décodage, pas celle de l'en-tête ({FIN_DE_L_EN_TETE})"
    );
}

#[tokio::test]
async fn un_dash_avec_un_segment_verdit() {
    // Deux segments : un seul (45 056 trames, 1,02 s) frôle le seuil d'une seconde.
    let deux = crate::test_scratch::scratch_file("tune-sentinelle-deux-segments-7805", ".mp4");
    std::fs::write(deux.path(), &mp4_fragmente()[..FINS_DES_SEGMENTS[1]]).expect("écriture");
    let d = sonder_un_flux(&flux(format!("file://{}", deux.as_str()), "AAC"))
        .await
        .expect("un DASH avec ses premiers segments se décode");
    assert!(d.echantillons / 2 >= 44_100, "{d:?}");
    assert_eq!(d.octets_lus, FINS_DES_SEGMENTS[1] as u64);
}

#[tokio::test]
async fn moins_d_une_seconde_ou_du_silence_rougit_en_decodage() {
    let court = crate::test_scratch::scratch_file("tune-sentinelle-court-7805", ".wav");
    std::fs::write(court.path(), wav_de(22_050, 8000.0)).expect("écriture");
    let echec = sonder_un_flux(&flux(format!("file://{}", court.as_str()), "WAV"))
        .await
        .expect_err("#7805 : une demi-seconde décodée ne suffit pas à dire « ok »");
    assert_eq!(echec.cause, Cause::Decodage, "{echec:?}");

    let muet = crate::test_scratch::scratch_file("tune-sentinelle-muet-7805", ".wav");
    std::fs::write(muet.path(), wav_de(2 * 44_100, 0.0)).expect("écriture");
    let echec = sonder_un_flux(&flux(format!("file://{}", muet.as_str()), "WAV"))
        .await
        .expect_err("#7805 : du silence numérique ne suffit pas à dire « ok »");
    assert_eq!(echec.cause, Cause::Decodage, "{echec:?}");
}

#[tokio::test]
async fn un_echec_de_decodage_dit_les_premiers_octets_et_les_en_tetes() {
    let base = serveur().await;
    let echec = sonder_un_flux(&flux(
        format!("{base}/tete-html.flac?sig=SECRET123"),
        "FLAC",
    ))
    .await
    .expect_err("une page HTML annoncée audio/flac ne se décode pas");
    assert_eq!(echec.cause, Cause::Decodage, "{echec:?}");
    for attendu in [
        "3c 21 44 4f 43 54 59 50 45 20 68 74 6d 6c 3e 3c",
        "Content-Type : audio/flac",
        "Content-Length : 4096",
        "octets lus : 4096",
    ] {
        assert!(
            echec.detail.contains(attendu),
            "#7805 : le détail doit contenir « {attendu} » : {}",
            echec.detail
        );
    }
    assert!(
        !echec.detail.contains("SECRET123") && !echec.detail.contains("127.0.0.1"),
        "l'adresse signée ne part jamais dans le détail : {}",
        echec.detail
    );
}

#[tokio::test]
async fn un_echec_a_la_sonde_du_format_est_reessaye_une_fois() {
    let base = serveur().await;
    let d = sonder_un_flux(&flux(format!("{base}/instable.wav"), "WAV"))
        .await
        .expect("#7805 : un premier GET illisible puis un sain doit verdir au second essai");
    assert_eq!(d.sample_rate, 44_100);
}

/// Un service factice qui rend l'URL qu'on lui donne, ou une erreur.
struct ServiceFactice {
    connecte: bool,
    reponse: Result<String, String>,
}

#[async_trait::async_trait]
impl StreamingService for ServiceFactice {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        "factice"
    }
    fn enabled(&self) -> bool {
        true
    }
    fn set_enabled(&mut self, _enabled: bool) {}
    async fn authenticate(&mut self, _c: &serde_json::Value) -> Result<AuthStatus, TuneError> {
        Ok(AuthStatus::default())
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: self.connecte,
            ..AuthStatus::default()
        }
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_track(&self, _id: &str) -> Result<StreamTrack, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_track_url(&self, _id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        match &self.reponse {
            Ok(url) => Ok(flux(url.clone(), "WAV")),
            Err(e) => Err(e.clone().into()),
        }
    }
    async fn get_album(&self, _id: &str) -> Result<StreamAlbum, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_artist(&self, _id: &str) -> Result<StreamArtist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist(&self, _id: &str) -> Result<StreamPlaylist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn le_service_passe_par_la_resolution_puis_le_decodage() {
    let base = serveur().await;
    let mut sain = ServiceFactice {
        connecte: true,
        reponse: Ok(format!("{base}/sain.wav?a=1&b=2")),
    };
    sonder_le_service(&mut sain, "factice", "42")
        .await
        .expect("un service sain verdit");

    let mut casse = ServiceFactice {
        connecte: true,
        reponse: Ok(format!("{base}/sain.wav?a=1&amp;b=2")),
    };
    let echec = sonder_le_service(&mut casse, "factice", "42")
        .await
        .expect_err("une URL cassée rendue par le service rougit");
    assert_eq!(echec.cause, Cause::Http);
}

#[tokio::test]
async fn un_service_deconnecte_ou_qui_refuse_le_jeton_rougit_en_authentification() {
    let mut deconnecte = ServiceFactice {
        connecte: false,
        reponse: Ok("http://127.0.0.1:9/x".into()),
    };
    let echec = sonder_le_service(&mut deconnecte, "factice", "42")
        .await
        .expect_err("déconnecté");
    assert_eq!(echec.cause, Cause::Authentification);

    let mut refuse = ServiceFactice {
        connecte: true,
        reponse: Err("factice: HTTP 401 Unauthorized".into()),
    };
    let echec = sonder_le_service(&mut refuse, "factice", "42")
        .await
        .expect_err("401");
    assert_eq!(echec.cause, Cause::Authentification);
}

#[test]
fn une_radio_illisible_rougit() {
    // Le lecteur radio lui-même : un port fermé ne rend aucun flux.
    let echec = super::super::radio::sonder_une_station_sans_sortie("http://127.0.0.1:9/fip.mp3")
        .expect_err("aucune station sur ce port");
    assert!(echec.contains("radio HTTP"), "{echec}");
}

#[tokio::test]
async fn une_radio_saine_se_decode_par_le_lecteur_radio() {
    let base = serveur().await;
    let adresse = format!("{base}/sain.wav");
    let (cadence, canaux) = tokio::task::spawn_blocking(move || {
        super::super::radio::sonder_une_station_sans_sortie(&adresse)
    })
    .await
    .expect("tâche")
    .expect("un flux PCM se sonde jusqu'à sa première trame");
    assert_eq!(cadence, 44_100);
    assert_eq!(canaux, 2);
}

fn ligne(service: &str, etat: Etat) -> ResultatSonde {
    ResultatSonde {
        service: service.into(),
        nature: "service".into(),
        reference: None,
        etat,
        cause: None,
        detail: None,
        codec: None,
        sample_rate: None,
        channels: None,
        octets_lus: None,
        duree_ms: 0,
    }
}

#[test]
fn le_statut_global_ne_verdit_ni_sans_sonde_ni_sur_un_non_couvert() {
    use RapportSentinelle as R;
    assert_eq!(R::statut_des_resultats(&[]), "echec");
    assert_eq!(
        R::statut_des_resultats(&[ligne("spotify", Etat::NonCouvert)]),
        "echec"
    );
    assert_eq!(
        R::statut_des_resultats(&[ligne("qobuz", Etat::Ok), ligne("spotify", Etat::NonCouvert)]),
        "ok"
    );
    assert_eq!(
        R::statut_des_resultats(&[ligne("qobuz", Etat::Ok), ligne("tidal", Etat::Echec)]),
        "echec"
    );
}

fn tour(tidal: Etat) -> RapportSentinelle {
    RapportSentinelle {
        horodatage: 1,
        statut: "ok".into(),
        raison: None,
        resultats: vec![ligne("qobuz", Etat::Ok), ligne("tidal", tidal)],
        transitions: Vec::new(),
    }
}

fn courant(tidal: Etat) -> Vec<ResultatSonde> {
    vec![ligne("qobuz", Etat::Ok), ligne("tidal", tidal)]
}

#[test]
fn les_transitions_disent_le_second_echec_de_suite_et_le_retour() {
    let (ok, echec) = (tour(Etat::Ok), tour(Etat::Echec));
    // Un premier tour raté après un `ok` ne se publie pas encore.
    assert!(
        transitions(Some(&ok), Some(&ok), &courant(Etat::Echec)).is_empty(),
        "#7805 : un raté isolé ne doit pas publier ok → echec"
    );
    // Le second tour raté de suite publie `ok → echec`.
    assert_eq!(
        transitions(Some(&echec), Some(&ok), &courant(Etat::Echec)),
        vec![Transition {
            service: "service:tidal".into(),
            avant: Some(Etat::Ok),
            apres: Etat::Echec,
        }]
    );
    // Un échec qui DURE ne se redit pas.
    assert!(transitions(Some(&echec), Some(&echec), &courant(Etat::Echec)).is_empty());
    // Le retour à la normale d'un échec annoncé se dit.
    assert_eq!(
        transitions(Some(&echec), Some(&echec), &courant(Etat::Ok)),
        vec![Transition {
            service: "service:tidal".into(),
            avant: Some(Etat::Echec),
            apres: Etat::Ok,
        }]
    );
    // Un service vu pour la première fois en échec se dit tout de suite.
    assert_eq!(transitions(None, None, &courant(Etat::Echec)).len(), 2);
}

#[test]
fn un_echec_isole_suivi_d_un_succes_ne_publie_aucune_transition() {
    // Qobuz (#7805) : ok, echec, ok sur la même piste.
    let (ok, echec) = (tour(Etat::Ok), tour(Etat::Echec));
    assert!(
        transitions(Some(&ok), Some(&ok), &courant(Etat::Echec)).is_empty(),
        "#7805 : le raté isolé ne doit rien publier"
    );
    assert!(
        transitions(Some(&echec), Some(&ok), &courant(Etat::Ok)).is_empty(),
        "#7805 : le retour après un raté jamais annoncé ne doit rien publier"
    );
}

#[test]
fn le_masque_cache_les_requetes_et_nomme_amp() {
    let m = masquer_les_requetes(
        "upstream fetch https://sp-ad-cf.audio.tidal.com/x/0.mp4?token=abc&amp;info=1 refusé",
    );
    assert_eq!(
        m,
        "upstream fetch https://sp-ad-cf.audio.tidal.com/x/0.mp4?…[requête avec amp;] refusé"
    );
    assert_eq!(masquer_les_requetes("rien à cacher"), "rien à cacher");
}

#[test]
fn la_radio_par_defaut_et_sa_desactivation() {
    let defaut = ConfigSentinelle::default();
    assert_eq!(defaut.radio_effective().as_deref(), Some(RADIO_PAR_DEFAUT));
    let sans = ConfigSentinelle {
        radio: Some("  ".into()),
        ..ConfigSentinelle::default()
    };
    assert_eq!(sans.radio_effective(), None);
}

/// À la main, sur une machine qui a Internet : la radio de référence RÉELLE
/// passe par le lecteur radio jusqu'à sa première trame.
///
/// `cargo test -p tune-core --lib sentinelle_streaming::tests::radio_reelle -- --ignored`
#[test]
#[ignore = "réseau réel"]
fn radio_reelle_de_reference() {
    let (cadence, canaux) = super::super::radio::sonder_une_station_sans_sortie(RADIO_PAR_DEFAUT)
        .expect("la radio de référence se décode");
    assert!(cadence > 0 && canaux > 0);
    eprintln!("radio de référence : {cadence} Hz, {canaux} canaux");
}
