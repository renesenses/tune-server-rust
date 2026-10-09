//! #5662 — coalescence des commandes de volume, par sortie et par zone.
//!
//! Avant : chaque `set_volume` attendait la réponse de l'appareil (SOAP
//! `SetVolume` pour un renderer DLNA) en tenant le verrou de la sortie. Une
//! rafale de 30 pas partait donc en 30 commandes en série, et la valeur finale
//! arrivait des secondes après le geste.
//!
//! Désormais, pour un couple (sortie, zone) :
//!
//! - une seule commande de volume est en vol vers l'appareil ;
//! - pendant ce temps, seule la DERNIÈRE valeur demandée est gardée. Elle
//!   part dès que la commande en cours se termine ; les valeurs intermédiaires
//!   ne partent jamais ;
//! - l'état en mémoire et l'évènement `playback.volume` sont mis à jour dès
//!   la demande, dans l'ordre d'arrivée.
//!
//! Contrat de réponse (voir aussi la PR) :
//!
//! - une demande dont la valeur part attend le verdict de l'appareil pour
//!   CETTE valeur : `Ok`, ou l'erreur de la sortie (502 côté route) ;
//! - une demande remplacée par une plus récente avant d'être partie rend `Ok`
//!   tout de suite : sa valeur n'est plus voulue, il n'y a rien à rapporter ;
//! - en cas de refus de la DERNIÈRE valeur, la mémoire revient à la dernière
//!   valeur acceptée (ou à celle d'avant la rafale), un `playback.volume`
//!   le dit, PUIS `zone.playback_error` part : un client qui fige son curseur
//!   sur cette erreur la reçoit en dernier.
use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, Notify};

/// Les files de volume, une par (device_id, zone_id).
#[derive(Default)]
pub(crate) struct CoalesceurDeVolume {
    files: std::sync::Mutex<HashMap<(String, i64), Arc<FileDeVolume>>>,
}

impl CoalesceurDeVolume {
    pub(crate) fn file(&self, device_id: &str, zone_id: i64) -> Arc<FileDeVolume> {
        let mut files = self.files.lock().unwrap_or_else(|e| e.into_inner());
        files
            .entry((device_id.to_string(), zone_id))
            .or_insert_with(|| Arc::new(FileDeVolume::default()))
            .clone()
    }
}

#[derive(Default)]
pub(crate) struct FileDeVolume {
    pub(crate) etat: Mutex<EtatDeLaFile>,
    pub(crate) reveil: Notify,
}

#[derive(Default)]
pub(crate) struct EtatDeLaFile {
    /// Numéro de la demande la plus récente. Seule celle-là a le droit de
    /// partir quand la sortie se libère.
    pub(crate) dernier_ticket: u64,
    /// Une commande est en vol vers l'appareil.
    pub(crate) en_vol: bool,
    /// La valeur à rétablir si l'appareil refuse : la dernière acceptée
    /// pendant la rafale, ou celle d'avant la rafale. `None` hors rafale.
    pub(crate) confirme: Option<f64>,
    /// Un refus a été rapporté depuis la dernière acceptation : la prochaine
    /// acceptation ré-émet `playback.volume` pour lever l'erreur côté client.
    pub(crate) echec_depuis_succes: bool,
}

impl FileDeVolume {
    /// Attend que ce ticket puisse partir. `false` : une demande plus récente
    /// l'a remplacé, sa valeur ne partira pas.
    pub(crate) async fn attendre_son_tour(&self, ticket: u64) -> bool {
        loop {
            let reveil = self.reveil.notified();
            tokio::pin!(reveil);
            reveil.as_mut().enable();
            {
                let mut etat = self.etat.lock().await;
                if etat.dernier_ticket != ticket {
                    return false;
                }
                if !etat.en_vol {
                    etat.en_vol = true;
                    return true;
                }
            }
            reveil.await;
        }
    }
}
