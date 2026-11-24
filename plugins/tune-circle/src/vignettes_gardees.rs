//! #9337 — les vignettes de contact déjà vues, gardées en mémoire.
//!
//! Une grille de 24 albums d'un contact faisait 24 allers-retours par le
//! cloud (270 à 710 ms chacun sur le .18, jusqu'à 1,9 s), à chaque ouverture
//! depuis un autre navigateur, un autre appareil, ou par le pont, dont le
//! jeton change l'adresse et vide donc le cache du navigateur (web#2985).
//!
//! Seule une adresse VERSIONNÉE est gardée : `?v=` est le début du sha256
//! que le cloud rend en ETag ([`crate::routes`], `cache_de_vignette`). Elle
//! désigne ces octets-là et aucun autre ; une vignette changée change
//! d'adresse. La règle du greffon tient comme pour le cache du navigateur :
//! ces adresses ne s'obtiennent qu'en relisant la liste des albums du
//! contact, qui passe toujours par le cloud (404 au partage coupé). La copie
//! ne sort jamais sans session ([`crate::relais::Relais::session_ouverte`]).
//!
//! Bornes : [`OCTETS_MAX`] au total (≈ 1 500 vignettes de 10 Ko), chaque
//! vignette au plus [`OCTETS_PAR_VIGNETTE_MAX`], et [`DUREE`] de vie (celle
//! du cache du navigateur). Au-delà, la plus anciennement servie sort.
//! Rien sur disque.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::HeaderValue;

/// Le total des octets gardés.
pub const OCTETS_MAX: usize = 16 * 1024 * 1024;
/// Une vignette plus grosse n'est pas gardée (une vignette fait 5 à 12 Ko).
pub const OCTETS_PAR_VIGNETTE_MAX: usize = 256 * 1024;
/// La durée de vie d'une copie : celle de `max-age` côté navigateur.
pub const DUREE: Duration = Duration::from_secs(7 * 24 * 3600);

/// Une vignette gardée : ses octets, son type et son ETag, tels que rendus.
#[derive(Clone)]
pub struct Vignette {
    pub corps: Vec<u8>,
    pub type_mime: HeaderValue,
    pub etag: HeaderValue,
}

struct Entree {
    vignette: Vignette,
    gardee: Instant,
    servie: u64,
}

/// (contact, album, version).
type Cle = (String, String, String);

#[derive(Default)]
struct Etat {
    entrees: HashMap<Cle, Entree>,
    octets: usize,
    horloge: u64,
}

#[derive(Default)]
pub struct VignettesGardees {
    etat: Mutex<Etat>,
}

impl VignettesGardees {
    pub fn new() -> Self {
        Self::default()
    }

    fn cle(contact: &str, album: &str, version: &str) -> Cle {
        (
            contact.to_owned(),
            album.to_owned(),
            version.to_ascii_lowercase(),
        )
    }

    /// La copie de cette adresse versionnée, si elle est gardée et fraîche.
    pub fn lire(&self, contact: &str, album: &str, version: &str) -> Option<Vignette> {
        self.lire_a(contact, album, version, Instant::now())
    }

    fn lire_a(
        &self,
        contact: &str,
        album: &str,
        version: &str,
        maintenant: Instant,
    ) -> Option<Vignette> {
        let mut e = self.etat.lock().unwrap_or_else(|p| p.into_inner());
        let cle = Self::cle(contact, album, version);
        let perimee = e
            .entrees
            .get(&cle)
            .is_some_and(|x| maintenant.duration_since(x.gardee) >= DUREE);
        if perimee {
            if let Some(x) = e.entrees.remove(&cle) {
                e.octets -= x.vignette.corps.len();
            }
            return None;
        }
        e.horloge += 1;
        let horloge = e.horloge;
        let x = e.entrees.get_mut(&cle)?;
        x.servie = horloge;
        Some(x.vignette.clone())
    }

