//! La porte de la découverte réseau (#8189).
//!
//! Sur iOS, le premier paquet multicast (SSDP, mDNS) ouvre la fenêtre système
//! « Autoriser Tune à trouver des appareils sur le réseau local ». Si le moteur
//! embarqué découvre dès le lancement, cette fenêtre arrive avant que
//! l'utilisateur ait demandé quoi que ce soit, sans contexte, et un refus
//! réflexe coupe ensuite toute la découverte.
//!
//! L'hôte peut donc FERMER la porte avant le démarrage : les tâches qui
//! émettent en multicast attendent (`attendre`) qu'elle s'ouvre, et les
//! recherches à la demande rendent ce qui est déjà connu (`est_ouverte`). Elle
//! s'ouvre quand l'utilisateur ouvre l'écran « Sources du réseau » ou demande
//! une recherche d'appareils, ou d'emblée au lancement suivant s'il l'a déjà
//! fait une fois.
//!
//! Ouverte par défaut : un serveur de bureau, Android ou un test ne voient
//! aucune différence.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

static OUVERTE: AtomicBool = AtomicBool::new(true);

fn signal() -> &'static tokio::sync::Notify {
    static SIGNAL: OnceLock<tokio::sync::Notify> = OnceLock::new();
    SIGNAL.get_or_init(tokio::sync::Notify::new)
}

/// Ferme la porte : à appeler AVANT le démarrage des tâches de découverte.
pub fn fermer() {
    OUVERTE.store(false, Ordering::SeqCst);
}

/// Ouvre la porte et réveille les tâches qui attendent. Rend `true` si elle
/// était fermée (c'est donc cet appel qui l'a ouverte).
pub fn ouvrir() -> bool {
    let etait_fermee = !OUVERTE.swap(true, Ordering::SeqCst);
    if etait_fermee {
        tracing::info!("decouverte_reseau_ouverte");
    }
    signal().notify_waiters();
    etait_fermee
}

pub fn est_ouverte() -> bool {
    OUVERTE.load(Ordering::SeqCst)
}

/// Rend la main dès que la porte est ouverte (tout de suite si elle l'est).
pub async fn attendre() {
    loop {
        let reveil = signal().notified();
        tokio::pin!(reveil);
        // S'inscrire AVANT de relire l'état : un `ouvrir` glissé entre la
        // lecture et l'attente ne serait sinon jamais vu.
        reveil.as_mut().enable();
        if est_ouverte() {
            return;
        }
        reveil.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// L'état est celui du processus : un seul test le manipule, du début à
    /// la fin, pour ne pas courir contre un autre.
    #[tokio::test]
    async fn fermee_elle_retient_les_taches_ouverte_elle_les_libere() {
        assert!(est_ouverte(), "ouverte par défaut");
        attendre().await; // ne bloque pas

        fermer();
        assert!(!est_ouverte());
        let tache = tokio::spawn(attendre());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !tache.is_finished(),
            "porte fermée : la tâche doit attendre"
        );

        assert!(ouvrir(), "c'est cet appel qui ouvre");
        tokio::time::timeout(std::time::Duration::from_secs(2), tache)
            .await
            .expect("la tâche doit repartir à l'ouverture")
            .unwrap();
        assert!(!ouvrir(), "déjà ouverte");
        assert!(est_ouverte());
    }
}
