//! La passe de NUIT de l'identification (#4805, décision du 13/10/2026).
//!
//! # Ce qu'elle reprend, et seulement cela
//!
//! Les albums que MusicBrainz a **refusés** pendant une passe de lot (`503`,
//! limite de débit, coupure, délai) : ils n'ont pas été jugés. La mesure A en
//! a compté 718 sur une seule bibliothèque (540 + 178), tous dus à la
//! cadence, aucun à l'album. Un album introuvable (MusicBrainz a répondu « je
//! n'ai pas ce pressage ») ou ambigu n'y entre pas : il a été jugé, le
//! reprendre ne changerait rien.
//!
//! La liste vit dans `settings` ([`CLE_REFUS`]), tenue par
//! [`noter_le_verdict`] à chaque album d'un lot ou de cette passe : un refus
//! l'y ajoute, toute réponse l'en retire. Aucune migration.
//!
//! # Les gardes
//!
//! - **Éteinte par défaut** : [`CLE_ACTIVE`] vaut `false` tant que
//!   l'utilisateur ne l'allume pas (`POST /library/identify-all/nuit`).
//! - Entre [`HEURE_DEBUT`] et [`HEURE_FIN`], heure locale, une fois par nuit.
//! - **Débit réduit** : [`DELAI_ENTRE_ALBUMS`] entre deux albums, en plus de la
//!   cadence MusicBrainz du limiteur partagé — environ cinq fois moins de
//!   requêtes à la minute qu'un lot lancé à la main.
//! - La fenêtre et le délai sont des réglages (#6845), lus et bornés par
//!   [`ReglagesDeNuit`] ; les constantes ci-dessus en sont les défauts.
//! - Même droit que le lot (Premium), même pause (`Tache::Identification`),
//!   même disjoncteur (douze refus d'affilée : on s'arrête pour la nuit).
//! - Elle s'efface devant la lecture et devant une passe d'identification
//!   lancée à la main, entre deux albums.
//! - Seuls les albums encore SANS identifiant sont repris (décision 2 du
//!   23/09) : un album identifié entre-temps à la main sort de la liste.

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use tracing::{info, warn};

use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::license::Feature;
use tune_core::metadata::musicbrainz_release::{self, OrigineDeLaRecherche};
use tune_core::taches_de_fond::{Tache, est_en_pause};

use super::{
    Disjoncteur, EchecIdentification, Identification, consigner_la_tentative,
    effet_sur_le_disjoncteur, identifier_album, sql_candidats_identification,
};
use crate::state::AppState;

/// Le réglage : `"true"` allume la passe. Absent = éteinte.
pub(super) const CLE_ACTIVE: &str = "identification_nuit_active";
/// Les albums refusés par MusicBrainz, en tableau JSON d'identifiants.
pub(super) const CLE_REFUS: &str = "identification_refus_musicbrainz";
/// La date locale (`AAAA-MM-JJ`) de la dernière nuit où la passe est partie.
const CLE_DERNIERE_NUIT: &str = "identification_nuit_derniere";
/// Le bilan de la dernière passe de nuit, lu par l'écran.
const CLE_BILAN: &str = "identification_nuit_bilan";

/// L'identifiant de la passe au registre des tâches de fond.
const TACHE_NUIT: &str = "identification_nuit";

/// La fenêtre par défaut, en heures locales : de 2 h à 6 h.
pub(super) const HEURE_DEBUT: u8 = 2;
pub(super) const HEURE_FIN: u8 = 6;

/// Le délai ajouté par défaut entre deux albums. Un album coûte ~2,8 s au lot
/// (deux requêtes au limiteur partagé) ; avec 10 s de plus, la passe envoie
/// environ cinq fois moins de requêtes à la minute. 718 albums tiennent en
/// ~2 h 30, dans une seule fenêtre.
pub(super) const DELAI_ENTRE_ALBUMS: Duration = Duration::from_secs(10);

/// Les réglages de la fenêtre et du délai (#6845). Absents : les défauts.
const CLE_HEURE_DEBUT: &str = "identification_nuit_heure_debut";
const CLE_HEURE_FIN: &str = "identification_nuit_heure_fin";
const CLE_DELAI: &str = "identification_nuit_delai_s";

/// Les bornes du délai. En dessous de 5 s, la passe ne serait plus « au débit
/// réduit » (un album coûte ~2,8 s au lot) ; au-delà d'une heure, elle ne
/// traiterait plus que quelques albums par nuit.
pub(super) const DELAI_MIN: Duration = Duration::from_secs(5);
pub(super) const DELAI_MAX: Duration = Duration::from_secs(3_600);

