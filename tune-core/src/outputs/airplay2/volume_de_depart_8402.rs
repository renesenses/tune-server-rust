//! SÉCURITÉ volume — une zone AirPlay 2 démarrait toujours au volume
//! maximal du récepteur (#8402, fil 2749).
//!
//! ## Le défaut
//!
//! `airplay-daemon` (dépôt `airplay2-rs`) ouvre une `Connection` qui naît à
//! `volume: 1.0` et, au départ de chaque flux (`start_streaming`), envoie
//! `SET_PARAMETER volume` avec ce niveau : `volume: 0.00`, le maximum AirPlay.
//! Tune ne lui donnait jamais le volume de la zone avant `play`, et `stop`
//! tue le démon : chaque session repartait d'une `Connection` neuve, donc de
//! 0 dB. Une consigne donnée zone arrêtée échouait en « daemon not running »
//! et était perdue (fil 2391), et la sortie elle-même naissait à 1.0.
//!
//! ## Le correctif
//!
//! - la sortie naît à [`VOLUME_DE_DEPART_PRUDENT`], et la découverte la sème
//!   du volume mémorisé de sa zone ([`volume_de_depart`]), limite de sécurité
//!   comprise, sourdine comprise ;
//! - une consigne (volume ou sourdine) donnée sans démon est gardée ;
//! - `play_media` donne ce volume au démon avant `play` : le démon traite ses
//!   commandes une à une, sa `Connection` le retient, et c'est lui qui part
//!   au récepteur au départ du flux.
//!
//! ## Volume fixe et PURE verrouillé (décision de Bertrand, 11/11)
//!
//! Ces zones partaient encore à 100 % (0 dB), comme les sorties locales :
//! l'enceinte PURE du fil 2749. Désormais une session AirPlay 2 NEUVE d'une
//! zone qui tient le volume part à 0 %, et l'orchestrateur prévient
//! l'utilisateur (`zone.playback_error` non fatal, codes
//! `airplay2_start_volume_zero_*`). Dans la session, la piste suivante garde
//! le niveau en cours : 0 %, ou le 100 % qu'un geste de l'utilisateur a
//! donné depuis.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tracing::{info, warn};

use super::Airplay2Output;
use crate::db::backend::DbBackend;

/// Le niveau d'une sortie AirPlay 2 dont aucune zone ne dit le volume : le
/// « sans repère » des appareils réseau (30 sur le curseur, #6787).
pub const VOLUME_DE_DEPART_PRUDENT: f64 = crate::audio::reglages_de_volume::CURSEUR_SANS_REPERE;

/// `send` sans démon attaché : la consigne attend la session suivante.
pub(super) fn sans_demon(erreur: &str) -> bool {
    erreur == "daemon not running"
}

/// Le volume (0..1) et la sourdine de départ d'une sortie AirPlay 2, d'après
/// ce que la base dit de la zone de `device_id`.
///
/// PURE verrouillé ou volume fixe : 0 % (décision du 11/11, voir l'en-tête),
/// jamais la pleine échelle. Sinon, mêmes règles que la consigne d'un geste
/// (`set_volume` de l'orchestrateur) : le volume de la zone, un trim positif ignoré (volume matériel, #6739), borné par la
/// limite de sécurité (#6541). Jamais au-dessus du volume mémorisé. Sans zone
/// connue : [`VOLUME_DE_DEPART_PRUDENT`].
pub fn volume_de_depart(db: &Arc<dyn DbBackend>, device_id: &str) -> (f64, bool) {
    let zone = match crate::db::zone_repo::ZoneRepo::with_backend(db.clone())
        .get_by_device_id(device_id)
    {
        Ok(Some(zone)) => zone,
        _ => return (VOLUME_DE_DEPART_PRUDENT, false),
    };
    let Some(zone_id) = zone.id else {
        return (VOLUME_DE_DEPART_PRUDENT, zone.muted);
    };
    if !zone.volume.is_finite() {
        return (VOLUME_DE_DEPART_PRUDENT, zone.muted);
    }
    if tient_le_volume(db, &zone, zone_id).is_some() {
        return (0.0, zone.muted);
    }
    let memorise = (zone.volume / 100.0).clamp(0.0, 1.0);
    let trim_db = crate::orchestrator::gain_trim_db_enregistre(db, zone_id);
    let avec_trim = crate::orchestrator::volume_appareil_avec_trim(memorise, trim_db, true);
    let borne = crate::audio::volume_limites::lire_effectives(db, zone_id)
        .borner_appareil(avec_trim)
        .min(memorise);
    (borne.clamp(0.0, 1.0), zone.muted)
}

