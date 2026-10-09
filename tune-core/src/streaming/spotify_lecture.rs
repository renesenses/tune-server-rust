//! La lecture d'un titre Spotify dans une zone de Tune (#6018).
//!
//! Spotify ne publie aucune URL de flux : `SpotifyService::get_track_url`
//! refuse par construction. Le son vient donc du récepteur librespot que Tune
//! lance déjà (`streaming::spotify_connect`) :
//!
//! 1. librespot est connecté au compte par le jeton OAuth de Tune, ce qui le
//!    rend désignable par l'API Web ;
//! 2. la pompe du récepteur est attachée à CE titre ;
//! 3. l'API Web demande à l'appareil librespot de jouer le titre
//!    (`PUT /me/player/play?device_id=`, un seul titre : c'est la file de Tune
//!    qui enchaîne) ;
//! 4. le PCM de librespot (S16LE 44,1 kHz stéréo) entre dans la chaîne de
//!    lecture par la porte des sources PCM (`crate::source_pcm`), exactement
//!    comme la lecture d'un CD : session `/stream/<id>.wav` de longueur
//!    exacte, jouée par toutes les sorties (locale, OAAT, DLNA, AirPlay…),
//!    avance dans le titre par `seek_ms`, titre suivant par la file.
//!
//! Aucun ffmpeg, aucun décodage, rien sur disque : les octets passent tels
//! quels de librespot à la session (conditions de Spotify : pas de cache).

use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Mutex, mpsc};

use crate::source_pcm::{FluxPcm, FormatPcm, FournisseurPcm};
use crate::streaming::registry::ServiceRegistry;
use crate::streaming::spotify::SpotifyService;
use crate::streaming::spotify_connect::{
    OCTETS_PAR_SECONDE, OCTETS_PAR_TRAME, SpotifyConnectManager,
};

/// Le nom de la source, celui que portent les lignes de file Spotify.
pub const SOURCE: &str = "spotify";

/// Fin de titre sans évènement : à moins de [`MARGE_DE_FIN`] de la longueur
/// annoncée, un silence de cette durée clôt le titre.
const ATTENTE_DE_FIN: Duration = Duration::from_millis(1500);
/// Ce qui manque encore quand librespot s'est tu, au plus, pour que la fin
/// soit NORMALE (complétée par du silence) et non une interruption.
const MARGE_DE_FIN: u64 = 3 * OCTETS_PAR_SECONDE as u64;

pub const MOTIF_NON_CONNECTE: &str =
    "Spotify n'est pas connecté : se connecter dans Réglages → Services → Spotify.";

/// Ce que la lecture demande à Spotify. Séparé pour être doublé en test : la
/// production passe par le `SpotifyService` du registre.
#[async_trait]
pub trait PiloteSpotify: Send + Sync {
    /// Un jeton d'accès valide (rafraîchi au besoin).
    async fn jeton(&self) -> Result<String, String>;
    /// La durée du titre, en millisecondes.
    async fn duree_ms(&self, piste: &str) -> Result<u64, String>;
    /// Demande à l'appareil `appareil` de jouer `piste` depuis `position_ms`.
    async fn lancer(&self, appareil: &str, piste: &str, position_ms: u64) -> Result<(), String>;
}

/// Le pilote de production : le `SpotifyService` inscrit dans le registre.
pub struct PiloteDuRegistre {
    services: Arc<Mutex<ServiceRegistry>>,
}

impl PiloteDuRegistre {
    pub fn new(services: Arc<Mutex<ServiceRegistry>>) -> Self {
        Self { services }
    }

    async fn service(
        &self,
    ) -> Result<Arc<tokio::sync::RwLock<Box<dyn crate::streaming::StreamingService>>>, String> {
        let registre = self.services.lock().await;
        registre
            .get(SOURCE)
            .ok_or_else(|| "service Spotify absent".to_string())
    }
}

#[async_trait]
impl PiloteSpotify for PiloteDuRegistre {
    async fn jeton(&self) -> Result<String, String> {
        let svc = self.service().await?;
        let mut svc = svc.write().await;
        // Un jeton périmé se rafraîchit ici : librespot s'y connecte une fois,
        // l'API Web s'en sert aussitôt.
        let _ = svc.refresh_if_needed().await;
        svc.as_any()
            .downcast_ref::<SpotifyService>()
            .and_then(SpotifyService::jeton_d_acces)
            .ok_or_else(|| MOTIF_NON_CONNECTE.to_string())
    }

    async fn duree_ms(&self, piste: &str) -> Result<u64, String> {
        let svc = self.service().await?;
        let svc = svc.read().await;
        let titre = svc.get_track(piste).await.map_err(|e| e.to_string())?;
        Ok(titre.duration_ms)
    }

