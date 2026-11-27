//! Fil 3115 : une pause peut arriver entre ALLUMER et le premier paquet AIR.
//! Le départ garde son verrou et termine la publication du nouvel état avant la pause.
use super::PlaybackOrchestrator;
use crate::outputs::{OutputCommand, OutputCommandError, OutputCommandResult};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::watch;

type Issue = Option<Result<(), String>>;
type Cle = (i64, String);

#[derive(Default)]
pub(super) struct Reveils(Mutex<HashMap<Cle, watch::Receiver<Issue>>>);

pub(super) struct Reveil<'a> {
    registre: &'a Reveils,
    cle: Cle,
    tx: watch::Sender<Issue>,
}

impl Reveils {
    pub(super) fn inscrire(&self, zone: i64, appareil: &str) -> Reveil<'_> {
        let cle = (zone, appareil.to_owned());
        let (tx, rx) = watch::channel(None);
        self.0.lock().unwrap().insert(cle.clone(), rx);
        Reveil {
            registre: self,
            cle,
            tx,
        }
    }

    pub(super) async fn attendre(&self, zone: i64, appareil: &str) -> OutputCommandResult<bool> {
        let rx = self
            .0
            .lock()
            .unwrap()
            .get(&(zone, appareil.to_owned()))
            .cloned();
        let Some(mut rx) = rx else {
            return Ok(false);
        };
        // Le départ comprend le pré-tampon radio (8 s), l'allumage (20 s),
        // l'entrée (3 s) et l'ouverture bornée du décodeur. Une tâche
        // bloquée ailleurs ne peut pas retenir la commande indéfiniment.
        let resultat = tokio::time::timeout(Duration::from_secs(60), rx.wait_for(Option::is_some))
            .await
            .map_err(|_| {
                OutputCommandError::failed(
                    OutputCommand::Pause,
                    "Le lecteur ne termine pas son réveil",
                )
            })?
            .map_err(|_| {
                OutputCommandError::failed(
                    OutputCommand::Pause,
                    "Le démarrage du lecteur a été interrompu",
                )
            })?;
        resultat
            .as_ref()
            .unwrap()
            .clone()
            .map(|()| true)
            .map_err(|e| OutputCommandError::failed(OutputCommand::Pause, e))
    }
}

impl Reveil<'_> {
    pub(super) fn terminer(&self, issue: Result<(), String>) {
        self.tx.send_replace(Some(issue));
    }
}

impl Drop for Reveil<'_> {
    fn drop(&mut self) {
        let mut m = self.registre.0.lock().unwrap();
        if m.get(&self.cle)
            .is_some_and(|rx| rx.same_channel(&self.tx.subscribe()))
        {
            m.remove(&self.cle);
        }
    }
}

impl PlaybackOrchestrator {
    /// État transitoire de la sortie de cette zone, également publié sur le bus.
    pub fn statut_de_reveil(&self, zone: i64, appareil: Option<&str>) -> Option<&'static str> {
        let appareil = appareil?;
        let actifs = self.reveils.0.lock().unwrap();
        actifs
            .keys()
            .any(|(z, id)| *z == zone && id == appareil)
            .then_some("waking")
    }

    pub(super) async fn signaler_le_reveil(
        &self,
        zone: i64,
        appareil: Option<&str>,
    ) -> Option<Reveil<'_>> {
        let id = appareil?;
        let sortie = {
            let r = self.outputs.lock().await;
            if r.type_of(id).as_deref() != Some(crate::outputs::devialet_air::TYPE_DE_SORTIE) {
                return None;
            }
            r.get(id)?
        };
        let garde = sortie.lock().await;
        let air = garde
            .as_any()
            .downcast_ref::<crate::outputs::devialet_air::DevialetAirOutput>()?;
        if !air.doit_reveiller_ampli() {
            return None;
        }
        let reveil = self.reveils.inscrire(zone, id);
        if let Some(bus) = &self.event_bus {
            bus.emit_typed(
                crate::event_types::EventType::ZoneNotice,
                serde_json::json!({
                    "zone_id": zone, "code": "renderer_waking", "status": "waking",
                    "message": "Waking up the player…", "message_key": "playback.renderer_waking", "fatal": false
                }),
            );
        }
        Some(reveil)
    }
}
