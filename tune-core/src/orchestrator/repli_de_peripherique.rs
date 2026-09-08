//! #2269 — la zone joue, mais pas là où elle le dit.
//!
//! Le repli lui-même est ancien et il est VOULU : quand le périphérique
//! enregistré par une zone locale n'est plus trouvable — débranché, renommé
//! par le pilote au changement de cadence, réordonné —, la sortie locale ouvre
//! le périphérique système plutôt que de ne rien jouer
//! ([`LocalDeviceFallback::NotFoundFellBackToDefault`]). #2501 a rendu ce repli
//! beaucoup plus rare en appariant d'abord par identifiant d'endpoint stable,
//! qui survit à un renommage ; il n'a pas disparu pour autant.
//!
//! Ce qui restait entier, et qui EST le titre du ticket, c'est son SILENCE :
//! l'auditeur croit écouter sa zone bit-perfect et entend la sortie système,
//! sans que rien ne le dise. Arbitrage de Bertrand, 01/09/2026 : « Jouer, et le
//! dire : réutiliser `zone.playback_error` avec `fatal: false` — le canal et le
//! champ existent déjà. Le repli n'est pas le défaut, son silence l'est. »
//!
//! Aucun second canal n'est ouvert :
//!
//! - `zone.playback_error` est celui que six autres échecs de lecture
//!   empruntent déjà (radio morte, décodage, session perdue, refus de
//!   sortie…), et `routes/ws.rs` le pousse verbatim à tous les clients ;
//! - `fatal` y est un champ existant. Les autres émetteurs le posent à `true`
//!   parce qu'ils accompagnent un ARRÊT. Ici la lecture continue : marquer
//!   fatal dirait à l'écran que la zone s'est arrêtée, c'est-à-dire
//!   exactement le mensonge inverse de celui qu'on corrige ;
//! - le fait lui-même reste par ailleurs LISIBLE à tout instant dans
//!   `audio_backend_status.device` de l'instantané de zone (`routes/ws.rs`,
//!   `routes/zones/lecture.rs`, depuis #2207/#3230). Cet événement n'invente
//!   donc pas la donnée : il ALERTE sur une donnée déjà publiée.
//!
//! Le canal `take_output_failure()` — l'autre remontée de la sortie locale —
//! n'est délibérément pas emprunté : le sondeur l'interprète en ARRÊT de zone
//! (`poller/tick.rs`, `output_reported_failure_stopping_zone`). Le brancher ici
//! changerait « ça joue sur le mauvais haut-parleur » en « ça ne joue plus ».

use super::*;
use crate::outputs::local::{LocalDeviceFallback, LocalDeviceStatus};

/// Le repli du périphérique local mérite-t-il d'être dit — et en quels mots ?
///
/// `None` couvre tout ce qui n'est PAS le repli silencieux de ce ticket :
///
/// - aucun motif, ou un motif autre. [`LocalDeviceFallback::ForeignHost`] est
///   un REFUS (rien n'est ouvert, `opened` est vide) : il remonte déjà par
///   `take_output_failure()`, et l'annoncer ici en doublon donnerait deux
///   messages pour un seul fait ;
/// - un périphérique ouvert sans nom : on ne fabrique pas la moitié de la
///   phrase qui manque.
///
/// La phrase nomme les DEUX périphériques — celui que la zone demande et celui
/// qui joue — parce que c'est le seul couple qui permette à l'auditeur de
/// décider quoi faire : rebrancher, ou re-choisir la sortie de sa zone.
pub(super) fn message_de_repli_de_peripherique(device: &LocalDeviceStatus) -> Option<String> {
    if device.reason != Some(LocalDeviceFallback::NotFoundFellBackToDefault) {
        return None;
    }
    let demande = device.requested.trim();
    let ouvert = device.opened.trim();
    if demande.is_empty() || ouvert.is_empty() {
        return None;
    }
    Some(format!(
        "Le son de cette zone ne sort PAS sur la sortie choisie : « {demande} » est \
         introuvable (débranché, ou renommé par son pilote), la lecture se fait donc \
         sur « {ouvert} », la sortie par défaut du système. Rebranchez l'appareil, ou \
         choisissez à nouveau la sortie de cette zone."
    ))
}