/// Pourquoi une zone tient le volume, ce qui fait partir sa session à 0 %.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeTenu {
    /// `zones.fixed_volume` : l'ampli ou le DAC tient le volume.
    Fixe,
    /// PURE armé avec « Forcer à 100 % ».
    PureVerrouille,
}

fn tient_le_volume(
    db: &Arc<dyn DbBackend>,
    zone: &crate::db::zone_repo::Zone,
    zone_id: i64,
) -> Option<VolumeTenu> {
    if zone.fixed_volume {
        Some(VolumeTenu::Fixe)
    } else if crate::audio::audiophile::zone_enabled(db, zone_id)
        && crate::audio::audiophile::volume_lock_enabled(db, zone_id)
    {
        Some(VolumeTenu::PureVerrouille)
    } else {
        None
    }
}

/// La zone `zone_id` tient-elle le volume (fixe, ou PURE verrouillé) ? Lu en
/// base à chaque piste : un réglage changé zone en marche compte tout de
/// suite.
pub fn volume_tenu_par_la_zone(db: &Arc<dyn DbBackend>, zone_id: i64) -> Option<VolumeTenu> {
    let zone = crate::db::zone_repo::ZoneRepo::with_backend(db.clone())
        .get(zone_id)
        .ok()
        .flatten()?;
    tient_le_volume(db, &zone, zone_id)
}

impl Airplay2Output {
    /// L'orchestrateur, avant chaque piste : la zone tient-elle le volume ?
    pub fn tenir_le_volume_de_la_zone(&self, tenu: bool) {
        self.volume_tenu_par_la_zone.store(tenu, Ordering::SeqCst);
    }

    /// La dernière lecture a-t-elle ouvert sa session à 0 % parce que la
    /// zone tient le volume ? Rend `true` une seule fois.
    pub fn prendre_le_depart_muet(&self) -> bool {
        self.depart_muet.swap(false, Ordering::SeqCst)
    }

    /// Une session est-elle déjà ouverte sur le récepteur (piste suivante) ?
    /// Démon absent, connexion perdue (#7921), session à rouvrir (#8347) ou
    /// connexion pas encore confirmée : non, la lecture en ouvre une neuve.
    pub(super) async fn session_deja_ouverte(&self) -> bool {
        let proc = self.process.lock().await;
        proc.is_some()
            && matches!(*self.pairing.lock().await, super::PairingPhase::Connected)
            && !self.connexion_perdue.load(Ordering::SeqCst)
            && !self.session_a_rouvrir.load(Ordering::SeqCst)
    }

    /// Décision du 11/11 : une session NEUVE d'une zone qui tient le volume
    /// part à 0 %. Le niveau est gardé : la piste suivante de la même session
    /// repart du même niveau, sauf geste de l'utilisateur entre-temps.
    pub(super) async fn partir_muet_si_la_zone_tient_le_volume(&self, session_neuve: bool) {
        self.depart_muet.store(false, Ordering::SeqCst);
        if !session_neuve || !self.volume_tenu_par_la_zone.load(Ordering::SeqCst) {
            return;
        }
        *self.volume.lock().await = 0.0;
        self.depart_muet.store(true, Ordering::SeqCst);
        warn!(
            device = %self.name,
            "airplay2_depart_a_zero_volume_tenu_par_la_zone"
        );
    }

    /// Sème la sortie qui vient de naître du volume mémorisé de sa zone.
    pub async fn ensemencer_depuis_la_base(&self, db: &Arc<dyn DbBackend>) {
        let (volume, sourdine) = volume_de_depart(db, &self.device_id);
        let tenu = crate::db::zone_repo::ZoneRepo::with_backend(db.clone())
            .get_by_device_id(&self.device_id)
            .ok()
            .flatten()
            .and_then(|zone| zone.id)
            .and_then(|zone_id| volume_tenu_par_la_zone(db, zone_id))
            .is_some();
        self.tenir_le_volume_de_la_zone(tenu);
        *self.volume.lock().await = volume;
        self.muted.store(sourdine, Ordering::SeqCst);
        info!(
            device = %self.name,
            volume,
            muted = sourdine,
            "airplay2: starting volume seeded"
        );
    }

