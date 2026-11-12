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

use crate::source_pcm::{FluxPcm, FormatPcm, FournisseurPcm, QualiteRecue};
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
    /// « Le dernier geste gagne » — met l'appareil en pause, pour que
    /// l'application Spotify voie s'arrêter une session Connect que Tune a
    /// reprise sans pouvoir jouer. Par défaut : rien.
    async fn mettre_en_pause(&self, appareil: &str) -> Result<(), String> {
        let _ = appareil;
        Ok(())
    }
    /// #6933 — la liaison porte-t-elle la portée `streaming` ? `None` : on
    /// ne le sait pas.
    async fn porte_la_lecture(&self) -> Option<bool> {
        None
    }
}

/// #6933 — l'erreur de librespot est-elle un refus d'identifiants ?
pub fn est_un_refus_d_identifiants(erreur: &str) -> bool {
    [
        "INVALID_CREDENTIALS",
        "BadCredentials",
        "Login request was denied",
    ]
    .iter()
    .any(|m| erreur.contains(m))
}

/// #6933 — ce que le motif d'échec doit savoir du récepteur.
pub struct ContexteDeLibrespot<'a> {
    /// Le binaire lancé, décrit (chemin, origine, version).
    pub binaire: &'a str,
    /// Le nom de l'appareil Spotify Connect.
    pub appareil: &'a str,
    /// Le dossier où librespot garde sa connexion, s'il y en a un.
    pub dossier_identifiants: Option<&'a str>,
}

