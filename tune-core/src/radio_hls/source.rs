//! `HlsSource` : un direct HLS rendu comme un flux continu (`Read`).
//!
//! Le lecteur radio ne voit qu'un `impl Read`, posé dans `ReadOnlySource`
//! comme la réponse `reqwest` d'une station Icecast. Derrière, un fil
//! relit la liste média au rythme de `EXT-X-TARGETDURATION`, télécharge les
//! segments neufs dans l'ordre de leur séquence, les démultiplexe et pousse
//! le flux élémentaire dans un canal borné.
//!
//! Deux niveaux d'erreur :
//!
//! * **passagère** (délai, 5xx, 404 sur un segment) : absorbée ici, par de
//!   nouveaux essais bornés. Un segment perdu pour de bon est sauté, et dit
//!   (`hls_segment_saute`). Le décodeur n'en voit rien ;
//! * **durable** (la liste échoue `echecs_liste_max` fois de suite, plusieurs
//!   segments d'affilée sont perdus) : `Read` rend une erreur, la boucle
//!   `'reconnect` du lecteur reprend la main et ouvre une source neuve,
//!   avec ses propres bornes (`MAX_RECONNECTS`).

use super::liste::{self, Cle, Liste, ListeMedia, Segment, TitreListe};
use super::ts::{self, Charge, Demux};
use crate::orchestrator::{RADIO_HLS_INTERDIT, RADIO_HLS_UNSUPPORTED};
use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Les bornes du chargeur. Les valeurs de production sont celles de
/// [`Reglages::default`] ; les témoins les raccourcissent.
#[derive(Debug, Clone)]
pub(crate) struct Reglages {
    /// Multiplie l'attente entre deux relectures de la liste (1 = la RFC).
    pub(crate) facteur_relecture: f64,
    /// Essais d'un même segment avant de le sauter.
    pub(crate) essais_segment: u32,
    /// Première attente entre deux essais ; elle double à chaque échec.
    pub(crate) attente_essai: Duration,
    /// Échecs consécutifs de la liste avant de rendre la main au lecteur.
    pub(crate) echecs_liste_max: u32,
    /// Segments perdus d'affilée avant de rendre la main au lecteur.
    pub(crate) segments_perdus_max: u32,
    /// Combien de segments avant la fin du direct on démarre (RFC 8216
    /// §6.3.3 : pas moins de trois durées cibles), et combien de segments
    /// d'avance le canal garde.
    pub(crate) segments_d_avance: usize,
    /// Délai total d'une requête (liste, segment ou clé).
    pub(crate) delai_requete: Duration,
    /// Combien de durées cibles un direct peut rester sans segment neuf
    /// avant d'être tenu pour figé : `Read` rend alors une erreur, et le
    /// lecteur se reconnecte (ou s'arrête, si l'auditeur est parti).
    pub(crate) cibles_immobiles: f64,
}

impl Default for Reglages {
    fn default() -> Self {
        Self {
            facteur_relecture: 1.0,
            essais_segment: 3,
            attente_essai: Duration::from_millis(500),
            echecs_liste_max: 5,
            segments_perdus_max: 3,
            segments_d_avance: 3,
            delai_requete: Duration::from_secs(15),
            cibles_immobiles: 4.0,
        }
    }
}

/// Ce que l'ouverture a établi, pour le lecteur et le chemin du signal.
#[derive(Debug, Clone)]
pub(crate) struct InfosHls {
    pub(crate) charge: Charge,
    /// `CODECS` de la variante retenue, quand la liste maîtresse le donne.
    pub(crate) codecs: Option<String>,
    /// Le débit mesuré sur les trames ADTS du premier segment.
    pub(crate) debit_kbps: Option<u32>,
}

/// Un morceau de flux élémentaire : un segment démultiplexé.
struct Bloc {
    octets: Vec<u8>,
    titre: Option<TitreListe>,
}

/// La fin du flux, vue du canal.
enum Fin {
    /// `EXT-X-ENDLIST` atteint, ou une rupture de format qui impose une
    /// session neuve : `Read` rend 0.
    Terminee,
    /// Une panne durable : `Read` rend cette erreur.
    Erreur(String),
}

type Message = Result<Bloc, Fin>;

/// Une erreur de requête : le statut HTTP quand il y en a un.
struct EchecHttp {
    statut: Option<u16>,
    texte: String,
}

