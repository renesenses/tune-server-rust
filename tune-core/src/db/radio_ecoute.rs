//! #6682 — « Mon écoute » : le temps d'écoute RADIO, par station, à part.
//!
//! Gros Bidon (Didier), fil forum 2353, 20/10/2026 : « les écoutes radio ne
//! sont pas comptabilisées dans les indicateurs "Mon écoute". Normal ou un
//! manque ? ». C'était voulu : depuis la v0.8.258, la radio n'entre pas dans
//! `listen_history` (le titre d'un flux en direct est un instantané figé), et
//! les classements filtrent `source != 'radio'`. La réponse promise : un temps
//! d'écoute par station, À PART des classements de titres et d'artistes, qui
//! ne bougent pas.
//!
//! # Comment le temps se mesure
//!
//! Une veille ([`veille`]) échantillonne les zones toutes les
//! [`PERIODE_ECHANTILLON`] : une zone qui JOUE (`PlayState::Playing`) une
//! radio (`source == "radio"`) ajoute à sa station le temps écoulé depuis
//! l'échantillon précédent, si elle jouait déjà la même station. Rien ne
//! dépend des chemins de lecture (station de « Mes radios », de l'annuaire,
//! préréglage, réveil, transfert de zone) : ce qui compte, c'est ce qui joue.
//!
//! * Une pause, un arrêt ou un changement de station clôt la session ; la
//!   reprise en ouvre une nouvelle.
//! * Un écart de plus de [`ECART_MAX`] entre deux échantillons (machine en
//!   veille, exécuteur gelé) n'est pas compté : on n'invente pas une heure
//!   d'écoute que personne n'a vue.
//!
//! # Où il se range
//!
//! Table `radio_ecoutes` (migration SQLite 132 / PG 096), une ligne par
//! jour, station (adresse du flux), zone et profil propriétaire de la
//! session. Les classements de `listen_history` n'en lisent rien ; seul le
//! champ `radio_listening` du tableau de bord la lit
//! ([`RadioEcouteRepo::par_station`]).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::db::backend::DbBackend;
use crate::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use crate::playback::{PlayState, PlaybackManager, ZoneState};

/// Intervalle entre deux échantillons de la veille.
pub const PERIODE_ECHANTILLON: Duration = Duration::from_secs(15);

/// Au-delà de cet écart entre deux échantillons, le temps n'est pas compté.
pub const ECART_MAX: Duration = Duration::from_secs(60);

/// Ce qu'un échantillon ajoute à une station.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Increment {
    pub zone_id: i64,
    /// Le profil propriétaire de la session (`session_profile_id`), 0 si
    /// personne : une écoute sans propriétaire n'est pas attribuée.
    pub profile_id: i64,
    /// L'adresse du flux : c'est elle qui désigne la station.
    pub station_url: String,
    pub station_name: String,
    pub ms: i64,
    /// Une session commence à cet échantillon.
    pub nouvelle_session: bool,
}

/// La station qu'une zone JOUE en ce moment, `(adresse, nom)`.
fn station_jouee(z: &ZoneState) -> Option<(String, String)> {
    if z.state != PlayState::Playing {
        return None;
    }
    let np = z.now_playing.as_ref()?;
    if np.source != "radio" {
        return None;
    }
    let url = np
        .source_id
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())?
        .to_string();
    // Le nom de la station est posé en `album_title` au lancement
    // (`jouer_un_flux`) ; `title` peut porter le titre ICY du moment.
    let nom = np
        .album_title
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or(np.title.trim())
        .to_string();
    Some((url, nom))
}

/// Ce que la veille a vu de chaque zone au passage précédent.
#[derive(Debug, Default)]
pub struct Echantillonneur {
    vu: HashMap<i64, (String, Instant)>,
}

impl Echantillonneur {
    /// Un échantillon : les incréments à écrire. Fonction sans effet de bord
    /// hors de l'état de l'échantillonneur, pour être éprouvée sans horloge.
    pub fn observer(&mut self, zones: &[ZoneState], maintenant: Instant) -> Vec<Increment> {
        let mut increments = Vec::new();
        let mut vues = HashMap::new();
        for z in zones {
            let Some((url, nom)) = station_jouee(z) else {
                continue;
            };
            let base = |ms: i64, nouvelle_session: bool| Increment {
                zone_id: z.zone_id,
                profile_id: z.session_profile_id.unwrap_or(0),
                station_url: url.clone(),
                station_name: nom.clone(),
                ms,
                nouvelle_session,
            };
            match self.vu.get(&z.zone_id) {
                Some((avant, quand)) if *avant == url => {
                    let ecart = maintenant.saturating_duration_since(*quand);
                    if ecart <= ECART_MAX {
                        let ms = ecart.as_millis() as i64;
                        if ms > 0 {
                            increments.push(base(ms, false));
                        }
                    }
                }
                _ => increments.push(base(0, true)),
            }
            vues.insert(z.zone_id, (url, maintenant));
        }
        self.vu = vues;
        increments
    }
}

/// Le temps d'écoute d'une station sur une période (`radio_listening`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EcouteRadio {
    pub station_name: String,
    /// La station de « Mes radios » qui porte cette adresse, s'il y en a une.
    pub radio_id: Option<i64>,
    pub listening_ms: i64,
    pub sessions: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
}

/// La table `radio_ecoutes`.
pub struct RadioEcouteRepo {
    db: Arc<dyn DbBackend>,
}