/// Tous les combien la boucle se demande s'il est l'heure.
const CADENCE_DE_VEILLE: Duration = Duration::from_secs(300);

/// Au plus tant d'albums gardés dans la liste des refus : la liste vit dans
/// une ligne de `settings`, et une bibliothèque dont tout a été refusé se
/// reprend d'abord par un lot à la main.
const REFUS_MAX: usize = 20_000;

/// La fenêtre et le délai de la passe, tels qu'elle les applique.
///
/// Bornés à la lecture : une heure de début entre 0 et 23, une heure de fin
/// entre 1 et 24 (24 = minuit), un délai entre [`DELAI_MIN`] et
/// [`DELAI_MAX`]. Une fenêtre vide ou à cheval sur minuit (début ≥ fin) est
/// refusée et remplacée par la fenêtre par défaut : la passe ne part qu'une
/// fois par DATE locale, une fenêtre de 23 h à 2 h partirait deux fois.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ReglagesDeNuit {
    pub heure_debut: u8,
    pub heure_fin: u8,
    pub delai_entre_albums: Duration,
}

impl Default for ReglagesDeNuit {
    fn default() -> Self {
        Self {
            heure_debut: HEURE_DEBUT,
            heure_fin: HEURE_FIN,
            delai_entre_albums: DELAI_ENTRE_ALBUMS,
        }
    }
}

impl ReglagesDeNuit {
    /// `true` quand `heure` (locale, 0–23) est dans la fenêtre de nuit.
    pub(super) fn dans_la_fenetre(&self, heure: u8) -> bool {
        (self.heure_debut..self.heure_fin).contains(&heure)
    }

    /// Borne des valeurs brutes ; une valeur absente prend son défaut.
    /// `Err` quand la fenêtre obtenue est vide (début ≥ fin).
    pub(super) fn bornes(
        heure_debut: Option<i64>,
        heure_fin: Option<i64>,
        delai_s: Option<i64>,
    ) -> Result<Self, &'static str> {
        let debut = heure_debut.map_or(HEURE_DEBUT, |h| h.clamp(0, 23) as u8);
        let fin = heure_fin.map_or(HEURE_FIN, |h| h.clamp(1, 24) as u8);
        let delai = delai_s.map_or(DELAI_ENTRE_ALBUMS, |s| {
            Duration::from_secs(
                s.clamp(DELAI_MIN.as_secs() as i64, DELAI_MAX.as_secs() as i64) as u64,
            )
        });
        if debut >= fin {
            return Err("fenetre_vide");
        }
        Ok(Self {
            heure_debut: debut,
            heure_fin: fin,
            delai_entre_albums: delai,
        })
    }

    /// Lit les réglages en base. Illisibles : leur défaut ; fenêtre vide :
    /// la fenêtre par défaut, le délai lu étant gardé.
    pub(super) fn lire(backend: &Arc<dyn DbBackend>) -> Self {
        let reglages = SettingsRepo::with_backend(backend.clone());
        let entier = |cle: &str| {
            reglages
                .get(cle)
                .ok()
                .flatten()
                .and_then(|v| v.trim().parse::<i64>().ok())
        };
        let delai = entier(CLE_DELAI);
        Self::bornes(entier(CLE_HEURE_DEBUT), entier(CLE_HEURE_FIN), delai).unwrap_or_else(
            |motif| {
                warn!(motif, "identification_nuit_fenetre_ignoree");
                Self::bornes(None, None, delai).unwrap_or_default()
            },
        )
    }

    fn ecrire(&self, backend: &Arc<dyn DbBackend>) {
        let reglages = SettingsRepo::with_backend(backend.clone());
        for (cle, valeur) in [
            (CLE_HEURE_DEBUT, self.heure_debut.to_string()),
            (CLE_HEURE_FIN, self.heure_fin.to_string()),
            (CLE_DELAI, self.delai_entre_albums.as_secs().to_string()),
        ] {
            if let Err(e) = reglages.set(cle, &valeur) {
                warn!(cle, error = %e, "identification_nuit_reglage_non_ecrit");
            }
        }
    }
}

/// Ce dont la passe a besoin du temps : l'heure et la date locales, et
/// attendre. Injectée (#6845) : un essai fait passer une nuit entière sans
/// l'attendre, ni dépendre de l'heure à laquelle il tourne.
#[doc(hidden)]
pub trait Horloge: Sync {
    /// L'heure locale, 0–23.
    fn heure(&self) -> u8;
    /// La date locale, `AAAA-MM-JJ`.
    fn date(&self) -> String;
    /// Laisse passer `duree`.
    fn attendre(&self, duree: Duration) -> impl Future<Output = ()> + Send;
}