/// #6933 — le motif d'échec quand librespot n'a pas pu se connecter au
/// compte.
///
/// La cause établie (sources de librespot 0.8.0) : le jeton de Tune est émis
/// pour le Client ID de Tune ; librespot s'en sert pour le point d'accès,
/// puis ouvre sa session Spotify Connect par login5 au nom de SON propre
/// client, qui refuse cette connexion (`INVALID_CREDENTIALS`). Se reconnecter
/// dans Tune ne change donc rien ; ce qui marche, c'est une connexion émise
/// pour librespot lui-même, qu'il garde ensuite.
///
/// « Reconnecter Spotify » reste conseillé dans un seul cas : la liaison ne
/// porte pas la portée `streaming` (compte relié avant la lecture Spotify).
pub fn motif_echec_librespot(
    erreur: &str,
    porte_la_lecture: Option<bool>,
    ctx: &ContexteDeLibrespot<'_>,
) -> String {
    let binaire = ctx.binaire;
    if !est_un_refus_d_identifiants(erreur) {
        return format!(
            "librespot ({binaire}) s'est arrêté sans se connecter au compte Spotify : « {erreur} »."
        );
    }
    if porte_la_lecture == Some(false) {
        return format!(
            "Spotify a refusé la connexion de librespot : la liaison du compte dans Tune ne porte \
             pas le droit de lecture (« streaming »), elle date d'avant la lecture Spotify. \
             Reconnecter Spotify une fois dans Réglages → Services → Spotify. (librespot : « {erreur} »)"
        );
    }
    let garde = ctx
        .dossier_identifiants
        .map(|d| format!(" (dans {d})"))
        .unwrap_or_default();
    let appareil = ctx.appareil;
    let reconnexion = match porte_la_lecture {
        Some(true) => "La liaison du compte est complète : se reconnecter n'y changera rien.",
        _ => {
            "Se reconnecter dans Tune n'y changera rien, sauf si le compte a été relié avant la \
             lecture Spotify."
        }
    };
    format!(
        "Spotify a refusé la connexion de librespot (« {erreur} »). {reconnexion} librespot \
         n'ouvre sa session Spotify Connect qu'avec une connexion émise pour lui, pas avec le \
         jeton de Tune. À faire une fois : dans l'application Spotify (téléphone ou ordinateur, \
         même réseau, même compte), choisir l'appareil « {appareil} » ; librespot garde cette \
         connexion{garde} et Tune s'en sert ensuite. Binaire lancé : {binaire}."
    )
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
        if let Err(e) = svc.refresh_if_needed().await {
            tracing::warn!(erreur = %e, "spotify_rafraichissement_avant_lecture_echoue");
        }
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

    async fn mettre_en_pause(&self, appareil: &str) -> Result<(), String> {
        let svc = self.service().await?;
        let svc = svc.read().await;
        let spotify = svc
            .as_any()
            .downcast_ref::<SpotifyService>()
            .ok_or("service Spotify inattendu")?;
        spotify.mettre_en_pause_l_appareil(appareil).await
    }

    async fn porte_la_lecture(&self) -> Option<bool> {
        let svc = self.service().await.ok()?;
        let svc = svc.read().await;
        svc.as_any()
            .downcast_ref::<SpotifyService>()
            .and_then(SpotifyService::porte_la_lecture)
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

/// #6857 (Alesp, fil 2395) — le chemin RÉEL du réglage : il n'y a pas de
/// page « Réglages > Spotify » ; l'interrupteur est dans l'onglet « Accès et
/// jetons », carte « Spotify Connect », masquée au niveau Débutant
/// (`v2Settings.ts`, `min: 'intermediate'`, pastille « Avancé »).
pub const MOTIF_NON_ACTIVEE: &str = "Lecture Spotify non activée (Réglages › Accès et jetons › Spotify Connect › « Lecture Spotify (expérimental) », niveau Avancé)";

/// #8154 — code stable du refus « liaison Spotify Connect requise » : Spotify
/// a refusé le jeton de Tune à librespot, et le geste qui débloque est connu
/// (choisir une fois l'appareil de Tune dans l'application Spotify).
pub const CODE_LIAISON_REQUISE: &str = "spotify_liaison_connect_requise";

/// #8154 — préfixe de ce refus, reconnu par la route de lecture qui le rend
/// en 409 avec [`CODE_LIAISON_REQUISE`] au lieu d'une « Erreur de lecture ».
pub const SENTINELLE_LIAISON_REQUISE: &str = "spotify_liaison_connect_requise:";

/// Séparateur des champs de la sentinelle : ni un nom d'appareil ni un motif
/// ne le contiennent.
const SEPARATEUR_LIAISON: char = '\u{1f}';

/// #8154 — ce que la route de lecture doit savoir pour dire quoi faire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiaisonRequise {
    /// Le nom de l'appareil à choisir dans l'application Spotify.
    pub appareil: String,
    /// librespot tourne-t-il, prêt à recevoir la liaison ? Sinon, l'écran
    /// propose « Activer le récepteur ».
    pub recepteur_actif: bool,
    /// Le motif technique complet (raison de librespot, binaire lancé).
    pub detail: String,
}

impl LiaisonRequise {
    /// La forme d'erreur que rend `ouvrir` ; le motif reste lisible en fin.
    pub fn en_sentinelle(&self) -> String {
        let s = SEPARATEUR_LIAISON;
        format!(
            "{SENTINELLE_LIAISON_REQUISE}{}{s}{}{s}{}",
            if self.recepteur_actif { "1" } else { "0" },
            self.appareil,
            self.detail
        )
    }

    /// Relit la sentinelle n'importe où dans l'erreur (l'orchestrateur la
    /// préfixe de « source « spotify » : »).
    pub fn depuis_sentinelle(e: &str) -> Option<Self> {
        let debut = e.find(SENTINELLE_LIAISON_REQUISE)? + SENTINELLE_LIAISON_REQUISE.len();
        let mut champs = e[debut..].splitn(3, SEPARATEUR_LIAISON);
        let recepteur_actif = match champs.next()? {
            "1" => true,
            "0" => false,
            _ => return None,
        };
        let appareil = champs.next()?.to_string();
        let detail = champs.next().unwrap_or_default().to_string();
        Some(Self {
            appareil,
            recepteur_actif,
            detail,
        })
    }
}

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
        // « Le dernier geste gagne » (Bertrand, 12/11) : une session Spotify
        // Connect ouverte perd la pompe. Le pilote des sessions la ferme côté
        // zone tout de suite (la zone ne reste pas à moitié Connect).
        let session_connect = pompe.consommateur_continu();
        let rx = pompe.attacher(Some(piste.clone()));
        if session_connect {
            self.recepteur.signaler_reprise_par_tune();
        }
        let lancement = rt.block_on(self.pilote.lancer(
            self.recepteur.device_name(),
            &piste,
            depuis_ms,
        ));
        // La reprise a échoué : l'appareil jouerait encore la session dans le
        // vide. Pause, pour que l'application Spotify le voie s'arrêter.
        if lancement.is_err()
            && session_connect
            && let Err(e) = rt.block_on(self.pilote.mettre_en_pause(self.recepteur.device_name()))
        {
            tracing::warn!(erreur = %e, "spotify_pause_apres_reprise_impossible");
        }
        if let Err(e) = lancement {
            pompe.detacher();
            // #6933 — si librespot a dit pourquoi il ne s'est pas connecté,
            // c'est SA raison qui est rendue, pas un « reconnecter » générique.
            if let Some(erreur) = self.recepteur.derniere_erreur_librespot() {
                let portee = rt.block_on(self.pilote.porte_la_lecture());
                let lance = self.recepteur.librespot_lance();
                let binaire = lance
                    .as_ref()
                    .map(|l| l.decrire())
                    .unwrap_or_else(|| self.recepteur.binaire_librespot());
                let dossier = self
                    .recepteur
                    .dossier_identifiants_librespot()
                    .map(|d| d.display().to_string());
                tracing::warn!(
                    erreur_librespot = %erreur,
                    porte_la_lecture = ?portee,
                    binaire = %binaire,
                    echec = %e,
                    "spotify_lecture_librespot_non_connecte"
                );
                // Refus du jeton de Tune : librespot repart en Spotify Connect
                // seul, pour recevoir de l'application Spotify une connexion
                // à lui. Une connexion déjà gardée qui est refusée, elle, ne
                // relance rien : le message le dit.
                if est_un_refus_d_identifiants(&erreur)
                    && portee != Some(false)
                    && !self.recepteur.librespot_a_sa_connexion()
                {
                    rt.block_on(self.recepteur.relancer_pour_la_liaison());
                }
                let motif = motif_echec_librespot(
                    &erreur,
                    portee,
                    &ContexteDeLibrespot {
                        binaire: &binaire,
                        appareil: self.recepteur.device_name(),
                        dossier_identifiants: dossier.as_deref(),
                    },
                );
                // #8154 — le geste qui débloque est connu (choisir l'appareil
                // dans l'application Spotify) : un code stable le porte
                // jusqu'à l'écran, au lieu d'une « Erreur de lecture ». La
                // liaison sans portée `streaming` garde son motif (reconnecter).
                if est_un_refus_d_identifiants(&erreur) && portee != Some(false) {
                    let recepteur_actif = rt.block_on(self.recepteur.recepteur_actif());
                    return Err(LiaisonRequise {
                        appareil: self.recepteur.device_name().to_string(),
                        recepteur_actif,
                        detail: motif,
                    }
                    .en_sentinelle());
                }
                return Err(motif);
            }
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

    /// #6946 — librespot reçoit de l'Ogg Vorbis, au plus 320 kbit/s : le PCM
    /// qu'il rend n'est pas une source sans perte.
    fn qualite_recue(&self) -> Option<QualiteRecue> {
        let q = self.recepteur.qualite_recue();
        Some(QualiteRecue {
            codec: q.codec.to_string(),
            debit_kbps: Some(q.bitrate_kbps),
        })
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
        banc_avec_identifiants(duree_ms, octets_joues, false)
    }

    /// `identifiants` : librespot a déjà, dans son dossier, une connexion
    /// à lui (`credentials.json`).
    pub(crate) fn banc_avec_identifiants(
        duree_ms: u64,
        octets_joues: u64,
        identifiants: bool,
    ) -> Banc {
        let dossier = tempfile::tempdir().unwrap();
        let bin = dossier.path().join("librespot");
        std::fs::write(&bin, FAUX_LIBRESPOT).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cache = dossier.path().join("identifiants");
        if identifiants {
            std::fs::create_dir_all(&cache).unwrap();
            std::fs::write(
                cache.join(crate::streaming::spotify_connect::FICHIER_IDENTIFIANTS),
                "{}",
            )
            .unwrap();
        }
        let recepteur = Arc::new(
            SpotifyConnectManager::avec_binaire(
                "Tune".into(),
                0,
                Some(bin.to_string_lossy().into_owned()),
            )
            .avec_dossier_identifiants(Some(cache)),
        );
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

    /// #6933 — LA cause du fil 2419. librespot 0.8 fait passer le jeton
    /// (`LIBRESPOT_ACCESS_TOKEN`) avant sa connexion gardée en cache, et
    /// Spotify refuse ce jeton à sa session Connect. Quand librespot a une
    /// connexion à lui, Tune ne doit PAS lui passer son jeton, et lui donne
    /// son dossier ; sans connexion gardée, le jeton passe, et le dossier
    /// n'est PAS donné (librespot y écrirait une connexion inutilisable).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spotify_6933_la_connexion_de_librespot_passe_avant_le_jeton_de_tune() {
        let b = banc_avec_identifiants(1000, OCTETS_PAR_SECONDE as u64, true);
        let f = b.fournisseur();
        tokio::task::spawn_blocking(move || ouvrir(f, "piste", 0))
            .await
            .unwrap();
        let args = b.args();
        assert!(!args.is_empty(), "le faux librespot n'a pas été lancé");
        assert_eq!(
            b.jeton_recu(),
            "",
            "librespot a une connexion à lui : le jeton de Tune la supplanterait (fil 2419)"
        );
        assert!(
            args.contains("--system-cache") && args.contains("identifiants"),
            "librespot doit recevoir le dossier de sa connexion : {args}"
        );

        let sans = banc_avec_identifiants(1000, OCTETS_PAR_SECONDE as u64, false);
        let f = sans.fournisseur();
        tokio::task::spawn_blocking(move || ouvrir(f, "piste", 0))
            .await
            .unwrap();
        assert_eq!(
            sans.jeton_recu(),
            "jeton-essai",
            "sans connexion gardée, le jeton passe"
        );
        assert!(
            !sans.args().contains("--system-cache"),
            "avec le jeton de Tune, librespot ne doit pas écrire son cache : {}",
            sans.args()
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

    /// #6933 — le librespot 0.8.0 du testeur : Spotify refuse la connexion
    /// (`INVALID_CREDENTIALS`), librespot l'écrit sur stderr puis sort (1).
    const LIBRESPOT_REFUSE: &str = r#"#!/bin/sh
echo "[2026-10-20T18:02:11Z INFO  librespot] librespot 0.8.0 (Built on 2025-10-01)" >&2
echo "[2026-10-20T18:02:12Z ERROR librespot] could not initialize spirc: Invalid state { Login request was denied: INVALID_CREDENTIALS }" >&2
exit 1
"#;

    /// L'API Web accepte le jeton, mais l'appareil librespot n'apparaît pas.
    struct PiloteAppareilAbsent {
        portee: Option<bool>,
    }

    #[async_trait]
    impl PiloteSpotify for PiloteAppareilAbsent {
        async fn jeton(&self) -> Result<String, String> {
            Ok("jeton-essai".into())
        }
        async fn duree_ms(&self, _: &str) -> Result<u64, String> {
            Ok(1000)
        }
        async fn lancer(&self, appareil: &str, _: &str, _: u64) -> Result<(), String> {
            // Le vrai `lancer_sur_l_appareil` cherche l'appareil plusieurs
            // fois avant de renoncer : librespot a le temps d'écrire.
            tokio::time::sleep(Duration::from_millis(500)).await;
            Err(crate::streaming::spotify::motif_appareil_absent(appareil))
        }
        async fn porte_la_lecture(&self) -> Option<bool> {
            self.portee
        }
    }

    fn refus_6933(portee: Option<bool>) -> (String, String) {
        let dossier = tempfile::tempdir().unwrap();
        let bin = dossier.path().join("librespot");
        std::fs::write(&bin, LIBRESPOT_REFUSE).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let binaire = bin.to_string_lossy().into_owned();
        let recepteur = Arc::new(SpotifyConnectManager::avec_binaire(
            "Tune".into(),
            0,
            Some(binaire.clone()),
        ));
        let f = FournisseurSpotify::new(
            recepteur,
            Arc::new(PiloteAppareilAbsent { portee }),
            Arc::new(|| true),
        );
        let refus = f
            .ouvrir("spotify:track:4uLU6hMCjMI75M1A2tKUQC", 0)
            .err()
            .expect("librespot refusé : la lecture doit échouer");
        (refus, binaire)
    }

    /// #6933 — liaison complète (portée `streaming` accordée) : le refus de
    /// librespot n'est PAS présenté comme « reconnecter Spotify » ; le motif
    /// rend la raison écrite par librespot et le binaire lancé.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spotify_6933_refus_de_librespot_n_est_pas_une_demande_de_reconnexion() {
        let (refus, binaire) = tokio::task::spawn_blocking(|| refus_6933(Some(true)))
            .await
            .unwrap();
        assert!(
            refus.contains("INVALID_CREDENTIALS") && refus.contains(&binaire),
            "le motif ne rend pas la raison écrite par librespot ni le binaire lancé : {refus}"
        );
        assert!(
            refus.contains("se reconnecter n'y changera rien"),
            "liaison complète : le motif ne doit pas renvoyer à une reconnexion : {refus}"
        );
    }

    /// #6933 — liaison sans la portée `streaming` : là, et là seulement,
    /// reconnecter est la bonne consigne.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spotify_6933_sans_portee_streaming_la_reconnexion_est_conseillee() {
        let (refus, _) = tokio::task::spawn_blocking(|| refus_6933(Some(false)))
            .await
            .unwrap();
        assert!(
            refus.contains("Reconnecter Spotify") && refus.contains("streaming"),
            "{refus}"
        );
    }

    /// #8154 — le refus du jeton de Tune (liaison complète, ou portée
    /// inconnue) porte le code stable de la liaison Spotify Connect, avec
    /// l'appareil à choisir : l'écran peut dire quoi faire au lieu de
    /// « Erreur de lecture ». Le motif technique reste dans le détail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spotify_8154_refus_du_jeton_porte_le_code_de_liaison() {
        for portee in [Some(true), None] {
            let (refus, binaire) = tokio::task::spawn_blocking(move || refus_6933(portee))
                .await
                .unwrap();
            let enveloppe = format!("source « spotify » : {refus}");
            let liaison = LiaisonRequise::depuis_sentinelle(&enveloppe).unwrap_or_else(|| {
                panic!("{portee:?} : le refus ne porte pas {CODE_LIAISON_REQUISE} : {refus}")
            });
            assert_eq!(liaison.appareil, "Tune");
            assert!(
                liaison.detail.contains("INVALID_CREDENTIALS") && liaison.detail.contains(&binaire),
                "{portee:?} : le détail perd la raison de librespot : {}",
                liaison.detail
            );
        }
    }

    /// #8154 — sans la portée `streaming`, le geste est de reconnecter le
    /// compte, pas de choisir l'appareil : pas de code de liaison.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spotify_8154_sans_portee_streaming_pas_de_code_de_liaison() {
        let (refus, _) = tokio::task::spawn_blocking(|| refus_6933(Some(false)))
            .await
            .unwrap();
        assert!(
            LiaisonRequise::depuis_sentinelle(&refus).is_none(),
            "{refus}"
        );
    }

    #[test]
    fn spotify_8154_la_sentinelle_se_relit() {
        for recepteur_actif in [true, false] {
            let l = LiaisonRequise {
                appareil: "Tune: salon".into(),
                recepteur_actif,
                detail: "librespot : « INVALID_CREDENTIALS »".into(),
            };
            let e = format!("source « spotify » : {}", l.en_sentinelle());
            assert_eq!(LiaisonRequise::depuis_sentinelle(&e), Some(l));
        }
        assert!(LiaisonRequise::depuis_sentinelle("decode timeout").is_none());
    }

    #[test]
    fn spotify_6933_autre_erreur_de_librespot_rendue_telle_quelle() {
        let m = motif_echec_librespot(
            "Invalid `--password` / `-p`: Password authentication no longer supported, use OAuth",
            Some(true),
            &ContexteDeLibrespot {
                binaire: "/usr/local/bin/librespot",
                appareil: "Tune",
                dossier_identifiants: None,
            },
        );
        assert!(
            m.contains("Password authentication") && m.contains("/usr/local/bin/librespot"),
            "{m}"
        );
        assert!(!m.to_lowercase().contains("reconnect"), "{m}");
    }

    /// « Le dernier geste gagne » — un pilote dont le lancement réussit ou
    /// échoue, et qui note les pauses demandées.
    struct PiloteReprise {
        echoue: bool,
        pauses: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl PiloteSpotify for PiloteReprise {
        async fn jeton(&self) -> Result<String, String> {
            Ok("jeton-essai".into())
        }
        async fn duree_ms(&self, _: &str) -> Result<u64, String> {
            Ok(1000)
        }
        async fn lancer(&self, _: &str, _: &str, _: u64) -> Result<(), String> {
            if self.echoue {
                Err("Spotify a refusé la lecture (HTTP 500) : panne".into())
            } else {
                Ok(())
            }
        }
        async fn mettre_en_pause(&self, appareil: &str) -> Result<(), String> {
            self.pauses.lock().unwrap().push(appareil.to_string());
            Ok(())
        }
    }

    /// Ouvre un titre depuis Tune pendant qu'une session Connect tient la
    /// pompe (ou non) ; rend les évènements reçus par le pilote des sessions
    /// et les pauses demandées.
    fn reprise(session_ouverte: bool, echoue: bool) -> (Vec<String>, Vec<String>) {
        let b = banc(1000, 0);
        let (tx, mut rx) = mpsc::unbounded_channel();
        b.recepteur.brancher_les_evenements(tx);
        let _session = session_ouverte.then(|| b.recepteur.pompe().attacher_continu());
        let pilote = Arc::new(PiloteReprise {
            echoue,
            pauses: std::sync::Mutex::new(Vec::new()),
        });
        let f = FournisseurSpotify::new(b.recepteur.clone(), pilote.clone(), Arc::new(|| true));
        let resultat = f.ouvrir("spotify:track:4uLU6hMCjMI75M1A2tKUQC", 0);
        assert_eq!(resultat.is_err(), echoue);
        let mut evenements = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            evenements.push(ev.evenement);
        }
        let pauses = pilote.pauses.lock().unwrap().clone();
        (evenements, pauses)
    }

    const PREND: &str = crate::streaming::spotify_connect::EVENEMENT_TUNE_PREND_LA_MAIN;

    /// Session Connect ouverte, Tune lance un titre : le pilote des sessions
    /// est prévenu (il ferme la zone) ; le titre de Tune joue sur l'appareil,
    /// aucune pause.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dernier_geste_tune_prend_la_main_sur_la_session_connect() {
        let (evenements, pauses) = tokio::task::spawn_blocking(|| reprise(true, false))
            .await
            .unwrap();
        assert!(evenements.iter().any(|e| e == PREND), "{evenements:?}");
        assert!(pauses.is_empty(), "{pauses:?}");
    }

    /// La reprise échoue : la session est fermée quand même, et l'appareil
    /// est mis en pause pour que l'application Spotify le voie s'arrêter.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dernier_geste_reprise_echouee_ferme_et_met_en_pause() {
        let (evenements, pauses) = tokio::task::spawn_blocking(|| reprise(true, true))
            .await
            .unwrap();
        assert!(evenements.iter().any(|e| e == PREND), "{evenements:?}");
        assert_eq!(pauses, vec!["Tune".to_string()]);
    }

    /// Contre-épreuve : sans session Connect, Tune ne signale rien et ne met
    /// rien en pause, même en échec.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dernier_geste_sans_session_connect_rien_a_reprendre() {
        for echoue in [false, true] {
            let (evenements, pauses) = tokio::task::spawn_blocking(move || reprise(false, echoue))
                .await
                .unwrap();
            assert!(
                !evenements.iter().any(|e| e == PREND),
                "{echoue} : {evenements:?}"
            );
            assert!(pauses.is_empty(), "{echoue} : {pauses:?}");
        }
    }
}