impl RadioEcouteRepo {
    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        RadioEcouteRepo { db }
    }

    fn ph(&self, n: usize) -> String {
        match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(n),
            Engine::Postgres => PostgresDialect.placeholder(n),
        }
    }

    /// Ajoute un incrément au jour `jour` (`AAAA-MM-JJ`).
    pub fn ajouter(&self, jour: &str, inc: &Increment) -> Result<usize, String> {
        let sql = format!(
            "INSERT INTO radio_ecoutes \
             (jour, station_url, station_name, zone_id, profile_id, listening_ms, sessions) \
             VALUES ({}, {}, {}, {}, {}, {}, {}) \
             ON CONFLICT (jour, station_url, zone_id, profile_id) DO UPDATE SET \
             station_name = excluded.station_name, \
             listening_ms = radio_ecoutes.listening_ms + excluded.listening_ms, \
             sessions = radio_ecoutes.sessions + excluded.sessions",
            self.ph(1),
            self.ph(2),
            self.ph(3),
            self.ph(4),
            self.ph(5),
            self.ph(6),
            self.ph(7)
        );
        let sessions: i64 = i64::from(inc.nouvelle_session);
        self.db.execute(
            &sql,
            &[
                &jour,
                &inc.station_url.as_str(),
                &inc.station_name.as_str(),
                &inc.zone_id,
                &inc.profile_id,
                &inc.ms,
                &sessions,
            ],
        )
    }

    /// Le temps d'écoute par station, du plus écouté au moins écouté.
    ///
    /// `depuis` : le premier jour compté (`AAAA-MM-JJ`), `None` pour tout.
    /// `profile_id` : égalité stricte, comme les classements du tableau de
    /// bord — une écoute sans propriétaire (0) ne compte pas pour un profil.
    pub fn par_station(
        &self,
        depuis: Option<&str>,
        zone_id: Option<i64>,
        profile_id: Option<i64>,
        limite: i64,
    ) -> Result<Vec<EcouteRadio>, String> {
        let mut conditions: Vec<String> = Vec::new();
        if let Some(jour) = depuis {
            // Un jour que nous fabriquons (`jour_de`), jamais une saisie.
            let jour: String = jour
                .chars()
                .filter(|c| c.is_ascii_digit() || *c == '-')
                .collect();
            conditions.push(format!("jour >= '{jour}'"));
        }
        if let Some(z) = zone_id {
            conditions.push(format!("zone_id = {z}"));
        }
        if let Some(p) = profile_id {
            conditions.push(format!("profile_id = {p}"));
        }
        let filtre = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };
        // Agrégat d'abord, station ensuite : une jointure directe compterait
        // deux fois le temps d'une adresse que deux lignes de `radio_stations`
        // portent.
        let sql = format!(
            "SELECT a.nom, a.ms, a.sessions, \
               (SELECT MIN(r.id) FROM radio_stations r WHERE r.url = a.station_url), \
               (SELECT MAX(r.logo_url) FROM radio_stations r WHERE r.url = a.station_url) \
             FROM (SELECT station_url, MAX(station_name) AS nom, \
                     CAST(SUM(listening_ms) AS BIGINT) AS ms, \
                     CAST(SUM(sessions) AS BIGINT) AS sessions \
                   FROM radio_ecoutes {filtre} GROUP BY station_url) a \
             WHERE a.ms > 0 ORDER BY a.ms DESC, a.nom LIMIT {}",
            limite.max(1)
        );
        let entier = |v: Option<&crate::db::backend::SqlValue>| {
            v.and_then(|x| {
                x.as_i64()
                    .or_else(|| x.as_string().and_then(|s| s.trim().parse().ok()))
            })
        };
        Ok(self
            .db
            .query_many(&sql, &[])?
            .into_iter()
            .map(|c| EcouteRadio {
                station_name: c.first().and_then(|v| v.as_string()).unwrap_or_default(),
                listening_ms: entier(c.get(1)).unwrap_or(0),
                sessions: entier(c.get(2)).unwrap_or(0),
                radio_id: entier(c.get(3)),
                cover_url: c.get(4).and_then(|v| v.as_string()),
            })
            .collect())
    }
}

/// Le jour (UTC) d'un instant, au format de la colonne `jour`.
pub fn jour_de(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%Y-%m-%d").to_string()
}

/// Le premier jour compté pour une fenêtre de `jours` jours CALENDAIRES,
/// aujourd'hui compris : « today » (1) ne compte que le jour même.
pub fn premier_jour(maintenant: chrono::DateTime<chrono::Utc>, jours: i64) -> String {
    jour_de(maintenant - chrono::Duration::days((jours - 1).max(0)))
}

/// La veille d'écoute radio : un échantillon toutes les
/// [`PERIODE_ECHANTILLON`], pour toute la vie du serveur.
pub async fn veille(backend: Arc<dyn DbBackend>, playback: Arc<PlaybackManager>) {
    let repo = RadioEcouteRepo::with_backend(backend);
    let mut echantillonneur = Echantillonneur::default();
    let mut horloge = tokio::time::interval(PERIODE_ECHANTILLON);
    horloge.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        horloge.tick().await;
        let zones = playback.all_states().await;
        let increments = echantillonneur.observer(&zones, Instant::now());
        if increments.is_empty() {
            continue;
        }
        let jour = jour_de(chrono::Utc::now());
        for inc in &increments {
            if let Err(e) = repo.ajouter(&jour, inc) {
                warn!(zone_id = inc.zone_id, erreur = %e, "radio_ecoute_non_ecrite");
            }
        }
    }
}

#[cfg(test)]
#[path = "radio_ecoute_tests_6682.rs"]
mod tests;