    /// Donne au démon le volume de départ, avant `play`.
    pub(super) async fn donner_le_volume_au_demon(&self) -> Result<(), String> {
        let niveau = if self.muted.load(Ordering::SeqCst) {
            0.0
        } else {
            *self.volume.lock().await
        };
        self.send(&serde_json::json!({"cmd": "volume", "level": niveau}))
            .await
            .map_err(|erreur| format!("airplay2: starting volume not sent: {erreur}"))?;
        info!(device = %self.name, volume = niveau, "airplay2: starting volume sent");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::Mutex;

    use super::super::{Airplay2Output, DaemonProcess, PairingPhase};
    use super::*;
    use crate::db::sqlite::SqliteDb;
    use crate::db::zone_repo::ZoneRepo;
    use crate::outputs::traits::{OutputTarget, PlayMedia};

    const APPAREIL: &str = "airplay2:fixture-8402";

    fn enceinte() -> Airplay2Output {
        Airplay2Output::new(
            "Enceinte 8402".into(),
            "127.0.0.1".into(),
            7000,
            APPAREIL.into(),
            "00:11:22:33:44:55".into(),
        )
    }

    fn piste() -> PlayMedia<'static> {
        PlayMedia {
            url: "/tmp/fixture-8402.flac",
            mime_type: "audio/flac",
            title: Some("Fixture"),
            ..Default::default()
        }
    }

    /// Le dB que `airplay-rtsp` (`build_set_volume`) écrit pour un niveau.
    fn decibels(niveau: f64) -> String {
        let db = if niveau <= 0.0 {
            -144.0_f32
        } else if niveau >= 1.0 {
            0.0
        } else {
            20.0 * (niveau as f32).log10()
        };
        format!("{db:.2}")
    }

    /// Un faux `airplay-daemon` fidèle au vrai sur le volume : `connect`
    /// ouvre une `Connection` neuve à 1.0, `volume` change le niveau qu'elle
    /// tient (et ne répond `volume_set` que connecté), `play` envoie au
    /// récepteur `SET_PARAMETER volume` avec ce niveau. Rend ce que le
    /// récepteur a reçu au départ de chaque flux, en dB.
    async fn demon(sortie: &Airplay2Output, deja_connecte: bool) -> Arc<Mutex<Vec<String>>> {
        let (server_stdin, daemon_stdin) = tokio::io::duplex(4096);
        let (mut daemon_stdout, server_stdout) = tokio::io::duplex(4096);
        *sortie.process.lock().await = Some(DaemonProcess {
            child: None,
            stdin: Box::new(server_stdin),
        });
        if deja_connecte {
            *sortie.pairing.lock().await = PairingPhase::Connected;
        }
        sortie.start_event_reader(BufReader::new(server_stdout));
        let recus = Arc::new(Mutex::new(Vec::new()));
        let notes = recus.clone();
        tokio::spawn(async move {
            let mut connexion: Option<f64> = deja_connecte.then_some(1.0);
            let mut lignes = BufReader::new(daemon_stdin).lines();
            while let Ok(Some(ligne)) = lignes.next_line().await {
                let commande: serde_json::Value = serde_json::from_str(&ligne).unwrap();
                let evenement = match commande["cmd"].as_str().unwrap() {
                    "connect" => {
                        connexion = Some(1.0);
                        Some(serde_json::json!({"event": "connected"}))
                    }
                    "volume" => match connexion.as_mut() {
                        Some(niveau) => {
                            *niveau = commande["level"].as_f64().unwrap_or(0.5);
                            Some(serde_json::json!({"event": "volume_set", "volume": *niveau}))
                        }
                        None => None,
                    },
                    "play" => match connexion {
                        Some(niveau) => {
                            notes.lock().await.push(decibels(niveau));
                            Some(serde_json::json!({"event": "playing"}))
                        }
                        None => {
                            Some(serde_json::json!({"event": "error", "message": "not connected"}))
                        }
                    },
                    "status" => Some(serde_json::json!({"event": "status", "position_s": 0.0})),
                    _ => None,
                };
                if let Some(evenement) = evenement {
                    let ligne = format!("{}\n", serde_json::to_string(&evenement).unwrap());
                    daemon_stdout.write_all(ligne.as_bytes()).await.unwrap();
                }
            }
        });
        recus
    }

    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    /// Fil 2749 : une sortie neuve, dont personne n'a encore réglé le volume,
    /// ne part jamais à 0 dB.
    #[tokio::test]
    async fn une_sortie_neuve_ne_demarre_pas_au_maximum_8402() {
        let sortie = enceinte();
        let recus = demon(&sortie, false).await;
        sortie.play_media(&piste()).await.unwrap();
        let recus = recus.lock().await.clone();
        assert_eq!(
            recus,
            vec![decibels(VOLUME_DE_DEPART_PRUDENT)],
            "départ du flux : {recus:?}"
        );
        assert_ne!(recus[0], "0.00");
    }

