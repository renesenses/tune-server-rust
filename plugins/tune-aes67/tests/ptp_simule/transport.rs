use crate::horloge_virtuelle::HorlogeVirtuelle;
use crate::sync::mpsc::Attente;
use crate::systeme::{
    cell::RefCell,
    io,
    net::SocketAddrV4,
    sync::{Arc, atomic::AtomicBool, mpsc::Sender},
    thread::JoinHandle,
};
use statime::{Clock, time::Time};
pub use tune_aes67::ptp::transport::{Canal, ConfigReseauPtp, Recu};

thread_local! {
    // demarrer() prend les deux extrémités sur le fil appelant avant son spawn.
    pub(crate) static RECEPTION: RefCell<Option<Sender<Attente<Recu>>>> = const { RefCell::new(None) };
    pub(crate) static EMISSION: RefCell<Option<Sender<Vec<u8>>>> = const { RefCell::new(None) };
}

pub struct TransportUdp {
    cfg: ConfigReseauPtp,
    vers_maitre: Sender<Vec<u8>>,
}
impl TransportUdp {
    pub fn ouvrir(cfg: ConfigReseauPtp) -> io::Result<Self> {
        Ok(Self {
            cfg,
            vers_maitre: EMISSION.with(|e| e.borrow_mut().take().expect("maître du banc")),
        })
    }
    pub fn adresses_locales(&self) -> io::Result<(SocketAddrV4, SocketAddrV4)> {
        Ok((self.cfg.liaison_evenement, self.cfg.liaison_generale))
    }
    pub fn lancer_la_reception(
        &self,
        _: HorlogeVirtuelle,
        _: (),
        _: Arc<AtomicBool>,
    ) -> io::Result<Vec<JoinHandle<()>>> {
        Ok(Vec::new())
    }
    pub fn envoyer_evenement(
        &self,
        donnees: &[u8],
        horloge: &HorlogeVirtuelle,
    ) -> io::Result<Time> {
        self.vers_maitre
            .send(donnees.to_vec())
            .map_err(io::Error::other)?;
        Ok(horloge.now())
    }
    pub fn envoyer_general(&self, _: &[u8]) -> io::Result<()> {
        Ok(())
    }
}
