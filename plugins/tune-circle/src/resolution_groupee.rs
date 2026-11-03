//! #7022 : résoudre les morceaux d'une playlist de cercle PLUSIEURS À LA
//! FOIS, et se souvenir un moment de ce qu'on a trouvé.
//!
//! Avant : `resoudre_les_morceaux` cherchait un morceau, attendait, puis
//! passait au suivant (bibliothèque, puis Qobuz/TIDAL…). `POST
//! /playlists/{id}/resolve` répondait au bout de la SOMME des recherches, et
//! l'écran n'affichait les pochettes qu'à ce moment-là ; chaque réouverture
//! recommençait tout.
//!
//! # Concurrence bornée, ordre préservé
//!
//! Au plus [`PARALLELISME_MAX`] résolutions en vol (`buffered`, qui rend les
//! résultats DANS L'ORDRE des morceaux, quel que soit celui qui finit le
//! premier) : la réponse garde l'ordre et la forme de la playlist.
//!
//! Les services ne portent aucun limiteur de débit côté client : chaque
//! appel passe par le verrou en lecture du service (`RwLock::read`), qui
//! admet des lecteurs simultanés, et le registre n'est lu qu'une fois par
//! playlist. La borne à 6 est le seul garde-fou ajouté ; elle ne contourne
//! rien (un 429 reste une erreur du service, lue « introuvable » comme
//! avant).
//!
//! # Cache en mémoire
//!
//! Clé : la [`Reference`] telle que [`Reference::lire`] la lit, plus les
//! noms des services utilisables à cet instant (une connexion ou une
//! déconnexion change la clé, donc la réponse). Durée bornée
//! ([`DUREE_TROUVEE`], et [`DUREE_INTROUVABLE`], plus courte : un morceau
//! ajouté à la bibliothèque, ou un service qui refusait un instant, se
//! retrouve vite) ; taille bornée ([`ENTREES_MAX`]). Rien n'est écrit sur
//! disque.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use futures_util::stream::{self, StreamExt};

use crate::references::Reference;
use crate::resolution::Trouvee;

/// Résolutions en vol au plus, pour une playlist.
pub const PARALLELISME_MAX: usize = 6;
/// Durée de vie d'un morceau retrouvé.
pub const DUREE_TROUVEE: Duration = Duration::from_secs(10 * 60);
/// Durée de vie d'un « introuvable ».
pub const DUREE_INTROUVABLE: Duration = Duration::from_secs(60);
/// Entrées gardées au plus (une playlist pleine en fait 2 000).
pub const ENTREES_MAX: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Cle {
    reference: Reference,
    services: Vec<String>,
}

struct Entree {
    jusqu_a: Instant,
    insere: Instant,
    trouvee: Option<Trouvee>,
}

/// Ce que les dernières résolutions ont donné, pour un temps.
pub struct CacheDeResolution {
    entrees: Mutex<HashMap<Cle, Entree>>,
    duree_trouvee: Duration,
    duree_introuvable: Duration,
    capacite: usize,
}

impl Default for CacheDeResolution {
    fn default() -> Self {
        Self::avec(DUREE_TROUVEE, DUREE_INTROUVABLE, ENTREES_MAX)
    }
}

impl CacheDeResolution {
    pub fn avec(duree_trouvee: Duration, duree_introuvable: Duration, capacite: usize) -> Self {
        Self {
            entrees: Mutex::new(HashMap::new()),
            duree_trouvee,
            duree_introuvable,
            capacite: capacite.max(1),
        }
    }

    /// `Some(résultat)` si la clé est connue et encore valable.
    fn lire(&self, cle: &Cle) -> Option<Option<Trouvee>> {
        let mut entrees = self.entrees.lock().unwrap_or_else(|e| e.into_inner());
        let maintenant = Instant::now();
        match entrees.get(cle) {
            Some(e) if e.jusqu_a > maintenant => Some(e.trouvee.clone()),
            Some(_) => {
                entrees.remove(cle);
                None
            }
            None => None,
        }
    }