    /// Le volume réglé zone arrêtée (aucun démon) est gardé, et c'est lui
    /// qui part au récepteur à la lecture suivante.
    #[tokio::test]
    async fn le_volume_regle_sans_session_part_au_demarrage_8402() {
        let sortie = enceinte();
        sortie.set_volume(0.2).await.unwrap();
        let recus = demon(&sortie, false).await;
        sortie.play_media(&piste()).await.unwrap();
        assert_eq!(*recus.lock().await, vec![decibels(0.2)]);
        assert!((sortie.get_status().await.unwrap().volume - 0.2).abs() < 1e-9);
    }

    /// Une session déjà ouverte (piste suivante) repart aussi du volume
    /// mémorisé, pas de celui que tient la `Connection` du démon.
    #[tokio::test]
    async fn une_session_deja_ouverte_repart_du_volume_memorise_8402() {
        let sortie = enceinte();
        sortie.set_volume(0.25).await.unwrap();
        let recus = demon(&sortie, true).await;
        sortie.play_media(&piste()).await.unwrap();
        sortie.play_media(&piste()).await.unwrap();
        assert_eq!(*recus.lock().await, vec![decibels(0.25), decibels(0.25)]);
    }

    /// Une sourdine posée sans session tient au démarrage : -144 dB.
    #[tokio::test]
    async fn la_sourdine_posee_sans_session_tient_au_demarrage_8402() {
        let sortie = enceinte();
        sortie.set_volume(0.4).await.unwrap();
        sortie.set_mute(true).await.unwrap();
        let recus = demon(&sortie, false).await;
        sortie.play_media(&piste()).await.unwrap();
        assert_eq!(*recus.lock().await, vec!["-144.00".to_string()]);
    }

    /// La graine de la découverte : le volume mémorisé de la zone, borné par
    /// la limite de sécurité ; sans zone, le niveau prudent.
    #[tokio::test]
    async fn la_graine_suit_la_zone_et_sa_limite_de_securite_8402() {
        let db = base();
        assert_eq!(
            volume_de_depart(&db, APPAREIL),
            (VOLUME_DE_DEPART_PRUDENT, false)
        );

        let repo = ZoneRepo::with_backend(db.clone());
        let zone_id = repo
            .create("Salon 8402", Some("airplay2"), Some(APPAREIL))
            .unwrap();
        repo.update_volume(zone_id, 25.0).unwrap();
        let (volume, sourdine) = volume_de_depart(&db, APPAREIL);
        assert!((volume - 0.25).abs() < 1e-9, "{volume}");
        assert!(!sourdine);

        repo.update_volume(zone_id, 80.0).unwrap();
        crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
            .set(&crate::audio::volume_limites::cle_securite(zone_id), "0.5")
            .unwrap();
        let (volume, _) = volume_de_depart(&db, APPAREIL);
        assert!((volume - 0.5).abs() < 1e-9, "{volume}");

        let sortie = enceinte();
        sortie.ensemencer_depuis_la_base(&db).await;
        let recus = demon(&sortie, false).await;
        sortie.play_media(&piste()).await.unwrap();
        assert_eq!(*recus.lock().await, vec![decibels(0.5)]);
    }
    /// Décision du 11/11 — volume fixe (l'enceinte PURE du fil 2749 vue
    /// côté zone) : la session neuve part à 0 % (-144 dB), jamais à 0 dB, et
    /// le départ muet est levé une seule fois pour l'orchestrateur.
    #[tokio::test]
    async fn une_zone_en_volume_fixe_part_a_zero_8402() {
        let db = base();
        let repo = ZoneRepo::with_backend(db.clone());
        let zone_id = repo
            .create("Fixe 8402", Some("airplay2"), Some(APPAREIL))
            .unwrap();
        repo.update_volume(zone_id, 100.0).unwrap();
        repo.update_fixed_volume(zone_id, true).unwrap();
        assert_eq!(
            volume_tenu_par_la_zone(&db, zone_id),
            Some(VolumeTenu::Fixe)
        );
        assert_eq!(volume_de_depart(&db, APPAREIL), (0.0, false));

        let sortie = enceinte();
        sortie.ensemencer_depuis_la_base(&db).await;
        let recus = demon(&sortie, false).await;
        sortie.play_media(&piste()).await.unwrap();
        assert_eq!(*recus.lock().await, vec!["-144.00".to_string()]);
        assert!(sortie.prendre_le_depart_muet(), "le départ muet est à dire");
        assert!(!sortie.prendre_le_depart_muet(), "une seule fois");
    }

