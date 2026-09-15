//! Lecture synchrone pour le producteur audio, I/O HTTP annulables (#4220).
//!
//! Un Read de reqwest::blocking avec timeout(None) peut retenir le fil ET
//! son backend pendant Stop. Ici le meme fil pilote une requete asynchrone :
//! l'attente des en-tetes et de chaque bloc observe l'arret, sans borner la
//! duree totale d'une piste ni abandonner une connexion simplement lente.
use std::future::Future;
use std::io::{self, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const PAS_ANNULATION: Duration = Duration::from_millis(25);

fn interrompue() -> io::Error {
    // Interrupted est une erreur transitoire que Read::read_exact/read_to_end
    // retentent : Stop doit au contraire terminer aussi ces consommateurs.
    io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "lecture HTTP locale annulee",
    )
}

async fn attendre<T>(
    arret: &AtomicBool,
    travail: impl Future<Output = Result<T, reqwest::Error>>,
) -> io::Result<T> {
    tokio::pin!(travail);
    loop {
        if arret.load(Ordering::SeqCst) {
            return Err(interrompue());
        }
        tokio::select! {
            resultat = &mut travail => {
                if arret.load(Ordering::SeqCst) {
                    return Err(interrompue());
                }
                return resultat.map_err(io::Error::other);
            }
            _ = tokio::time::sleep(PAS_ANNULATION) => {}
        }
    }
}

/// La destruction ne doit pas attendre un travail DNS bloquant deja lance.
/// Les taches HTTP asynchrones sont annulees ; aucun second fil producteur
/// ni tampon de telechargement sans borne n'est detache.
struct Moteur(Option<tokio::runtime::Runtime>);
impl Drop for Moteur {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}

pub(super) struct LecteurHttpAnnulable {
    // Ordre de destruction : le corps HTTP est rendu avant son executant.
    response: reqwest::Response,
    tampon: Vec<u8>,
    position: usize,
    arret: Arc<AtomicBool>,
    moteur: Moteur,
}

impl LecteurHttpAnnulable {
    pub(super) fn ouvrir(url: &str, arret: Arc<AtomicBool>) -> io::Result<Self> {
        let moteur = Moteur(Some(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?,
        ));
        let response = moteur.0.as_ref().unwrap().block_on(async {
            let client = crate::http::client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()
                .map_err(io::Error::other)?;
            attendre(&arret, client.get(url).send()).await
        })?;
        Ok(Self {
            response,
            tampon: Vec::new(),
            position: 0,
            arret,
            moteur,
        })
    }

    pub(super) fn status(&self) -> reqwest::StatusCode {
        self.response.status()
    }
}

