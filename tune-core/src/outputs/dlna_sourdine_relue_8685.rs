//! #8685 — la sourdine d'un renderer DLNA se RELIT sur l'appareil.
//!
//! Recette rc12 du 13/11 (.18, Eversolo DMP-A8) : bouton « non muet », pas
//! de son ; bouton « muet », du son. L'API disait `muted:false`, et chaque
//! `SetMute` du journal était acquitté. Tune n'a jamais relu l'état réel :
//! `muted` n'était que le dernier ordre posé (#2263), et #8151 renvoie
//! `SetMute(1)` à chaque piste, à chaque `Play`, à chaque enchaînement, sans
//! savoir si l'appareil était déjà muet. Un appareil qui applique la valeur
//! demandée n'en souffre pas ; un firmware qui BASCULE à chaque `SetMute`,
//! ou qui traite mal un ordre redondant, sort de ces réaffirmations avec une
//! sourdine inversée — exactement ce que le journal montre (une réaffirmation
//! à l'enchaînement, puis levée et pose acquittées, et le son à l'envers).
//!
//! Désormais : avant `SetMute`, `GetMute` ; un appareil déjà dans l'état
//! voulu ne reçoit rien. Après `SetMute`, `GetMute` encore ; un état relu
//! contraire (après une courte attente, pour un appareil qui applique en
//! différé) fait échouer l'ordre au lieu d'annoncer un état faux. Un
//! appareil qui ne sait pas répondre à `GetMute` garde le comportement
//! d'avant : l'acquittement fait foi.

use super::{DlnaOutput, RELECTURE_VOLUME_APRES, extract_tag, faute_commande_soap};
use tracing::{info, warn};

/// La sourdine lue dans une réponse `GetMute`. `None` pour une faute ou une
/// valeur absente ou inconnue : l'état n'est alors pas établi.
pub(super) fn sourdine_lue(reponse: &str) -> Option<bool> {
    if faute_commande_soap(reponse) {
        return None;
    }
    let valeur = extract_tag(reponse, "CurrentMute")?;
    match valeur.trim() {
        "1" => Some(true),
        "0" => Some(false),
        v if v.eq_ignore_ascii_case("true") => Some(true),
        v if v.eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    }
}

impl DlnaOutput {
    /// `GetMute` sur `Master`. `None` si l'appareil ne répond pas, refuse
    /// l'action, ou rend une valeur illisible.
    pub(super) async fn lire_la_sourdine(&self) -> Option<bool> {
        let reponse = self
            .rc_action(
                "GetMute",
                "<InstanceID>0</InstanceID><Channel>Master</Channel>",
            )
            .await
            .ok()?;
        sourdine_lue(&reponse)
    }

    /// Relit la sourdine après un `SetMute(attendu)` acquitté. L'état poussé
    /// prend la valeur établie ; un état contraire, encore relu après
    /// [`RELECTURE_VOLUME_APRES`], est une erreur.
    pub(super) async fn confirmer_la_sourdine(&self, attendu: bool) -> Result<(), String> {
        let mut lu = self.lire_la_sourdine().await;
        if lu == Some(!attendu) {
            tokio::time::sleep(RELECTURE_VOLUME_APRES).await;
            lu = self.lire_la_sourdine().await;
        }
        info!(
            device = %self.name,
            device_id = %self.device_id,
            attendu,
            lu = lu.map_or("illisible", |m| if m { "muet" } else { "son" }),
            "dlna_sourdine_relue"
        );
        if lu == Some(!attendu) {
            self.event_state.lock().await.muted = Some(!attendu);
            warn!(
                device = %self.name,
                device_id = %self.device_id,
                attendu,
                "dlna_sourdine_non_appliquee"
            );
            return Err(if attendu {
                format!(
                    "« {} » a acquitté la sourdine sans l'appliquer. Coupez le son sur l'appareil lui-même.",
                    self.name
                )
            } else {
                format!(
                    "« {} » est resté en sourdine. Levez-la sur l'appareil lui-même.",
                    self.name
                )
            });
        }
        self.event_state.lock().await.muted = Some(attendu);
        Ok(())
    }
}