/// L'horloge de service : l'heure du système, et `tokio::time::sleep`.
struct HorlogeLocale;

impl Horloge for HorlogeLocale {
    fn heure(&self) -> u8 {
        heure_et_date_locales().0
    }
    fn date(&self) -> String {
        heure_et_date_locales().1
    }
    fn attendre(&self, duree: Duration) -> impl Future<Output = ()> + Send {
        tokio::time::sleep(duree)
    }
}

/// Ce que la veille voit, à un tour donné.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Constat {
    pub active: bool,
    pub heure: u8,
    pub reglages: ReglagesDeNuit,
    pub deja_partie_cette_nuit: bool,
    pub premium: bool,
    pub en_pause: bool,
    pub identification_en_cours: bool,
    pub lecture_en_cours: bool,
    pub refus_en_attente: usize,
}

/// 🔴 La décision de partir, sortie de la boucle pour être prouvable. Rend
/// `Err(motif)` quand la passe ne part pas.
pub(super) fn doit_partir(c: &Constat) -> Result<(), &'static str> {
    if !c.active {
        return Err("eteinte");
    }
    if !c.reglages.dans_la_fenetre(c.heure) {
        return Err("hors_fenetre");
    }
    if c.deja_partie_cette_nuit {
        return Err("deja_partie");
    }
    if !c.premium {
        return Err("premium_requis");
    }
    if c.en_pause {
        return Err("en_pause");
    }
    if c.identification_en_cours {
        return Err("identification_en_cours");
    }
    if c.lecture_en_cours {
        return Err("lecture_en_cours");
    }
    if c.refus_en_attente == 0 {
        return Err("rien_a_reprendre");
    }
    Ok(())
}

fn lire_refus(backend: &Arc<dyn DbBackend>) -> BTreeSet<i64> {
    SettingsRepo::with_backend(backend.clone())
        .get(CLE_REFUS)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Vec<i64>>(&s).ok())
        .unwrap_or_default()
        .into_iter()
        .collect()
}

fn ecrire_refus(backend: &Arc<dyn DbBackend>, refus: &BTreeSet<i64>) {
    let liste: Vec<i64> = refus.iter().copied().take(REFUS_MAX).collect();
    if let Err(e) = SettingsRepo::with_backend(backend.clone()).set(
        CLE_REFUS,
        &serde_json::to_string(&liste).unwrap_or_default(),
    ) {
        warn!(error = %e, "identification_nuit_liste_non_ecrite");
    }
}

/// Tient la liste des refus : un refus de MusicBrainz y met l'album, toute
/// réponse (identifié, introuvable, ambigu) ou un album sans piste l'en sort.
/// Appelé à chaque album d'un lot d'identification et de la passe de nuit.
pub(super) fn noter_le_verdict(
    backend: &Arc<dyn DbBackend>,
    album_id: i64,
    refus_musicbrainz: bool,
) {
    let mut refus = lire_refus(backend);
    let change = if refus_musicbrainz {
        refus.len() < REFUS_MAX && refus.insert(album_id)
    } else {
        refus.remove(&album_id)
    };
    if change {
        ecrire_refus(backend, &refus);
    }
}

/// Les albums à reprendre : la liste des refus, restreinte aux candidats du
/// lot (locaux, avec pistes, encore SANS identifiant). Rend aussi la liste
/// nettoyée des albums sortis entre-temps.
pub(super) fn albums_a_reprendre(backend: &Arc<dyn DbBackend>) -> Result<Vec<i64>, String> {
    let refus = lire_refus(backend);
    if refus.is_empty() {
        return Ok(Vec::new());
    }
    let candidats: BTreeSet<i64> = backend
        .query_many(sql_candidats_identification(), &[])
        .map_err(|e| e.to_string())?
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect();
    let gardes: BTreeSet<i64> = refus.intersection(&candidats).copied().collect();
    if gardes.len() != refus.len() {
        ecrire_refus(backend, &gardes);
    }
    Ok(gardes.into_iter().collect())
}

fn identification_en_cours(state: &AppState) -> bool {
    state
        .background_tasks
        .snapshot()
        .iter()
        .any(|t| t.kind == "identification" && t.id != TACHE_NUIT)
}

fn heure_et_date_locales() -> (u8, String) {
    let maintenant =
        time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    let d = maintenant.date();
    (
        maintenant.hour(),
        format!("{:04}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day()),
    )
}

/// Câblée au démarrage par `background.rs`.
pub(crate) fn spawn_passe_de_nuit(state: &AppState) {
    let state = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(CADENCE_DE_VEILLE).await;
            let _ = un_tour_de_veille_avec(&state, &HorlogeLocale).await;
        }
    });
}