    async fn lancer(&self, appareil: &str, piste: &str, position_ms: u64) -> Result<(), String> {
        let svc = self.service().await?;
        let svc = svc.read().await;
        let spotify = svc
            .as_any()
            .downcast_ref::<SpotifyService>()
            .ok_or("service Spotify inattendu")?;
        spotify
            .lancer_sur_l_appareil(appareil, piste, position_ms)
            .await
    }
}

/// L'identifiant base62 d'un titre, quelle que soit la forme reçue
/// (`spotify:track:<id>`, lien `open.spotify.com/track/<id>?si=…`, ou l'id).
pub fn identifiant_de_piste(source_id: &str) -> &str {
    let s = source_id.trim();
    let s = s.strip_prefix("spotify:track:").unwrap_or(s);
    let s = s.rsplit("/track/").next().unwrap_or(s);
    s.split(['?', '#']).next().unwrap_or(s)
}

/// La source PCM `spotify`, inscrite au démarrage du serveur.
pub struct FournisseurSpotify {
    recepteur: Arc<SpotifyConnectManager>,
    pilote: Arc<dyn PiloteSpotify>,
    autorisee: Arc<dyn Fn() -> bool + Send + Sync>,
}

/// Le réglage persistant « Lecture Spotify (expérimental) », désactivé par
/// défaut (décision de Bertrand, 09/10) : librespot n'est pas un client
/// officiel de Spotify.
pub const CLE_REGLAGE: &str = "spotify_lecture_experimentale";

/// Préfixe du refus « option non activée », reconnu par la route de lecture
/// qui le rend en 409 avec un code stable (`spotify_playback_disabled`).
pub const SENTINELLE_NON_ACTIVEE: &str = "spotify_playback_disabled:";

pub const MOTIF_NON_ACTIVEE: &str =
    "Lecture Spotify non activée (option expérimentale dans les Réglages)";

/// Le réglage est-il activé ? Absent ou illisible : non.
pub fn lecture_activee(db: &Arc<dyn crate::db::backend::DbBackend>) -> bool {
    crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .get(CLE_REGLAGE)
        .ok()
        .flatten()
        .is_some_and(|v| v == "true")
}

impl FournisseurSpotify {
    /// `autorisee` est relue à CHAQUE ouverture : le réglage s'applique sans
    /// redémarrer le serveur.
    pub fn new(
        recepteur: Arc<SpotifyConnectManager>,
        pilote: Arc<dyn PiloteSpotify>,
        autorisee: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        Self {
            recepteur,
            pilote,
            autorisee,
        }
    }
}

impl FournisseurPcm for FournisseurSpotify {
    fn ouvrir(&self, source_id: &str, depuis_ms: u64) -> Result<FluxPcm, String> {
        // Appelé depuis un fil bloquant de l'orchestrateur : on y attend les
        // appels asynchrones sur le runtime qui l'a lancé.
        // Option expérimentale désactivée : refus propre, AVANT de toucher à
        // librespot ou à l'API Web.
        if !(self.autorisee)() {
            return Err(format!("{SENTINELLE_NON_ACTIVEE}{MOTIF_NON_ACTIVEE}"));
        }
        let rt = tokio::runtime::Handle::current();
        let piste = identifiant_de_piste(source_id).to_string();
        if piste.is_empty() {
            return Err("titre Spotify sans identifiant".into());
        }
        let jeton = rt.block_on(self.pilote.jeton())?;
        rt.block_on(self.recepteur.assurer_le_recepteur_connecte(&jeton))?;
        let duree_ms = rt.block_on(self.pilote.duree_ms(&piste))?;
        if duree_ms == 0 || depuis_ms >= duree_ms {
            return Err(format!(
                "titre Spotify « {piste} » : position {depuis_ms} ms hors du titre ({duree_ms} ms)"
            ));
        }
        // La pompe est attachée AVANT de lancer le titre : pas un octet du
        // début ne se perd.
        let pompe = self.recepteur.pompe();
        let rx = pompe.attacher(Some(piste.clone()));
        if let Err(e) = rt.block_on(self.pilote.lancer(
            self.recepteur.device_name(),
            &piste,
            depuis_ms,
        )) {
            pompe.detacher();
            return Err(e);
        }
        let octets = octets_a_servir(duree_ms, depuis_ms);
        tracing::info!(piste = %piste, depuis_ms, duree_ms, octets, "spotify_lecture_ouverte");
        Ok(FluxPcm {
            format: FormatPcm::CD,
            octets,
            duree_ms,
            lecteur: Box::new(LecteurDePompe {
                rx,
                rt,
                courant: Vec::new(),
                pos: 0,
                restant: octets,
                silence: false,
            }),
        })
    }