    fn garder(&self, cle: Cle, trouvee: Option<Trouvee>) {
        let duree = if trouvee.is_some() {
            self.duree_trouvee
        } else {
            self.duree_introuvable
        };
        if duree.is_zero() {
            return;
        }
        let maintenant = Instant::now();
        let mut entrees = self.entrees.lock().unwrap_or_else(|e| e.into_inner());
        if entrees.len() >= self.capacite && !entrees.contains_key(&cle) {
            entrees.retain(|_, e| e.jusqu_a > maintenant);
            while entrees.len() >= self.capacite {
                let Some(plus_ancienne) = entrees
                    .iter()
                    .min_by_key(|(_, e)| e.insere)
                    .map(|(c, _)| c.clone())
                else {
                    break;
                };
                entrees.remove(&plus_ancienne);
            }
        }
        entrees.insert(
            cle,
            Entree {
                jusqu_a: maintenant + duree,
                insere: maintenant,
                trouvee,
            },
        );
    }

    /// Entrées tenues (valables ou non).
    pub fn len(&self) -> usize {
        self.entrees.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Résout `references` par `resoudre`, [`PARALLELISME_MAX`] à la fois, en
/// passant par `cache`. Le résultat est DANS L'ORDRE de `references`.
///
/// `services` : les noms des services utilisables, qui entrent dans la clé.
pub async fn resoudre_en_ordre<F, Fut>(
    references: Vec<Reference>,
    services: &[String],
    cache: &CacheDeResolution,
    resoudre: F,
) -> Vec<Option<Trouvee>>
where
    F: Fn(Reference) -> Fut,
    Fut: Future<Output = Option<Trouvee>>,
{
    let resoudre = &resoudre;
    stream::iter(references.into_iter().map(|reference| {
        let cle = Cle {
            reference,
            services: services.to_vec(),
        };
        async move {
            if let Some(connue) = cache.lire(&cle) {
                return connue;
            }
            let trouvee = resoudre(cle.reference.clone()).await;
            cache.garder(cle, trouvee.clone());
            trouvee
        }
    }))
    .buffered(PARALLELISME_MAX)
    .collect()
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolution::Methode;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tune_core::streaming::traits::StreamTrack;

    fn reference(n: usize) -> Reference {
        Reference {
            title: format!("Titre {n}"),
            artist_name: "Artiste".into(),
            duration_ms: 1000 + n as u64,
            ..Default::default()
        }
    }

    fn trouvee(r: &Reference) -> Option<Trouvee> {
        Some(Trouvee::Service {
            service: "qobuz".into(),
            piste: Box::new(
                serde_json::from_value::<StreamTrack>(serde_json::json!({
                    "id": r.title, "title": r.title, "artist": "",
                    "duration_ms": 0, "explicit": false,
                }))
                .expect("StreamTrack de test"),
            ),
            methode: Methode::Texte,
        })
    }

    fn titre(t: &Option<Trouvee>) -> String {
        match t {
            Some(Trouvee::Service { piste, .. }) => piste.title.clone(),
            _ => "?".into(),
        }
    }

    fn services() -> Vec<String> {
        vec!["qobuz".into()]
    }

    /// (a) Les premiers morceaux sont les plus lents : l'ordre rendu reste
    /// celui de la playlist.
    #[tokio::test]
    async fn l_ordre_de_la_playlist_est_garde_quelle_que_soit_la_duree() {
        let refs: Vec<_> = (0..12).map(reference).collect();
        let cache = CacheDeResolution::default();
        let rendus = resoudre_en_ordre(refs.clone(), &services(), &cache, |r| async move {
            let n: u64 = r.duration_ms - 1000;
            tokio::time::sleep(Duration::from_millis((12 - n) * 15)).await;
            trouvee(&r)
        })
        .await;
        let attendus: Vec<String> = refs.iter().map(|r| r.title.clone()).collect();
        let obtenus: Vec<String> = rendus.iter().map(titre).collect();
        assert_eq!(obtenus, attendus, "l'ordre de la playlist n'est pas gardé");
    }

    /// (b) 12 morceaux à 200 ms chacun : 6 à la fois, moins d'une seconde
    /// (2,4 s un par un).
    #[tokio::test]
    async fn les_morceaux_sont_resolus_plusieurs_a_la_fois() {
        let refs: Vec<_> = (0..12).map(reference).collect();
        let cache = CacheDeResolution::default();
        let en_vol = AtomicUsize::new(0);
        let pic = AtomicUsize::new(0);
        let debut = Instant::now();
        let rendus = resoudre_en_ordre(refs, &services(), &cache, |r| {
            let (en_vol, pic) = (&en_vol, &pic);
            async move {
                let n = en_vol.fetch_add(1, Ordering::SeqCst) + 1;
                pic.fetch_max(n, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(200)).await;
                en_vol.fetch_sub(1, Ordering::SeqCst);
                trouvee(&r)
            }
        })
        .await;
        let duree = debut.elapsed();
        assert_eq!(rendus.len(), 12);
        assert!(
            duree < Duration::from_secs(1),
            "résolution séquentielle : 12 morceaux à 200 ms ont pris {duree:?} \
             (attendu < 1 s avec {PARALLELISME_MAX} à la fois)"
        );
        let pic = pic.load(Ordering::SeqCst);
        assert!(
            pic <= PARALLELISME_MAX,
            "{pic} résolutions en vol : la borne de {PARALLELISME_MAX} n'est pas tenue"
        );
    }

    /// (c) La réouverture ne rappelle pas le résolveur.
    #[tokio::test]
    async fn la_reouverture_ne_rappelle_pas_le_resolveur() {
        let refs: Vec<_> = (0..12).map(reference).collect();
        let cache = CacheDeResolution::default();
        let appels = AtomicUsize::new(0);
        let resoudre = |r: Reference| {
            let appels = &appels;
            async move {
                appels.fetch_add(1, Ordering::SeqCst);
                trouvee(&r)
            }
        };
        let premier = resoudre_en_ordre(refs.clone(), &services(), &cache, resoudre).await;
        assert_eq!(appels.load(Ordering::SeqCst), 12);
        let second = resoudre_en_ordre(refs, &services(), &cache, resoudre).await;
        assert_eq!(
            appels.load(Ordering::SeqCst),
            12,
            "pas de cache : la réouverture a rappelé le résolveur"
        );
        assert_eq!(
            premier.iter().map(titre).collect::<Vec<_>>(),
            second.iter().map(titre).collect::<Vec<_>>()
        );
    }

    /// Une connexion ou une déconnexion de service change la clé.
    #[tokio::test]
    async fn d_autres_services_ne_lisent_pas_le_meme_cache() {
        let cache = CacheDeResolution::default();
        let appels = AtomicUsize::new(0);
        let resoudre = |r: Reference| {
            let appels = &appels;
            async move {
                appels.fetch_add(1, Ordering::SeqCst);
                trouvee(&r)
            }
        };
        resoudre_en_ordre(vec![reference(1)], &services(), &cache, resoudre).await;
        resoudre_en_ordre(vec![reference(1)], &["tidal".into()], &cache, resoudre).await;
        assert_eq!(appels.load(Ordering::SeqCst), 2);
    }

    /// Un « introuvable » expire selon sa propre durée, un trouvé selon la
    /// sienne.
    #[tokio::test]
    async fn un_introuvable_est_garde_moins_longtemps() {
        let cache = CacheDeResolution::avec(Duration::from_secs(600), Duration::ZERO, 16);
        let appels = AtomicUsize::new(0);
        let resoudre = |r: Reference| {
            let appels = &appels;
            async move {
                appels.fetch_add(1, Ordering::SeqCst);
                r.duration_ms
                    .is_multiple_of(2)
                    .then(|| trouvee(&r))
                    .flatten()
            }
        };
        // 1000 : trouvé ; 1001 : introuvable.
        let refs = vec![reference(0), reference(1)];
        resoudre_en_ordre(refs.clone(), &services(), &cache, resoudre).await;
        resoudre_en_ordre(refs, &services(), &cache, resoudre).await;
        assert_eq!(
            appels.load(Ordering::SeqCst),
            3,
            "seul l'introuvable doit être recherché de nouveau"
        );
    }

    #[test]
    fn la_taille_est_bornee() {
        let cache = CacheDeResolution::avec(Duration::from_secs(600), Duration::from_secs(60), 3);
        for n in 0..10 {
            let r = reference(n);
            let t = trouvee(&r);
            cache.garder(
                Cle {
                    reference: r,
                    services: services(),
                },
                t,
            );
        }
        assert_eq!(cache.len(), 3);
        let derniere = Cle {
            reference: reference(9),
            services: services(),
        };
        assert!(
            cache.lire(&derniere).is_some(),
            "la plus récente doit rester"
        );
    }
}
