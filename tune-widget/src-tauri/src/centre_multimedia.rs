//! macOS : le Widget se déclare comme LECTEUR auprès du système (MediaPlayer),
//! pour que les touches multimédia du clavier (⏯ ⏭ ⏮) et le Centre de
//! contrôle pilotent la zone du Widget (accord de Bertrand du 23/11, rc14).
//!
//! - `MPRemoteCommandCenter` reçoit Lecture/Pause, Lecture, Pause, Suivant,
//!   Précédent et le curseur de position ; chaque commande part vers le
//!   serveur Tune par [`lecteur_systeme::envoyer`], comme le menu Lecture.
//! - `MPNowPlayingInfoCenter` annonce le titre, l'artiste, l'album, la
//!   pochette, la durée, la position et l'état de lecture de la zone, d'après
//!   `GET /api/v1/widget/data` (la même réponse que le Widget lit déjà).
//!
//! AUCUNE autorisation : c'est le chemin public des lecteurs (Musique,
//! Spotify…), sans `CGEventTap` — donc ni Accessibilité, ni Surveillance de
//! l'entrée (le piège de `tauri-plugin-global-shortcut`, #9255) — et sans
//! aucune clé dans `Info.plist`.
//!
//! Tout ce qui touche à AppKit/MediaPlayer passe sur le fil principal
//! (`run_on_main_thread`) ; le réseau reste sur le fil asynchrone.

use std::ptr::NonNull;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::AnyThread;
use objc2_app_kit::NSImage;
use objc2_foundation::{NSData, NSMutableDictionary, NSNumber, NSString};
use objc2_media_player::{
    MPChangePlaybackPositionCommandEvent, MPMediaItemArtwork, MPMediaItemPropertyAlbumTitle,
    MPMediaItemPropertyArtist, MPMediaItemPropertyArtwork, MPMediaItemPropertyPlaybackDuration,
    MPMediaItemPropertyTitle, MPNowPlayingInfoCenter, MPNowPlayingInfoPropertyElapsedPlaybackTime,
    MPNowPlayingInfoPropertyIsLiveStream, MPNowPlayingInfoPropertyPlaybackRate,
    MPNowPlayingPlaybackState, MPRemoteCommand, MPRemoteCommandCenter, MPRemoteCommandEvent,
    MPRemoteCommandHandlerStatus,
};
use tauri::Manager;
use tokio::sync::mpsc::UnboundedSender;

use crate::lecteur_systeme::{self, Commande, Ecoute, Etat};

/// Fréquence de lecture de l'état de la zone. Entre deux lectures, le système
/// fait avancer la position seul (vitesse 1).
const PERIODE: Duration = Duration::from_secs(2);

/// Pochette plus lourde : ignorée (une image de 10 Mo n'a rien à faire dans
/// le Centre de contrôle).
const POCHETTE_MAX_OCTETS: usize = 8 * 1024 * 1024;

/// Branche les commandes du système et lance le suivi « À l'écoute ».
pub fn installer(app: &tauri::AppHandle) {
    let (envoi, mut recu) = tokio::sync::mpsc::unbounded_channel::<Commande>();

    let h = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(commande) = recu.recv().await {
            executer(&h, commande).await;
        }
    });

    if let Err(e) = app.run_on_main_thread(move || brancher_les_commandes(envoi)) {
        tracing::warn!(error = %e, "centre_multimedia_non_branche");
        return;
    }
    tauri::async_runtime::spawn(suivre_l_ecoute(app.clone()));
}

/// Une commande reçue : sur la zone du Widget (`zone_visee`), en tâche de fond.
async fn executer(app: &tauri::AppHandle, commande: Commande) {
    let state = app.state::<crate::AppState>();
    let url = state.server_url.read().await.clone();
    let zone_id = crate::zone_visee(&state).await;
    let http = state.http.clone();
    tracing::info!(?commande, zone_id, "commande_systeme_recue");
    if let Err(e) = lecteur_systeme::envoyer(&http, &url, zone_id, commande).await {
        tracing::warn!(?commande, zone_id, error = %e, "commande_systeme_echouee");
    }
}

