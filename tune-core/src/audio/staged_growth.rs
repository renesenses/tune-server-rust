//! Staging PIPELINÉ d'un fichier de montage réseau (phase 2 des lenteurs Yves).
//!
//! Le staging de la phase 1 copie le fichier ENTIER avant de décoder : sur un
//! ALAC de 152 Mo depuis un NAS WiFi, 41 s avant la première note. Ici, la
//! copie réseau → temp tourne en tâche de fond et le décodeur lit le temp AU
//! FUR ET À MESURE — pour un fichier « faststart » (atome `moov` en tête), la
//! lecture démarre en 2-3 s au lieu d'attendre les 41 s.
//!
//! Diffère de [`super::dash_growth`] sur un point crucial : cette source est
//! SEEKABLE et connaît sa taille finale. Le fMP4 DASH est décodable en avant
//! seulement, mais l'ALAC/m4a « moov-at-end » exige un `SeekFrom::End` pour
//! trouver ses tables — refusé par `GrowingFileSource`. Ici, tout seek est
//! honoré : un seek dans la zone déjà copiée est immédiat, un seek au-delà de
//! la frontière BLOQUE jusqu'à ce que la copie l'atteigne (un moov-at-end
//! attend donc la fin de copie — pas pire que la phase 1, jamais faux).
//!
//! Garde-fou : tout passe derrière `TUNE_STAGE_STREAM_DECODE`. Flag absent =
//! registre vide = le décodeur ouvre un `File` nu, octet pour octet identique
//! à la phase 1.

use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use symphonia::core::io::MediaSource;

struct GrowthState {
    /// Octets durablement écrits dans le temp et sûrs à lire.
    available: u64,
    /// La copie est terminée (bien ou mal) ; plus aucun octet ne viendra.
    done: bool,
    /// La copie a échoué — le lecteur doit remonter une erreur, pas un EOF muet.
    failed: bool,
}

/// Progression partagée entre le copieur (écrivain) et le décodeur (lecteur).
pub struct StageGrowth {
    inner: Mutex<GrowthState>,
    cv: Condvar,
    /// Taille finale connue d'avance (métadonnées du fichier source). C'est ce
    /// qui rend la source seekable, contrairement au cas DASH.
    size: u64,
}

impl StageGrowth {
    pub fn new(size: u64) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(GrowthState {
                available: 0,
                done: false,
                failed: false,
            }),
            cv: Condvar::new(),
            size,
        })
    }

    pub fn advance(&self, available: u64) {
        let mut g = self.inner.lock().unwrap();
        if available > g.available {
            g.available = available;
        }
        self.cv.notify_all();
    }

    pub fn finish(&self) {
        let mut g = self.inner.lock().unwrap();
        g.done = true;
        self.cv.notify_all();
    }

    pub fn fail(&self) {
        let mut g = self.inner.lock().unwrap();
        g.failed = true;
        g.done = true;
        self.cv.notify_all();
    }
}

/// [`MediaSource`] SEEKABLE sur un temp qu'un thread copie encore. Une lecture
/// ou un seek au-delà de la frontière d'écriture bloque jusqu'à l'arrivée des
/// octets (ou la fin de copie) ; en deçà, c'est immédiat.
pub struct SeekableGrowingSource {
    file: std::fs::File,
    pos: u64,
    growth: Arc<StageGrowth>,
}

impl SeekableGrowingSource {
    pub fn open(path: &str, growth: Arc<StageGrowth>) -> io::Result<Self> {
        Ok(Self {
            file: std::fs::File::open(path)?,
            pos: 0,
            growth,
        })
    }

    /// Bloque jusqu'à ce que `target` octets soient disponibles ou la copie
    /// finisse. Rend l'`available` courant (< target seulement si `done`), ou
    /// une erreur si la copie a échoué avant d'atteindre `target`.
    fn wait_until(&self, target: u64) -> io::Result<u64> {
        let mut g = self.growth.inner.lock().unwrap();
        loop {
            if target <= g.available {
                return Ok(g.available);
            }
            if g.failed {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "staging copy failed",
                ));
            }
            if g.done {
                return Ok(g.available);
            }
            g = self.growth.cv.wait(g).unwrap();
        }
    }
}

