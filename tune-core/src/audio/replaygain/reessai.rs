//! « Tout relancer » rend à la cascade les pistes en échec (#6330).
//!
//! Benjithom, fil 2270 : ReplayGain bloqué à 99 % (84 pistes « en échec de
//! mesure »), plage dynamique à 95 % (79 « illisibles ou en échec »). Les deux
//! marques qui font sortir une piste du balayage sont posées pour de bon :
//!
//! * `rg_analyzed` sans `rg_track_gain` : la passe ReplayGain a essayé et la
//!   mesure n'a rien donné (fichier illisible, délai dépassé…). C'est
//!   exactement ce que compte `library_failed`
//!   ([`super::bibliotheque::BibliothequeReplayGain::echecs`]) ;
//! * `dr_indisponible` : le rattrapage de la plage dynamique a essayé, en
//!   vain.
//!
//! Ces marques évitent une boucle infinie sur un fichier cassé (#1155), et
//! elles doivent rester posées sans geste de l'utilisateur. Mais aucun geste
//! ne les levait : un fichier réparé, un partage remonté ou un correctif du
//! décodeur laissaient la piste hors du balayage pour toujours, et la jauge
//! sous 100 %. Le geste existe désormais : « Tout relancer »
//! (`POST /system/background-tasks/relaunch-all`).
//!
//! Le report de chemin (`rg_path_unresolved`, #1865) est levé aussi : il
//! périme seul, mais « relancer » veut dire maintenant, pas dans un jour.
//!
//! Rien n'est mesuré ici, aucun fichier n'est touché, et aucun gain, crête ou
//! plage déjà mesurés ne sont effacés : seules les marques d'échec partent.
//! La piste qui échoue encore sera re-marquée par la passe, une fois.

use std::sync::{Arc, LazyLock};

use serde::Serialize;
use tokio::sync::Notify;

use crate::db::backend::DbBackend;

use super::{DR_INDISPONIBLE_KEY, PATH_UNRESOLVED_KEY};

/// Les pistes rendues à la cascade par [`rendre_les_pistes_en_echec`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct PistesRendues {
    /// Témoins `rg_analyzed` sans gain effacés : pistes en échec de mesure
    /// ReplayGain, de nouveau candidates.
    pub replaygain: usize,
    /// Marques `dr_indisponible` effacées : pistes de nouveau candidates du
    /// rattrapage de la plage dynamique.
    pub dynamic_range: usize,
    /// Reports de chemin (`rg_path_unresolved`) levés.
    pub deferred: usize,
}

impl PistesRendues {
    /// Le nombre total de lignes effacées.
    pub fn total(&self) -> usize {
        self.replaygain + self.dynamic_range + self.deferred
    }
}

/// Efface les marques d'échec, puis réveille la cascade.
///
/// Idempotent : un second appel ne trouve plus rien. À appeler hors des fils
/// de l'exécuteur (trois `DELETE` sur `track_metadata`).
pub fn rendre_les_pistes_en_echec(backend: &Arc<dyn DbBackend>) -> Result<PistesRendues, String> {
    let replaygain = backend.execute(
        "DELETE FROM track_metadata WHERE key = 'rg_analyzed' \
         AND track_id NOT IN (SELECT g.track_id FROM track_metadata g \
                              WHERE g.key = 'rg_track_gain')",
        &[],
    )?;
    let dynamic_range = backend.execute(
        &format!("DELETE FROM track_metadata WHERE key = '{DR_INDISPONIBLE_KEY}'"),
        &[],
    )?;
    let deferred = backend.execute(
        &format!("DELETE FROM track_metadata WHERE key = '{PATH_UNRESOLVED_KEY}'"),
        &[],
    )?;
    let rendues = PistesRendues {
        replaygain,
        dynamic_range,
        deferred,
    };
    tracing::info!(
        replaygain,
        dynamic_range,
        deferred,
        "replaygain_pistes_en_echec_rendues"
    );
    reveiller_la_cascade();
    Ok(rendues)
}

/// Le réveil de la boucle de fond quand elle dort au repos.
///
/// Au repos, la cascade dort [`super::IDLE_SLEEP_SECS`] (15 min) : des pistes
/// rendues juste après son endormissement attendraient un quart d'heure, et
/// « Tout relancer » paraîtrait ne rien faire. `Notify` garde un permis quand
/// personne n'attend : un réveil donné pendant un lot n'est pas perdu, il
/// raccourcit le prochain repos.
static REVEIL: LazyLock<Notify> = LazyLock::new(Notify::new);

