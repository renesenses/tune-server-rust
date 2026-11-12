//! #6718 — un pré-transcodage abandonné ne laisse plus son fichier derrière lui.
//!
//! Le pré-transcodage complet d'une piste de service (téléchargement, décodage,
//! traitement, encodage) tourne dans un `spawn_blocking`. Quand la future qui
//! l'attend est abandonnée, par exemple quand la préparation de la suivante
//! dépasse son budget (`gapless_preparation_abandonnee_hors_budget`, #5970),
//! la tâche bloquante, elle, va au bout : on ne peut pas l'interrompre. Elle
//! écrivait alors `tune-stream-pretranscode-*` dans le dossier temporaire, et
//! plus personne n'était là pour créer la session ou supprimer le fichier.
//!
//! [`attendre_ou_nettoyer`] attend la tâche comme avant. Si la future est
//! abandonnée avant la fin, une garde confie la tâche à un ramasseur qui
//! attend sa fin, puis supprime les fichiers donnés.

use tokio::task::{JoinError, JoinHandle};
use tracing::info;

/// La garde : tant qu'elle tient la tâche, celle-ci n'est pas terminée côté
/// appelant. Relâchée sans avoir été désarmée, elle délègue le nettoyage.
struct GardeDeTacheBloquante<T: Send + 'static> {
    tache: Option<JoinHandle<T>>,
    fichiers: Vec<String>,
}

impl<T: Send + 'static> Drop for GardeDeTacheBloquante<T> {
    fn drop(&mut self) {
        let Some(tache) = self.tache.take() else {
            return;
        };
        let fichiers = std::mem::take(&mut self.fichiers);
        // Hors d'un runtime (arrêt du serveur), rien à faire de mieux : le
        // processus s'en va avec la tâche.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            let _ = tache.await;
            let mut supprimes = 0usize;
            for f in &fichiers {
                if std::fs::remove_file(f).is_ok() {
                    supprimes += 1;
                }
            }
            info!(supprimes, "pretranscode_abandonne_fichiers_nettoyes");
        });
    }
}

/// Attend `tache`. Si l'attente est abandonnée avant la fin de la tâche, les
/// `fichiers` sont supprimés dès que la tâche se termine.
pub(crate) async fn attendre_ou_nettoyer<T: Send + 'static>(
    tache: JoinHandle<T>,
    fichiers: Vec<String>,
) -> Result<T, JoinError> {
    let mut garde = GardeDeTacheBloquante {
        tache: Some(tache),
        fichiers,
    };
    let resultat = garde
        .tache
        .as_mut()
        .expect("tâche présente jusqu'à sa fin")
        .await;
    // Terminée : le résultat (et ses fichiers) revient à l'appelant.
    garde.tache = None;
    resultat
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn fichier_temporaire(nom: &str) -> String {
        // tmp-autorise: fichier au nom aléatoire (UUID v4), propre au test.
        std::env::temp_dir()
            .join(format!("tune-test-6718-{nom}-{}", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .to_string()
    }

    /// La tâche bloquante écrit son fichier APRÈS l'abandon de l'attente,
    /// comme le pré-transcodage d'une suivante hors budget.
    fn tache_lente(chemin: String) -> JoinHandle<Result<u64, String>> {
        tokio::task::spawn_blocking(move || {
            std::thread::sleep(Duration::from_millis(300));
            std::fs::write(&chemin, b"flac").map_err(|e| e.to_string())?;
            Ok(4)
        })
    }

    async fn attendre_que(cond: impl Fn() -> bool) -> bool {
        for _ in 0..100 {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        cond()
    }

    #[tokio::test]
    async fn un_pretranscode_abandonne_ne_laisse_pas_son_fichier_6718() {
        let chemin = fichier_temporaire("abandon");
        let attente = attendre_ou_nettoyer(tache_lente(chemin.clone()), vec![chemin.clone()]);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), attente)
                .await
                .is_err(),
            "le budget doit expirer avant la fin de la tâche"
        );
        // La tâche finit d'écrire, puis le ramasseur supprime.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            attendre_que(|| !std::path::Path::new(&chemin).exists()).await,
            "le fichier du pré-transcodage abandonné est resté : {chemin}"
        );
    }

    /// CONTRE-ÉPREUVE PERMANENTE : la même tâche attendue sans la garde laisse
    /// son fichier orphelin. Le scénario du test précédent est discriminant.
    #[tokio::test]
    async fn sans_la_garde_le_fichier_reste_orphelin_6718() {
        let chemin = fichier_temporaire("sans-garde");
        let tache = tache_lente(chemin.clone());
        assert!(
            tokio::time::timeout(Duration::from_millis(50), tache)
                .await
                .is_err()
        );
        assert!(
            attendre_que(|| std::path::Path::new(&chemin).exists()).await,
            "sans la garde, la tâche écrit quand même son fichier"
        );
        let _ = std::fs::remove_file(&chemin);
    }

    /// Une tâche attendue jusqu'au bout rend son résultat et garde son
    /// fichier : c'est à l'appelant d'en faire une session.
    #[tokio::test]
    async fn un_pretranscode_mene_a_terme_garde_son_fichier_6718() {
        let chemin = fichier_temporaire("termine");
        let r = attendre_ou_nettoyer(tache_lente(chemin.clone()), vec![chemin.clone()])
            .await
            .expect("tâche terminée")
            .expect("écriture");
        assert_eq!(r, 4);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(std::path::Path::new(&chemin).exists());
        let _ = std::fs::remove_file(&chemin);
    }
}
