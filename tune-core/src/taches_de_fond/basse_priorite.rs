//! Les décodages des analyses de fond tournent en priorité processeur basse
//! (#7915).
//!
//! Sur un NAS lent (Xeon E3-1260, OpenMediaVault), le premier scan finissait
//! en 2 h, puis le processeur restait haut toute la nuit : les empreintes et
//! la plage dynamique décodaient toute la bibliothèque sur le pool bloquant
//! ordinaire de tokio, en priorité NORMALE. Les fils du scan étaient déjà à
//! `nice 10` (`scanner/walker.rs`), pas ceux des analyses.
//!
//! ## Pourquoi un fil neuf par travail, et pas le pool bloquant
//!
//! `politique.rs` l'a écrit : un compte ordinaire ne peut pas RELEVER un
//! `nice` qu'il a baissé, et les fils du pool bloquant servent ensuite
//! d'autres travaux (lecture, API). On ne peut donc pas baisser un de ces
//! fils le temps d'un décodage.
//!
//! Un pool dédié à taille fixe aurait un autre défaut : un fichier qui fige
//! le décodeur (#1155) y garde son fil pour toujours, et quatre fichiers de
//! ce genre arrêteraient toutes les analyses. Un fil neuf par travail, baissé
//! à sa naissance, se comporte comme `spawn_blocking` face à un blocage (le
//! fil reste pris, rien d'autre ne l'attend) et coûte quelques dizaines de
//! microsecondes, pour un travail qui dure de quelques secondes (segment de
//! 30 s) à une minute.
//!
//! Le nombre de travaux en vol reste celui de la vitesse réglée
//! ([`super::vitesse::largeur_courante`]) : ce module baisse la priorité, il ne
//! change pas le parallélisme.
//!
//! ## Ce qui est baissé, par système
//!
//! * **Linux** : `nice 10` et classe d'E/S best-effort niveau 7, comme les fils
//!   du scan.
//! * **macOS** : l'état « arrière-plan » du fil (`PRIO_DARWIN_BG`), qui abaisse
//!   le processeur et bride les E/S.
//! * Ailleurs : rien, le travail tourne comme avant.
//!
//! Jamais d'écriture en base sur ces fils : elles passent par
//! [`super::priorite::hors_du_fil_async`] (inversion de priorité sur le verrou
//! d'écriture unique, voir `politique.rs`).

/// Le `nice` posé sur les fils des analyses de fond, le même que le scan.
pub const NICE_DES_ANALYSES: i32 = 10;

/// Le travail n'a pas rendu de résultat : il a paniqué, ou le fil n'a pas pu
/// être créé. L'appelant le traite comme il traitait l'échec d'un
/// `spawn_blocking`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interrompu;

impl std::fmt::Display for Interrompu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("travail d'analyse de fond interrompu")
    }
}

/// Faire tourner un travail synchrone d'analyse (un décodage) sur un fil neuf
/// en priorité basse, et en attendre le résultat sans tenir un fil de
/// l'exécuteur.
pub async fn en_priorite_basse<T, F>(travail: F) -> Result<T, Interrompu>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    let lance = std::thread::Builder::new()
        .name("analyse-fond".into())
        .spawn(move || {
            baisser_le_fil_courant();
            // Un décodeur qui panique ne doit faire tomber ni le fil appelant
            // ni le processus : le canal se ferme, l'appelant lit Interrompu.
            if let Ok(v) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(travail)) {
                let _ = tx.send(v);
            }
        });
    if let Err(e) = lance {
        tracing::warn!(error = %e, "analyse_fond_fil_non_cree");
        return Err(Interrompu);
    }
    rx.await.map_err(|_| Interrompu)
}

/// Baisser, pour toute sa vie, la priorité du fil courant. Réservé aux fils
/// créés par [`en_priorite_basse`].
fn baisser_le_fil_courant() {
    #[cfg(target_os = "linux")]
    // SAFETY: `gettid` n'a pas d'argument ; `setpriority` et `ioprio_set`
    // visent le seul fil courant (PRIO_PROCESS avec un tid, IOPRIO_WHO_PROCESS).
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid);
        libc::setpriority(
            libc::PRIO_PROCESS as _,
            tid as libc::id_t,
            NICE_DES_ANALYSES,
        );
        const IOPRIO_CLASS_BE: libc::c_long = 2;
        const IOPRIO_CLASS_SHIFT: libc::c_long = 13;
        const IOPRIO_WHO_PROCESS: libc::c_long = 1;
        let ioprio = (IOPRIO_CLASS_BE << IOPRIO_CLASS_SHIFT) | 7;
        libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, tid, ioprio);
    }
    #[cfg(target_os = "macos")]
    // SAFETY: change l'état d'arrière-plan du seul fil courant (who = 0).
    unsafe {
        libc::setpriority(libc::PRIO_DARWIN_THREAD, 0, libc::PRIO_DARWIN_BG);
    }
}

/// Le `nice` du fil courant, tel que le noyau le voit. Pour les témoins.
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub fn nice_du_fil_courant() -> i32 {
    // SAFETY: lecture de la priorité du seul fil courant.
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid);
        libc::getpriority(libc::PRIO_PROCESS as _, tid as libc::id_t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn le_travail_rend_son_resultat() {
        assert_eq!(en_priorite_basse(|| 6 * 7).await, Ok(42));
    }

    #[tokio::test]
    async fn une_panique_rend_interrompu_sans_tomber() {
        let r: Result<(), _> = en_priorite_basse(|| panic!("décodeur fou")).await;
        assert_eq!(r, Err(Interrompu));
    }

    /// #7915 — le décodage d'une analyse tourne à `nice 10`, pas en priorité
    /// normale, et le fil de l'appelant n'est pas touché.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn le_fil_d_analyse_tourne_a_nice_10_7915() {
        let avant = nice_du_fil_courant();
        let nice = en_priorite_basse(nice_du_fil_courant).await.unwrap();
        assert!(
            nice >= NICE_DES_ANALYSES,
            "le fil d'analyse doit tourner à nice >= {NICE_DES_ANALYSES}, lu {nice} (#7915)"
        );
        assert_eq!(
            nice_du_fil_courant(),
            avant,
            "le fil appelant garde sa priorité"
        );
    }
}
