//! Streaming decode of a still-downloading DASH temp file (#1146 Plan C step 2).
//!
//! The Tidal HI-RES DASH path assembles all fMP4 segments into one temp file,
//! THEN pre-transcodes it to FLAC — the download and the decode run one after
//! the other. fMP4 is forward-decodable (each `moof`+`mdat` fragment is
//! self-contained after the `moov` init segment), so the decode can start on the
//! init segment and consume fragments as they land, overlapping the ~10s download
//! under the ~20-30s transcode.
//!
//! This module provides the two halves that make that safe:
//! - [`DashGrowth`]: shared writer→reader progress (bytes available, done/failed)
//!   with a condvar so the reader blocks instead of hitting a premature EOF.
//! - [`GrowingFileSource`]: a symphonia [`MediaSource`] over the temp file whose
//!   `read` returns available bytes and *blocks* at the write frontier until more
//!   arrives (or the download finishes), rather than returning EOF early — which
//!   would silently truncate the track (`decode_symphonia` breaks at EOF).
//!
//! A process-global registry hands the growth handle from the downloader
//! (`tidal.rs`) to the decoder (`decode.rs`) keyed by temp path, WITHOUT changing
//! the `get_track_url`/`StreamUrl` contract. The orchestrator renames the temp to
//! `{path}.decoding` before decoding, so [`take_for`] strips that suffix. This
//! engages behind `TUNE_DASH_STREAM_DECODE` for every output, and — since #7266 —
//! by default for a LOCAL or OAAT output (FLAC only) ([`pendant_le_telechargement`]); otherwise the
//! registry is empty and the decoder opens a plain `File` (byte-identical).

use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use symphonia::core::io::MediaSource;

struct GrowthState {
    /// Bytes durably written to the temp file and safe to read.
    available: u64,
    /// The downloader finished (successfully or not); no more bytes will come.
    done: bool,
    /// The download failed — the reader should surface an error, not silent EOF.
    failed: bool,
}

/// Shared progress between the DASH downloader (writer) and the decoder (reader).
pub struct DashGrowth {
    inner: Mutex<GrowthState>,
    cv: Condvar,
}

impl DashGrowth {
    /// Create a handle with `initial` bytes already available (the init segment).
    pub fn new(initial: u64) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(GrowthState {
                available: initial,
                done: false,
                failed: false,
            }),
            cv: Condvar::new(),
        })
    }

    /// Publish that the temp file now holds at least `available` bytes.
    pub fn advance(&self, available: u64) {
        let mut g = self.inner.lock().unwrap();
        if available > g.available {
            g.available = available;
        }
        self.cv.notify_all();
    }

    /// The download completed — the current `available` is the final size.
    pub fn finish(&self) {
        let mut g = self.inner.lock().unwrap();
        g.done = true;
        self.cv.notify_all();
    }

    /// The download failed mid-flight — readers get an error at the frontier.
    pub fn fail(&self) {
        let mut g = self.inner.lock().unwrap();
        g.failed = true;
        g.done = true;
        self.cv.notify_all();
    }
}

impl DashGrowth {
    /// #8228 — le téléchargement a-t-il échoué ? Distingue une fin de
    /// conteneur d'un flux coupé.
    pub fn a_echoue(&self) -> bool {
        self.inner.lock().unwrap().failed
    }

    /// #8228 — bloque jusqu'à la fin du téléchargement, pour un lecteur qui
    /// a besoin du fichier complet. Erreur si le téléchargement a échoué.
    pub fn attendre_la_fin(&self) -> io::Result<u64> {
        let mut g = self.inner.lock().unwrap();
        while !g.done {
            g = self.cv.wait(g).unwrap();
        }
        if g.failed {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "streaming download failed",
            ));
        }
        Ok(g.available)
    }
}

/// A blocking [`MediaSource`] over a file another task is still appending to.
/// `read`/`seek` block at the write frontier until more bytes arrive or the
/// download finishes, so the forward-only fMP4 decode never sees a premature EOF.
pub struct GrowingFileSource {
    file: std::fs::File,
    pos: u64,
    growth: Arc<DashGrowth>,
    /// #7266 — au-delà, une frontière qui n'avance plus est un téléchargement
    /// perdu (tâche disparue, CDN muet) : erreur, jamais une attente à vie.
    delai_sans_progres: std::time::Duration,
}