/// Dire le repli à l'auditeur, sans arrêter la lecture.
///
/// Rend le message émis, ou `None` quand il n'y avait rien à dire — c'est ce
/// que l'appelant mémorise pour ne pas répéter la même phrase à chaque piste.
///
/// Sans bus (démarrage partiel, tests) on ne panique pas : on se tait, comme
/// [`emit_radio_playback_error`].
pub(super) fn dire_le_repli_de_peripherique(
    bus: &Option<Arc<EventBus>>,
    zone_id: i64,
    device: Option<&LocalDeviceStatus>,
) -> Option<String> {
    let device = device?;
    let message = message_de_repli_de_peripherique(device)?;
    // `local_device_fallback_announced` est un littéral : il survit à la
    // compilation et discrimine donc deux binaires, contrairement à un nom de
    // fonction que l'édition de liens efface.
    warn!(
        zone_id,
        demande = %device.requested,
        ouvert = %device.opened,
        backend = device.backend,
        "local_device_fallback_announced"
    );
    if let Some(bus) = bus {
        bus.emit(
            "zone.playback_error",
            serde_json::json!({
                "zone_id": zone_id,
                "error": message,
                // `false` : la zone JOUE. C'est tout le sens de l'arbitrage —
                // le repli n'est pas le défaut, son silence l'est.
                "fatal": false,
            }),
        );
    }
    Some(message)
}

