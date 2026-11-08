use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::{Mutex, mpsc, oneshot};

use super::traits::{OutputCapabilities, OutputStatus, OutputTarget, PlayMedia};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BridgeCommand {
    pub id: String,
    pub device_id: String,
    pub command: String,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BridgeResponse {
    pub id: String,
    pub ok: bool,
    pub data: Option<serde_json::Value>,
    pub error: Option<String>,
}

pub struct BridgeOutput {
    name: String,
    device_id: String,
    output_type_str: String,
    _bridge_id: String,
    command_tx: mpsc::Sender<BridgeCommand>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<BridgeResponse>>>>,
    connected: Arc<AtomicBool>,
    /// #7379 — le dernier `get_status` n'a reçu aucune donnée du pont : le
    /// volume rendu (0,5 de `OutputStatus::default()`) est un repli.
    volume_illisible: AtomicBool,
}

impl BridgeOutput {
    pub fn new(
        name: String,
        device_id: String,
        output_type_str: String,
        bridge_id: String,
        command_tx: mpsc::Sender<BridgeCommand>,
        connected: Arc<AtomicBool>,
    ) -> Self {
        Self {
            name,
            device_id,
            output_type_str,
            _bridge_id: bridge_id,
            command_tx,
            pending: Arc::new(Mutex::new(HashMap::new())),
            connected,
            volume_illisible: AtomicBool::new(false),
        }
    }

    pub fn with_shared_pending(
        name: String,
        device_id: String,
        output_type_str: String,
        bridge_id: String,
        command_tx: mpsc::Sender<BridgeCommand>,
        connected: Arc<AtomicBool>,
        pending: Arc<Mutex<HashMap<String, oneshot::Sender<BridgeResponse>>>>,
    ) -> Self {
        Self {
            name,
            device_id,
            output_type_str,
            _bridge_id: bridge_id,
            command_tx,
            pending,
            connected,
            volume_illisible: AtomicBool::new(false),
        }
    }

    pub fn pending_responses(
        &self,
    ) -> Arc<Mutex<HashMap<String, oneshot::Sender<BridgeResponse>>>> {
        self.pending.clone()
    }

    async fn send_command(
        &self,
        command: &str,
        payload: serde_json::Value,
    ) -> Result<BridgeResponse, String> {
        let id = uuid::Uuid::new_v4().to_string();
        let cmd = BridgeCommand {
            id: id.clone(),
            device_id: self.device_id.clone(),
            command: command.to_owned(),
            payload,
        };

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);

        self.command_tx
            .send(cmd)
            .await
            .map_err(|e| format!("bridge channel closed: {e}"))?;

        match tokio::time::timeout(Duration::from_secs(10), rx).await {
            Ok(Ok(resp)) => {
                if resp.ok {
                    Ok(resp)
                } else {
                    Err(resp.error.unwrap_or_else(|| "unknown error".into()))
                }
            }
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&id);
                Err("bridge response channel dropped".into())
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err("bridge command timeout (10s)".into())
            }
        }
    }
}

#[async_trait::async_trait]
impl OutputTarget for BridgeOutput {
    fn volume_du_statut_illisible(&self) -> bool {
        self.volume_illisible.load(Ordering::Relaxed)
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        &self.output_type_str
    }

    fn capabilities(&self) -> OutputCapabilities {
        OutputCapabilities::v1(true, true, true, true, true, true)
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        let payload = serde_json::json!({
            "url": media.url,
            "mime_type": media.mime_type,
            "title": media.title,
            "artist": media.artist,
            "album": media.album,
            "cover_url": media.cover_url,
        });
        self.send_command("play_media", payload).await?;
        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        self.send_command("pause", serde_json::json!({})).await?;
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        self.send_command("resume", serde_json::json!({})).await?;
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        self.send_command("stop", serde_json::json!({})).await?;
        Ok(())
    }

    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        self.send_command("seek", serde_json::json!({"position_ms": position_ms}))
            .await?;
        Ok(())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        self.send_command("set_volume", serde_json::json!({"volume": volume}))
            .await?;
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        self.send_command("set_mute", serde_json::json!({"muted": muted}))
            .await?;
        Ok(())
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let resp = self
            .send_command("get_status", serde_json::json!({}))
            .await?;
        if let Some(data) = resp.data {
            let statut: OutputStatus =
                serde_json::from_value(data).map_err(|e| format!("status parse: {e}"))?;
            self.volume_illisible
                .store(!statut.volume.is_finite(), Ordering::Relaxed);
            Ok(statut)
        } else {
            // #7379 (troisième revue, B3b) — aucune donnée : le statut par
            // défaut (volume 0,5) est un repli, signalé au sondeur.
            self.volume_illisible.store(true, Ordering::Relaxed);
            Ok(OutputStatus::default())
        }
    }

    async fn is_available(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    async fn set_next_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        let payload = serde_json::json!({
            "url": media.url,
            "mime_type": media.mime_type,
            "title": media.title,
            "artist": media.artist,
        });
        self.send_command("set_next_media", payload).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un faux pont : chaque commande reçoit `data`.
    fn faux_pont(data: Arc<std::sync::Mutex<Option<serde_json::Value>>>) -> BridgeOutput {
        let (tx, mut rx) = mpsc::channel::<BridgeCommand>(8);
        let sortie = BridgeOutput::new(
            "Pont".into(),
            "pont-7379".into(),
            "dlna".into(),
            "pont".into(),
            tx,
            Arc::new(AtomicBool::new(true)),
        );
        let pending = sortie.pending_responses();
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                let reponse = BridgeResponse {
                    id: cmd.id.clone(),
                    ok: true,
                    data: data.lock().unwrap().clone(),
                    error: None,
                };
                if let Some(envoi) = pending.lock().await.remove(&cmd.id) {
                    let _ = envoi.send(reponse);
                }
            }
        });
        sortie
    }

    /// #7379 (troisième revue, B3b) — une réponse `ok` SANS données rend le
    /// statut par défaut (volume 0,5) : la sortie le signale au sondeur ; un
    /// statut transmis par le pont est une vraie valeur.
    #[tokio::test]
    async fn un_statut_sans_donnees_signale_le_repli_7379() {
        let data = Arc::new(std::sync::Mutex::new(None));
        let sortie = faux_pont(data.clone());
        let statut = sortie.get_status().await.unwrap();
        assert_eq!(statut.volume, 0.5);
        assert!(
            sortie.volume_du_statut_illisible(),
            "repli du pont non signalé"
        );
        *data.lock().unwrap() = Some(
            serde_json::to_value(OutputStatus {
                volume: 0.4,
                ..Default::default()
            })
            .unwrap(),
        );
        let statut = sortie.get_status().await.unwrap();
        assert!((statut.volume - 0.4).abs() < 1e-9);
        assert!(!sortie.volume_du_statut_illisible());
    }
}
