//! #8624 — SlimProto levait la sourdine du lecteur à chaque `set_volume` :
//! une limite de sécurité, une alarme ou le sondeur rendaient le son à une
//! zone toujours affichée en sourdine. Règle de #8165 : seul un geste de
//! volume (consigne puis `set_mute(false)`) lève la sourdine, au niveau du
//! geste. Aucun son : le canal de commandes du lecteur est lu par le test.
use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::*;

const MAC: &str = "00:11:22:33:86:24";

fn sortie() -> (SlimProtoOutput, tokio::sync::mpsc::Receiver<ServerMessage>) {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let mut canaux = HashMap::new();
    canaux.insert(MAC.to_string(), tx);
    let sortie = SlimProtoOutput::new(
        "Squeezelite 8624".into(),
        "slimproto-8624".into(),
        MAC.into(),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(canaux)),
    );
    (sortie, rx)
}

/// Les gains `audg` envoyés au lecteur, dans l'ordre.
fn gains(rx: &mut tokio::sync::mpsc::Receiver<ServerMessage>) -> Vec<u32> {
    let mut gains = Vec::new();
    while let Ok(message) = rx.try_recv() {
        if let ServerMessage::Audg {
            left_gain,
            right_gain,
            ..
        } = message
        {
            assert_eq!(left_gain, right_gain);
            gains.push(left_gain);
        }
    }
    gains
}

fn gain(volume: f64) -> u32 {
    (volume * 65536.0).round() as u32
}

/// Lecteur en sourdine : un `set_volume` interne n'envoie aucun gain et la
/// sortie reste muette.
#[tokio::test]
async fn set_volume_garde_la_sourdine_du_lecteur_8624() {
    let (sortie, mut rx) = sortie();
    sortie.set_volume(0.40).await.unwrap();
    sortie.set_mute(true).await.unwrap();
    sortie.set_volume(0.25).await.unwrap();
    sortie.set_volume(1.0).await.unwrap();
    assert_eq!(
        gains(&mut rx),
        vec![gain(0.40), 0],
        "#8624 : en sourdine, aucun gain ne doit partir vers le lecteur"
    );
    assert!(
        sortie.muted.load(Ordering::Relaxed),
        "#8624 : set_volume a levé la sourdine"
    );
}

/// Le geste (consigne puis levée) rend le son au niveau du geste, en un seul
/// envoi : pas de passage par l'ancien niveau.
#[tokio::test]
async fn un_geste_leve_la_sourdine_au_niveau_du_geste_8624() {
    let (sortie, mut rx) = sortie();
    sortie.set_volume(0.40).await.unwrap();
    sortie.set_mute(true).await.unwrap();
    sortie.set_volume(0.20).await.unwrap();
    sortie.set_mute(false).await.unwrap();
    assert_eq!(gains(&mut rx), vec![gain(0.40), 0, gain(0.20)]);
    assert!(!sortie.muted.load(Ordering::Relaxed));
}

/// Hors sourdine, rien ne change : la consigne part tout de suite.
#[tokio::test]
async fn hors_sourdine_la_consigne_part_8624() {
    let (sortie, mut rx) = sortie();
    sortie.set_volume(0.30).await.unwrap();
    sortie.set_volume(0.50).await.unwrap();
    assert_eq!(gains(&mut rx), vec![gain(0.30), gain(0.50)]);
}
