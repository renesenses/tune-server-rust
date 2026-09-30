//! #5440 — capturer un journal `tracing` dans un test de la lib sans dépendre
//! de l'ordre dans lequel les tests voisins atteignent un point d'appel.
//!
//! ## Le mécanisme (tracing-core 0.1.36, `callsite.rs`)
//!
//! Un point d'appel (`info!(…)`) met en cache, pour tout le processus, son
//! « intérêt » : `never` saute l'événement sans consulter personne. Ce cache
//! se calcule à la PREMIÈRE exécution du point d'appel (`register`) et se
//! recalcule à chaque nouvel abonné (`Dispatch::new` → `register_dispatch`).
//!
//! Le calcul a un raccourci, `has_just_one` : tant qu'au plus UN abonné vit
//! au moment du dernier `register_dispatch`, `register` ne lit pas la liste
//! des abonnés, il interroge l'abonné COURANT DU FIL qui exécute le point
//! d'appel (`Rebuilder::JustOne` → `dispatcher::get_default`). Dans la lib de
//! `tune-core`, aucun abonné global n'est posé : le seul abonné vivant est
//! celui qu'un test a posé par `set_default` sur SON fil. Si un test voisin,
//! sur un AUTRE fil, atteint le premier le point d'appel pendant ce temps, il
//! l'enregistre avec son propre abonné — aucun — et le fige à `never`. Le
//! test qui capture perd alors sa ligne, alors qu'il passe seul à coup sûr.
//!
//! ## Le remède
//!
//! Garder en vie, pour tout le processus, DEUX abonnés témoins qui ne sont
//! l'abonné courant d'aucun fil et ne s'intéressent à rien. Le second
//! `register_dispatch` compte deux abonnés vivants et pose `has_just_one` à
//! faux ; il ne peut plus redevenir vrai, puisque les témoins ne meurent
//! jamais. Deux et non un : le drapeau est ainsi baissé AVANT que le test ne
//! pose son propre abonné, et aucun fil voisin ne peut l'avoir lu vrai
//! pendant cette pose. Dès lors, chaque enregistrement de point d'appel lit
//! la liste COMPLÈTE sous verrou de lecture — sérialisé avec la pose de
//! l'abonné du test, qui prend le verrou d'écriture et recalcule tout.
//! L'intérêt devient `sometimes`, et la décision revient à l'abonné du fil
//! qui émet. Rien n'est attendu, rien n'est rejoué : l'ordre des fils
//! n'entre plus dans le résultat.

use std::sync::OnceLock;

use tracing::level_filters::LevelFilter;
use tracing::span;
use tracing::subscriber::Interest;
use tracing::{Dispatch, Event, Metadata, Subscriber};

/// L'abonné témoin : il n'est l'abonné courant d'aucun fil, n'accepte aucun
/// événement et n'élève pas le niveau maximal global (`OFF`).
struct Temoin;

impl Subscriber for Temoin {
    fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
        Interest::never()
    }
    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::OFF)
    }
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        false
    }
    fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }
    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
    fn event(&self, _: &Event<'_>) {}
    fn enter(&self, _: &span::Id) {}
    fn exit(&self, _: &span::Id) {}
}

/// À appeler AVANT `tracing::subscriber::set_default` dans un test qui
/// capture le journal — au début du test plutôt qu'à la pose de l'abonné :
/// un fil voisin qui aurait lu le drapeau vrai juste avant la création des
/// témoins a ainsi fini son enregistrement bien avant que l'abonné du test
/// n'existe, et la pose de celui-ci recalcule tout. Idempotent : les deux témoins sont créés une fois par
/// processus et ne meurent jamais.
pub fn fiabiliser_la_capture() {
    static TEMOINS: OnceLock<[Dispatch; 2]> = OnceLock::new();
    TEMOINS.get_or_init(|| [Dispatch::new(Temoin), Dispatch::new(Temoin)]);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Compte les événements qu'il reçoit ; intéressé par tout.
    struct Compteur(Arc<AtomicUsize>);

    impl Subscriber for Compteur {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }
        fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
        fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
        fn event(&self, _: &Event<'_>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn enter(&self, _: &span::Id) {}
        fn exit(&self, _: &span::Id) {}
    }

    /// Un seul point d'appel, que seul ce test atteint.
    fn emettre() {
        tracing::info!("temoin_5440");
    }

    /// La course de #5440, rendue certaine : pendant que le test capture, un
    /// fil VOISIN sans abonné atteint le premier le point d'appel. Sans les
    /// témoins, il l'enregistre avec son abonné à lui — aucun — et le fige à
    /// `never` : l'événement émis ensuite par le test n'arrive jamais.
    #[test]
    fn un_point_d_appel_atteint_d_abord_par_un_fil_voisin_reste_capture() {
        fiabiliser_la_capture();
        let recus = Arc::new(AtomicUsize::new(0));
        let _garde = tracing::subscriber::set_default(Compteur(recus.clone()));

        std::thread::spawn(emettre).join().unwrap();
        emettre();

        assert_eq!(
            recus.load(Ordering::SeqCst),
            1,
            "le fil voisin a figé le point d'appel à `never` : l'abonné du test \
             n'a pas reçu son événement"
        );
    }
}
