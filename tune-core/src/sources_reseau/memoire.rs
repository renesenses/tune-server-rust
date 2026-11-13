//! Un faux système distant, tenu en mémoire, pour les épreuves (#8189).
//!
//! Il compte ce qu'on lui demande (octets lus, dossiers listés) et sait
//! simuler une coupure — tout de suite, ou après N listes — pour éprouver la
//! lecture par plages et la reprise du parcours sans réseau ni NAS.
//!
//! Il n'est enregistré par aucun chemin de production : seule une épreuve
//! (ici ou dans `tune-server`) le pose dans le registre.

use std::collections::BTreeMap;
use std::io;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

use super::systeme::{Entree, SystemeDistant};

/// chemin interne → (octets, mtime)
type Fichiers = BTreeMap<String, (std::sync::Arc<Vec<u8>>, i64)>;

#[derive(Default)]
pub struct SystemeMemoire {
    /// chemin interne → (octets, mtime)
    fichiers: RwLock<Fichiers>,
    octets_lus: AtomicU64,
    lectures: AtomicU64,
    listes: AtomicU64,
    coupe: AtomicBool,
    /// Coupure après ce nombre de listes réussies ; -1 = jamais.
    coupure_apres: AtomicI64,
    reseau_changes: AtomicU64,
}

fn coupure() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "source coupée (épreuve)")
}

impl SystemeMemoire {
    pub fn nouveau() -> Self {
        let s = Self::default();
        s.coupure_apres.store(-1, Ordering::SeqCst);
        s
    }

    /// Ajoute (ou remplace) un fichier.
    pub fn ajouter(&self, chemin: &str, octets: Vec<u8>, mtime: i64) {
        self.fichiers.write().unwrap().insert(
            chemin.trim_matches('/').to_string(),
            (std::sync::Arc::new(octets), mtime),
        );
    }

    pub fn retirer(&self, chemin: &str) {
        self.fichiers
            .write()
            .unwrap()
            .remove(chemin.trim_matches('/'));
    }

    /// Coupe (ou rétablit) la source : toute demande échoue en `NotConnected`.
    pub fn couper(&self, coupe: bool) {
        self.coupe.store(coupe, Ordering::SeqCst);
    }

    /// Coupe la source après `n` listes réussies à partir de maintenant.
    pub fn couper_apres_listes(&self, n: u64) {
        let deja = self.listes.load(Ordering::SeqCst);
        self.coupure_apres
            .store((deja + n) as i64, Ordering::SeqCst);
    }

    /// Rétablit la source, et oublie la coupure programmée.
    pub fn retablir(&self) {
        self.coupure_apres.store(-1, Ordering::SeqCst);
        self.couper(false);
    }

    pub fn octets_lus(&self) -> u64 {
        self.octets_lus.load(Ordering::SeqCst)
    }
    pub fn lectures(&self) -> u64 {
        self.lectures.load(Ordering::SeqCst)
    }
    pub fn listes(&self) -> u64 {
        self.listes.load(Ordering::SeqCst)
    }
    pub fn reseau_changes(&self) -> u64 {
        self.reseau_changes.load(Ordering::SeqCst)
    }

    fn verifier(&self) -> io::Result<()> {
        if self.coupe.load(Ordering::SeqCst) {
            return Err(coupure());
        }
        Ok(())
    }

    fn est_dossier(fichiers: &Fichiers, d: &str) -> bool {
        d.is_empty() || fichiers.keys().any(|k| k.starts_with(&format!("{d}/")))
    }
}

impl SystemeDistant for SystemeMemoire {
    fn type_de_source(&self) -> &'static str {
        "memoire"
    }

    fn lister(&self, dossier: &str) -> io::Result<Vec<Entree>> {
        self.verifier()?;
        let limite = self.coupure_apres.load(Ordering::SeqCst);
        if limite >= 0 && self.listes.load(Ordering::SeqCst) as i64 >= limite {
            self.couper(true);
            return Err(coupure());
        }
        let dossier = dossier.trim_matches('/');
        let fichiers = self.fichiers.read().unwrap();
        if !Self::est_dossier(&fichiers, dossier) {
            return Err(io::Error::new(io::ErrorKind::NotFound, "dossier absent"));
        }
        let prefixe = if dossier.is_empty() {
            String::new()
        } else {
            format!("{dossier}/")
        };
        let mut vus: BTreeMap<String, Entree> = BTreeMap::new();
        for (k, (octets, mtime)) in fichiers.iter() {
            let Some(reste) = k.strip_prefix(&prefixe) else {
                continue;
            };
            match reste.split_once('/') {
                Some((sous, _)) => {
                    vus.entry(sous.to_string()).or_insert(Entree {
                        nom: sous.to_string(),
                        dossier: true,
                        taille: 0,
                        mtime: None,
                        etag: None,
                    });
                }
                None => {
                    vus.insert(
                        reste.to_string(),
                        Entree {
                            nom: reste.to_string(),
                            dossier: false,
                            taille: octets.len() as u64,
                            mtime: Some(*mtime),
                            etag: Some(format!("{}-{}", octets.len(), mtime)),
                        },
                    );
                }
            }
        }
        self.listes.fetch_add(1, Ordering::SeqCst);
        Ok(vus.into_values().collect())
    }

    fn stat(&self, chemin: &str) -> io::Result<Entree> {
        self.verifier()?;
        let chemin = chemin.trim_matches('/');
        let fichiers = self.fichiers.read().unwrap();
        let nom = chemin.rsplit('/').next().unwrap_or("").to_string();
        if let Some((octets, mtime)) = fichiers.get(chemin) {
            return Ok(Entree {
                nom,
                dossier: false,
                taille: octets.len() as u64,
                mtime: Some(*mtime),
                etag: Some(format!("{}-{}", octets.len(), mtime)),
            });
        }
        if Self::est_dossier(&fichiers, chemin) {
            return Ok(Entree {
                nom,
                dossier: true,
                taille: 0,
                mtime: None,
                etag: None,
            });
        }
        Err(io::Error::new(io::ErrorKind::NotFound, "fichier absent"))
    }

    fn lire_a(&self, chemin: &str, position: u64, tampon: &mut [u8]) -> io::Result<usize> {
        self.verifier()?;
        let fichiers = self.fichiers.read().unwrap();
        let (octets, _) = fichiers
            .get(chemin.trim_matches('/'))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "fichier absent"))?;
        let debut = (position as usize).min(octets.len());
        let n = tampon.len().min(octets.len() - debut);
        tampon[..n].copy_from_slice(&octets[debut..debut + n]);
        self.octets_lus.fetch_add(n as u64, Ordering::SeqCst);
        self.lectures.fetch_add(1, Ordering::SeqCst);
        Ok(n)
    }

    fn reseau_change(&self) {
        self.reseau_changes.fetch_add(1, Ordering::SeqCst);
    }
}
