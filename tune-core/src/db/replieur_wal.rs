//! Replier le WAL hors de la connexion d'écriture.
//!
//! ## Le défaut
//!
//! En mode WAL, SQLite replie le journal dans la base (point de contrôle
//! automatique) au `COMMIT` qui le fait passer au-delà de mille pages, et il
//! le fait SUR la connexion qui valide. Chez Tune, c'est l'unique connexion
//! d'écriture, tenue par son verrou pendant tout ce repli : copie des pages,
//! puis deux `fsync`. Sur un disque lent (machine virtuelle sur une box, carte
//! SD), le `COMMIT` d'un lot de scan a ainsi tenu la connexion jusqu'à 5,4 s
//! (`ecriture_sqlite_detention_longue`, retour de terrain). Pendant ce
//! temps, même les lectures fortes de la file et des zones attendaient.
//!
//! Mesure sur une base synthétique (`replieur_wal_tests.rs`, 40 lots de 500
//! pistes, trois tours) : le plus long `COMMIT` passe de 206–866 ms à
//! 35–75 ms, et la pire attente d'une lecture forte de 204–865 ms à 40–74 ms.
//! Découper le lot en transactions plus courtes, essayé d'abord, ne gagnait
//! rien : chaque petit `COMMIT` déclenchait à son tour un repli complet.
//!
//! ## Le correctif
//!
//! La connexion d'écriture ne replie plus (`wal_autocheckpoint=0`). Un fil à
//! part, avec SA connexion, replie en mode `PASSIVE` toutes les
//! [`PERIODE`] : ce mode n'attend ni les lecteurs ni l'écrivain, et
//! l'écrivain continue d'ajouter au journal pendant qu'il copie.
//!
//! Si la connexion du replieur ne s'ouvre pas, rien ne change : la connexion
//! d'écriture garde son repli automatique.
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};

/// Intervalle entre deux replis. Deux secondes d'écritures de scan restent
/// très en deçà des mille pages qui déclenchaient le repli automatique.
pub(crate) const PERIODE: Duration = Duration::from_secs(2);

/// Au-delà, un repli est dit au journal : c'est le temps que la connexion
/// d'écriture aurait été tenue.
const REPLI_LONG: Duration = Duration::from_secs(1);

/// Tant qu'une copie de ce jeton vit (une par `SqliteDb` et ses clones), le
/// replieur tourne ; il s'arrête au plus une [`PERIODE`] après la dernière.
pub(crate) type Vigie = Arc<()>;

/// Ouvre la connexion du replieur, coupe le repli automatique de la connexion
/// d'écriture, et lance le fil. `None` : rien n'a changé.
pub(crate) fn armer(ecriture: &Connection, chemin: &str) -> Option<Vigie> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let replieur = match Connection::open_with_flags(chemin, flags) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "sqlite_replieur_wal_indisponible");
            return None;
        }
    };
    if let Err(e) = ecriture.execute_batch("PRAGMA wal_autocheckpoint=0;") {
        tracing::warn!(error = %e, "sqlite_replieur_wal_indisponible");
        return None;
    }
    let vigie: Vigie = Arc::new(());
    let suivi = Arc::downgrade(&vigie);
    let lance = std::thread::Builder::new()
        .name("sqlite-replieur-wal".into())
        .spawn(move || tourner(replieur, suivi));
    if let Err(e) = lance {
        tracing::warn!(error = %e, "sqlite_replieur_wal_indisponible");
        let _ = ecriture.execute_batch("PRAGMA wal_autocheckpoint=1000;");
        return None;
    }
    Some(vigie)
}

fn tourner(conn: Connection, vigie: Weak<()>) {
    loop {
        std::thread::sleep(PERIODE);
        if vigie.upgrade().is_none() {
            return;
        }
        replier(&conn);
    }
}

/// Un repli `PASSIVE`. Rend (pages du journal, pages repliées).
pub(crate) fn replier(conn: &Connection) -> Option<(i64, i64)> {
    let debut = Instant::now();
    let r = conn.query_row("PRAGMA wal_checkpoint(PASSIVE);", [], |l| {
        Ok((l.get::<_, i64>(1)?, l.get::<_, i64>(2)?))
    });
    match r {
        Ok((journal, repliees)) => {
            let duree = debut.elapsed();
            if duree >= REPLI_LONG {
                tracing::info!(
                    pages_journal = journal,
                    pages_repliees = repliees,
                    duree_ms = duree.as_millis() as u64,
                    "sqlite_repli_wal_long — hors de la connexion d'écriture"
                );
            }
            Some((journal, repliees))
        }
        Err(e) => {
            tracing::debug!(error = %e, "sqlite_repli_wal_echec");
            None
        }
    }
}