    /// Ouvrir un titre Spotify, c'est le LANCER sur l'appareil librespot : un
    /// pré-armement gapless couperait le titre qui joue.
    fn pre_armable(&self) -> bool {
        false
    }
}

/// Le lecteur remis à la session : les blocs de la pompe, jusqu'à la
/// longueur annoncée, complétés par du silence si librespot finit un peu
/// avant (sa durée réelle diffère de quelques millisecondes de celle de
/// l'API Web).
struct LecteurDePompe {
    rx: mpsc::Receiver<Vec<u8>>,
    rt: tokio::runtime::Handle,
    courant: Vec<u8>,
    pos: usize,
    restant: u64,
    silence: bool,
}

impl Read for LecteurDePompe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.restant == 0 || buf.is_empty() {
                return Ok(0);
            }
            let voulu = (buf.len() as u64).min(self.restant) as usize;
            if self.silence {
                buf[..voulu].fill(0);
                self.restant -= voulu as u64;
                return Ok(voulu);
            }
            if self.pos < self.courant.len() {
                let n = voulu.min(self.courant.len() - self.pos);
                buf[..n].copy_from_slice(&self.courant[self.pos..self.pos + n]);
                self.pos += n;
                self.restant -= n as u64;
                return Ok(n);
            }
            match self
                .rt
                .block_on(tokio::time::timeout(ATTENTE_DE_FIN, self.rx.recv()))
            {
                Ok(Some(bloc)) => {
                    self.courant = bloc;
                    self.pos = 0;
                }
                // Fin du titre (évènement `end_of_track`) ou silence prolongé
                // tout près de la fin : le reste est du silence.
                Ok(None) | Err(_) if self.restant <= MARGE_DE_FIN => {
                    self.silence = true;
                    self.rx.close();
                }
                Ok(None) => {
                    return Err(std::io::Error::other(format!(
                        "librespot s'est arrêté avant la fin du titre ({} octets manquants)",
                        self.restant
                    )));
                }
                // librespot charge ou met en tampon : on attend encore.
                Err(_) => {}
            }
        }
    }
}

fn octets_a_servir(duree_ms: u64, depuis_ms: u64) -> u64 {
    let ms = duree_ms.saturating_sub(depuis_ms);
    ms * (OCTETS_PAR_SECONDE / OCTETS_PAR_TRAME) as u64 / 1000 * OCTETS_PAR_TRAME as u64
}

#[cfg(all(test, unix))]
pub(crate) mod essais {
    //! Un faux librespot (script `sh`) et un faux pilote, partagés avec les
    //! témoins de l'orchestrateur. Aucun compte Spotify n'est touché.
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    /// Le faux librespot : relève ses arguments, puis attend qu'on lui
    /// demande de jouer (le fichier `jouer`, écrit par le faux pilote comme
    /// l'API Web le demanderait au vrai). Il écrit alors `N` octets 0x55 sur
    /// sa sortie standard, entourés des évènements `--onevent` d'un vrai.
    const FAUX_LIBRESPOT: &str = r#"#!/bin/sh
D=$(dirname "$0")
printf '%s\n' "$*" > "$D/args"
printf '%s' "$LIBRESPOT_ACCESS_TOKEN" > "$D/jeton"
EV=""
while [ $# -gt 0 ]; do
  case "$1" in
    --onevent) EV="$2"; shift ;;
  esac
  shift
done
while :; do
  if [ -f "$D/jouer" ]; then
    read N P < "$D/jouer"
    rm -f "$D/jouer"
    [ -n "$EV" ] && PLAYER_EVENT=track_changed TRACK_ID="$P" NAME="Titre essai" $EV
    [ -n "$EV" ] && PLAYER_EVENT=playing TRACK_ID="$P" POSITION_MS=0 $EV
    head -c "$N" /dev/zero | tr '\000' '\125'
    [ -n "$EV" ] && PLAYER_EVENT=end_of_track TRACK_ID="$P" $EV
  fi
  sleep 0.05
done
"#;

    pub(crate) struct Banc {
        pub dossier: tempfile::TempDir,
        pub recepteur: Arc<SpotifyConnectManager>,
        pub pilote: Arc<FauxPilote>,
    }

