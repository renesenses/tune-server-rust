//! Qui a ouvert la transaction en cours sur la connexion d'écriture SQLite, et
//! qui attend qu'elle se ferme.
//!
//! ## Le défaut
//!
//! SQLite n'a qu'UNE connexion d'écriture pour tout le serveur. Un lot de scan
//! y ouvre `BEGIN IMMEDIATE` et la garde ouverte à travers des centaines
//! d'appels, jusqu'à son `COMMIT`. Le verrou de la connexion, lui, est relâché
//! entre deux appels. Tout autre écrivain qui passait dans l'intervalle —
//! favori, note, playlist, édition manuelle, enrichissement, ReplayGain —
//! écrivait donc DANS la transaction du lot :
//!
//! - un `ROLLBACK` du lot (son `COMMIT` refusé, ou un lot laissé ouvert puis
//!   annulé par le suivant) emportait son écriture ;
//! - sa relecture par le pool de lecture, fait d'autres connexions, ne voyait
//!   pas ce qu'il venait d'écrire tant que le lot n'avait pas validé ;
//! - un `write_tx` échouait net : « cannot start a transaction within a
//!   transaction ».
//!
//! La porte `sqlite_write_gate` de `tune-server` ne protégeait que ceux qui la
//! prenaient (la file d'attente, puis le surveillant, #5072).
//!
//! ## Le correctif, au niveau de la connexion
//!
//! Le fil qui laisse une transaction OUVERTE en rendant la connexion en est
//! le propriétaire : c'est celui qui vient d'exécuter le `BEGIN`. Tant que la
//! transaction reste ouverte, une prise de la connexion par un AUTRE fil
//! attend qu'elle se ferme (voir `VerrouEcriture::lock`). Le propriétaire,
//! lui, passe : ses propres écritures sont le lot.
//!
//! Attendre la fin d'un lot entier coûterait trop cher : un lot relit les
//! balises de ses fichiers (métadonnées étendues), et sur un partage réseau
//! lent cela se compte en dizaines de secondes. Le lot CÈDE donc la place :
//! entre deux fichiers, s'il voit un écrivain en attente, il valide ce qu'il
//! a fait, laisse passer les écrivains, puis rouvre sa transaction (voir
//! `VerrouEcriture::ceder_aux_ecrivains`). L'attente d'une écriture d'API
//! est ainsi bornée par le travail d'UN fichier, pas d'un lot.
use std::sync::{Condvar, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

/// Au-delà, un écrivain cesse d'attendre et écrit comme avant : dans la
/// transaction ouverte. Garde-fou contre une transaction orpheline (fil
/// propriétaire mort en la laissant ouverte) : sans lui, tous les écrivains
/// suivants attendraient à jamais.
pub const ATTENTE_MAX_FIN_DU_LOT: Duration = Duration::from_secs(30);

/// Après avoir validé pour céder la place, le propriétaire attend au plus ce
/// délai que les écrivains inscrits soient passés avant de rouvrir sa
/// transaction.
pub const CESSION_MAX: Duration = Duration::from_secs(1);

/// Réveil périodique d'un écrivain en attente, même sans signal.
const REVEIL: Duration = Duration::from_millis(50);

#[derive(Default)]
struct Etat {
    /// Le fil qui a laissé la transaction ouverte ; `None` hors transaction.
    proprio: Option<ThreadId>,
    /// Écrivains qui attendent la fermeture de la transaction.
    en_attente: usize,
}

/// L'état de la transaction ouverte sur UNE connexion d'écriture.
#[derive(Default)]
pub(crate) struct TransactionDuLot {
    etat: Mutex<Etat>,
    fermee: Condvar,
}

impl TransactionDuLot {
    fn etat(&self) -> std::sync::MutexGuard<'_, Etat> {
        self.etat.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// À appeler en RENDANT la connexion, avant de la relâcher : sous le
    /// verrou de la connexion, personne ne peut s'intercaler entre le `BEGIN`
    /// et l'inscription de son propriétaire.
    pub(crate) fn au_rendu(&self, autocommit: bool) {
        let mut e = self.etat();
        if autocommit {
            if e.proprio.take().is_some() {
                self.fermee.notify_all();
            }
        } else if e.proprio.is_none() {
            e.proprio = Some(std::thread::current().id());
        }
    }

    /// Une transaction ouverte par un AUTRE fil ?
    pub(crate) fn ouverte_par_un_autre(&self, autocommit: bool) -> bool {
        if autocommit {
            return false;
        }
        let e = self.etat();
        e.proprio.is_some_and(|p| p != std::thread::current().id())
    }

    /// Le fil courant tient-il la transaction ouverte ?
    pub(crate) fn est_au_fil_courant(&self) -> bool {
        self.etat().proprio == Some(std::thread::current().id())
    }

    pub(crate) fn inscrire(&self) {
        self.etat().en_attente += 1;
    }

    pub(crate) fn desinscrire(&self) {
        let mut e = self.etat();
        e.en_attente = e.en_attente.saturating_sub(1);
    }

    pub(crate) fn en_attente(&self) -> usize {
        self.etat().en_attente
    }

    /// Dormir jusqu'à la fermeture de la transaction, ou jusqu'à `limite`.
    pub(crate) fn attendre_la_fermeture(&self, limite: Instant) {
        let mut e = self.etat();
        while e.proprio.is_some() {
            let reste = limite.saturating_duration_since(Instant::now());
            if reste.is_zero() {
                return;
            }
            e = self
                .fermee
                .wait_timeout(e, reste.min(REVEIL))
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }
}