/// #7266 — délai total d'une requête de segment (client HTTP de TIDAL).
pub const DELAI_D_UNE_REQUETE_DE_SEGMENT: std::time::Duration = std::time::Duration::from_secs(30);
/// #7266 — un segment refusé est redemandé une fois avant l'échec.
pub const REPRISES_D_UN_SEGMENT: u64 = 1;
/// #7266 — marge au-delà du pire cas légitime (requête, puis sa reprise).
const MARGE_SANS_PROGRES: std::time::Duration = std::time::Duration::from_secs(15);

/// #7266 — attente maximale à la frontière d'écriture sans qu'un octet arrive.
/// Dérivée du pire cas d'un réseau lent mais vivant : une requête de segment
/// qui va au bout de son délai, puis sa reprise, plus une marge (75 s). En
/// deçà, une piste qui aurait fini par jouer serait coupée.
pub const ATTENTE_MAX_SANS_PROGRES: std::time::Duration = std::time::Duration::from_secs(
    DELAI_D_UNE_REQUETE_DE_SEGMENT.as_secs() * (1 + REPRISES_D_UN_SEGMENT)
        + MARGE_SANS_PROGRES.as_secs(),
);

impl GrowingFileSource {
    pub fn open(path: &str, growth: Arc<DashGrowth>) -> io::Result<Self> {
        Ok(Self {
            file: std::fs::File::open(path)?,
            pos: 0,
            growth,
            delai_sans_progres: ATTENTE_MAX_SANS_PROGRES,
        })
    }

    /// Le même lecteur avec un autre délai d'inactivité (essais).
    pub fn avec_delai_sans_progres(mut self, delai: std::time::Duration) -> Self {
        self.delai_sans_progres = delai;
        self
    }

    /// Block until `target` bytes are available or the download ends. Returns the
    /// current `available` (which may be < target only when `done`), or an error
    /// if the download failed before reaching `target`.
    ///
    /// #7266 — l'échec n'est PAS rendu en `UnexpectedEof` : les décodeurs
    /// prennent ce genre-là pour la fin normale du flux, et une piste coupée
    /// au segment 30 sur 86 finissait en silence, comme si elle était jouée.
    /// Une frontière immobile plus de `delai_sans_progres` vaut échec.
    fn wait_until(&self, target: u64) -> io::Result<u64> {
        let mut g = self.growth.inner.lock().unwrap();
        let mut vu = g.available;
        let mut depuis = std::time::Instant::now();
        loop {
            if target <= g.available {
                return Ok(g.available);
            }
            if g.failed {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "dash streaming download failed",
                ));
            }
            if g.done {
                return Ok(g.available); // final size reached, no more bytes
            }
            if g.available != vu {
                vu = g.available;
                depuis = std::time::Instant::now();
            }
            let reste = self.delai_sans_progres.saturating_sub(depuis.elapsed());
            if reste.is_zero() {
                g.failed = true;
                g.done = true;
                self.growth.cv.notify_all();
                tracing::warn!(
                    octets = g.available,
                    delai_s = self.delai_sans_progres.as_secs_f64(),
                    "dash_croissance_frontiere_immobile_lecture_interrompue"
                );
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "dash streaming download stalled",
                ));
            }
            g = self.growth.cv.wait_timeout(g, reste).unwrap().0;
        }
    }
}

impl Read for GrowingFileSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Block until at least one unread byte is available (or real EOF).
        let available = self.wait_until(self.pos + 1)?;
        if self.pos >= available {
            return Ok(0); // done and fully consumed
        }
        let to_read = ((available - self.pos) as usize).min(buf.len());
        self.file.seek(SeekFrom::Start(self.pos))?;
        let n = self.file.read(&mut buf[..to_read])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for GrowingFileSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(n) => n,
            SeekFrom::Current(d) => (self.pos as i64 + d).max(0) as u64,
            // byte_len() is None and is_seekable() is false, so symphonia must not
            // seek from the end; refuse rather than guess.
            SeekFrom::End(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "no end-relative seek on a growing dash file",
                ));
            }
        };
        // Only forward within (or up to) what's been downloaded; block otherwise.
        self.wait_until(target)?;
        self.pos = target;
        Ok(self.pos)
    }
}

