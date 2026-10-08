//! Les fils de travail tokio vus par leurs crochets de garage (#5677).
//!
//! Le relevé de #4924 liste chaque fil du processus, mais un fil de travail
//! tokio et un fil de `spawn_blocking` y portent le même nom
//! (`tokio-runtime-w`) : impossible d'y lire LESQUELS des fils de travail
//! étaient pris, ni depuis quand. Les crochets `on_thread_park` /
//! `on_thread_unpark` du moteur (posés par
//! [`crate::fils_de_travail::construire_le_moteur`]) datent ici, pour chaque
//! fil de travail, le moment où il a repris du travail ; zéro quand il est
//! garé. Le relevé en déduit « fil tid N au travail depuis X ms sans se
//! garer » : un fil pris dans un appel bloquant y reste, un fil sain se gare
//! plusieurs fois par seconde.
//!
//! Coût : une lecture d'horloge et un stockage atomique à chaque garage et
//! réveil — ce que tokio fait déjà pour ses propres compteurs.
//!
//! **Ce que le fil exécute.** Savoir qu'un fil de travail est pris ne dit pas
//! par quoi. Le relevé du banc (#5677, un processeur, un fil) montrait le
//! seul fil de travail « au travail depuis 3 s », en état R, sans lecture
//! SQLite à son nom : du calcul synchrone dans un gestionnaire HTTP. La couche
//! [`surveiller_les_polls`], posée sur `/api/v1`, inscrit dans la place du
//! fil la route (méthode et gabarit, `GET /library/tracks` — jamais l'URL ni
//! ses paramètres) pendant chaque `poll` de son gestionnaire. Un `poll` qui
//! rend la main en attendant (`spawn_blocking`, E/S) ne reste pas inscrit ;
//! un `poll` qui calcule, si.

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

/// Places du tableau. Une place est rendue quand son fil s'arrête
/// (`on_thread_stop`), donc ce nombre borne les fils vivants, pas ceux qu'a
/// vus le processus.
const PLACES: usize = 256;

struct Place {
    /// Identifiant du fil (tid noyau sous Linux, sinon un numéro interne) ;
    /// 0 = place libre.
    id: AtomicI64,
    /// Millisecondes depuis [`origine`] + 1 au dernier réveil ; 0 = garé.
    au_travail_depuis: AtomicU64,
    /// La route dont le gestionnaire est en plein `poll` sur ce fil, et
    /// depuis quand.
    poll: Mutex<Option<(Arc<str>, Instant)>>,
}

#[allow(clippy::declare_interior_mutable_const)]
const LIBRE: Place = Place {
    id: AtomicI64::new(0),
    au_travail_depuis: AtomicU64::new(0),
    poll: Mutex::new(None),
};

static TABLEAU: [Place; PLACES] = [LIBRE; PLACES];

static NUMERO_INTERNE: AtomicI64 = AtomicI64::new(-1);

thread_local! {
    static MA_PLACE: Cell<Option<usize>> = const { Cell::new(None) };
}

fn origine() -> Instant {
    static O: OnceLock<Instant> = OnceLock::new();
    *O.get_or_init(Instant::now)
}

fn maintenant_ms() -> u64 {
    origine().elapsed().as_millis() as u64 + 1
}

fn ma_place() -> Option<usize> {
    MA_PLACE.with(|c| {
        if let Some(i) = c.get() {
            return Some(i);
        }
        let mut id = tune_core::db::verrou_ecriture::tid_courant();
        if id == 0 {
            id = NUMERO_INTERNE.fetch_sub(1, Ordering::Relaxed);
        }
        let i = TABLEAU.iter().position(|p| {
            p.id.compare_exchange(0, id, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        })?;
        c.set(Some(i));
        Some(i)
    })
}

/// Crochet `on_thread_unpark` : le fil reprend du travail.
pub fn au_reveil() {
    if let Some(i) = ma_place() {
        TABLEAU[i]
            .au_travail_depuis
            .store(maintenant_ms(), Ordering::Relaxed);
    }
}

/// Crochet `on_thread_park` : le fil n'a plus rien à faire.
pub fn au_garage() {
    if let Some(i) = ma_place() {
        TABLEAU[i].au_travail_depuis.store(0, Ordering::Relaxed);
    }
}

/// Crochet `on_thread_stop` : la place est rendue.
pub fn a_l_arret() {
    if let Some(i) = MA_PLACE.with(|c| c.take()) {
        TABLEAU[i].au_travail_depuis.store(0, Ordering::Relaxed);
        if let Ok(mut p) = TABLEAU[i].poll.lock() {
            *p = None;
        }
        TABLEAU[i].id.store(0, Ordering::Release);
    }
}

/// Inscrit `route` dans la place du fil courant le temps d'un `poll`. Un fil
/// sans place (pas un fil de travail) n'inscrit rien.
struct PollInscrit(Option<usize>);

impl PollInscrit {
    fn debut(route: &Arc<str>) -> Self {
        let place = MA_PLACE.with(|c| c.get());
        if let Some(i) = place
            && let Ok(mut p) = TABLEAU[i].poll.lock()
        {
            *p = Some((route.clone(), Instant::now()));
        }
        PollInscrit(place)
    }
}

impl Drop for PollInscrit {
    fn drop(&mut self) {
        if let Some(i) = self.0
            && let Ok(mut p) = TABLEAU[i].poll.lock()
        {
            *p = None;
        }
    }
}

/// Un futur dont chaque `poll` est inscrit sous `route`.
pub struct Surveille<F> {
    inner: Pin<Box<F>>,
    route: Arc<str>,
}

impl<F: Future> Surveille<F> {
    pub fn new(route: Arc<str>, inner: F) -> Self {
        Surveille {
            inner: Box::pin(inner),
            route,
        }
    }
}

impl<F: Future> Future for Surveille<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let this = &mut *self;
        let _inscrit = PollInscrit::debut(&this.route);
        this.inner.as_mut().poll(cx)
    }
}