/// La passe est-elle allumée ? Relu par la veille ET à chaque album : une
/// passe éteinte pendant qu'elle tourne s'arrête avant l'album suivant.
fn passe_active(backend: &Arc<dyn DbBackend>) -> bool {
    SettingsRepo::with_backend(backend.clone())
        .get(CLE_ACTIVE)
        .ok()
        .flatten()
        .as_deref()
        == Some("true")
}

/// Un tour de la veille, à l'heure que dit `horloge` : la passe part si
/// [`doit_partir`] le permet, et le tour rend quand sa nuit est finie.
/// `Err(motif)` quand elle ne part pas.
///
/// `pub` pour le témoin d'intégration de #6845 seulement, qui fait passer une
/// nuit simulée contre un faux MusicBrainz.
#[doc(hidden)]
pub async fn un_tour_de_veille_avec(
    state: &AppState,
    horloge: &impl Horloge,
) -> Result<(), &'static str> {
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    let active = passe_active(&state.backend);
    if !active {
        return Err("eteinte");
    }
    let (heure, date) = (horloge.heure(), horloge.date());
    let fenetre = ReglagesDeNuit::lire(&state.backend);
    let derniere = reglages.get(CLE_DERNIERE_NUIT).ok().flatten();
    let backend = state.backend.clone();
    let a_reprendre = tokio::task::spawn_blocking(move || albums_a_reprendre(&backend))
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
    let albums = match a_reprendre {
        Ok(a) => a,
        Err(e) => {
            warn!(error = %e, "identification_nuit_selection_echouee");
            return Err("selection_echouee");
        }
    };
    let constat = Constat {
        active,
        heure,
        reglages: fenetre,
        deja_partie_cette_nuit: derniere.as_deref() == Some(date.as_str()),
        premium: state.license.check_feature(Feature::AutoEnrichment).await,
        en_pause: est_en_pause(Tache::Identification),
        identification_en_cours: identification_en_cours(state),
        lecture_en_cours: tune_core::audio::replaygain::playing_zone_name(&state.backend).is_some(),
        refus_en_attente: albums.len(),
    };
    if let Err(motif) = doit_partir(&constat) {
        tracing::debug!(motif, "identification_nuit_ne_part_pas");
        return Err(motif);
    }
    // Inscrite AVANT le départ : une nuit ne part qu'une fois.
    reglages.set(CLE_DERNIERE_NUIT, &date).ok();
    executer_la_nuit_avec(state, albums, fenetre, horloge, |album_id| {
        identifier_album(state, album_id, None, OrigineDeLaRecherche::Lot)
    })
    .await;
    Ok(())
}