/// Une commande du système, rendue au fil asynchrone. Le bloc est appelé par
/// MediaPlayer sur le fil principal : il ne fait qu'envoyer dans le canal.
fn brancher(
    commande: &MPRemoteCommand,
    envoi: &UnboundedSender<Commande>,
    lire: impl Fn(NonNull<MPRemoteCommandEvent>) -> Option<Commande> + 'static,
) {
    let envoi = envoi.clone();
    let bloc =
        RcBlock::new(
            move |evenement: NonNull<MPRemoteCommandEvent>| match lire(evenement) {
                Some(c) if envoi.send(c).is_ok() => MPRemoteCommandHandlerStatus::Success,
                _ => MPRemoteCommandHandlerStatus::CommandFailed,
            },
        );
    // SAFETY : fil principal ; le centre de commandes retient le bloc (la
    // cible rendue est opaque et vit avec lui).
    unsafe {
        commande.setEnabled(true);
        let _cible: Retained<AnyObject> = commande.addTargetWithHandler(&bloc);
    }
}

fn brancher_les_commandes(envoi: UnboundedSender<Commande>) {
    // SAFETY : appelé sur le fil principal (`run_on_main_thread`).
    unsafe {
        let centre = MPRemoteCommandCenter::sharedCommandCenter();
        let fixe = |c: Commande| move |_| Some(c);
        brancher(
            &centre.togglePlayPauseCommand(),
            &envoi,
            fixe(Commande::LecturePause),
        );
        brancher(&centre.playCommand(), &envoi, fixe(Commande::Lecture));
        brancher(&centre.pauseCommand(), &envoi, fixe(Commande::Pause));
        brancher(&centre.nextTrackCommand(), &envoi, fixe(Commande::Suivante));
        brancher(
            &centre.previousTrackCommand(),
            &envoi,
            fixe(Commande::Precedente),
        );
        brancher(
            &centre.changePlaybackPositionCommand(),
            &envoi,
            |evenement: NonNull<MPRemoteCommandEvent>| {
                // SAFETY : cette commande-là ne reçoit que des
                // MPChangePlaybackPositionCommandEvent (doc d'Apple).
                let e = evenement.cast::<MPChangePlaybackPositionCommandEvent>();
                let secondes = e.as_ref().positionTime();
                secondes
                    .is_finite()
                    .then(|| Commande::Position((secondes * 1000.0).round() as i64))
            },
        );
        // Ce que Tune ne sait pas faire : grisé dans le Centre de contrôle.
        for sans_suite in [
            centre.stopCommand(),
            centre.seekForwardCommand(),
            centre.seekBackwardCommand(),
        ] {
            sans_suite.setEnabled(false);
        }
        centre.skipForwardCommand().setEnabled(false);
        centre.skipBackwardCommand().setEnabled(false);
        centre.changePlaybackRateCommand().setEnabled(false);
    }
    tracing::info!("centre_multimedia_branche");
}

/// Lit l'état de la zone du Widget et le ré-annonce quand il change.
async fn suivre_l_ecoute(app: tauri::AppHandle) {
    let mut annoncee: Option<(Ecoute, Instant)> = None;
    // (adresse, octets) de la dernière pochette lue : une seule lecture par
    // morceau.
    let mut pochette: Option<(String, Option<Vec<u8>>)> = None;
    loop {
        let state = app.state::<crate::AppState>();
        let url = state.server_url.read().await.clone();
        let http = state.http.clone();
        let donnees = crate::fetch_widget_data(&http, &url, None).await;
        let choisie = *state.active_zone_id.read().await;
        let ecoute = donnees.as_ref().and_then(|d| {
            let zone_id = choisie.or_else(|| crate::zone_cible::zone_par_defaut(d))?;
            lecteur_systeme::ecoute_de(d, zone_id, &url)
        });
        match ecoute {
            Some(e) => {
                let ecoule = annoncee
                    .as_ref()
                    .map(|(_, quand)| quand.elapsed().as_millis() as i64)
                    .unwrap_or(0);
                if lecteur_systeme::a_reannoncer(annoncee.as_ref().map(|(p, _)| p), &e, ecoule) {
                    let octets = match (&e.pochette, &pochette) {
                        (None, _) => None,
                        (Some(a), Some((deja, o))) if a == deja => o.clone(),
                        (Some(a), _) => {
                            let o = lire_pochette(&http, a).await;
                            pochette = Some((a.clone(), o.clone()));
                            o
                        }
                    };
                    let copie = e.clone();
                    let _ = app.run_on_main_thread(move || annoncer(&copie, octets));
                    annoncee = Some((e, Instant::now()));
                }
            }
            None => {
                if annoncee.take().is_some() {
                    let _ = app.run_on_main_thread(effacer);
                }
            }
        }
        tokio::time::sleep(PERIODE).await;
    }
}

