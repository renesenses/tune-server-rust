//! #8404 — point d'arrêt d'essai, juste avant le contrôle de la session et
//! l'envoi à la sortie (`envoyer_a_la_sortie`), là où une lecture DLNA
//! attend son pré-tampon (jusqu'à 4 s) et une sortie lente son verrou.
//!
//! Un essai y retient une lecture A pendant qu'une lecture B la remplace sur
//! la même zone, puis la relâche : c'est la course de deux `play` sur une
//! zone, rendue déterministe. Armé par zone et pour un seul passage, il ne
//! retient jamais la lecture qui le croise ensuite.
#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::sync::{Arc, LazyLock, Mutex};
#[cfg(test)]
use tokio::sync::Notify;

#[cfg(test)]
type Point = (Arc<Notify>, Arc<Notify>);

#[cfg(test)]
static POINTS: LazyLock<Mutex<HashMap<i64, Point>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Arme le point pour la prochaine lecture de `zone_id`. Rend (arrivée,
/// reprise) : l'essai attend la première, puis notifie la seconde.
#[cfg(test)]
pub(crate) fn armer(zone_id: i64) -> Point {
    let point = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    POINTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(zone_id, point.clone());
    point
}

/// Appelé par `envoyer_a_la_sortie`. Hors des tests, ou sans point armé pour
/// la zone : rien.
#[cfg(test)]
pub(crate) async fn traverser(zone_id: i64) {
    let point = POINTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&zone_id);
    if let Some((arrivee, reprise)) = point {
        arrivee.notify_one();
        reprise.notified().await;
    }
}

/// Hors des tests : rien.
#[cfg(not(test))]
#[inline(always)]
pub(crate) async fn traverser(_zone_id: i64) {}
