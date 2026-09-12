use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tokio::sync::Mutex;

use super::airplay::AirplayOutput;
use super::airplay2::Airplay2Output;
use super::bluos::BluosOutput;
use super::bridge::BridgeOutput;
use super::chromecast::ChromecastOutput;
use super::dlna::DlnaOutput;
use super::hqplayer::HqplayerOutput;
#[cfg(feature = "local-audio")]
use super::local::LocalOutput;
use super::mock::MockOutput;
#[cfg(feature = "oaat")]
use super::oaat::{OaatMultiroomOutput, OaatOutput};
use super::openhome::OpenHomeOutput;
use super::slimproto::SlimProtoOutput;
use super::squeezebox::SqueezeboxOutput;
use super::{
    OutputCapabilities, OutputCommand, OutputCommandError, OutputStatus, OutputTarget,
    TransportState,
};

struct LegacyNoopOutput {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl OutputTarget for LegacyNoopOutput {
    fn name(&self) -> &str {
        "legacy"
    }

    fn device_id(&self) -> &str {
        "legacy-1"
    }

    fn output_type(&self) -> &str {
        "legacy"
    }

    async fn pause(&self) -> Result<(), String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        Ok(())
    }

    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        Ok(())
    }

    async fn set_volume(&self, _volume: f64) -> Result<(), String> {
        Ok(())
    }

    async fn set_mute(&self, _muted: bool) -> Result<(), String> {
        Ok(())
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        Ok(OutputStatus {
            state: TransportState::Stopped,
            ..Default::default()
        })
    }

    async fn is_available(&self) -> bool {
        true
    }
}

fn unsupported(command: OutputCommand) -> OutputCommandError {
    OutputCommandError::Unsupported { command }
}

#[tokio::test]
async fn toutes_les_sorties_integrees_declarent_le_contrat_v1() {
    let (bridge_tx, _bridge_rx) = tokio::sync::mpsc::channel(1);
    let players = Arc::new(Mutex::new(HashMap::new()));
    let channels = Arc::new(Mutex::new(HashMap::new()));
    #[allow(unused_mut)] // Les ajouts OAAT et local dépendent des features actives.
    let mut outputs: Vec<Box<dyn OutputTarget>> = vec![
        Box::new(AirplayOutput::new(
            "AirPlay".into(),
            "airplay-1".into(),
            "127.0.0.1".into(),
            9,
        )),
        Box::new(Airplay2Output::new(
            "AirPlay 2".into(),
            "127.0.0.1".into(),
            9,
            "airplay2-1".into(),
            "00:11:22:33:44:55".into(),
        )),
        Box::new(BluosOutput::new(
            "BluOS".into(),
            "bluos-1".into(),
            "127.0.0.1".into(),
            9,
        )),
        Box::new(BridgeOutput::new(
            "Bridge".into(),
            "bridge-1".into(),
            "bridge".into(),
            "bridge-host".into(),
            bridge_tx,
            Arc::new(AtomicBool::new(true)),
        )),
        Box::new(ChromecastOutput::new(
            "Cast".into(),
            "cast-1".into(),
            "127.0.0.1".into(),
            9,
        )),
        Box::new(DlnaOutput::new(
            "DLNA".into(),
            "dlna-1".into(),
            "127.0.0.1".into(),
            "http://127.0.0.1:9/transport".into(),
            "http://127.0.0.1:9/rendering".into(),
            None,
        )),
        Box::new(HqplayerOutput::new(
            "HQPlayer".into(),
            "hqplayer-1".into(),
            "127.0.0.1".into(),
            9,
        )),
        Box::new(MockOutput::new("mock-1", "Mock")),
        Box::new(OpenHomeOutput::new(
            "OpenHome".into(),
            "openhome-1".into(),
            "127.0.0.1".into(),
            9,
            HashMap::new(),
            None,
            HashMap::new(),
        )),
        Box::new(SlimProtoOutput::new(
            "SlimProto".into(),
            "slimproto-1".into(),
            "00:11:22:33:44:55".into(),
            players,
            channels,
        )),
        Box::new(SqueezeboxOutput::new(
            "Squeezebox".into(),
            "squeezebox-00:11:22:33:44:55".into(),
            "127.0.0.1".into(),
            9,
        )),
    ];
    #[cfg(feature = "oaat")]
    {
        outputs.push(Box::new(OaatOutput::new(
            "OAAT".into(),
            "127.0.0.1".into(),
            9,
            "oaat-1".into(),
        )));
        outputs.push(Box::new(OaatMultiroomOutput::new(
            "OAAT group".into(),
            "group-1".into(),
            Vec::new(),
        )));
    }
    #[cfg(feature = "local-audio")]
    outputs.push(Box::new(LocalOutput::new("Test local".into())));

    for output in outputs {
        let capabilities = output.capabilities();
        assert_eq!(
            capabilities.version,
            crate::outputs::traits::OUTPUT_CAPABILITIES_VERSION,
            "{} doit déclarer explicitement le contrat courant",
            output.output_type()
        );
        // #1274 — la finesse REELLE, celle que set_volume envoie sur le
        // fil. Un type de sortie inconnu fait echouer ce test : c'est
        // volontaire. Une sortie neuve doit declarer sa grille, faute de quoi
        // une consigne en dB s'y arrondirait en silence sans que rien ne le
        // dise.
        let grille_attendue = match output.output_type() {
            "dlna" | "bluos" | "openhome" | "squeezebox" | "hqplayer" | "oaat"
            | "oaat-multiroom" => crate::outputs::VolumeResolution::Linear { steps: 100 },
            "slimproto" => crate::outputs::VolumeResolution::Linear { steps: 65536 },
            "local" => crate::outputs::VolumeResolution::Linear { steps: 1000 },
            "airplay" => crate::outputs::VolumeResolution::Decibels { step_mdb: 100 },
            "chromecast" | "airplay2" | "bridge" | "mock" => {
                crate::outputs::VolumeResolution::Continuous
            }
            autre => panic!("sortie « {autre} » : declarez sa grille de volume (#1274)"),
        };
        assert_eq!(
            capabilities.volume_resolution,
            grille_attendue,
            "{} ne declare pas la grille que son set_volume envoie",
            output.output_type()
        );
        assert_eq!(
            capabilities.can_gapless,
            output.supports_internal_gapless(),
            "{} ne doit pas publier deux vérités gapless",
            output.output_type()
        );
    }
}