/// La boucle de nuit : celle du lot, au débit réduit, et qui s'efface. La
/// fenêtre, le délai, l'horloge et l'identification d'un album sont donnés
/// par l'appelant (les essais n'attendent pas la nuit, et certains
/// n'interrogent pas MusicBrainz).
async fn executer_la_nuit_avec<H, F, Fut>(
    state: &AppState,
    albums: Vec<i64>,
    reglages: ReglagesDeNuit,
    horloge: &H,
    mut identifier: F,
) where
    H: Horloge,
    F: FnMut(i64) -> Fut,
    Fut: std::future::Future<Output = Result<Identification, EchecIdentification>>,
{
    let _garde = state.background_tasks.begin(
        TACHE_NUIT,
        "Identification de nuit (albums refusés par MusicBrainz)…",
        "identification",
    );
    let total = albums.len();
    info!(total, "identification_nuit_demarre");
    let mut disjoncteur = Disjoncteur::default();
    let (mut traites, mut identifies, mut encore_refuses) = (0usize, 0usize, 0usize);
    let mut raison = "terminee";

    for (rang, album_id) in albums.into_iter().enumerate() {
        // L'attente AVANT les gardes (#6845) : après elle, la fenêtre peut
        // s'être refermée, la lecture avoir commencé. Gardes puis attente
        // envoyait un album après l'heure de fin.
        if rang > 0 {
            horloge.attendre(reglages.delai_entre_albums).await;
            musicbrainz_release::rate_limit_delay().await;
        }
        // Éteinte depuis l'écran pendant la nuit : rien de plus ne part.
        if !passe_active(&state.backend) {
            raison = "desactivee";
            break;
        }
        if est_en_pause(Tache::Identification) {
            raison = "pause_utilisateur";
            break;
        }
        if !reglages.dans_la_fenetre(horloge.heure()) {
            raison = "fin_de_fenetre";
            break;
        }
        if identification_en_cours(state) {
            raison = "identification_a_la_main";
            break;
        }
        if tune_core::audio::replaygain::playing_zone_name(&state.backend).is_some() {
            raison = "lecture_en_cours";
            break;
        }
        traites += 1;
        match identifier(album_id).await {
            Ok(issue) => {
                if matches!(issue.verdict, "reidentified" | "unchanged") {
                    identifies += 1;
                }
                if issue.refus_musicbrainz {
                    encore_refuses += 1;
                }
                consigner_la_tentative(
                    &state.backend,
                    album_id,
                    issue.verdict,
                    issue.refus_musicbrainz,
                );
                noter_le_verdict(&state.backend, album_id, issue.refus_musicbrainz);
                super::super::credits_apres_identification::apres_identification(
                    state,
                    album_id,
                    issue.verdict,
                );
                disjoncteur.enregistrer(effet_sur_le_disjoncteur(
                    issue.verdict,
                    issue.refus_musicbrainz,
                ));
            }
            Err(EchecIdentification::AlbumIntrouvable) => {
                noter_le_verdict(&state.backend, album_id, false);
            }
            Err(EchecIdentification::Base(e)) => {
                warn!(album_id, error = %e, "identification_nuit_album_echoue");
            }
        }
        if disjoncteur.a_saute() {
            raison = super::RAISON_MUSICBRAINZ_INJOIGNABLE;
            break;
        }
    }

    info!(
        total,
        traites, identifies, encore_refuses, raison, "identification_nuit_terminee"
    );
    SettingsRepo::with_backend(state.backend.clone())
        .set(
            CLE_BILAN,
            &json!({
                "date": horloge.date(),
                "total": total,
                "traites": traites,
                "identifies": identifies,
                "encore_refuses": encore_refuses,
                "raison": raison,
            })
            .to_string(),
        )
        .ok();
}

/// `GET /library/identify-all/nuit` — le réglage, la fenêtre et le délai
/// appliqués, la liste en attente, le bilan de la dernière nuit.
pub(crate) async fn reglage_nuit(State(state): State<AppState>) -> Json<Value> {
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    let active = reglages.get(CLE_ACTIVE).ok().flatten().as_deref() == Some("true");
    let bilan = reglages
        .get(CLE_BILAN)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    let nuit = ReglagesDeNuit::lire(&state.backend);
    Json(json!({
        "active": active,
        "fenetre": { "debut": nuit.heure_debut, "fin": nuit.heure_fin },
        "delai_entre_albums_s": nuit.delai_entre_albums.as_secs(),
        "bornes": {
            "debut": { "min": 0, "max": 23 },
            "fin": { "min": 1, "max": 24 },
            "delai_entre_albums_s": { "min": DELAI_MIN.as_secs(), "max": DELAI_MAX.as_secs() },
        },
        "refus_en_attente": lire_refus(&state.backend).len(),
        "derniere_nuit": bilan,
    }))
}

/// Le corps de `POST /library/identify-all/nuit` : chaque champ absent garde
/// sa valeur.
#[derive(serde::Deserialize)]
pub(crate) struct ReglageNuit {
    active: Option<bool>,
    heure_debut: Option<i64>,
    heure_fin: Option<i64>,
    delai_entre_albums_s: Option<i64>,
}

