//! #8701 — les fMP4 DASH assemblés (`tune-dash-<uuid>.mp4`) des pistes qu'une
//! zone joue ou s'apprête à jouer restent servis par le cache d'URL de TIDAL
//! au-delà de son TTL de 240 s, et le ramasse-miettes ne les retire pas.
//!
//! Sans cela, un saut dans une piste qui joue depuis plus de 4 min ne
//! retrouvait plus le fichier : le cache l'avait oublié, et la piste entière
//! était retéléchargée (89 segments) avant la moindre note à la nouvelle
//! position, alors que le fichier complet était toujours sur le disque.
//!
//! Chaque zone tient au plus deux épingles, chacune à sa place :
//! - la piste EN COURS ([`noter_la_piste_resolue`] hors préparation) : une
//!   lecture, un saut, une reprise ;
//! - la piste PRÉPARÉE par le gapless ([`pendant_la_preparation`]), promue
//!   piste en cours à l'enchaînement ([`promouvoir_la_preparee`]).
//!
//! Préparer la suivante ne déloge donc jamais la piste qui joue. Une
//! résolution qui ne rend pas de fMP4 DASH (Qobuz, bibliothèque, radio…) vide
//! sa place ; l'arrêt de la zone, sa suppression ou une file vidée libèrent
//! les deux ([`liberer_la_zone`]). Une épingle oubliée ne vaut plus au-delà de
//! [`DUREE_MAX`].

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Au-delà, une épingle ne retient plus le fichier : bien plus longue que
/// n'importe quelle piste, elle borne ce qu'une zone oubliée laisse sur le
/// disque.
pub const DUREE_MAX: Duration = Duration::from_secs(4 * 3600);

struct Epingle {
    chemin: String,
    depuis: Instant,
}

impl Epingle {
    fn de(chemin: Option<&str>) -> Option<Self> {
        chemin.map(|c| Self {
            chemin: c.to_string(),
            depuis: Instant::now(),
        })
    }

    fn vaut(&self, chemin: &str) -> bool {
        self.chemin == chemin && self.depuis.elapsed() < DUREE_MAX
    }
}

#[derive(Default)]
struct Places {
    courante: Option<Epingle>,
    preparee: Option<Epingle>,
}

#[derive(Default)]
struct Registre {
    places: HashMap<i64, Places>,
    /// Avancée à chaque [`liberer_la_zone`] : une préparation partie avant
    /// l'arrêt ne réépingle pas la zone arrêtée.
    generations: HashMap<i64, u64>,
}

impl Registre {
    fn generation(&self, zone_id: i64) -> u64 {
        self.generations.get(&zone_id).copied().unwrap_or(0)
    }
}

fn registre() -> &'static Mutex<Registre> {
    static R: OnceLock<Mutex<Registre>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Registre::default()))
}

fn avec_le_registre<T>(f: impl FnOnce(&mut Registre) -> T) -> T {
    f(&mut registre().lock().unwrap_or_else(|e| e.into_inner()))
}

/// La préparation en cours : sa zone et la génération de la zone à son départ.
#[derive(Clone, Copy)]
struct Preparation {
    zone_id: i64,
    generation: u64,
}

tokio::task_local! {
    /// Posé autour de la résolution de la piste SUIVANTE (gapless).
    static EN_PREPARATION: Preparation;
}

/// Exécute `f` (la résolution de la piste suivante de `zone_id`, préparée par
/// le gapless) : le fichier qu'elle rend prend la place de la piste préparée,
/// jamais celle de la piste en cours. Si la zone est libérée (arrêtée) pendant
/// la résolution, le fichier n'est pas épinglé.
pub async fn pendant_la_preparation<F: std::future::Future>(zone_id: i64, f: F) -> F::Output {
    let generation = avec_le_registre(|r| r.generation(zone_id));
    EN_PREPARATION
        .scope(
            Preparation {
                zone_id,
                generation,
            },
            f,
        )
        .await
}