impl EchecHttp {
    /// Un refus du diffuseur (403, 451) ne guérira pas en réessayant : le
    /// lecteur le dit et la relance automatique se désarme.
    fn en_erreur_radio(self) -> String {
        match self.statut {
            Some(403 | 451) => format!(
                "{RADIO_HLS_INTERDIT}: le diffuseur refuse l'accès à ce flux HLS (HTTP {}), \
                 souvent une restriction géographique",
                self.statut.unwrap_or(403)
            ),
            _ => format!("radio HLS: {}", self.texte),
        }
    }
}

fn refus(raison: impl std::fmt::Display) -> String {
    format!("{RADIO_HLS_UNSUPPORTED}: {raison}")
}

struct Http {
    client: reqwest::blocking::Client,
}

impl Http {
    fn new(reglages: &Reglages) -> Result<Self, String> {
        crate::http::client::blocking_builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(reglages.delai_requete)
            .build()
            .map(|client| Self { client })
            .map_err(|e| format!("radio HLS: client HTTP: {e}"))
    }

    /// GET borné en taille ; rend le corps et l'adresse finale.
    fn obtenir(&self, url: &str, plafond: u64) -> Result<(Vec<u8>, String), EchecHttp> {
        let reponse = self.client.get(url).send().map_err(|e| EchecHttp {
            statut: None,
            texte: crate::http::client::decrire_erreur_http(&e),
        })?;
        let statut = reponse.status();
        if !statut.is_success() {
            return Err(EchecHttp {
                statut: Some(statut.as_u16()),
                texte: format!("HTTP {statut}"),
            });
        }
        let finale = reponse.url().to_string();
        let mut corps = Vec::new();
        reponse
            .take(plafond)
            .read_to_end(&mut corps)
            .map_err(|e| EchecHttp {
                statut: None,
                texte: format!("lecture du corps : {e}"),
            })?;
        Ok((corps, finale))
    }
}

const PLAFOND_LISTE: u64 = 2 * 1024 * 1024;
const PLAFOND_SEGMENT: u64 = 16 * 1024 * 1024;

/// Le chargeur : tout l'état qui passe d'une relecture à la suivante.
struct Chargeur {
    http: Http,
    reglages: Reglages,
    url_media: String,
    demux: Demux,
    charge: Charge,
    /// Le dernier segment rendu (ou sauté) : jamais rechargé.
    derniere: u64,
    /// La configuration ADTS (objet, fréquence, canaux) du flux en cours.
    config_adts: Option<(u8, u8, u8)>,
    cles: HashMap<String, [u8; 16]>,
    perdus_d_affilee: u32,
}

/// Le flux élémentaire d'un segment, et son titre.
enum Issue {
    Bloc(Bloc),
    /// Segment perdu pour de bon, sauté.
    Saute,
    Fin(Fin),
}

impl Chargeur {
    fn lire_liste(&self) -> Result<ListeMedia, String> {
        let (corps, finale) = self
            .http
            .obtenir(&self.url_media, PLAFOND_LISTE)
            .map_err(EchecHttp::en_erreur_radio)?;
        match liste::lire(&String::from_utf8_lossy(&corps), &finale)? {
            Liste::Media(m) => Ok(m),
            Liste::Maitresse { .. } => {
                Err("radio HLS: la liste média est devenue maîtresse".into())
            }
        }
    }

    fn cle(&mut self, uri: &str) -> Result<[u8; 16], EchecHttp> {
        if let Some(cle) = self.cles.get(uri) {
            return Ok(*cle);
        }
        let (corps, _) = self.http.obtenir(uri, 64)?;
        let cle: [u8; 16] = corps.as_slice().try_into().map_err(|_| EchecHttp {
            statut: None,
            texte: format!("clé AES-128 de {} octets au lieu de 16", corps.len()),
        })?;
        self.cles.insert(uri.to_string(), cle);
        Ok(cle)
    }

