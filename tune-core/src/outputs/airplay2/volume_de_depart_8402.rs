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

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tracing::info;

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
/// Mêmes règles que la consigne d'un geste (`set_volume` de l'orchestrateur)
/// et que la graine des sorties locales : PURE verrouillé ou volume fixe à
/// pleine échelle (choix explicite de l'utilisateur) ; sinon le volume de la
/// zone, un trim positif ignoré (volume matériel, #6739), borné par la
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
    let pure_force = crate::audio::audiophile::zone_enabled(db, zone_id)
        && crate::audio::audiophile::volume_lock_enabled(db, zone_id);
    if zone.fixed_volume || pure_force {
        return (1.0, zone.muted);
    }
    let memorise = (zone.volume / 100.0).clamp(0.0, 1.0);
    let trim_db = crate::orchestrator::gain_trim_db_enregistre(db, zone_id);
    let avec_trim = crate::orchestrator::volume_appareil_avec_trim(memorise, trim_db, true);
    let borne = crate::audio::volume_limites::lire_effectives(db, zone_id)
        .borner_appareil(avec_trim)
        .min(memorise);
    (borne.clamp(0.0, 1.0), zone.muted)
}

impl Airplay2Output {
    /// Sème la sortie qui vient de naître du volume mémorisé de sa zone.
    pub async fn ensemencer_depuis_la_base(&self, db: &Arc<dyn DbBackend>) {
        let (volume, sourdine) = volume_de_depart(db, &self.device_id);
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
}