impl Read for LecteurHttpAnnulable {
    fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        if destination.is_empty() {
            return Ok(0);
        }
        loop {
            if self.arret.load(Ordering::SeqCst) {
                return Err(interrompue());
            }
            if self.position < self.tampon.len() {
                let n = destination.len().min(self.tampon.len() - self.position);
                destination[..n].copy_from_slice(&self.tampon[self.position..self.position + n]);
                self.position += n;
                return Ok(n);
            }
            let bloc = self
                .moteur
                .0
                .as_ref()
                .unwrap()
                .block_on(attendre(&self.arret, self.response.chunk()))?;
            match bloc {
                Some(bloc) => {
                    self.tampon = bloc.to_vec();
                    self.position = 0;
                }
                None => return Ok(0),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outputs::traits::OutputTarget;
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread::JoinHandle;

    struct ServeurMuet {
        url: String,
        pret: mpsc::Receiver<()>,
        liberer: Option<mpsc::Sender<()>>,
        fil: Option<JoinHandle<()>>,
    }

    impl ServeurMuet {
        fn neuf(corps_partiel: bool, statut: u16) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/audio", listener.local_addr().unwrap());
            listener.set_nonblocking(true).unwrap();
            let (pret_tx, pret) = mpsc::channel();
            let (liberer, attente) = mpsc::channel();
            let fil = std::thread::spawn(move || {
                let limite = std::time::Instant::now() + Duration::from_secs(5);
                let mut socket = loop {
                    if attente.try_recv().is_ok() || std::time::Instant::now() >= limite {
                        return;
                    }
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5))
                        }
                        Err(e) => panic!("accept: {e}"),
                    }
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut requete = Vec::new();
                let mut bloc = [0u8; 1024];
                while !requete.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut bloc) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => requete.extend_from_slice(&bloc[..n]),
                    }
                }
                let entete = format!(
                    "HTTP/1.1 {statut} Test\r\nContent-Length: 8\r\nConnection: close\r\n\r\n"
                );
                if corps_partiel {
                    let _ = socket.write_all(entete.as_bytes());
                    let _ = socket.write_all(b"abcd");
                }
                let _ = pret_tx.send(());
                let _ = attente.recv_timeout(Duration::from_secs(10));
                // Le client annule peut deja avoir ferme le socket : c'est attendu.
                if !corps_partiel {
                    let _ = socket.write_all(entete.as_bytes());
                    let _ = socket.write_all(b"abcd");
                }
                let _ = socket.write_all(b"efgh");
            });
            Self {
                url,
                pret,
                liberer: Some(liberer),
                fil: Some(fil),
            }
        }
        fn pret(&self) {
            self.pret.recv_timeout(Duration::from_secs(3)).unwrap();
        }
        fn finir(&mut self) {
            if let Some(tx) = self.liberer.take() {
                let _ = tx.send(());
            }
            if let Some(fil) = self.fil.take() {
                fil.join().unwrap();
            }
        }
    }
    impl Drop for ServeurMuet {
        fn drop(&mut self) {
            self.finir();
        }
    }

    #[test]
    fn i4220_les_deux_ouvertures_de_production_sont_annulables() {
        let source = include_str!("../local.rs");
        assert_eq!(
            source.matches("LecteurHttpAnnulable::ouvrir(").count(),
            2,
            "piste initiale ET gapless doivent utiliser le lecteur annulable"
        );
        assert!(
            !source.contains("blocking_builder()"),
            "aucun acces HTTP bloquant divergent"
        );
    }

    #[test]
    fn i4220_annule_une_attente_deja_entamee_sans_attendre_sa_reponse() {
        let arret = Arc::new(AtomicBool::new(false));
        let flag = arret.clone();
        let (entre_tx, entre) = mpsc::channel();
        let (fin_tx, fin) = mpsc::channel();
        let (liberer, attente) = tokio::sync::oneshot::channel();
        let fil = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let resultat = runtime.block_on(attendre(&flag, async {
                entre_tx.send(()).unwrap();
                let _ = attente.await;
                Ok::<_, reqwest::Error>(7)
            }));
            let _ = fin_tx.send(resultat.map_err(|e| e.kind()));
        });
        entre.recv_timeout(Duration::from_secs(3)).unwrap();
        arret.store(true, Ordering::SeqCst);
        let resultat = fin.recv_timeout(Duration::from_secs(1));
        let _ = liberer.send(());
        fil.join().unwrap();
        assert_eq!(
            resultat.unwrap(),
            Err(io::ErrorKind::ConnectionAborted),
            "Stop doit annuler la requete deja en attente"
        );
    }

    #[test]
    fn i4220_annule_le_corps_http_apres_avoir_lu_des_octets() {
        let mut serveur = ServeurMuet::neuf(true, 200);
        let arret = Arc::new(AtomicBool::new(false));
        let flag = arret.clone();
        let url = serveur.url.clone();
        let (lu_tx, lu) = mpsc::channel();
        let (fin_tx, fin) = mpsc::channel();
        let fil = std::thread::spawn(move || {
            let mut reader = LecteurHttpAnnulable::ouvrir(&url, flag).unwrap();
            let mut prefixe = [0; 4];
            reader.read_exact(&mut prefixe).unwrap();
            lu_tx.send(prefixe).unwrap();
            let _ = fin_tx.send(reader.read_to_end(&mut Vec::new()).map_err(|e| e.kind()));
        });
        serveur.pret();
        assert_eq!(lu.recv_timeout(Duration::from_secs(3)).unwrap(), *b"abcd");
        arret.store(true, Ordering::SeqCst);
        let resultat = fin.recv_timeout(Duration::from_secs(1));
        serveur.finir();
        fil.join().unwrap();
        assert_eq!(
            resultat.unwrap(),
            Err(io::ErrorKind::ConnectionAborted),
            "un corps muet ne doit pas retenir le backend pendant Stop"
        );
    }

    #[test]
    fn i4220_un_flux_lent_garde_ses_octets_et_son_vrai_eof() {
        let mut serveur = ServeurMuet::neuf(true, 200);
        let url = serveur.url.clone();
        let (lu_tx, lu) = mpsc::channel();
        let fil = std::thread::spawn(move || {
            let mut reader =
                LecteurHttpAnnulable::ouvrir(&url, Arc::new(AtomicBool::new(false))).unwrap();
            assert_eq!(reader.status(), reqwest::StatusCode::OK);
            let mut sortie = [0u8; 8];
            reader.read_exact(&mut sortie[..2]).unwrap();
            reader.read_exact(&mut sortie[2..4]).unwrap();
            lu_tx.send(()).unwrap();
            reader.read_exact(&mut sortie[4..]).unwrap();
            assert_eq!(reader.read(&mut [0; 1]).unwrap(), 0);
            sortie
        });
        serveur.pret();
        lu.recv_timeout(Duration::from_secs(3)).unwrap();
        std::thread::sleep(PAS_ANNULATION * 5);
        serveur.finir();
        assert_eq!(fil.join().unwrap(), *b"abcdefgh");
    }

    #[test]
    fn i4220_le_refus_http_reste_un_refus_http() {
        let mut serveur = ServeurMuet::neuf(true, 403);
        let lecteur =
            LecteurHttpAnnulable::ouvrir(&serveur.url, Arc::new(AtomicBool::new(false))).unwrap();
        assert_eq!(lecteur.status(), reqwest::StatusCode::FORBIDDEN);
        drop(lecteur);
        serveur.finir();
    }

    #[tokio::test]
    async fn i4220_stop_libere_le_fil_muet_et_sa_reponse_tardive_ne_stoppe_pas_la_suivante() {
        let sortie = crate::outputs::local::LocalOutput::new("fixture-sans-carte-audio".into());
        let mut premier = ServeurMuet::neuf(false, 200);
        let mut suivant = ServeurMuet::neuf(false, 200);
        sortie
            .play_url(&premier.url, "audio/wav", None, None)
            .await
            .unwrap();
        premier.pret();
        let ancien_fil = sortie
            .sentinelle_du_fil
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .clone();
        sortie.stop().await.unwrap();
        let ancien_libere_par_stop = !ancien_fil.load(Ordering::SeqCst);

        sortie
            .play_url(&suivant.url, "audio/wav", None, None)
            .await
            .unwrap();
        suivant.pret();
        premier.finir();
        let fin_ancien = tokio::time::timeout(Duration::from_secs(2), async {
            while ancien_fil.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        let etat_suivant = sortie.get_status().await.unwrap();
        sortie.stop().await.unwrap();
        suivant.finir();
        assert!(
            ancien_libere_par_stop,
            "Stop a detache un fil encore bloque sur les en-tetes HTTP"
        );
        assert!(fin_ancien.is_ok(), "le premier fil doit etre termine");
        assert_eq!(
            etat_suivant.state,
            crate::outputs::traits::TransportState::Playing,
            "la reponse HTTP perimee ne doit pas eteindre la nouvelle tentative"
        );
    }
    #[test]
    fn i4220_une_annulation_pendant_read_n_est_pas_une_fin_naturelle() {
        use crate::outputs::local::{
            BoucleProducteur, CompteursDePiste, FinDeBoucle, RoleDeLaBoucle,
            empreinte_du_puits_r1::{DspAuRepos, etage},
        };
        use crate::outputs::traits::{CaptureOutput, FormatOuvert};
        use std::sync::atomic::AtomicU64;
        struct AnnulePendantRead<'a>(&'a AtomicBool);
        impl Read for AnnulePendantRead<'_> {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                self.0.store(true, Ordering::SeqCst);
                Err(interrompue())
            }
        }
        for role in [
            RoleDeLaBoucle::PisteInitiale,
            RoleDeLaBoucle::PisteEnchainee,
        ] {
            let arret = AtomicBool::new(false);
            let disparu = AtomicBool::new(false);
            let position = AtomicU64::new(0);
            let erreur = std::sync::Mutex::new(None);
            let (_tx, rx) = mpsc::channel();
            let producteur = BoucleProducteur {
                role,
                backend: "capture",
                device_name: "4220",
                cle_de_flux: None,
                stop_rx: &rx,
                force_silent: &arret,
                device_gone: &disparu,
                position_ms: &position,
                open_failure: &erreur,
                debut_du_flux: std::time::Instant::now(),
            };
            let dsp = DspAuRepos::neuf();
            let mut conversion = etage(&dsp, Vec::new(), 44100, 2, 16, 44100, 2);
            let mut puits = CaptureOutput::ouvert(FormatOuvert::new(44100, 2));
            let mut compteurs = CompteursDePiste {
                total_bytes_read: 0,
                total_frames_fed: 0,
                seek_offset: 0,
                skip_bytes: 0,
                skipped_bytes: 0,
                premiere_donnee_journalisee: false,
            };
            let mut rappels = 0;
            let fin = producteur.tourner(
                &mut AnnulePendantRead(&arret),
                &mut [0; 16],
                &mut conversion,
                &mut puits,
                &mut |_, _, _| false,
                &mut compteurs,
                &mut |_| {
                    rappels += 1;
                    true
                },
            );
            assert!(matches!(fin, FinDeBoucle::Interrompue));
            assert_eq!(rappels, 0);
            assert_eq!(puits.mots(), 0);
            assert!(erreur.lock().unwrap().is_none());
        }
    }
}
