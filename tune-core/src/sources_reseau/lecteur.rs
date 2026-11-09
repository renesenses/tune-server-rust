//! Lecture d'un fichier distant par PLAGES, avec un petit tampon (#8189).
//!
//! Rien n'est copié sur l'appareil : le lecteur demande au protocole des blocs
//! de [`BLOC`] octets alignés, et n'en garde que les [`BLOCS_GARDES`] derniers
//! en mémoire (2 Mio). C'est assez pour les deux motifs de lecture qui comptent :
//!
//! - les étiquettes, lues au DÉBUT et à la FIN du fichier (ID3v2 et FLAC au
//!   début, ID3v1/APE et l'atome `moov` d'un MP4 souvent à la fin) : deux ou
//!   trois blocs, jamais le fichier ;
//! - la lecture d'un morceau, séquentielle : chaque bloc est demandé une fois,
//!   et un retour en arrière court (symphonia relit souvent quelques octets)
//!   retombe dans le tampon.
//!
//! Le scan ouvre un même fichier plusieurs fois (étiquettes, étiquettes
//! étendues, pochette intégrée, empreinte) : chaque ouverture relisait le même
//! premier bloc sur le réseau. Un cache PARTAGÉ, borné à [`BLOCS_PARTAGES`]
//! blocs (16 Mio), garde donc les derniers blocs lus, par source, fichier,
//! taille et date : un fichier modifié change de clé.

use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;

use super::systeme::SystemeDistant;

/// La taille d'un bloc demandé au protocole.
pub const BLOC: u64 = 256 * 1024;
/// Le nombre de blocs gardés en mémoire par lecteur.
pub const BLOCS_GARDES: usize = 8;
/// Le nombre de blocs du cache partagé entre lecteurs.
pub const BLOCS_PARTAGES: usize = 64;

/// (source, chemin, taille, date, index du bloc)
type CleDeBloc = (String, String, u64, Option<i64>, u64);
/// Le cache partagé : (clé, octets), le plus récent à la fin.
type CachePartage = std::sync::Mutex<VecDeque<(CleDeBloc, Arc<Vec<u8>>)>>;

fn cache_partage() -> &'static CachePartage {
    static C: std::sync::OnceLock<CachePartage> = std::sync::OnceLock::new();
    C.get_or_init(|| std::sync::Mutex::new(VecDeque::with_capacity(BLOCS_PARTAGES)))
}

fn lire_partage(cle: &CleDeBloc) -> Option<Arc<Vec<u8>>> {
    let mut c = cache_partage().lock().ok()?;
    let i = c.iter().position(|(k, _)| k == cle)?;
    let e = c.remove(i)?;
    let octets = e.1.clone();
    c.push_back(e);
    Some(octets)
}

fn garder_partage(cle: CleDeBloc, octets: Arc<Vec<u8>>) {
    if let Ok(mut c) = cache_partage().lock() {
        if c.len() >= BLOCS_PARTAGES {
            c.pop_front();
        }
        c.push_back((cle, octets));
    }
}

/// Oublie les blocs partagés d'une source (retirée, réseau changé).
pub fn oublier_les_blocs(source: &str) {
    if let Ok(mut c) = cache_partage().lock() {
        c.retain(|(k, _)| k.0 != source);
    }
}

/// Un fichier distant ouvert : `Read + Seek`, comme un `File`.
pub struct LecteurDistant {
    systeme: Arc<dyn SystemeDistant>,
    chemin: String,
    taille: u64,
    mtime: Option<i64>,
    /// La source, pour le cache partagé ; `None` : pas de partage.
    source: Option<String>,
    position: u64,
    /// (index du bloc, octets), le plus récent à la fin.
    blocs: VecDeque<(u64, Arc<Vec<u8>>)>,
}