    impl Banc {
        pub fn args(&self) -> String {
            std::fs::read_to_string(self.dossier.path().join("args")).unwrap_or_default()
        }
        pub fn jeton_recu(&self) -> String {
            std::fs::read_to_string(self.dossier.path().join("jeton")).unwrap_or_default()
        }
        pub fn fournisseur(&self) -> FournisseurSpotify {
            self.fournisseur_autorise(true)
        }
        pub fn fournisseur_autorise(&self, autorisee: bool) -> FournisseurSpotify {
            FournisseurSpotify::new(
                self.recepteur.clone(),
                self.pilote.clone(),
                Arc::new(move || autorisee),
            )
        }
    }

    /// `duree_ms` : ce que l'API Web annonce ; `octets_joues` : ce que le
    /// faux librespot écrit vraiment.
    pub(crate) fn banc(duree_ms: u64, octets_joues: u64) -> Banc {
        let dossier = tempfile::tempdir().unwrap();
        let bin = dossier.path().join("librespot");
        std::fs::write(&bin, FAUX_LIBRESPOT).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let recepteur = Arc::new(SpotifyConnectManager::avec_binaire(
            "Tune".into(),
            0,
            Some(bin.to_string_lossy().into_owned()),
        ));
        let pilote = Arc::new(FauxPilote {
            dossier: dossier.path().to_path_buf(),
            duree_ms,
            octets_joues,
            lancements: std::sync::Mutex::new(Vec::new()),
        });
        Banc {
            dossier,
            recepteur,
            pilote,
        }
    }

    pub(crate) struct FauxPilote {
        dossier: PathBuf,
        duree_ms: u64,
        octets_joues: u64,
        pub lancements: std::sync::Mutex<Vec<(String, String, u64)>>,
    }

    impl FauxPilote {
        fn ecrire(dossier: &Path, n: u64, piste: &str) {
            let tmp = dossier.join("jouer.tmp");
            std::fs::write(&tmp, format!("{n} {piste}\n")).unwrap();
            std::fs::rename(tmp, dossier.join("jouer")).unwrap();
        }
    }

    #[async_trait]
    impl PiloteSpotify for FauxPilote {
        async fn jeton(&self) -> Result<String, String> {
            Ok("jeton-essai".into())
        }
        async fn duree_ms(&self, _: &str) -> Result<u64, String> {
            Ok(self.duree_ms)
        }
        async fn lancer(
            &self,
            appareil: &str,
            piste: &str,
            position_ms: u64,
        ) -> Result<(), String> {
            self.lancements
                .lock()
                .unwrap()
                .push((appareil.into(), piste.into(), position_ms));
            // Le vrai librespot ne joue que ce qui reste après `position_ms`.
            let saute = position_ms * (OCTETS_PAR_SECONDE as u64) / 1000;
            Self::ecrire(
                &self.dossier,
                self.octets_joues.saturating_sub(saute),
                piste,
            );
            Ok(())
        }
    }

    fn ouvrir(f: FournisseurSpotify, source_id: &str, depuis: u64) -> (FluxPcm, Vec<u8>) {
        let mut flux = f
            .ouvrir(source_id, depuis)
            .expect("ouverture du titre Spotify");
        let mut tout = Vec::new();
        flux.lecteur
            .read_to_end(&mut tout)
            .expect("lecture du flux");
        (flux, tout)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spotify_6018_le_pcm_de_librespot_devient_le_flux_du_titre() {
        let b = banc(1000, OCTETS_PAR_SECONDE as u64);
        let f = b.fournisseur();
        let (flux, tout) = tokio::task::spawn_blocking(move || {
            ouvrir(f, "spotify:track:4uLU6hMCjMI75M1A2tKUQC", 0)
        })
        .await
        .unwrap();
        assert_eq!(flux.format, FormatPcm::CD);
        assert_eq!(flux.duree_ms, 1000);
        assert_eq!(
            flux.octets, OCTETS_PAR_SECONDE as u64,
            "longueur exacte d'une seconde"
        );
        assert_eq!(tout.len() as u64, flux.octets);
        assert!(
            tout.iter().all(|&o| o == 0x55),
            "le flux est le PCM de librespot, octet pour octet"
        );
        assert_eq!(
            *b.pilote.lancements.lock().unwrap(),
            vec![("Tune".to_string(), "4uLU6hMCjMI75M1A2tKUQC".to_string(), 0)],
            "l'API Web désigne l'appareil librespot et le titre"
        );
        let args = b.args();
        for attendu in ["--backend pipe", "--autoplay off", "--onevent"] {
            assert!(
                args.contains(attendu),
                "librespot lancé sans « {attendu} » : {args}"
            );
        }
    }

    /// Le jeton OAuth ne doit JAMAIS figurer dans les arguments de librespot :
    /// `ps` les montre à tous les comptes de la machine. Il passe par
    /// `LIBRESPOT_ACCESS_TOKEN`, que librespot lit comme `--access-token`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spotify_6018_le_jeton_n_apparait_pas_dans_les_arguments() {
        let b = banc(1000, OCTETS_PAR_SECONDE as u64);
        let f = b.fournisseur();
        tokio::task::spawn_blocking(move || ouvrir(f, "piste", 0))
            .await
            .unwrap();
        let args = b.args();
        assert!(!args.is_empty(), "le faux librespot n'a pas été lancé");
        assert!(
            !args.contains("jeton-essai") && !args.contains("--access-token"),
            "le jeton OAuth est visible dans les arguments de librespot (ps) : {args}"
        );
        assert_eq!(
            b.jeton_recu(),
            "jeton-essai",
            "librespot doit recevoir le jeton par LIBRESPOT_ACCESS_TOKEN"
        );
    }