/// Couche axum : chaque `poll` d'un gestionnaire est inscrit sous sa route
/// (méthode + gabarit de chemin, jamais l'URL réelle ni ses paramètres).
pub async fn surveiller_les_polls(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let gabarit = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| "(sans route)".to_string());
    let route: Arc<str> = format!("{} {gabarit}", req.method()).into();
    Surveille::new(route, next.run(req)).await
}

/// Un fil de travail vu par ses crochets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Travailleur {
    /// tid noyau sous Linux ; un numéro négatif ailleurs.
    pub id: i64,
    /// `None` : garé. `Some(d)` : au travail depuis `d` sans s'être garé.
    pub au_travail_depuis: Option<Duration>,
    /// La route en plein `poll` sur ce fil, et depuis quand.
    pub poll: Option<(String, Duration)>,
}

/// Les fils de travail connus, le plus longtemps au travail d'abord.
pub fn etat() -> Vec<Travailleur> {
    let maintenant = maintenant_ms();
    let mut v: Vec<Travailleur> = TABLEAU
        .iter()
        .filter_map(|p| {
            let id = p.id.load(Ordering::Acquire);
            if id == 0 {
                return None;
            }
            let depuis = p.au_travail_depuis.load(Ordering::Relaxed);
            // `try_lock` : le relevé ne doit pas attendre (le verrou n'est tenu
            // que le temps d'une affectation).
            let poll = p
                .poll
                .try_lock()
                .ok()
                .and_then(|g| g.as_ref().map(|(r, t)| (r.to_string(), t.elapsed())));
            Some(Travailleur {
                id,
                au_travail_depuis: (depuis != 0)
                    .then(|| Duration::from_millis(maintenant.saturating_sub(depuis))),
                poll,
            })
        })
        .collect();
    v.sort_by_key(|t| std::cmp::Reverse(t.au_travail_depuis));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un moteur à 2 fils dont l'un est pris par un sommeil synchrone : ses
    /// crochets le montrent au travail depuis plus de 300 ms, et l'autre fil
    /// finit garé.
    #[test]
    fn un_fil_de_travail_pris_se_voit_au_travail_depuis_longtemps() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .on_thread_park(au_garage)
            .on_thread_unpark(au_reveil)
            .on_thread_stop(a_l_arret)
            .enable_all()
            .build()
            .unwrap();
        // Laisser les deux fils se garer une première fois : c'est là qu'ils
        // prennent leur place.
        std::thread::sleep(Duration::from_millis(200));
        let (tx, rx) = std::sync::mpsc::channel();
        rt.spawn(Surveille::new(Arc::from("GET /banc/5677"), async move {
            let id = tune_core::db::verrou_ecriture::tid_courant();
            tx.send(id).unwrap();
            std::thread::sleep(Duration::from_millis(900));
        }));
        let id_pris = rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let vus = etat();
        #[cfg(target_os = "linux")]
        {
            let pris = vus
                .iter()
                .find(|t| t.id == id_pris)
                .unwrap_or_else(|| panic!("fil pris absent : {vus:?}"));
            assert!(
                pris.au_travail_depuis
                    .is_some_and(|d| d >= Duration::from_millis(300)),
                "{pris:?}"
            );
            // Le `poll` qui calcule est inscrit sous sa route.
            assert!(
                pris.poll
                    .as_ref()
                    .is_some_and(|(r, d)| r == "GET /banc/5677" && *d >= Duration::from_millis(300)),
                "{pris:?}"
            );
        }
        let _ = id_pris;
        assert!(
            vus.iter().any(|t| t
                .au_travail_depuis
                .is_none_or(|d| d < Duration::from_millis(300))),
            "l'autre fil doit se garer : {vus:?}"
        );
        rt.shutdown_timeout(Duration::from_secs(5));
    }
}

#[cfg(test)]
mod tests_du_poll {
    use super::*;

    /// Un `poll` qui rend la main en attendant n'est PAS inscrit pendant
    /// l'attente : seul le calcul synchrone l'est.
    #[test]
    fn un_poll_qui_attend_ne_reste_pas_inscrit() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .on_thread_park(au_garage)
            .on_thread_unpark(au_reveil)
            .on_thread_stop(a_l_arret)
            .enable_all()
            .build()
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let (tx, rx) = std::sync::mpsc::channel();
        rt.spawn(Surveille::new(Arc::from("GET /banc/attente"), async move {
            tx.send(tune_core::db::verrou_ecriture::tid_courant())
                .unwrap();
            tokio::time::sleep(Duration::from_millis(800)).await;
        }));
        let id = rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let vus = etat();
        assert!(
            !vus.iter().any(|t| t.id == id
                && t.poll
                    .as_ref()
                    .is_some_and(|(r, _)| r == "GET /banc/attente")),
            "un poll en attente ne doit pas rester inscrit : {vus:?}"
        );
        rt.shutdown_timeout(Duration::from_secs(5));
    }
}
