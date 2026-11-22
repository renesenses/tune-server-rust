//! #9260 — lancer hors du fil appelant un travail qui touche la base.
//!
//! Le moteur PostgreSQL ([`super::backend::PostgresBackend`]) exécute chaque
//! requête par `block_in_place` + `Handle::current().block_on` : sur un fil
//! std nu, sans exécuteur Tokio, il panique (« there is no reactor
//! running »). Les fils de fond `cles-de-tri` et `debit-des-pistes` étaient
//! des fils nus : sous PostgreSQL ils mouraient à la première requête, et
//! leur drapeau « en cours » restait pris pour toute la vie du processus.
//!
//! [`lancer`] reprend le motif de #9183 (`arbre_des_dossiers`) :
//! - un exécuteur Tokio est là (le serveur) : le travail part sur son pool de
//!   fils BLOQUANTS (`spawn_blocking`), où le moteur PostgreSQL fonctionne ;
//! - aucun exécuteur (outil, test synchrone) : un fil à lui, dans lequel un
//!   petit exécuteur dédié est ENTRÉ avant le travail — `Handle::current()`
//!   y répond, le moteur PostgreSQL aussi.
//!
//! Le drapeau « en cours » de l'appelant se relâche dans le `Drop` d'une
//! garde que le travail emporte : à la fin normale, sur panique, et quand le
//! travail n'a jamais pu partir (fermeture abandonnée, garde détruite avec
//! elle).

/// Lance `travail` hors du fil appelant (voir le module). Rend `false` si
/// aucun fil n'a pu le prendre : `travail` est alors détruit sans avoir
/// tourné, ce qui relâche les gardes qu'il portait.
pub fn lancer(nom: &str, travail: impl FnOnce() + Send + 'static) -> bool {
    match tokio::runtime::Handle::try_current() {
        Ok(executeur) => {
            // Un exécuteur qui s'arrête rend une tâche annulée : la fermeture
            // est détruite, ses gardes relâchent leur drapeau.
            drop(executeur.spawn_blocking(travail));
            true
        }
        Err(_) => {
            let lance = std::thread::Builder::new()
                .name(nom.to_string())
                .spawn(move || match executeur_dedie() {
                    Ok(executeur) => {
                        let _dans = executeur.enter();
                        travail();
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "fil_de_fond_sans_executeur");
                        travail();
                    }
                });
            match lance {
                Ok(_) => true,
                Err(e) => {
                    tracing::warn!(fil = nom, error = %e, "fil_de_fond_impossible");
                    false
                }
            }
        }
    }
}

/// Multi-fil (un ouvrier) : `block_in_place` panique dans le contexte d'un
/// exécuteur mono-fil, et c'est l'ouvrier qui fait tourner le réacteur
/// pendant que le fil appelant attend dans `Handle::block_on`.
fn executeur_dedie() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("fil-de-fond-executeur")
        .enable_all()
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    struct Garde(Arc<AtomicBool>);
    impl Drop for Garde {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }

    /// Le travail voit un exécuteur, même lancé d'un fil sans exécuteur :
    /// c'est ce que réclame le moteur PostgreSQL.
    #[test]
    fn sans_executeur_le_travail_en_voit_un_9260() {
        assert!(tokio::runtime::Handle::try_current().is_err());
        let (tx, rx) = mpsc::channel();
        assert!(lancer("essai-9260", move || {
            let present = tokio::runtime::Handle::try_current().is_ok();
            // Le geste exact du moteur PostgreSQL.
            let rendu = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async { 7 })
            });
            let _ = tx.send((present, rendu));
        }));
        let (present, rendu) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(present, "aucun exécuteur dans le fil de fond");
        assert_eq!(rendu, 7);
    }

    /// Dans un exécuteur : le pool bloquant, où le geste du moteur
    /// PostgreSQL fonctionne aussi.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dans_un_executeur_le_pool_bloquant_9260() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        assert!(lancer("essai-9260", move || {
            let rendu = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async { 9 })
            });
            let _ = tx.send(rendu);
        }));
        assert_eq!(rx.await.unwrap(), 9);
    }

    /// Une panique du travail relâche la garde qu'il emporte.
    #[test]
    fn une_panique_relache_la_garde_9260() {
        let drapeau = Arc::new(AtomicBool::new(true));
        let garde = Garde(drapeau.clone());
        assert!(lancer("essai-9260", move || {
            let _garde = garde;
            panic!("panique voulue (#9260)");
        }));
        let debut = std::time::Instant::now();
        while drapeau.load(Ordering::SeqCst) {
            assert!(
                debut.elapsed() < Duration::from_secs(10),
                "la garde n'a pas relâché le drapeau après la panique"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