impl PlaybackOrchestrator {
    /// Après un démarrage de lecture LOCALE : la zone joue-t-elle ailleurs que
    /// là où elle le dit — et faut-il le dire maintenant ?
    ///
    /// L'observation lue ici est celle que la sortie locale a déposée en
    /// OUVRANT le périphérique (`OBSERVED_DEVICE`), la même que celle qui
    /// alimente déjà `audio_backend_status.device` des instantanés de zone.
    /// Comme elle, elle porte la DERNIÈRE ouverture locale de la machine : sur
    /// un serveur qui pilote plusieurs sorties locales à la fois, c'est une
    /// approximation, et c'est celle que le serveur publie déjà partout
    /// ailleurs — on n'en invente pas une seconde ici.
    ///
    /// **Une fois par divergence, pas une fois par piste.** Sans cette
    /// mémoire, une zone repliée annoncerait la même phrase à chaque avance
    /// gapless, ce qui transformerait l'information en bruit. L'entrée est
    /// effacée dès que le périphérique demandé est de nouveau celui qui joue,
    /// pour qu'un repli ultérieur soit dit à son tour.
    pub(super) fn dire_si_la_zone_joue_ailleurs(&self, zone_id: i64) {
        let (_exclusif, backend_demande) = self.reglages_sortie_locale();
        let statut = crate::outputs::local::active_backend_status(&backend_demande);
        let message = statut
            .device
            .as_ref()
            .and_then(message_de_repli_de_peripherique);
        let Ok(mut deja_dit) = self.replis_de_peripherique_dits.lock() else {
            return;
        };
        let Some(message) = message else {
            deja_dit.remove(&zone_id);
            return;
        };
        if deja_dit
            .get(&zone_id)
            .is_some_and(|precedent| *precedent == message)
        {
            return;
        }
        deja_dit.insert(zone_id, message);
        // Verrou std : jamais tenu pendant l'émission.
        drop(deja_dit);
        dire_le_repli_de_peripherique(&self.event_bus, zone_id, statut.device.as_ref());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le périphérique du cas décrit par DEvir : réglé sur un DAC, joué sur les
    /// haut-parleurs de la carte mère parce que le pilote a renommé la sortie.
    fn peripherique_introuvable() -> LocalDeviceStatus {
        LocalDeviceStatus {
            backend: "WASAPI",
            requested: "Topping D90".to_string(),
            opened: "Haut-parleurs (Realtek(R) Audio)".to_string(),
            opened_id: None,
            differs: true,
            reason: Some(LocalDeviceFallback::NotFoundFellBackToDefault),
            detail: Some(LocalDeviceFallback::NotFoundFellBackToDefault.detail()),
        }
    }

    /// Le même périphérique, retrouvé : rien à signaler.
    fn peripherique_trouve() -> LocalDeviceStatus {
        LocalDeviceStatus {
            backend: "WASAPI",
            requested: "Topping D90".to_string(),
            opened: "Topping D90".to_string(),
            opened_id: Some("{0.0.0.00000000}.{topping}".to_string()),
            differs: false,
            reason: None,
            detail: None,
        }
    }

    /// TÉMOIN — un périphérique introuvable : la lecture continue ET elle le
    /// dit, en nommant les deux périphériques.
    ///
    /// `fatal: false` est le fait à tenir, pas une décoration : les six autres
    /// émetteurs de `zone.playback_error` posent `true` parce qu'ils
    /// accompagnent un arrêt de zone. Ici la zone joue.
    #[tokio::test]
    async fn un_peripherique_introuvable_le_dit_sans_arreter_la_lecture() {
        let bus = Arc::new(EventBus::new());
        let mut rx = bus.subscribe();
        let statut = peripherique_introuvable();

        let rendu = dire_le_repli_de_peripherique(&Some(bus.clone()), 7, Some(&statut));
        assert!(
            rendu.is_some(),
            "un repli sur la sortie système doit produire un message"
        );

        let ev = rx.recv().await.expect("un événement doit être émis");
        assert_eq!(ev.event_type, "zone.playback_error");
        assert_eq!(ev.data["zone_id"], 7);
        assert_eq!(
            ev.data["fatal"], false,
            "la zone JOUE : marquer fatal dirait à l'écran qu'elle s'est arrêtée"
        );
        let msg = ev.data["error"].as_str().unwrap();
        assert!(
            msg.contains("Topping D90"),
            "le message doit nommer le périphérique DEMANDÉ : {msg}"
        );
        assert!(
            msg.contains("Haut-parleurs (Realtek(R) Audio)"),
            "le message doit nommer le périphérique réellement OUVERT : {msg}"
        );
    }

    /// TÉMOIN — le périphérique demandé est celui qui joue : silence total.
    ///
    /// La contre-épreuve de l'annonce : une alerte qui partirait aussi quand
    /// tout va bien serait pire que le silence qu'on corrige.
    #[tokio::test]
    async fn un_peripherique_trouve_ne_dit_rien() {
        let bus = Arc::new(EventBus::new());
        let mut rx = bus.subscribe();

        assert_eq!(
            dire_le_repli_de_peripherique(&Some(bus.clone()), 7, Some(&peripherique_trouve())),
            None
        );
        // Aucune observation du tout : même silence.
        assert_eq!(
            dire_le_repli_de_peripherique(&Some(bus.clone()), 7, None),
            None
        );

        assert!(
            rx.try_recv().is_err(),
            "aucun événement ne doit partir quand la zone joue sur le périphérique qu'elle nomme"
        );
    }

    /// Un REFUS (`ForeignHost`) n'est pas un repli : rien n'est ouvert, et le
    /// canal `take_output_failure()` le porte déjà. Deux messages pour un seul
    /// fait seraient le second canal que l'arbitrage écarte.
    #[test]
    fn un_refus_de_peripherique_ne_passe_pas_par_ce_canal() {
        let refus = LocalDeviceStatus {
            backend: "ASIO",
            requested: "Haut-parleurs".to_string(),
            opened: String::new(),
            opened_id: None,
            differs: true,
            reason: Some(LocalDeviceFallback::ForeignHost),
            detail: Some(LocalDeviceFallback::ForeignHost.detail()),
        };
        assert_eq!(message_de_repli_de_peripherique(&refus), None);
    }

    /// Sans bus (démarrage partiel, tests) on ne panique pas.
    #[test]
    fn sans_bus_ce_n_est_pas_une_panique() {
        assert!(
            dire_le_repli_de_peripherique(&None, 1, Some(&peripherique_introuvable())).is_some()
        );
    }
}