async fn lire_pochette(http: &reqwest::Client, adresse: &str) -> Option<Vec<u8>> {
    let r = http.get(adresse).send().await.ok()?;
    if !r.status().is_success() {
        return None;
    }
    let octets = r.bytes().await.ok()?;
    (!octets.is_empty() && octets.len() <= POCHETTE_MAX_OCTETS).then(|| octets.to_vec())
}

fn nombre(v: f64) -> Retained<NSNumber> {
    NSNumber::numberWithDouble(v)
}

/// Annonce `e` au système. Fil principal.
fn annoncer(e: &Ecoute, pochette: Option<Vec<u8>>) {
    let info = NSMutableDictionary::<NSString, AnyObject>::new();
    // SAFETY : fil principal ; les clés sont les constantes de MediaPlayer, les
    // valeurs du type qu'elles attendent (NSString, NSNumber en secondes,
    // MPMediaItemArtwork).
    unsafe {
        info.insert(MPMediaItemPropertyTitle, &*NSString::from_str(&e.titre));
        if let Some(a) = &e.artiste {
            info.insert(MPMediaItemPropertyArtist, &*NSString::from_str(a));
        }
        if let Some(a) = &e.album {
            info.insert(MPMediaItemPropertyAlbumTitle, &*NSString::from_str(a));
        }
        if e.duree_ms > 0 {
            info.insert(
                MPMediaItemPropertyPlaybackDuration,
                &*nombre(e.duree_ms as f64 / 1000.0),
            );
        } else {
            // Radio : pas de curseur.
            info.insert(
                MPNowPlayingInfoPropertyIsLiveStream,
                &*NSNumber::numberWithBool(true),
            );
        }
        info.insert(
            MPNowPlayingInfoPropertyElapsedPlaybackTime,
            &*nombre(e.position_ms as f64 / 1000.0),
        );
        let vitesse = if e.etat == Etat::EnLecture { 1.0 } else { 0.0 };
        info.insert(MPNowPlayingInfoPropertyPlaybackRate, &*nombre(vitesse));
        if let Some(illustration) = pochette.as_deref().and_then(illustration_de) {
            info.insert(MPMediaItemPropertyArtwork, &*illustration);
        }
        let centre = MPNowPlayingInfoCenter::defaultCenter();
        centre.setNowPlayingInfo(Some(&info));
        // macOS seulement, et indispensable : sans lui, le système ne sait pas
        // que l'app « joue » et ne lui envoie pas les touches.
        centre.setPlaybackState(match e.etat {
            Etat::EnLecture => MPNowPlayingPlaybackState::Playing,
            Etat::EnPause => MPNowPlayingPlaybackState::Paused,
            Etat::Arrete => MPNowPlayingPlaybackState::Stopped,
        });
    }
}

/// La pochette pour « À l'écoute », ou rien si l'image est illisible.
fn illustration_de(octets: &[u8]) -> Option<Retained<AnyObject>> {
    let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(octets))?;
    let taille = image.size();
    if taille.width <= 0.0 || taille.height <= 0.0 {
        return None;
    }
    let gardee = image.clone();
    // Le système demande l'image à la taille qu'il veut : on rend la même,
    // il la met à l'échelle. Le bloc garde l'image en vie.
    let bloc = RcBlock::new(move |_| NonNull::from(&*gardee));
    // SAFETY : le bloc rend un pointeur vers une NSImage vivante (retenue par
    // le bloc, lui-même retenu par l'illustration).
    let illustration = unsafe {
        MPMediaItemArtwork::initWithBoundsSize_requestHandler(
            MPMediaItemArtwork::alloc(),
            taille,
            &bloc,
        )
    };
    Some(illustration.into_super().into_super())
}

/// Plus rien en cours sur la zone. Fil principal.
fn effacer() {
    // SAFETY : fil principal.
    unsafe {
        let centre = MPNowPlayingInfoCenter::defaultCenter();
        centre.setNowPlayingInfo(None);
        centre.setPlaybackState(MPNowPlayingPlaybackState::Stopped);
    }
}