    /// Télécharge (et déchiffre) un segment, avec ses essais bornés.
    fn telecharger(&mut self, segment: &Segment) -> Result<Vec<u8>, EchecHttp> {
        let mut attente = self.reglages.attente_essai;
        let mut essai = 1;
        loop {
            let resultat =
                self.http
                    .obtenir(&segment.uri, PLAFOND_SEGMENT)
                    .and_then(|(octets, _)| match &segment.cle {
                        Cle::Aes128 { uri, iv } => {
                            let cle = self.cle(uri)?;
                            let iv =
                                iv.unwrap_or_else(|| (u128::from(segment.sequence)).to_be_bytes());
                            dechiffrer_aes128(octets, &cle, &iv).map_err(|texte| EchecHttp {
                                statut: None,
                                texte,
                            })
                        }
                        _ => Ok(octets),
                    });
            match resultat {
                Ok(octets) => return Ok(octets),
                Err(e) if essai >= self.reglages.essais_segment => return Err(e),
                Err(e) => {
                    debug!(sequence = segment.sequence, essai, erreur = %e.texte, "hls_segment_nouvel_essai");
                    std::thread::sleep(attente);
                    attente *= 2;
                    essai += 1;
                }
            }
        }
    }

    /// Un segment, du réseau au flux élémentaire.
    fn segment(&mut self, segment: &Segment) -> Issue {
        if let Cle::Autre(methode) = &segment.cle {
            return Issue::Fin(Fin::Erreur(refus(format!(
                "segments chiffrés en {methode} (DRM), que Tune ne lit pas"
            ))));
        }
        let octets = match self.telecharger(segment) {
            Ok(o) => o,
            Err(e) => {
                self.perdus_d_affilee += 1;
                warn!(
                    sequence = segment.sequence,
                    erreur = %e.texte,
                    d_affilee = self.perdus_d_affilee,
                    "hls_segment_saute"
                );
                if self.perdus_d_affilee > self.reglages.segments_perdus_max {
                    return Issue::Fin(Fin::Erreur(e.en_erreur_radio()));
                }
                return Issue::Saute;
            }
        };
        self.perdus_d_affilee = 0;
        if segment.discontinuite {
            // Nouveau codeur, nouveau programme : les PID et les compteurs
            // repartent de zéro.
            info!(sequence = segment.sequence, "hls_discontinuite");
            self.demux = Demux::default();
        }
        match elementaire(&mut self.demux, &octets) {
            Err(raison) => Issue::Fin(Fin::Erreur(refus(raison.0))),
            Ok((charge, flux, titre_id3)) => {
                if charge != self.charge {
                    warn!(avant = ?self.charge, apres = ?charge, "hls_charge_changee_session_neuve");
                    return Issue::Fin(Fin::Terminee);
                }
                let config = config_adts(&flux);
                if charge == Charge::Adts
                    && config.is_some()
                    && self.config_adts.is_some()
                    && config != self.config_adts
                {
                    // Le format change : l'en-tête WAV déjà servi ne
                    // correspondrait plus. La boucle du lecteur ouvre une
                    // session neuve (garde `expected_format`).
                    warn!(avant = ?self.config_adts, apres = ?config, "hls_format_change_session_neuve");
                    return Issue::Fin(Fin::Terminee);
                }
                if config.is_some() {
                    self.config_adts = config;
                }
                Issue::Bloc(Bloc {
                    octets: flux,
                    titre: segment.titre.clone().or(titre_id3),
                })
            }
        }
    }