    /// Garde la vignette de cette adresse versionnée. L'appelant ne la donne
    /// que si `version` désigne bien ces octets (début de l'ETag).
    pub fn garder(&self, contact: &str, album: &str, version: &str, vignette: Vignette) {
        self.garder_a(contact, album, version, vignette, Instant::now());
    }

    fn garder_a(
        &self,
        contact: &str,
        album: &str,
        version: &str,
        vignette: Vignette,
        maintenant: Instant,
    ) {
        let taille = vignette.corps.len();
        if taille == 0 || taille > OCTETS_PAR_VIGNETTE_MAX {
            return;
        }
        let mut e = self.etat.lock().unwrap_or_else(|p| p.into_inner());
        let cle = Self::cle(contact, album, version);
        if let Some(x) = e.entrees.remove(&cle) {
            e.octets -= x.vignette.corps.len();
        }
        while e.octets + taille > OCTETS_MAX {
            let Some(la_plus_ancienne) = e
                .entrees
                .iter()
                .min_by_key(|(_, x)| x.servie)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(x) = e.entrees.remove(&la_plus_ancienne) {
                e.octets -= x.vignette.corps.len();
            }
        }
        e.horloge += 1;
        let servie = e.horloge;
        e.octets += taille;
        e.entrees.insert(
            cle,
            Entree {
                vignette,
                gardee: maintenant,
                servie,
            },
        );
    }

    #[cfg(test)]
    fn octets(&self) -> usize {
        self.etat.lock().unwrap().octets
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vignette(n: usize) -> Vignette {
        Vignette {
            corps: vec![7; n],
            type_mime: HeaderValue::from_static("image/jpeg"),
            etag: HeaderValue::from_static("\"abc\""),
        }
    }

    #[test]
    fn une_copie_se_relit_par_contact_album_et_version() {
        let g = VignettesGardees::new();
        g.garder("7", "10", "7DD7622944E688DE", vignette(10));
        assert_eq!(
            g.lire("7", "10", "7dd7622944e688de").unwrap().corps.len(),
            10
        );
        assert!(g.lire("7", "10", "0000000000000000").is_none());
        assert!(g.lire("8", "10", "7dd7622944e688de").is_none());
        assert!(g.lire("7", "11", "7dd7622944e688de").is_none());
    }

    #[test]
    fn une_copie_perimee_ne_sort_plus() {
        let g = VignettesGardees::new();
        let t0 = Instant::now();
        g.garder_a("7", "10", "v", vignette(10), t0);
        assert!(
            g.lire_a("7", "10", "v", t0 + DUREE - Duration::from_secs(1))
                .is_some()
        );
        assert!(g.lire_a("7", "10", "v", t0 + DUREE).is_none());
        assert_eq!(g.octets(), 0);
    }

    #[test]
    fn le_total_reste_borne_et_la_moins_recemment_servie_sort() {
        let g = VignettesGardees::new();
        let n = OCTETS_MAX / OCTETS_PAR_VIGNETTE_MAX;
        for i in 0..n {
            g.garder("7", &i.to_string(), "v", vignette(OCTETS_PAR_VIGNETTE_MAX));
        }
        assert_eq!(g.octets(), OCTETS_MAX);
        // La première est relue : c'est la deuxième qui sortira.
        assert!(g.lire("7", "0", "v").is_some());
        g.garder("7", "neuve", "v", vignette(OCTETS_PAR_VIGNETTE_MAX));
        assert!(g.octets() <= OCTETS_MAX);
        assert!(g.lire("7", "0", "v").is_some());
        assert!(g.lire("7", "1", "v").is_none());
        assert!(g.lire("7", "neuve", "v").is_some());
    }

    #[test]
    fn trop_grosse_ou_vide_n_est_pas_gardee() {
        let g = VignettesGardees::new();
        g.garder("7", "10", "v", vignette(OCTETS_PAR_VIGNETTE_MAX + 1));
        g.garder("7", "11", "v", vignette(0));
        assert_eq!(g.octets(), 0);
    }
}