fn en_preparation() -> Option<Preparation> {
    EN_PREPARATION.try_with(|p| *p).ok()
}

/// Le chemin du fichier DASH désigné par `url_ou_chemin` (`file://…`, avec ou
/// sans le suffixe `.decoding` de l'orchestrateur), s'il porte le nom que lui
/// donne TIDAL. `None` pour tout autre fichier ou URL.
fn chemin_dash(url_ou_chemin: &str) -> Option<&str> {
    let chemin = url_ou_chemin
        .strip_prefix("file://")
        .unwrap_or(url_ou_chemin);
    let chemin = chemin.strip_suffix(".decoding").unwrap_or(chemin);
    let nom = std::path::Path::new(chemin).file_name()?.to_str()?;
    (nom.starts_with("tune-dash-") && nom.ends_with(".mp4")).then_some(chemin)
}

/// Une piste vient d'être résolue pour la zone : `url` est ce que la source a
/// rendu (`None`, ou toute URL qui n'est pas un fMP4 DASH de TIDAL : la place
/// est vidée). Hors préparation, c'est la piste en cours ; pendant
/// [`pendant_la_preparation`], la suivante.
pub fn noter_la_piste_resolue(zone_id: i64, url: Option<&str>) {
    let chemin = url.and_then(chemin_dash);
    let en_cours = en_preparation();
    let preparation = en_cours.is_some();
    let perimee = avec_le_registre(|r| {
        // La zone a été arrêtée pendant la préparation : rien à retenir.
        if en_cours.is_some_and(|p| p.zone_id == zone_id && p.generation != r.generation(zone_id)) {
            return true;
        }
        let places = r.places.entry(zone_id).or_default();
        if preparation {
            places.preparee = if places.courante.as_ref().map(|e| e.chemin.as_str()) == chemin {
                None
            } else {
                Epingle::de(chemin)
            };
        } else {
            if places.preparee.as_ref().map(|e| e.chemin.as_str()) == chemin {
                places.preparee = None;
            }
            places.courante = Epingle::de(chemin);
        }
        if places.courante.is_none() && places.preparee.is_none() {
            r.places.remove(&zone_id);
        }
        false
    });
    tracing::debug!(
        zone_id,
        preparation,
        perimee,
        dash = chemin.is_some(),
        "dash_epingle_notee_8701"
    );
}

/// L'enchaînement sans blanc : la piste préparée devient la piste en cours,
/// l'ancienne est libérée.
pub fn promouvoir_la_preparee(zone_id: i64) {
    avec_le_registre(|r| {
        if let Some(places) = r.places.get_mut(&zone_id) {
            places.courante = places.preparee.take();
            if places.courante.is_none() {
                r.places.remove(&zone_id);
            }
        }
    });
}

/// La zone s'arrête, est supprimée, ou sa file est vidée : ses fichiers
/// redeviennent nettoyables (ramasse-miettes, TTL du cache d'URL).
pub fn liberer_la_zone(zone_id: i64) {
    avec_le_registre(|r| {
        r.places.remove(&zone_id);
        *r.generations.entry(zone_id).or_default() += 1;
    });
}