    /// Le fil : relire, charger les segments neufs, attendre, recommencer.
    fn tourner(
        mut self,
        mut liste: Option<ListeMedia>,
        tx: SyncSender<Message>,
        arret: Arc<AtomicBool>,
    ) {
        let mut echecs = 0u32;
        let mut cible = 6.0f64;
        let mut dernier_neuf = Instant::now();
        loop {
            if arret.load(Ordering::Relaxed) {
                return;
            }
            let debut = Instant::now();
            let media = match liste.take().map(Ok).unwrap_or_else(|| self.lire_liste()) {
                Ok(m) => {
                    echecs = 0;
                    m
                }
                Err(e) => {
                    echecs += 1;
                    warn!(echecs, erreur = %e, "hls_liste_illisible");
                    if echecs >= self.reglages.echecs_liste_max || e.starts_with(RADIO_HLS_INTERDIT)
                    {
                        let _ = tx.send(Err(Fin::Erreur(e)));
                        return;
                    }
                    attendre(
                        &arret,
                        self.reglages.attente_essai * 2u32.pow(echecs.min(5)),
                    );
                    continue;
                }
            };
            cible = if media.cible > 0.0 {
                media.cible
            } else {
                cible
            };
            if media.carte.is_some() {
                let _ = tx.send(Err(Fin::Erreur(refus("segments fMP4/CMAF (EXT-X-MAP)"))));
                return;
            }

            let premiere = media.segments.first().map(|s| s.sequence);
            let derniere_de_la_liste = media.segments.last().map(|s| s.sequence);
            if let (Some(p), Some(d)) = (premiere, derniere_de_la_liste) {
                if d + 1 < self.derniere {
                    // La séquence est repartie en arrière (codeur relancé) :
                    // on se recale sur le direct.
                    warn!(
                        avant = self.derniere,
                        maintenant = d,
                        "hls_sequence_repartie"
                    );
                    self.derniere = d.saturating_sub(self.reglages.segments_d_avance as u64);
                } else if p > self.derniere + 1 {
                    warn!(
                        perdus = p - self.derniere - 1,
                        de = self.derniere + 1,
                        a = p - 1,
                        "hls_segment_perdu"
                    );
                }
            }
            let neufs: Vec<Segment> = media
                .segments
                .iter()
                .filter(|s| s.sequence > self.derniere)
                .cloned()
                .collect();
            for segment in &neufs {
                if arret.load(Ordering::Relaxed) {
                    return;
                }
                match self.segment(segment) {
                    Issue::Bloc(bloc) => {
                        if tx.send(Ok(bloc)).is_err() {
                            return;
                        }
                    }
                    Issue::Saute => {}
                    Issue::Fin(fin) => {
                        let _ = tx.send(Err(fin));
                        return;
                    }
                }
                self.derniere = segment.sequence;
            }
            if !neufs.is_empty() {
                dernier_neuf = Instant::now();
            } else if !media.fin
                && dernier_neuf.elapsed().as_secs_f64()
                    > cible * self.reglages.cibles_immobiles * self.reglages.facteur_relecture
            {
                let _ = tx.send(Err(Fin::Erreur(format!(
                    "radio HLS: le direct ne publie plus de segment depuis {} s",
                    dernier_neuf.elapsed().as_secs()
                ))));
                return;
            }
            if media.fin {
                debug!("hls_fin_de_liste");
                let _ = tx.send(Err(Fin::Terminee));
                return;
            }
            // RFC 8216 §6.3.4 : relire après une durée cible ; la moitié
            // seulement quand la liste n'a pas bougé.
            let pas = if neufs.is_empty() { cible / 2.0 } else { cible };
            let attente = Duration::from_secs_f64((pas * self.reglages.facteur_relecture).max(0.0));
            attendre(&arret, attente.saturating_sub(debut.elapsed()));
        }
    }
}

/// Attend `duree` par petits pas, pour s'arrêter vite quand la source tombe.
fn attendre(arret: &AtomicBool, duree: Duration) {
    let fin = Instant::now() + duree;
    while !arret.load(Ordering::Relaxed) {
        let reste = fin.saturating_duration_since(Instant::now());
        if reste.is_zero() {
            return;
        }
        std::thread::sleep(reste.min(Duration::from_millis(100)));
    }
}

/// AES-128-CBC sur le segment entier, bourrage PKCS#7 (RFC 8216 §4.3.2.4).
fn dechiffrer_aes128(
    mut octets: Vec<u8>,
    cle: &[u8; 16],
    iv: &[u8; 16],
) -> Result<Vec<u8>, String> {
    use cbc::cipher::{BlockDecryptMut, KeyIvInit};
    let n = cbc::Decryptor::<aes::Aes128>::new(cle.into(), iv.into())
        .decrypt_padded_mut::<cbc::cipher::block_padding::Pkcs7>(&mut octets)
        .map_err(|_| "segment AES-128 indéchiffrable (clé ou IV faux)".to_string())?
        .len();
    octets.truncate(n);
    Ok(octets)
}

/// La configuration du premier en-tête ADTS : (objet, indice de fréquence,
/// canaux).
fn config_adts(flux: &[u8]) -> Option<(u8, u8, u8)> {
    let i = flux
        .windows(2)
        .position(|w| w[0] == 0xFF && w[1] & 0xF6 == 0xF0)?;
    let h = flux.get(i..i + 4)?;
    Some((
        (h[2] >> 6) + 1,
        (h[2] >> 2) & 0x0F,
        ((h[2] & 0x01) << 2) | (h[3] >> 6),
    ))
}