impl MediaSource for GrowingFileSource {
    fn is_seekable(&self) -> bool {
        false
    }
    fn byte_len(&self) -> Option<u64> {
        None
    }
}

fn registry() -> &'static Mutex<HashMap<String, Arc<DashGrowth>>> {
    static R: OnceLock<Mutex<HashMap<String, Arc<DashGrowth>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a growth handle for a DASH temp path (called by the downloader).
pub fn register(path: &str, growth: Arc<DashGrowth>) {
    registry().lock().unwrap().insert(path.to_string(), growth);
}

/// Take the growth handle for a decode path, tolerating the orchestrator's
/// `{path}.decoding` rename. Returns `None` (→ plain-`File` decode) when the flag
/// is off or the file isn't a streaming DASH temp.
pub fn take_for(decode_path: &str) -> Option<Arc<DashGrowth>> {
    let key = decode_path.strip_suffix(".decoding").unwrap_or(decode_path);
    registry().lock().unwrap().remove(key)
}

/// Whether streaming DASH decode is enabled (`TUNE_DASH_STREAM_DECODE`).
pub fn stream_decode_enabled() -> bool {
    std::env::var("TUNE_DASH_STREAM_DECODE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

tokio::task_local! {
    /// #7266 — posé par l'orchestrateur autour de la résolution d'une piste
    /// destinée à une sortie LOCALE ou OAAT : sa session WAV est servie en continu,
    /// sans longueur annoncée, et peut donc commencer pendant que les
    /// segments DASH arrivent. Lu dans la tâche qui résout ; une tâche lancée
    /// à part (préchargement, sentinelle) ne le voit pas.
    static DECODAGE_PENDANT_LE_TELECHARGEMENT: bool;
}

/// #7266 — exécute `f` (la résolution d'une piste) en demandant, si `actif`,
/// le téléchargement DASH en croissance : le chemin revient dès le segment
/// d'initialisation, et le décodeur suit la frontière d'écriture.
pub async fn pendant_le_telechargement<F: std::future::Future>(actif: bool, f: F) -> F::Output {
    DECODAGE_PENDANT_LE_TELECHARGEMENT.scope(actif, f).await
}

/// Le téléchargement DASH qui commence doit-il rendre la main dès l'init ?
/// Oui si `TUNE_DASH_STREAM_DECODE` l'arme pour tout le serveur, ou si la
/// tâche courante résout pour une sortie locale ou OAAT ([`pendant_le_telechargement`]).
pub fn decodage_en_croissance() -> bool {
    stream_decode_enabled()
        || DECODAGE_PENDANT_LE_TELECHARGEMENT
            .try_with(|actif| *actif)
            .unwrap_or(false)
}

/// #7266 — `TUNE_DASH_STREAM_DECODE=0` (ou `false`) rend l'ancien
/// comportement aux sorties locales et OAAT : la piste entière avant la première note.
pub fn decodage_local_desarme() -> bool {
    std::env::var("TUNE_DASH_STREAM_DECODE")
        .map(|v| v == "0" || v.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
}

/// #7266 — l'état d'un téléchargement en croissance, vu du cache d'URL : il
/// dit s'il est en cours, complet ou perdu, et laisse une lecture qui a besoin
/// du fichier ENTIER (un saut dans la même piste) attendre sa fin au lieu de le
/// retélécharger. Tant que quelqu'un attend, le téléchargeur ne l'abandonne pas.
pub struct SuiviDuTelechargement {
    etat: std::sync::atomic::AtomicU8,
    attendants: std::sync::atomic::AtomicUsize,
    /// Octets écrits jusqu'ici : tant qu'ils augmentent, on attend.
    octets: std::sync::atomic::AtomicU64,
    fin: tokio::sync::Notify,
}

const EN_COURS: u8 = 0;
const COMPLET: u8 = 1;
const PERDU: u8 = 2;

impl SuiviDuTelechargement {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            etat: std::sync::atomic::AtomicU8::new(EN_COURS),
            attendants: std::sync::atomic::AtomicUsize::new(0),
            octets: std::sync::atomic::AtomicU64::new(0),
            fin: tokio::sync::Notify::new(),
        })
    }

    /// Le téléchargement est fini : `complet`, ou perdu.
    pub fn terminer(&self, complet: bool) {
        self.etat.store(
            if complet { COMPLET } else { PERDU },
            std::sync::atomic::Ordering::SeqCst,
        );
        self.fin.notify_waiters();
    }

    pub fn est_complet(&self) -> bool {
        self.etat.load(std::sync::atomic::Ordering::SeqCst) == COMPLET
    }

    /// Le fichier tient maintenant `octets` : réveille qui attend, pour qu'il
    /// reparte d'un nouveau délai.
    pub fn avancer(&self, octets: u64) {
        self.octets
            .fetch_max(octets, std::sync::atomic::Ordering::SeqCst);
        self.fin.notify_waiters();
    }

    pub fn en_cours(&self) -> bool {
        self.etat.load(std::sync::atomic::Ordering::SeqCst) == EN_COURS
    }

    /// Quelqu'un attend la fin (le téléchargement ne doit pas être abandonné).
    pub fn attendu(&self) -> bool {
        self.attendants.load(std::sync::atomic::Ordering::SeqCst) > 0
    }

    /// Attend la fin tant que le téléchargement progresse : l'attente n'est
    /// bornée que par `delai_sans_progres` SANS nouvel octet (une piste longue
    /// sur une ligne lente peut prendre des minutes, elle n'est jamais
    /// relancée de zéro tant qu'elle avance). `true` si le fichier est complet.
    pub async fn attendre_la_fin(&self, delai_sans_progres: std::time::Duration) -> bool {
        struct Attendant<'a>(&'a std::sync::atomic::AtomicUsize);
        impl Drop for Attendant<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        self.attendants
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _attendant = Attendant(&self.attendants);
        let mut vu = self.octets.load(std::sync::atomic::Ordering::SeqCst);
        let mut limite = tokio::time::Instant::now() + delai_sans_progres;
        loop {
            let notifie = self.fin.notified();
            tokio::pin!(notifie);
            notifie.as_mut().enable();
            if !self.en_cours() {
                return self.est_complet();
            }
            let maintenant = self.octets.load(std::sync::atomic::Ordering::SeqCst);
            if maintenant != vu {
                vu = maintenant;
                limite = tokio::time::Instant::now() + delai_sans_progres;
            }
            if tokio::time::timeout_at(limite, notifie).await.is_err() {
                let fin = self.octets.load(std::sync::atomic::Ordering::SeqCst);
                if fin == vu {
                    return self.est_complet();
                }
            }
        }
    }
}