impl std::fmt::Debug for LecteurDistant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LecteurDistant")
            .field("source", &self.systeme.type_de_source())
            .field("chemin", &self.chemin)
            .field("taille", &self.taille)
            .field("position", &self.position)
            .finish()
    }
}

impl LecteurDistant {
    /// Ouvre `chemin` : un `stat` pour la taille, aucune lecture.
    pub fn ouvrir(systeme: Arc<dyn SystemeDistant>, chemin: &str) -> io::Result<Self> {
        Self::ouvrir_partage(systeme, None, chemin)
    }

    /// Comme [`Self::ouvrir`], avec le cache de blocs partagé de `source`.
    pub fn ouvrir_partage(
        systeme: Arc<dyn SystemeDistant>,
        source: Option<&str>,
        chemin: &str,
    ) -> io::Result<Self> {
        let e = systeme.stat(chemin)?;
        if e.dossier {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "c'est un dossier",
            ));
        }
        Ok(Self {
            systeme,
            chemin: chemin.to_string(),
            taille: e.taille,
            mtime: e.mtime,
            source: source.map(str::to_string),
            position: 0,
            blocs: VecDeque::with_capacity(BLOCS_GARDES),
        })
    }

    pub fn taille(&self) -> u64 {
        self.taille
    }

    fn bloc(&mut self, index: u64) -> io::Result<Arc<Vec<u8>>> {
        if let Some(i) = self.blocs.iter().position(|(n, _)| *n == index) {
            let b = self.blocs.remove(i).expect("position trouvée");
            let octets = b.1.clone();
            self.blocs.push_back(b);
            return Ok(octets);
        }
        let cle = self.source.as_ref().map(|s| {
            (
                s.clone(),
                self.chemin.clone(),
                self.taille,
                self.mtime,
                index,
            )
        });
        if let Some(octets) = cle.as_ref().and_then(lire_partage) {
            if self.blocs.len() >= BLOCS_GARDES {
                self.blocs.pop_front();
            }
            self.blocs.push_back((index, octets.clone()));
            return Ok(octets);
        }
        let debut = index * BLOC;
        let longueur = BLOC.min(self.taille.saturating_sub(debut)) as usize;
        let mut octets = vec![0u8; longueur];
        let mut lus = 0usize;
        while lus < longueur {
            let n = self
                .systeme
                .lire_a(&self.chemin, debut + lus as u64, &mut octets[lus..])?;
            if n == 0 {
                break; // fichier raccourci depuis le `stat`
            }
            lus += n;
        }
        octets.truncate(lus);
        let octets = Arc::new(octets);
        if let Some(cle) = cle {
            garder_partage(cle, octets.clone());
        }
        if self.blocs.len() >= BLOCS_GARDES {
            self.blocs.pop_front();
        }
        self.blocs.push_back((index, octets.clone()));
        Ok(octets)
    }
}

impl Read for LecteurDistant {
    fn read(&mut self, tampon: &mut [u8]) -> io::Result<usize> {
        if tampon.is_empty() || self.position >= self.taille {
            return Ok(0);
        }
        let index = self.position / BLOC;
        let bloc = self.bloc(index)?;
        let dans_le_bloc = (self.position - index * BLOC) as usize;
        if dans_le_bloc >= bloc.len() {
            return Ok(0);
        }
        let n = tampon.len().min(bloc.len() - dans_le_bloc);
        tampon[..n].copy_from_slice(&bloc[dans_le_bloc..dans_le_bloc + n]);
        self.position += n as u64;
        Ok(n)
    }
}

impl Seek for LecteurDistant {
    fn seek(&mut self, depuis: SeekFrom) -> io::Result<u64> {
        let cible = match depuis {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.taille.checked_add_signed(d),
            SeekFrom::Current(d) => self.position.checked_add_signed(d),
        };
        let cible = cible.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "position avant le début")
        })?;
        self.position = cible;
        Ok(cible)
    }
}

impl symphonia::core::io::MediaSource for LecteurDistant {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.taille)
    }
}