/// Le flux élémentaire d'un segment, quel que soit son emballage.
fn elementaire(
    demux: &mut Demux,
    octets: &[u8],
) -> Result<(Charge, Vec<u8>, Option<TitreListe>), ts::NonPrisEnCharge> {
    if ts::est_du_fmp4(octets) {
        return Err(ts::NonPrisEnCharge("segments fMP4/CMAF".into()));
    }
    if octets.first() == Some(&0x47) {
        let mut sortie = Vec::with_capacity(octets.len());
        demux.pousser(octets, &mut sortie)?;
        demux.finir_id3();
        let charge = demux
            .charge()
            .ok_or_else(|| ts::NonPrisEnCharge("segment MPEG-TS sans PAT/PMT lisible".into()))?;
        return Ok((charge, sortie, demux.titre_id3.take()));
    }
    let (charge, flux, titre) = ts::empaquete(octets)?;
    Ok((charge, flux.to_vec(), titre))
}

/// Un direct HLS, vu comme un flux continu.
pub(crate) struct HlsSource {
    /// Sous `Mutex` pour être `Sync`, comme symphonia l'exige d'une source
    /// (`MediaSource`) ; `read` y accède par `get_mut`, sans verrou.
    rx: std::sync::Mutex<Receiver<Message>>,
    courant: Vec<u8>,
    pos: usize,
    pub(crate) infos: InfosHls,
    /// L'adresse de la station : la clé du titre en cours.
    station: String,
    arret: Arc<AtomicBool>,
}

impl HlsSource {
    /// Ouvre la station `url` : liste maîtresse et choix de variante, liste
    /// média, premier segment — tout ce qui peut refuser le fait ici, avant
    /// que le lecteur n'annonce un format. `deja_lue` porte la liste quand
    /// l'appelant l'a déjà reçue (URL sans extension, servie en type HLS) :
    /// (adresse finale, texte).
    pub(crate) fn ouvrir(
        url: &str,
        deja_lue: Option<(String, String)>,
        reglages: Reglages,
    ) -> Result<Self, String> {
        let http = Http::new(&reglages)?;
        let (finale, texte) = match deja_lue {
            Some(lue) => lue,
            None => {
                let (corps, finale) = http
                    .obtenir(url, PLAFOND_LISTE)
                    .map_err(EchecHttp::en_erreur_radio)?;
                (finale, String::from_utf8_lossy(&corps).into_owned())
            }
        };
        match liste::lire(&texte, &finale).map_err(|e| format!("radio HLS: {e}"))? {
            Liste::Media(media) => {
                Self::ouvrir_media(url, http, reglages, finale, Some(media), None)
            }
            Liste::Maitresse {
                variantes,
                pistes_audio,
            } => {
                let ordre = liste::ordre_des_variantes(&variantes, liste::plafond_regle());
                if ordre.is_empty() {
                    let codecs: Vec<&str> = variantes
                        .iter()
                        .filter_map(|v| v.codecs.as_deref())
                        .collect();
                    return Err(refus(format!(
                        "aucune variante en AAC ou MP3 (codecs annoncés : {})",
                        codecs.join(" ; ")
                    )));
                }
                let mut derniere_erreur = String::new();
                let mut http = Some(http);
                for (rang, variante) in ordre.iter().enumerate() {
                    let client = match http.take() {
                        Some(h) => h,
                        None => Http::new(&reglages)?,
                    };
                    let media = liste::liste_audio_de(variante, &pistes_audio);
                    match Self::ouvrir_media(
                        url,
                        client,
                        reglages.clone(),
                        media,
                        None,
                        variante.codecs.clone(),
                    ) {
                        Ok(source) => {
                            info!(
                                bande = variante.bande,
                                codecs = ?variante.codecs,
                                rang,
                                variantes = ordre.len(),
                                "hls_variante_retenue"
                            );
                            return Ok(source);
                        }
                        Err(e) => {
                            warn!(bande = variante.bande, rang, erreur = %e, "hls_variante_refusee_repli");
                            derniere_erreur = e;
                        }
                    }
                }
                Err(derniere_erreur)
            }
        }
    }