impl DashGrowth {
    /// #7266 — personne d'autre que le téléchargeur ne tient plus la poignée
    /// (ni le registre, ni un décodeur) : la lecture a été abandonnée, la
    /// suite du téléchargement ne servirait à rien.
    pub fn abandonnee(self: &Arc<Self>) -> bool {
        Arc::strong_count(self) == 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn growing_source_blocks_then_reads_all() {
        // Writer appends in two bursts; a reader thread must see every byte and
        // only hit EOF after finish(), never mid-stream.
        let fichier = crate::test_scratch::scratch_file("tune-growtest", ".bin");
        let path = fichier.to_string_lossy().to_string();
        std::fs::write(&path, b"AAAA").unwrap(); // 4 initial bytes
        let growth = DashGrowth::new(4);

        let g2 = growth.clone();
        let p2 = path.clone();
        let reader = std::thread::spawn(move || {
            let mut src = GrowingFileSource::open(&p2, g2).unwrap();
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

        // Append a second burst and publish it, then finish.
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

        let out = reader.join().unwrap();
        assert_eq!(out, b"AAAABBBBBB");
    }

    #[test]
    fn failed_download_surfaces_error_not_silent_eof() {
        let fichier = crate::test_scratch::scratch_file("tune-growtest-fail", ".bin");
        let path = fichier.to_string_lossy().to_string();
        std::fs::write(&path, b"AAAA").unwrap();
        let growth = DashGrowth::new(4);
        let mut src = GrowingFileSource::open(&path, growth.clone()).unwrap();
        // Consume the 4 available bytes.
        let mut buf = [0u8; 4];
        assert_eq!(src.read(&mut buf).unwrap(), 4);
        // Reader now at the frontier; a failed download must error, not EOF.
        growth.fail();
        let erreur = src.read(&mut buf).unwrap_err();
        // #7266 — pas `UnexpectedEof`, que les décodeurs prennent pour la fin.
        assert_ne!(erreur.kind(), io::ErrorKind::UnexpectedEof);
    }

    /// #7266 — un téléchargeur disparu sans clore la poignée ne bloque pas le
    /// lecteur à vie : passé le délai d'inactivité, erreur et poignée en échec.
    #[test]
    fn une_frontiere_immobile_finit_en_erreur_7266() {
        let fichier = crate::test_scratch::scratch_file("tune-growtest-immobile", ".bin");
        let path = fichier.to_string_lossy().to_string();
        std::fs::write(&path, b"AAAA").unwrap();
        let growth = DashGrowth::new(4);
        let mut src = GrowingFileSource::open(&path, growth.clone())
            .unwrap()
            .avec_delai_sans_progres(std::time::Duration::from_millis(200));
        let mut buf = [0u8; 4];
        assert_eq!(src.read(&mut buf).unwrap(), 4);
        let debut = std::time::Instant::now();
        let erreur = src.read(&mut buf).unwrap_err();
        assert_eq!(erreur.kind(), io::ErrorKind::TimedOut);
        assert!(debut.elapsed() < std::time::Duration::from_secs(5));
        assert!(growth.a_echoue(), "la poignée est marquée en échec");
    }

    /// La borne d'inactivité couvre une requête de segment qui va au bout de
    /// son délai puis sa reprise : un réseau lent mais vivant n'est pas coupé.
    #[test]
    fn l_attente_sans_progres_couvre_une_requete_et_sa_reprise_7266() {
        assert!(
            ATTENTE_MAX_SANS_PROGRES
                > DELAI_D_UNE_REQUETE_DE_SEGMENT * (1 + REPRISES_D_UN_SEGMENT as u32)
        );
        assert_eq!(ATTENTE_MAX_SANS_PROGRES, std::time::Duration::from_secs(75));
    }

    /// Un téléchargement qui avance n'est jamais lâché, même bien au-delà de
    /// la borne d'inactivité ; un téléchargement immobile l'est.
    #[tokio::test]
    async fn l_attente_dure_tant_que_le_telechargement_progresse_7266() {
        let suivi = SuiviDuTelechargement::new();
        let s = suivi.clone();
        let producteur = tokio::spawn(async move {
            for i in 1..=10u64 {
                tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                s.avancer(i * 1000);
            }
            s.terminer(true);
        });
        let debut = std::time::Instant::now();
        // Borne de 150 ms, téléchargement de 600 ms qui progresse.
        assert!(
            suivi
                .attendre_la_fin(std::time::Duration::from_millis(150))
                .await
        );
        assert!(debut.elapsed() >= std::time::Duration::from_millis(500));
        producteur.await.unwrap();

        let immobile = SuiviDuTelechargement::new();
        let debut = std::time::Instant::now();
        assert!(
            !immobile
                .attendre_la_fin(std::time::Duration::from_millis(150))
                .await
        );
        assert!(debut.elapsed() < std::time::Duration::from_secs(2));
    }

    #[tokio::test]
    async fn le_suivi_reveille_qui_attend_la_fin_7266() {
        let suivi = SuiviDuTelechargement::new();
        let s = suivi.clone();
        let attente =
            tokio::spawn(async move { s.attendre_la_fin(std::time::Duration::from_secs(5)).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(suivi.attendu());
        suivi.terminer(true);
        assert!(attente.await.unwrap());
        assert!(!suivi.attendu());
    }

    #[test]
    fn registry_take_strips_decoding_suffix() {
        let g = DashGrowth::new(1);
        register("/tmp/tune-dash-xyz.mp4", g);
        // Orchestrator renamed it to `.decoding` before decoding.
        assert!(take_for("/tmp/tune-dash-xyz.mp4.decoding").is_some());
        // Gone after take.
        assert!(take_for("/tmp/tune-dash-xyz.mp4.decoding").is_none());
    }
}