    /// Option expérimentale désactivée (le défaut) : refus PROPRE, avec la
    /// sentinelle que la route rend en 409 ; ni librespot ni l'API Web ne
    /// sont touchés.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spotify_6018_lecture_non_activee_refusee_proprement() {
        let b = banc(1000, OCTETS_PAR_SECONDE as u64);
        let f = b.fournisseur_autorise(false);
        let refus = tokio::task::spawn_blocking(move || f.ouvrir("piste", 0).err())
            .await
            .unwrap()
            .expect("la lecture doit être refusée tant que l'option est désactivée");
        assert!(
            refus.starts_with(SENTINELLE_NON_ACTIVEE) && refus.contains(MOTIF_NON_ACTIVEE),
            "{refus}"
        );
        assert!(b.pilote.lancements.lock().unwrap().is_empty());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            b.args().is_empty(),
            "librespot lancé alors que l'option est désactivée"
        );
    }

    /// Le réglage est désactivé par défaut et se lit en base.
    #[test]
    fn spotify_6018_le_reglage_est_desactive_par_defaut() {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        assert!(!lecture_activee(&db), "désactivé par défaut");
        let reglages = crate::db::settings_repo::SettingsRepo::with_backend(db.clone());
        reglages.set(CLE_REGLAGE, "true").unwrap();
        assert!(lecture_activee(&db));
        reglages.set(CLE_REGLAGE, "false").unwrap();
        assert!(!lecture_activee(&db));
    }

    /// librespot finit un peu avant la durée de l'API Web : le flux est
    /// complété par du silence, vite (évènement `end_of_track`), sans erreur.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spotify_6018_une_fin_un_peu_courte_est_completee_par_du_silence() {
        let b = banc(1000, 150_000);
        let f = b.fournisseur();
        let debut = std::time::Instant::now();
        let (flux, tout) = tokio::task::spawn_blocking(move || ouvrir(f, "piste", 0))
            .await
            .unwrap();
        assert_eq!(tout.len() as u64, flux.octets);
        assert!(tout[..150_000].iter().all(|&o| o == 0x55));
        assert!(tout[150_000..].iter().all(|&o| o == 0));
        assert!(
            debut.elapsed() < Duration::from_secs(5),
            "la fin du titre ne doit pas attendre ({:?})",
            debut.elapsed()
        );
    }

    /// L'avance dans le titre passe `position_ms` à l'API Web, et la longueur
    /// annoncée est celle du reste.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spotify_6018_l_avance_dans_le_titre_part_de_la_position() {
        let b = banc(1000, OCTETS_PAR_SECONDE as u64);
        let f = b.fournisseur();
        let (flux, tout) = tokio::task::spawn_blocking(move || ouvrir(f, "piste", 500))
            .await
            .unwrap();
        assert_eq!(flux.octets, OCTETS_PAR_SECONDE as u64 / 2);
        assert_eq!(tout.len() as u64, flux.octets);
        assert_eq!(b.pilote.lancements.lock().unwrap()[0].2, 500);
    }

    #[test]
    fn spotify_6018_identifiant_de_piste_sous_toutes_ses_formes() {
        for forme in [
            "4uLU6hMCjMI75M1A2tKUQC",
            "spotify:track:4uLU6hMCjMI75M1A2tKUQC",
            "https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC?si=abc",
        ] {
            assert_eq!(identifiant_de_piste(forme), "4uLU6hMCjMI75M1A2tKUQC");
        }
    }

    #[test]
    fn spotify_6018_un_titre_spotify_ne_se_pre_arme_pas() {
        let b = banc(1000, 0);
        assert!(!b.fournisseur().pre_armable());
    }
}