impl Read for SeekableGrowingSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available = self.wait_until(self.pos + 1)?;
        if self.pos >= available {
            return Ok(0); // fin de copie, tout consommé
        }
        let to_read = ((available - self.pos) as usize).min(buf.len());
        self.file.seek(SeekFrom::Start(self.pos))?;
        let n = self.file.read(&mut buf[..to_read])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for SeekableGrowingSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(n) => n,
            SeekFrom::Current(d) => (self.pos as i64 + d).max(0) as u64,
            // La taille finale est connue : un seek depuis la fin est légitime
            // (c'est ce que fait l'ALAC moov-at-end). On le résout en absolu,
            // puis on bloque jusqu'à ce que la copie l'ait atteint.
            SeekFrom::End(d) => (self.growth.size as i64 + d).max(0) as u64,
        };
        self.wait_until(target)?;
        self.pos = target;
        Ok(self.pos)
    }
}

impl MediaSource for SeekableGrowingSource {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.growth.size)
    }
}

fn registry() -> &'static Mutex<HashMap<String, Arc<StageGrowth>>> {
    static R: OnceLock<Mutex<HashMap<String, Arc<StageGrowth>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn register(path: &str, growth: Arc<StageGrowth>) {
    registry().lock().unwrap().insert(path.to_string(), growth);
}

pub fn take_for(decode_path: &str) -> Option<Arc<StageGrowth>> {
    registry().lock().unwrap().remove(decode_path)
}

/// Le staging pipeliné est-il activé (`TUNE_STAGE_STREAM_DECODE`) ? Défaut OFF :
/// tant qu'Yves ne l'a pas validé, la copie bloquante de la phase 1 reste la
/// voie par défaut, sans aucun risque de régression.
pub fn stream_decode_enabled() -> bool {
    std::env::var("TUNE_STAGE_STREAM_DECODE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp(name: &str) -> String {
        std::env::temp_dir()
            .join(format!(
                "{}.bin",
                crate::test_scratch::scratch_name(&format!("tune-stagegrow-{name}"))
            ))
            .to_string_lossy()
            .to_string()
    }

    #[test]
    fn lecture_sequentielle_bloque_a_la_frontiere_puis_lit_tout() {
        let path = temp("seq");
        std::fs::write(&path, b"AAAA").unwrap();
        let growth = StageGrowth::new(10);
        growth.advance(4);

        let g2 = growth.clone();
        let p2 = path.clone();
        let reader = std::thread::spawn(move || {
            let mut src = SeekableGrowingSource::open(&p2, g2).unwrap();
            let mut out = Vec::new();
            let mut buf = [0u8; 3];
            loop {
                let n = src.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n]);
            }
            out
        });

        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(b"BBBBBB").unwrap();
            f.flush().unwrap();
        }
        growth.advance(10);
        growth.finish();

        assert_eq!(reader.join().unwrap(), b"AAAABBBBBB");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn seek_dans_la_zone_copiee_est_immediat_seek_au_dela_bloque() {
        let path = temp("seek");
        std::fs::write(&path, b"0123456789").unwrap();
        let growth = StageGrowth::new(10);
        growth.advance(5); // seuls "01234" sont disponibles

        let g2 = growth.clone();
        let p2 = path.clone();
        let reader = std::thread::spawn(move || {
            let mut src = SeekableGrowingSource::open(&p2, g2).unwrap();
            // Seek dans la zone copiée : immédiat.
            src.seek(SeekFrom::Start(2)).unwrap();
            let mut b = [0u8; 2];
            src.read_exact(&mut b).unwrap();
            assert_eq!(&b, b"23");
            // Seek depuis la FIN (moov-at-end) : au-delà de la frontière, bloque
            // jusqu'à ce que la copie atteigne l'octet visé.
            src.seek(SeekFrom::End(-2)).unwrap(); // octet 8, pas encore dispo
            let mut e = [0u8; 2];
            src.read_exact(&mut e).unwrap();
            e
        });

        // Laisse le lecteur atteindre le seek bloquant, puis termine la copie.
        std::thread::sleep(std::time::Duration::from_millis(10));
        growth.advance(10);
        growth.finish();

        assert_eq!(&reader.join().unwrap(), b"89");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn une_copie_en_echec_remonte_une_erreur_pas_un_eof_muet() {
        let path = temp("fail");
        std::fs::write(&path, b"AAAA").unwrap();
        let growth = StageGrowth::new(10);
        growth.advance(4);
        let mut src = SeekableGrowingSource::open(&path, growth.clone()).unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(src.read(&mut buf).unwrap(), 4);
        growth.fail();
        assert!(src.read(&mut buf).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn seekable_et_taille_connue() {
        let path = temp("meta");
        std::fs::write(&path, b"xxxxx").unwrap();
        let growth = StageGrowth::new(5);
        growth.advance(5);
        growth.finish();
        let src = SeekableGrowingSource::open(&path, growth).unwrap();
        assert!(src.is_seekable());
        assert_eq!(src.byte_len(), Some(5));
        let _ = std::fs::remove_file(&path);
    }
}