/// Réveils demandés depuis le démarrage : lu par les témoins de
/// `POST /system/replaygain/analyze` (#6778), qui doivent prouver que le
/// geste réveille VRAIMENT la boucle, pas seulement qu'il répond 202.
static REVEILS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Réveiller la cascade si elle dort au repos.
pub fn reveiller_la_cascade() {
    REVEILS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    REVEIL.notify_one();
}

/// Combien de réveils ont été demandés depuis le démarrage du processus.
pub fn nombre_de_reveils() -> u64 {
    REVEILS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Le repos de la boucle de fond : [`super::IDLE_SLEEP_SECS`], ou moins si
/// [`reveiller_la_cascade`] est appelé.
pub(super) async fn dormir_au_repos(duree: std::time::Duration) {
    tokio::select! {
        _ = tokio::time::sleep(duree) => {}
        _ = REVEIL.notified() => {
            tracing::debug!("replaygain_cascade_reveillee");
        }
    }
}

#[cfg(test)]
mod tests_6330 {
    use super::*;
    use crate::db::backend::ToSqlValue;
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;

    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        let db: Arc<dyn DbBackend> = Arc::new(db);
        db.execute("INSERT INTO artists (id, name) VALUES (1, 'A')", &[])
            .unwrap();
        db.execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'B', 1)",
            &[],
        )
        .unwrap();
        for id in 1..=4i64 {
            let chemin = format!("/m-6330/{id}.flac");
            db.execute(
                "INSERT INTO tracks (id, title, album_id, artist_id, file_path, source) \
                 VALUES (?, 'T', 1, 1, ?, 'local')",
                &[&id as &dyn ToSqlValue, &chemin as &dyn ToSqlValue],
            )
            .unwrap();
        }
        db
    }

    fn poser(b: &Arc<dyn DbBackend>, id: i64, cle: &str, valeur: &str) {
        b.execute(
            "INSERT INTO track_metadata (track_id, key, value) VALUES (?, ?, ?)",
            &[
                &id as &dyn ToSqlValue,
                &cle as &dyn ToSqlValue,
                &valeur as &dyn ToSqlValue,
            ],
        )
        .unwrap();
    }

    fn compter(b: &Arc<dyn DbBackend>, cle: &str) -> i64 {
        b.query_one(
            "SELECT COUNT(*) FROM track_metadata WHERE key = ?",
            &[&cle as &dyn ToSqlValue],
        )
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap()
    }

    /// Seules les marques d'échec partent : le témoin d'une piste MESURÉE et
    /// sa plage dynamique restent.
    #[test]
    fn seules_les_marques_d_echec_partent() {
        let b = base();
        // 1 : mesurée. 2 : échec ReplayGain. 3 : échec DR. 4 : reportée.
        poser(&b, 1, "rg_analyzed", "1700000000");
        poser(&b, 1, "rg_track_gain", "-7.20 dB");
        poser(&b, 1, "dr_track", "11");
        poser(&b, 2, "rg_analyzed", "1700000000");
        poser(&b, 3, "rg_analyzed", "1700000000");
        poser(&b, 3, "rg_track_gain", "-5.00 dB");
        poser(&b, 3, DR_INDISPONIBLE_KEY, "1700000000");
        poser(&b, 4, PATH_UNRESOLVED_KEY, "0001700000000");

        let avant = super::super::bibliotheque::compter_la_bibliotheque_replaygain(&b).unwrap();
        assert_eq!(avant.echecs, 1, "la piste 2 est comptée en échec");

        let r = rendre_les_pistes_en_echec(&b).unwrap();
        assert_eq!(
            r,
            PistesRendues {
                replaygain: 1,
                dynamic_range: 1,
                deferred: 1
            }
        );
        assert_eq!(
            compter(&b, "rg_analyzed"),
            2,
            "pistes 1 et 3 gardent leur témoin"
        );
        assert_eq!(compter(&b, "rg_track_gain"), 2);
        assert_eq!(compter(&b, "dr_track"), 1);
        assert_eq!(compter(&b, DR_INDISPONIBLE_KEY), 0);
        assert_eq!(compter(&b, PATH_UNRESOLVED_KEY), 0);

        let apres = super::super::bibliotheque::compter_la_bibliotheque_replaygain(&b).unwrap();
        assert_eq!(apres.echecs, 0);

        // Idempotent.
        assert_eq!(rendre_les_pistes_en_echec(&b).unwrap().total(), 0);
    }

    /// Le réveil raccourcit le repos de 15 min, même donné AVANT que la boucle
    /// ne s'endorme.
    #[tokio::test(start_paused = true)]
    async fn le_reveil_ecourte_le_repos() {
        reveiller_la_cascade();
        let debut = tokio::time::Instant::now();
        dormir_au_repos(std::time::Duration::from_secs(900)).await;
        assert!(debut.elapsed() < std::time::Duration::from_secs(1));
    }
}