    /// Décision du 11/11 — PURE verrouillé (« Forcer à 100 % ») armé zone en
    /// marche, APRÈS la graine : l'orchestrateur le dit à la sortie avant la
    /// piste, et la session neuve part quand même à 0 %, pas au 100 % que
    /// l'armement de PURE avait posé.
    #[tokio::test]
    async fn une_zone_pure_verrouillee_part_a_zero_8402() {
        let db = base();
        let repo = ZoneRepo::with_backend(db.clone());
        let zone_id = repo
            .create("PURE 8402", Some("airplay2"), Some(APPAREIL))
            .unwrap();
        repo.update_volume(zone_id, 40.0).unwrap();
        let sortie = enceinte();
        sortie.ensemencer_depuis_la_base(&db).await;
        assert_eq!(volume_tenu_par_la_zone(&db, zone_id), None);

        crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
            .set(
                &format!("zone_{zone_id}_audiophile"),
                r#"{"enabled":true,"lock_volume":true}"#,
            )
            .unwrap();
        assert_eq!(
            volume_tenu_par_la_zone(&db, zone_id),
            Some(VolumeTenu::PureVerrouille)
        );
        assert_eq!(volume_de_depart(&db, APPAREIL), (0.0, false));
        // L'armement de PURE commande 100 % à la sortie (`set_volume_pure_force`).
        sortie.set_volume(1.0).await.unwrap();
        sortie.tenir_le_volume_de_la_zone(volume_tenu_par_la_zone(&db, zone_id).is_some());

        let recus = demon(&sortie, false).await;
        sortie.play_media(&piste()).await.unwrap();
        assert_eq!(*recus.lock().await, vec!["-144.00".to_string()]);
        assert!(sortie.prendre_le_depart_muet());
    }

    /// Dans la session ouverte à 0 %, la piste suivante ne remet pas à zéro
    /// le geste de l'utilisateur (PURE : 100 %), et ne relève pas de nouveau
    /// départ muet.
    #[tokio::test]
    async fn la_piste_suivante_garde_le_geste_de_l_utilisateur_8402() {
        let sortie = enceinte();
        sortie.tenir_le_volume_de_la_zone(true);
        let recus = demon(&sortie, false).await;
        sortie.play_media(&piste()).await.unwrap();
        assert!(sortie.prendre_le_depart_muet());
        sortie.set_volume(1.0).await.unwrap();
        sortie.play_media(&piste()).await.unwrap();
        assert_eq!(
            *recus.lock().await,
            vec!["-144.00".to_string(), "0.00".to_string()]
        );
        assert!(!sortie.prendre_le_depart_muet());
    }

    /// Une zone qui ne tient pas le volume garde son niveau : aucun départ
    /// muet, rien à dire.
    #[tokio::test]
    async fn une_zone_ordinaire_ne_part_pas_muette_8402() {
        let sortie = enceinte();
        sortie.set_volume(0.3).await.unwrap();
        sortie.tenir_le_volume_de_la_zone(false);
        let recus = demon(&sortie, false).await;
        sortie.play_media(&piste()).await.unwrap();
        assert_eq!(*recus.lock().await, vec![decibels(0.3)]);
        assert!(!sortie.prendre_le_depart_muet());
    }
    /// Une session dont le récepteur a fermé la connexion (#7921) ou dont le
    /// setup a échoué (#8347) se rouvre : c'est une session neuve, donc à 0 %
    /// pour une zone qui tient le volume.
    #[tokio::test]
    async fn une_session_a_rouvrir_n_est_pas_une_session_ouverte_8402() {
        use std::sync::atomic::Ordering;
        let sortie = enceinte();
        let _recus = demon(&sortie, true).await;
        assert!(sortie.session_deja_ouverte().await);
        sortie.connexion_perdue.store(true, Ordering::SeqCst);
        assert!(!sortie.session_deja_ouverte().await);
        sortie.connexion_perdue.store(false, Ordering::SeqCst);
        sortie.session_a_rouvrir.store(true, Ordering::SeqCst);
        assert!(!sortie.session_deja_ouverte().await);
    }
}