/// Une zone joue-t-elle (ou prépare-t-elle) ce fichier DASH ?
pub fn est_epingle(url_ou_chemin: &str) -> bool {
    let Some(chemin) = chemin_dash(url_ou_chemin) else {
        return false;
    };
    avec_le_registre(|r| {
        r.places.values().any(|p| {
            p.courante.as_ref().is_some_and(|e| e.vaut(chemin))
                || p.preparee.as_ref().is_some_and(|e| e.vaut(chemin))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un nom seulement : le registre ne lit que le chemin, aucun fichier
    /// n'est créé.
    fn chemin(nom: &str) -> String {
        format!("/nulle-part/tune-dash-{nom}.mp4")
    }

    fn courante(zone: i64, c: &str) {
        noter_la_piste_resolue(zone, Some(&format!("file://{c}")));
    }

    async fn preparee(zone: i64, c: &str) {
        pendant_la_preparation(zone, async { noter_la_piste_resolue(zone, Some(c)) }).await;
    }

    #[tokio::test]
    async fn preparer_la_suivante_ne_deloge_jamais_la_piste_en_cours_8701() {
        let zone = 870_101;
        let (a, b, c) = (chemin("ep-a"), chemin("ep-b"), chemin("ep-c"));
        courante(zone, &a);
        assert!(est_epingle(&a));
        assert!(est_epingle(&format!("{a}.decoding")));
        preparee(zone, &b).await;
        assert!(est_epingle(&a) && est_epingle(&b), "gapless : A et B");
        // La préparée change (file réordonnée) : A, qui joue, reste.
        preparee(zone, &c).await;
        assert!(est_epingle(&a), "la piste en cours a été délogée");
        assert!(est_epingle(&c) && !est_epingle(&b));
        // Enchaînement : C joue, A est libérée.
        promouvoir_la_preparee(zone);
        assert!(!est_epingle(&a) && est_epingle(&c));
        // Un saut dans C la garde.
        courante(zone, &c);
        assert!(est_epingle(&c));
        liberer_la_zone(zone);
        assert!(!est_epingle(&c));
    }

    #[tokio::test]
    async fn une_autre_source_libere_sa_place_8701() {
        let zone = 870_102;
        let (a, b) = (chemin("ep-src-a"), chemin("ep-src-b"));
        courante(zone, &a);
        preparee(zone, &b).await;
        // La suivante préparée est un FLAC Qobuz : B redevient nettoyable.
        pendant_la_preparation(zone, async {
            noter_la_piste_resolue(zone, Some("https://cdn.example/piste.flac"))
        })
        .await;
        assert!(est_epingle(&a) && !est_epingle(&b));
        // L'utilisateur lance une radio (ou une piste de la bibliothèque).
        noter_la_piste_resolue(zone, Some("http://radio.example/flux"));
        assert!(!est_epingle(&a), "la piste TIDAL quittée reste épinglée");
        noter_la_piste_resolue(zone, Some(&a));
        noter_la_piste_resolue(zone, None);
        assert!(!est_epingle(&a));
    }

    /// Une préparation en vol quand la zone s'arrête : le fichier qu'elle
    /// rend après l'arrêt n'est pas épinglé.
    #[tokio::test]
    async fn une_preparation_en_vol_ne_reepingle_pas_une_zone_arretee_8701() {
        let zone = 870_106;
        let (a, b) = (chemin("ep-vol-a"), chemin("ep-vol-b"));
        courante(zone, &a);
        pendant_la_preparation(zone, async {
            liberer_la_zone(zone);
            noter_la_piste_resolue(zone, Some(&b));
        })
        .await;
        assert!(!est_epingle(&a) && !est_epingle(&b));
        // La zone relancée épingle de nouveau, préparation comprise.
        courante(zone, &a);
        preparee(zone, &b).await;
        assert!(est_epingle(&a) && est_epingle(&b));
        liberer_la_zone(zone);
    }

    #[test]
    fn seuls_les_fichiers_dash_de_tidal_s_epinglent_8701() {
        let zone = 870_103;
        noter_la_piste_resolue(zone, Some("file:///musique/album/piste.flac"));
        assert!(!est_epingle("file:///musique/album/piste.flac"));
        assert!(!est_epingle("https://cdn.example/piste.flac"));
        liberer_la_zone(zone);
    }

    #[test]
    fn les_zones_ne_se_liberent_pas_entre_elles_8701() {
        let (z1, z2) = (870_104, 870_105);
        let a = chemin("ep-zones-a");
        courante(z1, &a);
        for n in 0..5 {
            courante(z2, &chemin(&format!("ep-zones-{n}")));
        }
        liberer_la_zone(z2);
        assert!(
            est_epingle(&a),
            "une autre zone ne libère pas le fichier de z1"
        );
        liberer_la_zone(z1);
        assert!(!est_epingle(&a));
    }
}