#[tokio::test]
async fn un_plugin_ancien_est_bloque_avant_son_noop() {
    let output = LegacyNoopOutput {
        calls: AtomicUsize::new(0),
    };

    assert_eq!(output.capabilities(), OutputCapabilities::default());
    assert_eq!(
        output.checked_pause().await,
        Err(unsupported(OutputCommand::Pause))
    );
    assert_eq!(output.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn les_noops_historiques_rendent_unsupported_sans_io() {
    let hqplayer = HqplayerOutput::new(
        "HQPlayer".into(),
        "hqplayer-1".into(),
        "127.0.0.1".into(),
        9,
    );
    assert_eq!(
        hqplayer.checked_set_mute(true).await,
        Err(unsupported(OutputCommand::SetMute))
    );
    assert!(hqplayer.set_mute(true).await.is_err());

    let openhome = OpenHomeOutput::new(
        "OpenHome sans services".into(),
        "openhome-1".into(),
        "127.0.0.1".into(),
        9,
        HashMap::new(),
        None,
        HashMap::new(),
    );
    assert_eq!(
        openhome.checked_seek(1_000).await,
        Err(unsupported(OutputCommand::Seek))
    );
    assert_eq!(
        openhome.checked_set_volume(0.5).await,
        Err(unsupported(OutputCommand::SetVolume))
    );
    assert_eq!(
        openhome.checked_set_mute(true).await,
        Err(unsupported(OutputCommand::SetMute))
    );
    assert!(openhome.seek(1_000).await.is_err());
    assert!(openhome.set_volume(0.5).await.is_err());
    assert!(openhome.set_mute(true).await.is_err());

    let players = Arc::new(Mutex::new(HashMap::new()));
    let channels = Arc::new(Mutex::new(HashMap::new()));
    let slimproto = SlimProtoOutput::new(
        "SlimProto".into(),
        "slimproto-1".into(),
        "00:11:22:33:44:55".into(),
        players,
        channels,
    );
    assert_eq!(
        slimproto.checked_seek(1_000).await,
        Err(unsupported(OutputCommand::Seek))
    );
    assert!(slimproto.seek(1_000).await.is_err());
}

#[cfg(feature = "oaat")]
#[tokio::test]
async fn oaat_multiroom_ne_promet_ni_seek_ni_gapless() {
    let output =
        super::oaat::OaatMultiroomOutput::new("Groupe OAAT".into(), "groupe-1".into(), Vec::new());

    assert!(!output.capabilities().can_seek);
    assert!(!output.capabilities().can_gapless);
    assert!(!output.supports_internal_gapless());
    assert_eq!(
        output.checked_seek(1_000).await,
        Err(unsupported(OutputCommand::Seek))
    );
    assert!(output.seek(1_000).await.is_err());
}

#[tokio::test]
async fn un_echec_backend_ne_change_pas_le_statut_confirme() {
    let airplay = AirplayOutput::new("AirPlay".into(), "airplay-1".into(), "127.0.0.1".into(), 9);
    let error = airplay.checked_set_volume(0.25).await.unwrap_err();
    assert!(matches!(
        error,
        OutputCommandError::Failed {
            command: OutputCommand::SetVolume,
            ..
        }
    ));
    let status = airplay.get_status().await.unwrap();
    assert_eq!(status.volume, 1.0);
    assert!(!status.muted);

    let airplay2 = Airplay2Output::new(
        "AirPlay 2".into(),
        "127.0.0.1".into(),
        9,
        "airplay2-1".into(),
        "00:11:22:33:44:55".into(),
    );
    assert!(airplay2.checked_set_volume(0.25).await.is_err());
    assert!(airplay2.resume().await.is_err());
    let status = airplay2.get_status().await.unwrap();
    assert_eq!(status.volume, 1.0);
    assert!(!status.muted);
}

#[tokio::test]
async fn une_commande_confirmee_met_le_statut_en_coherence() {
    let mock = MockOutput::new("mock-1", "Mock");
    mock.checked_set_volume(0.25).await.unwrap();
    mock.checked_set_mute(true).await.unwrap();
    let status = mock.get_status().await.unwrap();
    assert_eq!(status.volume, 0.25);
    assert!(status.muted);

    let players = Arc::new(Mutex::new(HashMap::new()));
    let channels = Arc::new(Mutex::new(HashMap::new()));
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    channels.lock().await.insert("00:11:22:33:44:55".into(), tx);
    let slimproto = SlimProtoOutput::new(
        "SlimProto".into(),
        "slimproto-1".into(),
        "00:11:22:33:44:55".into(),
        players,
        channels,
    );

    slimproto.checked_set_volume(0.4).await.unwrap();
    rx.recv().await.expect("commande volume envoyée");
    slimproto.checked_set_mute(true).await.unwrap();
    rx.recv().await.expect("commande mute envoyée");
    let status = slimproto.get_status().await.unwrap();
    assert_eq!(status.volume, 0.4);
    assert!(status.muted);
}