    fn ouvrir_media(
        station: &str,
        http: Http,
        reglages: Reglages,
        url_media: String,
        deja_lue: Option<ListeMedia>,
        codecs: Option<String>,
    ) -> Result<Self, String> {
        let mut chargeur = Chargeur {
            http,
            url_media,
            demux: Demux::default(),
            charge: Charge::Adts,
            derniere: 0,
            config_adts: None,
            cles: HashMap::new(),
            perdus_d_affilee: 0,
            reglages,
        };
        let media = match deja_lue {
            Some(m) => m,
            None => chargeur.lire_liste()?,
        };
        if media.carte.is_some() {
            return Err(refus("segments fMP4/CMAF (EXT-X-MAP)"));
        }
        if media.plages {
            return Err(refus("segments en plages d'octets (EXT-X-BYTERANGE)"));
        }
        // Le direct démarre à quelques segments de la fin ; une liste close
        // (`EXT-X-ENDLIST`) se lit depuis le début.
        let depart = if media.fin {
            0
        } else {
            media
                .segments
                .len()
                .saturating_sub(chargeur.reglages.segments_d_avance)
        };
        let premier = media
            .segments
            .get(depart)
            .cloned()
            .ok_or("radio HLS: la liste média ne porte aucun segment")?;
        if let Cle::Autre(methode) = &premier.cle {
            return Err(refus(format!(
                "segments chiffrés en {methode} (DRM), que Tune ne lit pas"
            )));
        }
        let octets = chargeur
            .telecharger(&premier)
            .map_err(EchecHttp::en_erreur_radio)?;
        let (charge, flux, titre_id3) =
            elementaire(&mut chargeur.demux, &octets).map_err(|e| refus(e.0))?;
        chargeur.charge = charge;
        chargeur.config_adts = config_adts(&flux);
        chargeur.derniere = premier.sequence;
        let infos = InfosHls {
            charge,
            codecs,
            debit_kbps: (charge == Charge::Adts)
                .then(|| ts::debit_adts_kbps(&flux))
                .flatten(),
        };
        info!(
            charge = ?infos.charge,
            debit_kbps = ?infos.debit_kbps,
            sequence = premier.sequence,
            cible = media.cible,
            segments = media.segments.len(),
            "hls_ouvert"
        );

        let (tx, rx) = sync_channel::<Message>(chargeur.reglages.segments_d_avance.max(1));
        let arret = Arc::new(AtomicBool::new(false));
        let arret_du_fil = arret.clone();
        // Les segments qui suivent le premier, dans la liste déjà lue.
        let reste = ListeMedia {
            segments: media.segments[depart + 1..].to_vec(),
            ..media
        };
        std::thread::Builder::new()
            .name("radio-hls".into())
            .spawn(move || chargeur.tourner(Some(reste), tx, arret_du_fil))
            .map_err(|e| format!("radio HLS: fil de chargement: {e}"))?;

        let mut source = Self {
            rx: std::sync::Mutex::new(rx),
            courant: Vec::new(),
            pos: 0,
            infos,
            station: station.to_string(),
            arret,
        };
        source.entamer(Bloc {
            octets: flux,
            titre: premier.titre.or(titre_id3),
        });
        Ok(source)
    }

    /// Un bloc commence à être lu : c'est maintenant qu'il joue, son titre
    /// devient le titre en cours.
    fn entamer(&mut self, bloc: Bloc) {
        if let Some(titre) = bloc.titre {
            super::publier_titre(&self.station, titre);
        }
        self.courant = bloc.octets;
        self.pos = 0;
    }
}

impl Read for HlsSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.pos < self.courant.len() {
                let n = buf.len().min(self.courant.len() - self.pos);
                buf[..n].copy_from_slice(&self.courant[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            let rx = self.rx.get_mut().unwrap_or_else(|e| e.into_inner());
            match rx.recv() {
                Ok(Ok(bloc)) => self.entamer(bloc),
                Ok(Err(Fin::Terminee)) | Err(_) => return Ok(0),
                Ok(Err(Fin::Erreur(e))) => return Err(std::io::Error::other(e)),
            }
        }
    }
}

impl Drop for HlsSource {
    fn drop(&mut self) {
        self.arret.store(true, Ordering::Relaxed);
        super::oublier_titre(&self.station);
    }
}