/// `POST /library/identify-all/nuit` `{"active": bool, "heure_debut": h,
/// "heure_fin": h, "delai_entre_albums_s": s}` — allumer ou éteindre, régler
/// la fenêtre et le délai. Les valeurs hors bornes sont ramenées aux bornes ;
/// une fenêtre vide (début ≥ fin) est refusée (`400`), rien n'est écrit.
pub(crate) async fn regler_nuit(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Json(corps): Json<ReglageNuit>,
) -> Response {
    let actuels = ReglagesDeNuit::lire(&state.backend);
    let demandes = ReglagesDeNuit::bornes(
        Some(corps.heure_debut.unwrap_or(actuels.heure_debut.into())),
        Some(corps.heure_fin.unwrap_or(actuels.heure_fin.into())),
        Some(
            corps
                .delai_entre_albums_s
                .unwrap_or(actuels.delai_entre_albums.as_secs() as i64),
        ),
    );
    let nuit = match demandes {
        Ok(n) => n,
        Err(motif) => {
            let (debut, fin) = (
                corps.heure_debut.unwrap_or(actuels.heure_debut.into()),
                corps.heure_fin.unwrap_or(actuels.heure_fin.into()),
            );
            return crate::message_api::refus(
                StatusCode::BAD_REQUEST,
                motif,
                "api.library.nightWindowEmpty",
                &[
                    ("start", debut.clamp(0, 23).to_string()),
                    ("end", fin.clamp(1, 24).to_string()),
                ],
            );
        }
    };
    if nuit != actuels {
        nuit.ecrire(&state.backend);
    }
    if let Some(active) = corps.active {
        SettingsRepo::with_backend(state.backend.clone())
            .set(CLE_ACTIVE, if active { "true" } else { "false" })
            .ok();
    }
    reglage_nuit(State(state)).await.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tune_core::db::backend::ToSqlValue;

    fn pret() -> Constat {
        Constat {
            active: true,
            heure: 3,
            premium: true,
            refus_en_attente: 5,
            ..Default::default()
        }
    }

    #[test]
    fn la_passe_est_eteinte_par_defaut() {
        let c = Constat {
            active: false,
            ..pret()
        };
        assert_eq!(doit_partir(&c), Err("eteinte"));
        // Le réglage absent n'allume rien : seul `"true"` allume.
        let etat = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
        let reglages = SettingsRepo::with_backend(etat.backend.clone());
        assert_eq!(reglages.get(CLE_ACTIVE).unwrap(), None);
    }

    #[test]
    fn chaque_garde_retient_la_passe() {
        assert_eq!(doit_partir(&pret()), Ok(()));
        let cas: [(Constat, &str); 7] = [
            (
                Constat {
                    heure: 23,
                    ..pret()
                },
                "hors_fenetre",
            ),
            (
                Constat {
                    heure: HEURE_FIN,
                    ..pret()
                },
                "hors_fenetre",
            ),
            (
                Constat {
                    deja_partie_cette_nuit: true,
                    ..pret()
                },
                "deja_partie",
            ),
            (
                Constat {
                    premium: false,
                    ..pret()
                },
                "premium_requis",
            ),
            (
                Constat {
                    en_pause: true,
                    ..pret()
                },
                "en_pause",
            ),
            (
                Constat {
                    identification_en_cours: true,
                    ..pret()
                },
                "identification_en_cours",
            ),
            (
                Constat {
                    lecture_en_cours: true,
                    ..pret()
                },
                "lecture_en_cours",
            ),
        ];
        for (c, motif) in cas {
            assert_eq!(doit_partir(&c), Err(motif), "{c:?}");
        }
        assert_eq!(
            doit_partir(&Constat {
                refus_en_attente: 0,
                ..pret()
            }),
            Err("rien_a_reprendre")
        );
        let defaut = ReglagesDeNuit::default();
        assert!(defaut.dans_la_fenetre(HEURE_DEBUT) && !defaut.dans_la_fenetre(HEURE_DEBUT - 1));
    }

    #[test]
    fn le_debit_est_reduit() {
        // Au moins trois fois le coût d'un album du lot (2,8 s).
        assert!(DELAI_ENTRE_ALBUMS >= Duration::from_secs(9));
        // Et le réglage ne descend pas sous le coût d'un album.
        assert!(DELAI_MIN >= Duration::from_secs(3));
    }

    /// #6845 — sans réglage, la passe garde la fenêtre de 2 h à 6 h et ses
    /// 10 s ; un réglage écrit est lu ; hors bornes, il est ramené aux bornes ;
    /// une fenêtre vide ou illisible retombe sur la fenêtre par défaut.
    #[test]
    fn les_reglages_sont_lus_et_bornes() {
        let etat = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
        let b = &etat.backend;
        let reglages = SettingsRepo::with_backend(b.clone());
        assert_eq!(
            ReglagesDeNuit::lire(b),
            ReglagesDeNuit {
                heure_debut: 2,
                heure_fin: 6,
                delai_entre_albums: Duration::from_secs(10),
            },
            "les défauts ont changé"
        );

        reglages.set(CLE_HEURE_DEBUT, "1").unwrap();
        reglages.set(CLE_HEURE_FIN, "5").unwrap();
        reglages.set(CLE_DELAI, "30").unwrap();
        let lus = ReglagesDeNuit::lire(b);
        assert_eq!((lus.heure_debut, lus.heure_fin), (1, 5));
        assert_eq!(lus.delai_entre_albums, Duration::from_secs(30));
        assert!(lus.dans_la_fenetre(1) && lus.dans_la_fenetre(4) && !lus.dans_la_fenetre(5));

        // Hors bornes : ramenés aux bornes.
        reglages.set(CLE_HEURE_DEBUT, "-3").unwrap();
        reglages.set(CLE_HEURE_FIN, "99").unwrap();
        reglages.set(CLE_DELAI, "0").unwrap();
        let lus = ReglagesDeNuit::lire(b);
        assert_eq!((lus.heure_debut, lus.heure_fin), (0, 24));
        assert_eq!(lus.delai_entre_albums, DELAI_MIN);
        assert!(lus.dans_la_fenetre(23));
        reglages.set(CLE_DELAI, "86400").unwrap();
        assert_eq!(ReglagesDeNuit::lire(b).delai_entre_albums, DELAI_MAX);

        // Fenêtre vide (ou à cheval sur minuit) : la fenêtre par défaut, le
        // délai lu étant gardé.
        reglages.set(CLE_HEURE_DEBUT, "23").unwrap();
        reglages.set(CLE_HEURE_FIN, "3").unwrap();
        reglages.set(CLE_DELAI, "20").unwrap();
        let lus = ReglagesDeNuit::lire(b);
        assert_eq!((lus.heure_debut, lus.heure_fin), (HEURE_DEBUT, HEURE_FIN));
        assert_eq!(lus.delai_entre_albums, Duration::from_secs(20));

        // Illisible : le défaut.
        reglages.set(CLE_HEURE_DEBUT, "deux heures").unwrap();
        reglages.set(CLE_HEURE_FIN, "").unwrap();
        reglages.set(CLE_DELAI, "dix").unwrap();
        assert_eq!(ReglagesDeNuit::lire(b), ReglagesDeNuit::default());
    }

    /// #6845 — la route règle la fenêtre et le délai, borne, et refuse une
    /// fenêtre vide sans rien écrire.
    #[tokio::test]
    async fn la_route_regle_la_fenetre_et_le_delai() {
        let etat = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
        let corps = |v: Value| Json(serde_json::from_value::<ReglageNuit>(v).unwrap());
        let lire = |r: Response| async move {
            let statut = r.status();
            let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .unwrap();
            (statut, serde_json::from_slice::<Value>(&octets).unwrap())
        };

        let (statut, v) = lire(
            regler_nuit(
                crate::auth::RequireAdmin,
                State(etat.clone()),
                corps(json!({ "heure_debut": 1, "heure_fin": 4, "delai_entre_albums_s": 2 })),
            )
            .await,
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{v}");
        assert_eq!(v["fenetre"], json!({ "debut": 1, "fin": 4 }), "{v}");
        assert_eq!(v["delai_entre_albums_s"], DELAI_MIN.as_secs(), "{v}");
        assert_eq!(
            v["active"], false,
            "régler la fenêtre n'allume pas la passe"
        );

        // Seul `active` : la fenêtre réglée reste.
        let (_, v) = lire(
            regler_nuit(
                crate::auth::RequireAdmin,
                State(etat.clone()),
                corps(json!({ "active": true })),
            )
            .await,
        )
        .await;
        assert_eq!(v["active"], true, "{v}");
        assert_eq!(v["fenetre"], json!({ "debut": 1, "fin": 4 }), "{v}");

        // Fenêtre vide : refusée, rien d'écrit.
        let (statut, v) = lire(
            regler_nuit(
                crate::auth::RequireAdmin,
                State(etat.clone()),
                corps(json!({ "heure_debut": 5 })),
            )
            .await,
        )
        .await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "{v}");
        // Un code stable et une phrase traduite, pas du français en dur (#7077).
        assert_eq!(v["error"], "fenetre_vide", "{v}");
        assert_eq!(v["message_key"], "api.library.nightWindowEmpty", "{v}");
        assert_eq!(v["params"], json!({ "start": "5", "end": "4" }), "{v}");
        let m = v["message"].as_str().unwrap_or_default();
        assert!(m.starts_with("The start hour (5:00)"), "{v}");
        let nuit = ReglagesDeNuit::lire(&etat.backend);
        assert_eq!((nuit.heure_debut, nuit.heure_fin), (1, 4));
    }

    fn base() -> crate::state::AppState {
        let etat = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
        for id in 1..=4_i64 {
            etat.backend
                .execute(
                    "INSERT INTO albums (id, title, source, musicbrainz_release_id) \
                     VALUES (?, 'a', 'local', NULL)",
                    &[&id as &dyn ToSqlValue],
                )
                .unwrap();
            let piste = id * 10;
            etat.backend
                .execute(
                    "INSERT INTO tracks (id, title, album_id, source) VALUES (?, 'p', ?, 'local')",
                    &[&piste as &dyn ToSqlValue, &id],
                )
                .unwrap();
        }
        etat
    }

    /// Un refus met l'album dans la liste ; toute réponse l'en sort.
    #[test]
    fn la_liste_ne_garde_que_les_refus() {
        let etat = base();
        let b = &etat.backend;
        noter_le_verdict(b, 1, true);
        noter_le_verdict(b, 2, true);
        noter_le_verdict(b, 3, false);
        assert_eq!(lire_refus(b).into_iter().collect::<Vec<_>>(), vec![1, 2]);
        // L'album 2 a enfin reçu une réponse (identifié, introuvable…).
        noter_le_verdict(b, 2, false);
        assert_eq!(lire_refus(b).into_iter().collect::<Vec<_>>(), vec![1]);
    }

    /// Un album identifié entre-temps (à la main) n'est pas repris : sa clé
    /// serait effacée puis réécrite (décision 2 du 23/09).
    #[test]
    fn un_album_identifie_entre_temps_n_est_pas_repris() {
        let etat = base();
        let b = &etat.backend;
        for id in [1, 2, 3] {
            noter_le_verdict(b, id, true);
        }
        b.execute(
            "UPDATE albums SET musicbrainz_release_id = 'x' WHERE id = 2",
            &[],
        )
        .unwrap();
        assert_eq!(albums_a_reprendre(b).unwrap(), vec![1, 3]);
        // La liste est nettoyée de l'album sorti.
        assert_eq!(lire_refus(b).into_iter().collect::<Vec<_>>(), vec![1, 3]);
    }

    /// Une horloge arrêtée sur une heure.
    struct Figee(u8);

    impl Horloge for Figee {
        fn heure(&self) -> u8 {
            self.0
        }
        fn date(&self) -> String {
            "2026-10-27".into()
        }
        fn attendre(&self, _duree: Duration) -> impl Future<Output = ()> + Send {
            std::future::ready(())
        }
    }

    /// Une identification fictive : MusicBrainz n'a rien trouvé, sans refus.
    fn introuvable() -> Identification {
        Identification {
            verdict: "not_found",
            tracks_total: 1,
            was_identified_before: false,
            previous_release_id: None,
            searched_title: "a".into(),
            searched_artist: String::new(),
            meilleur: None,
            applied: None,
            refus_musicbrainz: false,
            source: None,
            raison_ambigu: None,
            candidats: Vec::new(),
            concordance: None,
            artistes: None,
        }
    }

    /// Éteindre la passe depuis l'écran PENDANT qu'elle tourne arrête les
    /// albums suivants : le réglage est relu avant chaque album, et le bilan
    /// dit pourquoi la nuit s'est arrêtée.
    ///
    /// Contre-épreuve : retirer la relecture de `passe_active` dans la boucle
    /// fait rougir ce test (les trois albums partent).
    #[tokio::test]
    async fn eteindre_la_passe_en_cours_de_route_arrete_les_albums_suivants() {
        let etat = base();
        let reglages = SettingsRepo::with_backend(etat.backend.clone());
        reglages.set(CLE_ACTIVE, "true").unwrap();
        let appels = std::sync::Mutex::new(Vec::new());
        executer_la_nuit_avec(
            &etat,
            vec![1, 2, 3],
            ReglagesDeNuit::default(),
            &Figee(HEURE_DEBUT),
            |album_id| {
                appels.lock().unwrap().push(album_id);
                // L'utilisateur éteint la passe pendant le premier album.
                SettingsRepo::with_backend(etat.backend.clone())
                    .set(CLE_ACTIVE, "false")
                    .unwrap();
                async { Ok(introuvable()) }
            },
        )
        .await;
        assert_eq!(
            *appels.lock().unwrap(),
            vec![1],
            "la passe éteinte a continué sur les albums suivants"
        );
        let bilan: Value =
            serde_json::from_str(&reglages.get(CLE_BILAN).unwrap().unwrap()).unwrap();
        assert_eq!(bilan["raison"], "desactivee", "{bilan}");
        assert_eq!(bilan["traites"], 1, "{bilan}");
    }

    /// La veille est câblée au démarrage, et le lot à la main tient la liste.
    #[test]
    fn la_passe_est_cablee() {
        let background = include_str!("../../background.rs");
        assert!(
            background.contains(
                "crate::routes::library::identification_lot::nuit::spawn_passe_de_nuit(state);"
            ),
            "la passe de nuit n'est plus lancée au démarrage"
        );
        let lot = include_str!("identification_lot.rs");
        assert!(
            lot.contains(
                "nuit::noter_le_verdict(&state.backend, album_id, issue.refus_musicbrainz)"
            ),
            "le lot à la main ne tient plus la liste des refus"
        );
    }
}
