//! Les CRÉDITS d'un disque entier, lus sur MusicBrainz en UNE requête (#4767).
//!
//! FabienM (forum, fils 1875 et 1906) demande deux sections de plus sur la page
//! d'un artiste, d'après Roon : « Collaborations » (il joue ou chante sur le
//! disque d'un autre) et « Reprises » (il n'en est que l'auteur). Les deux ne
//! se lisent que dans `track_credits`, que rien ne remplissait en masse : la
//! mesure du 23/09/2026 sur le .18 l'a trouvée VIDE.
//!
//! # Une requête par disque, pas par piste
//!
//! `GET /ws/2/release/<mbid>?inc=recordings+artist-credits+recording-level-rels
//! +work-rels+work-level-rels+artist-rels` rend, pour chaque piste du disque,
//! l'enregistrement avec ses musiciens (instrument, chant, production…) ET
//! l'œuvre interprétée avec ses auteurs (compositeur, parolier). Les passes par
//! enregistrement (`/library/enrich-credits`) coûtent une requête par piste et
//! ne voient pas les œuvres : un disque de douze titres en coûte ici UNE.
//!
//! # Appariement : le MBID, puis la place — et rien d'autre
//!
//! Une piste locale reçoit les crédits d'une piste MusicBrainz :
//! 1. si elle porte le MÊME `musicbrainz_recording_id` ;
//! 2. sinon, si le disque local a EXACTEMENT autant de pistes que la release,
//!    que la piste occupe la même place (disque, numéro) ET que les titres
//!    concordent une fois normalisés.
//!
//! Aucun autre appariement : pas de recherche par titre sur le catalogue, pas
//! de « à peu près ». Un crédit posé sur la mauvaise piste ferait apparaître un
//! musicien sur un disque où il ne joue pas — pire que la section absente.
//!
//! # Rythme
//!
//! Chaque requête attend son créneau dans le limiteur MusicBrainz PARTAGÉ
//! ([`super::musicbrainz_release::rate_limit_delay`], clé commune avec les
//! pochettes) : une requête par seconde pour TOUT le serveur, pas par passe.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{Value, json};
use tracing::{debug, info, warn};

use super::credits_mb::{LigneCredit, lignes_artist_credit, lignes_relations};
use super::instruments::normaliser;
use crate::db::backend::{DbBackend, ToSqlValue};

/// Clé `settings` de l'avancement de `POST /system/enrich-credits`.
///
/// Même forme que les autres avancements d'enrichissement (`status`,
/// `processed`, `total`…) : le démarrage du serveur réécrit un `running`
/// orphelin en `interrupted` (voir `startup.rs`).
pub const REGLAGE_AVANCEMENT_CREDITS_RELEASES: &str = "enrich_credits_releases_status";

/// Identifiant de la passe au registre des tâches de fond du serveur. La passe
/// automatique par enregistrement (`credits_enrich_auto`) s'efface devant elle.
pub const TACHE_CREDITS_RELEASES: &str = "credits_releases";

/// Rôles AUDIBLES — la table UNIQUE (#7767, décision du 03/11) : ils font
/// entrer un disque d'autrui dans « Apparaît sur » (`collaborations`) et
/// comptent dans `credit_counts.performing`.
///
/// `artist` est l'`artist-credit` de l'enregistrement (l'interprète en tête du
/// titre) ; les autres viennent des relations (`instrument`, `vocal`,
/// `performer`, `performing orchestra`, `conductor`, `remixer`), canonisées
/// par `credits_mb::role_canonique`. L'arrangeur n'est PAS audible.
pub const ROLES_AUDIBLES: &[&str] = &["artist", "performer", "vocal", "conductor", "remixer"];

/// Rôles d'AUTEUR : seuls (aucun rôle audible sur tout le disque), ils font
/// entrer un disque dans « Reprises » (`covers`).
///
/// `writer` porte le parolier : `credits_mb::role_canonique` range `lyricist`,
/// `writer` et `librettist` sous ce mot depuis CRD-1, et les règles `credit`
/// des Smart Collections le lisent ainsi.
pub const ROLES_D_AUTEUR: &[&str] = &["composer", "writer"];

/// Rôles d'ÉCRITURE ET DE PRODUCTION (#7767) : ils font entrer un disque
/// hors de sa discographie dans « Écriture et production » (`writing`),
/// qu'il y soit audible ou non, et comptent dans `credit_counts.writing`.
/// `mastering` et `programming` n'en sont pas (hors de la liste du 03/11).
pub const ROLES_D_ECRITURE: &[&str] = &[
    "composer", "writer", "arranger", "producer", "engineer", "mixer",
];

pub fn est_role_audible(role: &str) -> bool {
    ROLES_AUDIBLES.contains(&role)
}

pub fn est_role_d_auteur(role: &str) -> bool {
    ROLES_D_AUTEUR.contains(&role)
}

pub fn est_role_d_ecriture(role: &str) -> bool {
    ROLES_D_ECRITURE.contains(&role)
}

/// `'a', 'b', …` pour un `IN (…)` : les rôles sont des mots du vocabulaire
/// canonique, jamais une saisie, et un paramètre lié par rôle alourdirait les
/// quatre branches de chaque requête.
fn liste_sql(roles: &[&str]) -> String {
    roles
        .iter()
        .map(|r| format!("'{r}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

// ── Lecture de la réponse ────────────────────────────────────────────────────

/// Une piste de la release, avec les crédits qui lui reviennent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PisteMb {
    /// `media[].position` — le numéro de disque, à partir de 1.
    pub disque: i64,
    /// `media[].tracks[].position` — le rang sur le disque, à partir de 1.
    pub numero: i64,
    pub titre: String,
    pub recording_mbid: String,
    pub lignes: Vec<LigneCredit>,
}

fn entier(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// Lignes d'une piste : l'`artist-credit` de l'enregistrement, ses relations
/// d'artistes, les auteurs de CHAQUE œuvre interprétée, puis les relations de
/// la release (qui valent pour toutes ses pistes) ; sans doublon.
fn lignes_de_l_enregistrement(
    recording: &Value,
    de_la_release: &[LigneCredit],
) -> Vec<LigneCredit> {
    let mut lignes = lignes_artist_credit(recording);
    lignes.extend(lignes_relations(recording));
    if let Some(rels) = recording.get("relations").and_then(Value::as_array) {
        for rel in rels {
            // `performance` → l'œuvre ; ses propres relations portent les
            // auteurs (`composer`, `lyricist`, `writer`, `arranger`…).
            if let Some(oeuvre) = rel.get("work") {
                lignes.extend(lignes_relations(oeuvre));
            }
        }
    }
    lignes.extend(de_la_release.iter().cloned());
    let mut vues: HashSet<(String, String, Option<String>)> = HashSet::new();
    lignes.retain(|l| {
        vues.insert((
            normaliser(&l.artist_name),
            l.role.clone(),
            l.instrument.clone(),
        ))
    });
    lignes
}

/// Les pistes d'une réponse `release?inc=…` et leurs crédits. Hors réseau :
/// testable sur une réponse réelle enregistrée.
pub fn pistes_de_la_release(release: &Value) -> Vec<PisteMb> {
    let de_la_release = lignes_relations(release);
    let mut out = Vec::new();
    let Some(media) = release.get("media").and_then(Value::as_array) else {
        return out;
    };
    for (i_disque, medium) in media.iter().enumerate() {
        let disque = medium
            .get("position")
            .and_then(entier)
            .unwrap_or(i_disque as i64 + 1);
        let Some(pistes) = medium.get("tracks").and_then(Value::as_array) else {
            continue;
        };
        for (i_piste, piste) in pistes.iter().enumerate() {
            let Some(recording) = piste.get("recording") else {
                continue;
            };
            let Some(recording_mbid) = recording
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
            else {
                continue;
            };
            let numero = piste
                .get("position")
                .and_then(entier)
                .unwrap_or(i_piste as i64 + 1);
            let titre = piste
                .get("title")
                .or_else(|| recording.get("title"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            out.push(PisteMb {
                disque,
                numero,
                titre,
                recording_mbid: recording_mbid.to_string(),
                lignes: lignes_de_l_enregistrement(recording, &de_la_release),
            });
        }
    }
    out
}

// ── Appariement ─────────────────────────────────────────────────────────────

/// Une piste locale du disque, telle que la base la connaît.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PisteLocale {
    pub id: i64,
    pub disque: i64,
    pub numero: i64,
    pub titre: String,
    pub recording_mbid: Option<String>,
}

fn titres_concordent(a: &str, b: &str) -> bool {
    let (a, b) = (normaliser(a), normaliser(b));
    !a.is_empty() && !b.is_empty() && (a == b || a.contains(&b) || b.contains(&a))
}

/// Couples `(id de piste locale, rang dans `mb`)`. Voir l'en-tête du module :
/// le MBID d'enregistrement d'abord, la place ensuite et seulement à nombre de
/// pistes égal, titres concordants. Une piste MusicBrainz ne sert qu'une fois.
pub fn apparier(mb: &[PisteMb], locales: &[PisteLocale]) -> Vec<(i64, usize)> {
    let mut out = Vec::new();
    let mut prises: HashSet<usize> = HashSet::new();
    let mut appariees: HashSet<i64> = HashSet::new();

    for l in locales {
        let Some(m) = l
            .recording_mbid
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        if let Some(i) = mb
            .iter()
            .enumerate()
            .position(|(i, p)| !prises.contains(&i) && p.recording_mbid.eq_ignore_ascii_case(m))
        {
            prises.insert(i);
            appariees.insert(l.id);
            out.push((l.id, i));
        }
    }

    if locales.len() == mb.len() {
        for l in locales.iter().filter(|l| !appariees.contains(&l.id)) {
            let disque = l.disque.max(1);
            if let Some(i) = mb.iter().enumerate().position(|(i, p)| {
                !prises.contains(&i)
                    && p.disque == disque
                    && p.numero == l.numero
                    && titres_concordent(&p.titre, &l.titre)
            }) {
                prises.insert(i);
                out.push((l.id, i));
            }
        }
    }
    out
}

// ── Écriture ────────────────────────────────────────────────────────────────

/// Écrit les crédits d'une piste : purge puis insertion, positions numérotées.
///
/// L'écriture UNIQUE de `track_credits` depuis MusicBrainz — les routes
/// `…/credits/enrich` y délèguent. Identifiants liés en CHAÎNE (miroir
/// PostgreSQL). La fiche artiste existante est LIÉE, par MBID d'abord puis par
/// nom ; jamais créée : un musicien de séance n'est pas un artiste de la
/// bibliothèque tant qu'aucun album ne le porte.
///
/// Idempotente : rejouée avec les mêmes lignes, elle laisse exactement les
/// mêmes lignes en base.
///
/// #7765 — `source` (voir [`super::source_des_credits`]) est posée sur chaque
/// ligne écrite, et la purge ne prend que les lignes de CETTE source et les
/// lignes de source inconnue : les crédits lus dans les fichiers, importés de
/// Roon ou venus d'une autre passe restent en place.
pub fn ecrire_credits_piste(
    backend: &Arc<dyn DbBackend>,
    track_id: i64,
    source: &str,
    lignes: &[LigneCredit],
) -> usize {
    // #4984 — l'entier, pas sa chaîne : sur PostgreSQL `track_id` est BIGINT,
    // et `bigint = text` y est refusé. L'erreur étant avalée par `.ok()`, la
    // purge n'avait jamais lieu : chaque réécriture AJOUTAIT ses crédits.
    backend
        .execute(
            "DELETE FROM track_credits WHERE track_id = ? AND (source = ? OR source IS NULL)",
            &[&track_id as &dyn ToSqlValue, &source as &dyn ToSqlValue],
        )
        .ok();
    inserer_credits_piste(backend, track_id, source, lignes)
}

/// L'insertion seule de [`ecrire_credits_piste`], sans purge : positions
/// numérotées depuis 0, fiche artiste LIÉE (MBID puis nom), jamais créée,
/// `source` posée sur chaque ligne. Rend le nombre de lignes écrites.
///
/// #8077 — les apports Qobuz purgent leurs SEULES lignes (`source =
/// 'qobuz'`), jamais les lignes de source inconnue : ils passent par ici.
pub fn inserer_credits_piste(
    backend: &Arc<dyn DbBackend>,
    track_id: i64,
    source: &str,
    lignes: &[LigneCredit],
) -> usize {
    let artistes = crate::db::artist_repo::ArtistRepo::with_backend(backend.clone());
    let mut ecrites = 0usize;
    for (pos, ligne) in lignes.iter().enumerate() {
        let pos = pos as i32;
        let fiche = ligne
            .artist_mbid
            .as_deref()
            .and_then(|m| artistes.get_by_musicbrainz_id(m).ok().flatten())
            .or_else(|| artistes.get_by_name(&ligne.artist_name).ok().flatten());
        // #4984 — des entiers, pas leurs chaînes : sur PostgreSQL `track_id`
        // est BIGINT et « column "track_id" is of type bigint but expression
        // is of type text » refusait TOUTE écriture de crédits. Un entier lié
        // passe aussi dans une colonne restée TEXT (conversion d'affectation).
        let artist_id: Option<i64> = fiche.and_then(|a| a.id);
        let ok = backend
            .execute(
                "INSERT INTO track_credits (track_id, artist_id, artist_name, role, instrument, position, artist_mbid, source) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                &[
                    &track_id as &dyn ToSqlValue,
                    &artist_id as &dyn ToSqlValue,
                    &ligne.artist_name as &dyn ToSqlValue,
                    &ligne.role as &dyn ToSqlValue,
                    &ligne.instrument as &dyn ToSqlValue,
                    &pos as &dyn ToSqlValue,
                    &ligne.artist_mbid as &dyn ToSqlValue,
                    &source as &dyn ToSqlValue,
                ],
            )
            .is_ok();
        if ok {
            ecrites += 1;
        }
    }
    ecrites
}

/// Pistes locales d'un album, pour l'appariement.
pub(crate) fn pistes_locales(backend: &Arc<dyn DbBackend>, album_id: i64) -> Vec<PisteLocale> {
    backend
        .query_many(
            "SELECT id, disc_number, track_number, title, musicbrainz_recording_id \
             FROM tracks WHERE album_id = ? ORDER BY disc_number, track_number, id",
            &[&album_id as &dyn ToSqlValue],
        )
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| {
            Some(PisteLocale {
                id: r.first()?.as_i64()?,
                disque: r.get(1).and_then(|v| v.as_i64()).unwrap_or(1),
                numero: r.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
                titre: r.get(3).and_then(|v| v.as_string()).unwrap_or_default(),
                recording_mbid: r.get(4).and_then(|v| v.as_string()),
            })
        })
        .collect()
}

/// Pose le curseur de reprise : ce disque a été interrogé.
fn marquer_album(backend: &Arc<dyn DbBackend>, album_id: i64) {
    let maintenant = chrono::Utc::now().to_rfc3339();
    if let Err(e) = backend.execute(
        "UPDATE albums SET credits_mb_at = ? WHERE id = ?",
        &[&maintenant as &dyn ToSqlValue, &album_id as &dyn ToSqlValue],
    ) {
        warn!(album_id, erreur = %e, "credits_release_marque_impossible");
    }
}

/// Bilan d'un disque.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BilanAlbum {
    /// Pistes locales appariées ET pourvues d'au moins un crédit, donc écrites.
    pub pistes_creditees: usize,
    /// Pistes locales qu'aucune piste de la release n'a pu recevoir.
    pub pistes_sans_correspondance: usize,
}

/// Applique une réponse `release?inc=…` à un album local, et le marque traité.
///
/// Une piste appariée SANS aucun crédit n'est pas touchée : écraser par du
/// vide ce qu'une autre source (pont Roon, saisie) avait écrit serait une
/// perte. Hors réseau : c'est ce que les essais jouent.
pub fn appliquer_release(
    backend: &Arc<dyn DbBackend>,
    album_id: i64,
    release: &Value,
) -> BilanAlbum {
    let mb = pistes_de_la_release(release);
    let locales = pistes_locales(backend, album_id);
    let couples = apparier(&mb, &locales);
    let mut bilan = BilanAlbum {
        pistes_sans_correspondance: locales.len().saturating_sub(couples.len()),
        ..Default::default()
    };
    for (track_id, i) in couples {
        let lignes = &mb[i].lignes;
        if lignes.is_empty() {
            continue;
        }
        if ecrire_credits_piste(
            backend,
            track_id,
            super::source_des_credits::MUSICBRAINZ,
            lignes,
        ) > 0
        {
            bilan.pistes_creditees += 1;
        }
    }
    // La release est en main : le constat de complétude (lot 1 MetaRust,
    // #4805) ne coûte aucune requête de plus, et alimente le filtre « albums
    // incomplets » de la Bibliothèque.
    if let Some(mbid) = release.get("id").and_then(Value::as_str) {
        super::completude::constater(backend, album_id, mbid, release);
    }
    marquer_album(backend, album_id);
    bilan
}

// ── La passe ────────────────────────────────────────────────────────────────

/// Les disques à interroger : un `musicbrainz_release_id` non vide et jamais
/// interrogés. Les autres — sans MBID — ne sont PAS candidats : les apparier
/// par titre ramènerait une édition voisine, donc des crédits faux.
pub fn albums_candidats(backend: &Arc<dyn DbBackend>) -> Vec<(i64, String)> {
    backend
        .query_many(
            "SELECT id, musicbrainz_release_id FROM albums \
             WHERE musicbrainz_release_id IS NOT NULL AND musicbrainz_release_id <> '' \
             AND credits_mb_at IS NULL ORDER BY id",
            &[],
        )
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| Some((r.first()?.as_i64()?, r.get(1)?.as_string()?)))
        .collect()
}

/// Nombre de disques portant un `musicbrainz_release_id` : le plafond de ce que
/// la passe peut couvrir sur cette bibliothèque.
pub fn albums_avec_mbid(backend: &Arc<dyn DbBackend>) -> i64 {
    backend
        .query_one(
            "SELECT COUNT(*) FROM albums \
             WHERE musicbrainz_release_id IS NOT NULL AND musicbrainz_release_id <> ''",
            &[],
        )
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
}

/// `force` : oublie les curseurs, pour tout réinterroger — réponses gardées
/// en base comprises (`musicbrainz_release_cache`) : sans quoi `force`
/// relirait la base au lieu de MusicBrainz.
pub fn oublier_les_curseurs(backend: &Arc<dyn DbBackend>) {
    if let Err(e) = backend.execute("UPDATE albums SET credits_mb_at = NULL", &[]) {
        warn!(erreur = %e, "credits_release_curseurs_non_effaces");
    }
    super::musicbrainz_release_cache::oublier_tout(backend);
}

/// Avancement de la passe, tel qu'il est publié.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Avancement {
    pub total: usize,
    /// Disques interrogés (réponse lue, identifiant inconnu ou panne).
    pub processed: usize,
    /// Disques dont au moins une piste a reçu des crédits.
    pub enriched: usize,
    pub tracks_credited: usize,
    /// Disques lus dont AUCUNE piste locale n'a pu être appariée.
    pub unmatched: usize,
    /// Identifiants que MusicBrainz ne connaît pas (404) : marqués traités.
    pub unknown: usize,
    /// Pannes (503, réseau) : NON marquées, retentées à la passe suivante.
    pub errors: usize,
}

impl Avancement {
    pub fn en_json(&self, status: &str, task_id: &str) -> Value {
        json!({
            "status": status,
            "task_id": task_id,
            "total": self.total,
            "processed": self.processed,
            "enriched": self.enriched,
            "tracks_credited": self.tracks_credited,
            "unmatched": self.unmatched,
            "unknown": self.unknown,
            "errors": self.errors,
        })
    }
}

/// Écrit l'avancement dans `settings`.
pub fn publier(backend: &Arc<dyn DbBackend>, av: &Avancement, status: &str, task_id: &str) {
    crate::db::settings_repo::SettingsRepo::with_backend(backend.clone())
        .set(
            REGLAGE_AVANCEMENT_CREDITS_RELEASES,
            &av.en_json(status, task_id).to_string(),
        )
        .ok();
}

/// Écritures de l'avancement : tous les `JALON` disques, et à la fin.
const JALON: usize = 10;

/// La passe : chaque disque candidat, une requête, dans l'ordre des
/// identifiants. Reprenable par construction — le curseur est la colonne
/// `albums.credits_mb_at`, posée disque par disque : un arrêt (redémarrage,
/// panne) ne perd que le disque en cours. Se GARE quand l'utilisateur suspend
/// l'enrichissement, comme la passe des types de sortie.
///
/// `sur_avancement` est appelée à chaque disque (le registre des tâches du
/// serveur s'y branche) ; l'avancement persisté l'est tous les [`JALON`].
///
/// #4805 (idée 3 de MetaRust) — chaque disque se lit D'ABORD dans
/// `musicbrainz_release_cache`, où l'identification a gardé la release avec
/// les `inc` des crédits. Une requête ne part que si la réponse manque, est
/// périmée ou ne couvre pas [`INC_CREDITS_RELEASE`] ; le créneau MusicBrainz
/// n'est réservé que dans ce cas.
///
/// [`INC_CREDITS_RELEASE`]: super::musicbrainz_release::INC_CREDITS_RELEASE
pub async fn remplir_credits_depuis_musicbrainz(
    backend: Arc<dyn DbBackend>,
    task_id: &str,
    sur_avancement: &(dyn Fn(&Avancement) + Send + Sync),
) -> Avancement {
    remplir_credits_par(backend, task_id, sur_avancement, |id, inc| async move {
        super::musicbrainz_release::rate_limit_delay().await;
        super::musicbrainz_release::lire_release_brute(&id, inc).await
    })
    .await
}

/// Le corps de [`remplir_credits_depuis_musicbrainz`], transport en
/// paramètre : `interroger` reçoit le MBID et les `inc` à demander, et porte
/// l'attente du créneau. La couture sert au banc #4805, qui compte les
/// requêtes sans réseau.
pub(crate) async fn remplir_credits_par<F, Fut>(
    backend: Arc<dyn DbBackend>,
    task_id: &str,
    sur_avancement: &(dyn Fn(&Avancement) + Send + Sync),
    interroger: F,
) -> Avancement
where
    F: Fn(String, &'static str) -> Fut,
    Fut: std::future::Future<Output = super::musicbrainz_release::LectureRelease>,
{
    let purgees = super::musicbrainz_release_cache::purger(&backend, chrono::Utc::now());
    if purgees > 0 {
        debug!(purgees, "credits_release_cache_purge");
    }
    let candidats = albums_candidats(&backend);
    let mut av = Avancement {
        total: candidats.len(),
        ..Default::default()
    };
    publier(&backend, &av, "running", task_id);
    sur_avancement(&av);
    info!(candidats = av.total, "credits_release_passe_demarree");

    for (album_id, release_id) in &candidats {
        crate::taches_de_fond::attendre_la_reprise(crate::taches_de_fond::Tache::Enrichissement)
            .await;
        let (lecture, _provenance) = super::musicbrainz_release::lire_release_gardee_par(
            &backend,
            release_id,
            super::musicbrainz_release::INC_CREDITS_RELEASE,
            &interroger,
        )
        .await;
        match lecture {
            super::musicbrainz_release::LectureRelease::Lue(release) => {
                let bilan = appliquer_release(&backend, *album_id, &release);
                if bilan.pistes_creditees > 0 {
                    av.enriched += 1;
                    av.tracks_credited += bilan.pistes_creditees;
                } else {
                    av.unmatched += 1;
                }
                debug!(album_id, ?bilan, "credits_release_album");
            }
            super::musicbrainz_release::LectureRelease::Inconnue => {
                av.unknown += 1;
                marquer_album(&backend, *album_id);
            }
            super::musicbrainz_release::LectureRelease::Panne(motif) => {
                av.errors += 1;
                debug!(album_id, motif = %motif, "credits_release_panne");
            }
        }
        av.processed += 1;
        sur_avancement(&av);
        if av.processed.is_multiple_of(JALON) {
            publier(&backend, &av, "running", task_id);
        }
    }

    publier(&backend, &av, "done", task_id);
    sur_avancement(&av);
    info!(?av, "credits_release_passe_terminee");
    av
}

// ── Lecture pour la page artiste ────────────────────────────────────────────

/// Un crédit de l'artiste sur une piste d'un disque qui n'est PAS le sien.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditSurAlbum {
    pub album_id: i64,
    pub track_id: i64,
    pub role: String,
    pub instrument: Option<String>,
}

/// Un disque d'autrui, vu depuis la page de l'artiste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumCredite {
    pub album_id: i64,
    /// Pistes où l'artiste est crédité au titre de la section — c'est le
    /// FOCUS : la fiche d'album ouverte depuis la section n'affiche qu'elles.
    pub track_ids: Vec<i64>,
    /// Ce qu'il y fait : instruments et rôles, sans doublon, dans l'ordre.
    pub roles: Vec<String>,
}

/// Classement d'un disque d'autrui, à partir des crédits de l'artiste.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Classement {
    pub collaborations: Vec<AlbumCredite>,
    pub reprises: Vec<AlbumCredite>,
    /// « Écriture et production » (#7767) : les disques hors de sa
    /// discographie où il tient un rôle d'écriture, qu'il y joue ou non.
    pub ecriture: Vec<AlbumCredite>,
}

/// Ce qu'on sait d'un disque pour le classer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisqueAClasser {
    pub album_id: i64,
    /// `albums.is_compilation`, ou un artiste d'album « Various Artists ».
    pub compilation: bool,
    /// L'artiste est l'artiste d'au moins une piste du disque : il est déjà
    /// dans « Compilations » ou « Apparitions », et n'entre ni dans
    /// `collaborations` ni dans `covers`. « Écriture et production » le
    /// garde (#7767) : y jouer n'efface pas l'avoir produit.
    pub artiste_de_piste: bool,
}

/// Un compteur de l'en-tête de la page artiste (#7767) : albums et pistes
/// DISTINCTS, toute la bibliothèque comprise, sa discographie avec.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CompteurCredits {
    pub albums: i64,
    pub tracks: i64,
}

/// `credit_counts` : « interprète » (artiste de la piste ou rôle audible) et
/// « écriture » (un rôle de [`ROLES_D_ECRITURE`]). Toujours rendu, zéros
/// compris : sa présence dit que le serveur sait compter.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CompteursCredits {
    pub performing: CompteurCredits,
    pub writing: CompteurCredits,
}

impl CompteursCredits {
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "performing": { "albums": self.performing.albums, "tracks": self.performing.tracks },
            "writing": { "albums": self.writing.albums, "tracks": self.writing.tracks },
        })
    }
}

/// Vrai pour un nom d'artiste d'album qui désigne une compilation. Même
/// vocabulaire que `scan_import::is_various_artists` côté serveur : celui de
/// LA règle ([`crate::library::regle_compilation::est_artistes_divers`]).
pub fn est_artistes_divers(nom: &str) -> bool {
    crate::library::regle_compilation::est_artistes_divers(nom)
}

/// Les trois sections de crédits, à partir des crédits de l'artiste sur
/// les disques d'AUTRUI (l'appelant a déjà écarté ses propres disques —
/// Discographie et Live — et dit, disque par disque, s'il y est l'artiste
/// d'une piste).
///
/// * **Apparaît sur** (`collaborations`) : au moins un rôle AUDIBLE, disque
///   qui n'est PAS une compilation, et il n'y est l'artiste d'aucune piste
///   (sinon il est déjà dans « Apparitions » ou « Compilations »). Focus =
///   les pistes où il est audible ; rôles = l'instrument quand il est connu,
///   sinon le rôle.
/// * **Reprises** (`covers`) : AUCUN rôle audible sur tout le disque, au
///   moins un rôle d'AUTEUR, et pas artiste d'une piste. Focus = les pistes
///   qu'il a écrites ; rôles comme avant (instrument sinon rôle). Une
///   compilation de reprises y a sa place.
/// * **Écriture et production** (`ecriture`, #7767) : au moins un rôle
///   d'ÉCRITURE, qu'il y joue ou non — un disque peut donc être à la fois ici
///   et dans l'une des deux autres. Focus = les pistes écrites ou produites ;
///   rôles = le vocabulaire canonique (`composer`, `producer`…), jamais
///   l'instrument.
/// * Un musicien sur une compilation, un `mastering` seul : nulle part.
pub fn classer(credits: &[CreditSurAlbum], disques: &[DisqueAClasser]) -> Classement {
    let disques: HashMap<i64, &DisqueAClasser> = disques.iter().map(|d| (d.album_id, d)).collect();
    let mut ordre: Vec<i64> = Vec::new();
    let mut par_album: HashMap<i64, Vec<&CreditSurAlbum>> = HashMap::new();
    for c in credits {
        par_album
            .entry(c.album_id)
            .or_insert_with(|| {
                ordre.push(c.album_id);
                Vec::new()
            })
            .push(c);
    }
    let resume = |cs: &[&CreditSurAlbum], garder: fn(&str) -> bool, par_instrument: bool| {
        let mut track_ids: Vec<i64> = Vec::new();
        let mut roles: Vec<String> = Vec::new();
        for c in cs.iter().filter(|c| garder(&c.role)) {
            if !track_ids.contains(&c.track_id) {
                track_ids.push(c.track_id);
            }
            let r = match (&c.instrument, par_instrument) {
                (Some(instrument), true) => instrument.clone(),
                _ => c.role.clone(),
            };
            if !roles.contains(&r) {
                roles.push(r);
            }
        }
        track_ids.sort_unstable();
        AlbumCredite {
            album_id: cs[0].album_id,
            track_ids,
            roles,
        }
    };
    let mut out = Classement::default();
    for album_id in ordre {
        let cs = &par_album[&album_id];
        let audible = cs.iter().any(|c| est_role_audible(&c.role));
        let auteur = cs.iter().any(|c| est_role_d_auteur(&c.role));
        let ecriture = cs.iter().any(|c| est_role_d_ecriture(&c.role));
        let disque = disques.get(&album_id);
        let est_compilation = disque.is_some_and(|d| d.compilation);
        let artiste_de_piste = disque.is_some_and(|d| d.artiste_de_piste);
        if !artiste_de_piste {
            if audible {
                if !est_compilation {
                    out.collaborations.push(resume(cs, est_role_audible, true));
                }
            } else if auteur {
                out.reprises.push(resume(cs, est_role_d_auteur, true));
            }
        }
        if ecriture {
            out.ecriture.push(resume(cs, est_role_d_ecriture, false));
        }
    }
    out
}

/// Les trois clés d'un artiste dans `track_credits` : sa fiche
/// (`artist_id`), son nom exact, son MBID (`artist_mbid` =
/// `artists.musicbrainz_id`). `None` quand la fiche n'existe pas.
struct ClesArtiste {
    id_str: String,
    nom: String,
    /// Un MBID absent est lié comme une chaîne vide, que la garde `<> ''`
    /// empêche d'égaler les crédits sans MBID.
    mbid: String,
}

fn cles_artiste(backend: &Arc<dyn DbBackend>, artist_id: i64) -> Option<ClesArtiste> {
    let fiche = backend
        .query_one(
            "SELECT name, musicbrainz_id FROM artists WHERE id = ?",
            &[&artist_id as &dyn ToSqlValue],
        )
        .ok()
        .flatten()?;
    Some(ClesArtiste {
        id_str: artist_id.to_string(),
        nom: fiche
            .first()
            .and_then(|v| v.as_string())
            .unwrap_or_default(),
        mbid: fiche.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
    })
}

/// La branche « fiche » des requêtes : sur PostgreSQL,
/// `track_credits.artist_id` est du TEXT sur une base migrée depuis SQLite et
/// du BIGINT ailleurs — la comparaison en texte vaut pour les deux, au prix
/// de l'index. Sur SQLite, la chaîne liée prend l'affinité INTEGER de la
/// colonne : l'index sert.
fn cle_fiche(backend: &Arc<dyn DbBackend>) -> &'static str {
    match backend.engine() {
        crate::db::engine::Engine::Postgres => "CAST(artist_id AS TEXT) = ?",
        crate::db::engine::Engine::Sqlite => "artist_id = ?",
    }
}

/// Les `track_credits.id` de l'artiste, par trois branches INDEXÉES réunies
/// en `UNION` (fiche, nom, MBID) : un `OR` sur trois colonnes ferait
/// parcourir toute la table à chaque page artiste. `filtre` restreint chaque
/// branche (les rôles) ; les paramètres liés sont, dans l'ordre, l'identifiant
/// en texte, le nom, le MBID.
fn credits_de_l_artiste_sql(backend: &Arc<dyn DbBackend>, filtre: &str) -> String {
    let cle_fiche = cle_fiche(backend);
    format!(
        "SELECT id FROM track_credits WHERE {cle_fiche}{filtre} \
         UNION SELECT id FROM track_credits WHERE artist_name = ?{filtre} \
         UNION SELECT id FROM track_credits WHERE artist_mbid = ? AND artist_mbid <> ''{filtre}"
    )
}

/// Les crédits d'un artiste sur les disques d'AUTRUI — hors de sa
/// discographie : l'artiste d'album n'est pas lui — avec de quoi les classer,
/// dont, disque par disque, s'il y est l'artiste d'une piste (#7767 :
/// « Écriture et production » garde ces disques-là, les deux autres sections
/// les écartent).
///
/// L'artiste est reconnu par sa fiche (`artist_id`), son MBID
/// (`artist_mbid` = `artists.musicbrainz_id`) ou son nom exact. Les albums
/// masqués sont exclus, comme partout sur la page artiste.
pub fn credits_hors_discographie(
    backend: &Arc<dyn DbBackend>,
    artist_id: i64,
) -> (Vec<CreditSurAlbum>, Vec<DisqueAClasser>) {
    let Some(cles) = cles_artiste(backend, artist_id) else {
        return (Vec::new(), Vec::new());
    };
    let sql = format!(
        "SELECT t.album_id, t.id, tc.role, tc.instrument, \
                COALESCE(a.is_compilation, 0), ar.name, \
                EXISTS (SELECT 1 FROM tracks t2 WHERE t2.album_id = a.id AND t2.artist_id = ?) \
         FROM track_credits tc \
         JOIN tracks t ON t.id = tc.track_id \
         JOIN albums a ON a.id = t.album_id \
         LEFT JOIN artists ar ON ar.id = a.artist_id \
         WHERE tc.id IN ({}) \
           AND (a.artist_id IS NULL OR a.artist_id <> ?) \
           AND {} \
         ORDER BY t.album_id, t.disc_number, t.track_number, t.id, tc.position",
        credits_de_l_artiste_sql(backend, ""),
        crate::db::facet_filter::hidden_albums_excluded()
    );
    let lignes = backend
        .query_many(
            &sql,
            &[
                &artist_id as &dyn ToSqlValue,
                &cles.id_str as &dyn ToSqlValue,
                &cles.nom as &dyn ToSqlValue,
                &cles.mbid as &dyn ToSqlValue,
                &artist_id as &dyn ToSqlValue,
            ],
        )
        .unwrap_or_else(|e| {
            warn!(artist_id, erreur = %e, "credits_page_artiste_lecture_impossible");
            Vec::new()
        });
    let mut credits = Vec::new();
    let mut disques: Vec<DisqueAClasser> = Vec::new();
    for r in lignes {
        let (Some(album_id), Some(track_id)) = (
            r.first().and_then(|v| v.as_i64()),
            r.get(1).and_then(|v| v.as_i64()),
        ) else {
            continue;
        };
        let role = r.get(2).and_then(|v| v.as_string()).unwrap_or_default();
        let instrument = r.get(3).and_then(|v| v.as_string());
        if !disques.iter().any(|d| d.album_id == album_id) {
            let drapeau = r.get(4).and_then(|v| v.as_i64()).unwrap_or(0) != 0;
            let va = r
                .get(5)
                .and_then(|v| v.as_string())
                .is_some_and(|n| est_artistes_divers(&n));
            // `EXISTS` rend un entier sur SQLite, un booléen sur PostgreSQL :
            // `as_bool` lit les deux.
            let artiste_de_piste = r.get(6).and_then(|v| v.as_bool()).unwrap_or(false);
            disques.push(DisqueAClasser {
                album_id,
                compilation: drapeau || va,
                artiste_de_piste,
            });
        }
        credits.push(CreditSurAlbum {
            album_id,
            track_id,
            role,
            instrument,
        });
    }
    (credits, disques)
}

/// `credit_counts` (#7767) : les pistes et albums DISTINCTS où l'artiste est
/// audible (artiste de la piste, ou crédité avec un rôle de
/// [`ROLES_AUDIBLES`]) et ceux où il tient un rôle de [`ROLES_D_ECRITURE`].
/// Toute la bibliothèque, sa discographie comprise ; albums masqués exclus.
/// Mêmes trois clés que [`credits_hors_discographie`]. Les doublons de
/// `track_credits` (une ligne `balises` et une ligne MusicBrainz pour le même
/// crédit, #7766) ne comptent qu'une fois : on compte des pistes, pas des
/// lignes.
pub fn compteurs_de_credits(backend: &Arc<dyn DbBackend>, artist_id: i64) -> CompteursCredits {
    let Some(cles) = cles_artiste(backend, artist_id) else {
        return CompteursCredits::default();
    };
    let compter = |pistes: &str, params: &[&dyn ToSqlValue]| -> CompteurCredits {
        let sql = format!(
            "SELECT COUNT(DISTINCT t.id), COUNT(DISTINCT t.album_id) \
             FROM tracks t JOIN albums a ON a.id = t.album_id \
             WHERE t.id IN ({pistes}) AND {}",
            crate::db::facet_filter::hidden_albums_excluded()
        );
        match backend.query_one(&sql, params) {
            Ok(Some(r)) => CompteurCredits {
                tracks: r.first().and_then(|v| v.as_i64()).unwrap_or(0),
                albums: r.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
            },
            Ok(None) => CompteurCredits::default(),
            Err(e) => {
                warn!(artist_id, erreur = %e, "compteurs_page_artiste_lecture_impossible");
                CompteurCredits::default()
            }
        }
    };
    let audibles = format!(" AND role IN ({})", liste_sql(ROLES_AUDIBLES));
    let ecriture = format!(" AND role IN ({})", liste_sql(ROLES_D_ECRITURE));
    let performing = compter(
        &format!(
            "SELECT id FROM tracks WHERE artist_id = ? \
             UNION SELECT track_id FROM track_credits WHERE id IN ({})",
            credits_de_l_artiste_sql(backend, &audibles)
        ),
        &[
            &artist_id as &dyn ToSqlValue,
            &cles.id_str as &dyn ToSqlValue,
            &cles.nom as &dyn ToSqlValue,
            &cles.mbid as &dyn ToSqlValue,
        ],
    );
    let writing = compter(
        &format!(
            "SELECT track_id FROM track_credits WHERE id IN ({})",
            credits_de_l_artiste_sql(backend, &ecriture)
        ),
        &[
            &cles.id_str as &dyn ToSqlValue,
            &cles.nom as &dyn ToSqlValue,
            &cles.mbid as &dyn ToSqlValue,
        ],
    );
    CompteursCredits {
        performing,
        writing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Réponse RÉELLE de MusicBrainz, enregistrée le 24/09/2026 :
    /// `release/f33a4c92-3275-4348-8bf2-237a26976f4e?inc=recordings+artist-credits
    /// +recording-level-rels+work-rels+work-level-rels+artist-rels` — *Déjà vu*,
    /// Crosby, Stills, Nash & Young, édition US, 10 titres. Neil Young y joue
    /// et chante sur « Helpless », qu'il a écrit.
    const DEJA_VU: &str =
        include_str!("../../tests/fixtures/musicbrainz/release_deja_vu_credits.json");

    const NEIL_YOUNG_MBID: &str = "75167b8b-44e4-407b-9d35-effe87b223cf";

    fn release() -> Value {
        serde_json::from_str(DEJA_VU).expect("fixture JSON")
    }

    #[test]
    fn la_release_reelle_rend_ses_dix_pistes_avec_leurs_places() {
        let pistes = pistes_de_la_release(&release());
        assert_eq!(pistes.len(), 10);
        assert!(pistes.iter().all(|p| p.disque == 1));
        assert_eq!(
            pistes.iter().map(|p| p.numero).collect::<Vec<_>>(),
            (1..=10).collect::<Vec<_>>()
        );
        assert_eq!(pistes[3].titre, "Helpless");
        assert_eq!(
            pistes[3].recording_mbid,
            "2d5bd5b4-9d6a-40a0-a708-216da280f837"
        );
    }

    /// Le cœur de la demande : sur « Helpless », Neil Young est MUSICIEN
    /// (guitare, chant) par les relations d'enregistrement, et AUTEUR
    /// (compositeur, parolier) par les relations de l'ŒUVRE — les deux, en une
    /// seule requête, avec son MBID.
    #[test]
    fn helpless_porte_neil_young_musicien_et_auteur_avec_son_mbid() {
        let pistes = pistes_de_la_release(&release());
        let helpless = &pistes[3];
        let de_neil: Vec<&LigneCredit> = helpless
            .lignes
            .iter()
            .filter(|l| l.artist_name == "Neil Young")
            .collect();
        assert!(
            de_neil
                .iter()
                .all(|l| l.artist_mbid.as_deref() == Some(NEIL_YOUNG_MBID)),
            "{de_neil:?}"
        );
        let roles: Vec<&str> = de_neil.iter().map(|l| l.role.as_str()).collect();
        for attendu in ["performer", "vocal", "producer", "composer", "writer"] {
            assert!(roles.contains(&attendu), "{attendu} manque : {roles:?}");
        }
        assert!(
            de_neil
                .iter()
                .any(|l| l.role == "performer" && l.instrument.as_deref() == Some("guitar")),
            "{de_neil:?}"
        );
        // Aucune ligne en double après le dédoublonnage.
        let mut cles: Vec<_> = helpless
            .lignes
            .iter()
            .map(|l| (l.artist_name.clone(), l.role.clone(), l.instrument.clone()))
            .collect();
        let n = cles.len();
        cles.sort();
        cles.dedup();
        assert_eq!(cles.len(), n, "doublons dans {:?}", helpless.lignes);
    }

    /// « Teach Your Children » : Graham Nash auteur, Neil Young seulement
    /// PRODUCTEUR — ni musicien, ni auteur.
    #[test]
    fn teach_your_children_neil_young_producteur_seulement() {
        let pistes = pistes_de_la_release(&release());
        let tyc = &pistes[1];
        assert_eq!(tyc.titre, "Teach Your Children");
        let roles_neil: Vec<&str> = tyc
            .lignes
            .iter()
            .filter(|l| l.artist_name == "Neil Young")
            .map(|l| l.role.as_str())
            .collect();
        assert_eq!(roles_neil, ["producer"]);
        assert!(
            tyc.lignes
                .iter()
                .any(|l| l.artist_name == "Graham Nash" && l.role == "composer")
        );
        assert!(
            tyc.lignes
                .iter()
                .any(|l| l.artist_name == "Graham Nash" && l.role == "writer")
        );
    }

    fn locale(id: i64, disque: i64, numero: i64, titre: &str, mbid: Option<&str>) -> PisteLocale {
        PisteLocale {
            id,
            disque,
            numero,
            titre: titre.into(),
            recording_mbid: mbid.map(str::to_string),
        }
    }

    #[test]
    fn apparier_par_mbid_d_abord_meme_hors_place() {
        let mb = pistes_de_la_release(&release());
        // Une seule piste locale, mal numérotée, mais avec le bon MBID.
        let locales = [locale(
            77,
            1,
            9,
            "Helpless (remaster)",
            Some("2D5BD5B4-9D6A-40A0-A708-216DA280F837"),
        )];
        assert_eq!(apparier(&mb, &locales), vec![(77, 3)]);
    }

    /// Sans MBID, la place ne suffit qu'à nombre de pistes ÉGAL et titres
    /// concordants. Une édition de 11 titres ne reçoit rien : pas d'invention.
    #[test]
    fn apparier_par_place_seulement_a_nombre_egal_et_titre_concordant() {
        let mb = pistes_de_la_release(&release());
        let mut locales: Vec<PisteLocale> = mb
            .iter()
            .enumerate()
            .map(|(i, p)| locale(100 + i as i64, 0, p.numero, &p.titre.to_uppercase(), None))
            .collect();
        // Le 5e titre local ne concorde pas : il reste sans crédit.
        locales[4].titre = "Autre chose".into();
        let couples = apparier(&mb, &locales);
        assert_eq!(couples.len(), 9, "{couples:?}");
        assert!(!couples.iter().any(|(id, _)| *id == 104));
        assert!(couples.contains(&(103, 3)));

        locales.push(locale(200, 1, 11, "Bonus", None));
        assert!(
            apparier(&mb, &locales).is_empty(),
            "11 pistes locales contre 10 : aucun appariement par la place"
        );
    }

    fn credit(
        album_id: i64,
        track_id: i64,
        role: &str,
        instrument: Option<&str>,
    ) -> CreditSurAlbum {
        CreditSurAlbum {
            album_id,
            track_id,
            role: role.into(),
            instrument: instrument.map(str::to_string),
        }
    }

    fn disque(album_id: i64, compilation: bool) -> DisqueAClasser {
        DisqueAClasser {
            album_id,
            compilation,
            artiste_de_piste: false,
        }
    }

    /// Collaborations / Reprises / Écriture / exclusions, disque par disque.
    #[test]
    fn classer_collaborations_reprises_et_exclusions() {
        let credits = [
            // 1 : il joue ET a écrit → Collaboration (audible l'emporte),
            // focus sur les pistes où il joue ; ET Écriture (#7767), focus
            // sur la piste écrite.
            credit(1, 10, "composer", None),
            credit(1, 11, "performer", Some("guitar")),
            credit(1, 11, "vocal", Some("vocals")),
            // 2 : il n'a qu'écrit → Reprise, et Écriture.
            credit(2, 20, "composer", None),
            credit(2, 21, "writer", None),
            // 3 : il joue sur une compilation → ni Collaboration ni Reprise.
            credit(3, 30, "performer", Some("piano")),
            // 4 : compilation de reprises → Reprise, et Écriture.
            credit(4, 40, "composer", None),
            // 5 : producteur seulement → Écriture seule (#7767).
            credit(5, 50, "producer", None),
            // 6 : mastering seul → nulle part.
            credit(6, 60, "mastering", None),
        ];
        let disques = [
            disque(1, false),
            disque(2, false),
            disque(3, true),
            disque(4, true),
            disque(5, false),
            disque(6, false),
        ];
        let c = classer(&credits, &disques);
        assert_eq!(
            c.collaborations,
            vec![AlbumCredite {
                album_id: 1,
                track_ids: vec![11],
                roles: vec!["guitar".into(), "vocals".into()],
            }]
        );
        assert_eq!(
            c.reprises.iter().map(|a| a.album_id).collect::<Vec<_>>(),
            vec![2, 4]
        );
        assert_eq!(c.reprises[0].track_ids, vec![20, 21]);
        assert_eq!(c.reprises[0].roles, vec!["composer", "writer"]);
        assert_eq!(
            c.ecriture.iter().map(|a| a.album_id).collect::<Vec<_>>(),
            vec![1, 2, 4, 5],
            "{:?}",
            c.ecriture
        );
        assert_eq!(c.ecriture[0].track_ids, vec![10]);
        assert_eq!(c.ecriture[0].roles, vec!["composer"]);
        assert_eq!(c.ecriture[3].roles, vec!["producer"]);
    }

    /// Les scénarios d'Aède (#7767), réécrits sur le classement pur :
    /// le remixeur est AUDIBLE (décision du 03/11), l'arrangeur ne l'est pas.
    #[test]
    fn remixeur_dans_apparait_sur_arrangeur_dans_ecriture_seulement_7767() {
        let credits = [
            // 1 : remixeur d'une piste → Apparaît sur, rôle `remixer`.
            credit(1, 10, "remixer", None),
            // 2 : remixeur ET compositeur → Apparaît sur (pas Reprises), et
            // Écriture par la composition.
            credit(2, 20, "remixer", None),
            credit(2, 21, "composer", None),
            // 3 : arrangeur seul → Écriture et production, pas Apparaît sur.
            credit(3, 30, "arranger", None),
            credit(3, 31, "arranger", None),
        ];
        let disques = [disque(1, false), disque(2, false), disque(3, false)];
        let c = classer(&credits, &disques);
        assert_eq!(
            c.collaborations
                .iter()
                .map(|a| a.album_id)
                .collect::<Vec<_>>(),
            vec![1, 2],
            "{:?}",
            c.collaborations
        );
        assert_eq!(c.collaborations[0].roles, vec!["remixer"]);
        assert!(c.reprises.is_empty(), "{:?}", c.reprises);
        assert_eq!(
            c.ecriture.iter().map(|a| a.album_id).collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert_eq!(c.ecriture[1].track_ids, vec![30, 31]);
        assert_eq!(c.ecriture[1].roles, vec!["arranger"]);
    }

    /// Un disque où il est l'artiste d'une piste est déjà dans « Apparitions »
    /// ou « Compilations » : ni Apparaît sur ni Reprises — mais Écriture et
    /// production le garde (#7767 : il produit *Second Session* et y joue).
    #[test]
    fn artiste_de_piste_garde_l_ecriture_mais_pas_les_deux_autres_sections_7767() {
        let credits = [
            credit(1, 10, "performer", Some("bass")),
            credit(1, 10, "producer", None),
            credit(1, 11, "producer", None),
            credit(2, 20, "composer", None),
        ];
        let disques = [
            DisqueAClasser {
                album_id: 1,
                compilation: false,
                artiste_de_piste: true,
            },
            DisqueAClasser {
                album_id: 2,
                compilation: false,
                artiste_de_piste: true,
            },
        ];
        let c = classer(&credits, &disques);
        assert!(c.collaborations.is_empty(), "{:?}", c.collaborations);
        assert!(c.reprises.is_empty(), "{:?}", c.reprises);
        assert_eq!(
            c.ecriture,
            vec![
                AlbumCredite {
                    album_id: 1,
                    track_ids: vec![10, 11],
                    roles: vec!["producer".into()],
                },
                AlbumCredite {
                    album_id: 2,
                    track_ids: vec![20],
                    roles: vec!["composer".into()],
                },
            ]
        );
    }

    /// La table unique des rôles audibles, et les deux autres : aucun rôle
    /// n'est à la fois audible et d'écriture, l'arrangeur n'est pas audible.
    #[test]
    fn tables_de_roles_7767() {
        assert_eq!(
            ROLES_AUDIBLES,
            ["artist", "performer", "vocal", "conductor", "remixer"]
        );
        assert_eq!(
            ROLES_D_ECRITURE,
            [
                "composer", "writer", "arranger", "producer", "engineer", "mixer"
            ]
        );
        assert!(ROLES_AUDIBLES.iter().all(|r| !est_role_d_ecriture(r)));
        assert!(ROLES_D_AUTEUR.iter().all(|r| est_role_d_ecriture(r)));
        assert!(!est_role_audible("arranger"));
        assert!(!est_role_d_ecriture("mastering") && !est_role_d_ecriture("programming"));
        assert_eq!(liste_sql(ROLES_D_AUTEUR), "'composer', 'writer'");
    }

    #[test]
    fn artistes_divers_reconnus() {
        for n in ["Various Artists", " various ", "VA", "Compilations"] {
            assert!(est_artistes_divers(n), "{n}");
        }
        assert!(!est_artistes_divers("Neil Young"));
    }

    /// 🔴 UN SEUL limiteur MusicBrainz. La passe des crédits attend son
    /// créneau par `rate_limit_delay`, qui réserve dans le limiteur PARTAGÉ
    /// `http::fetch::MUSICBRAINZ` sous la MÊME clé que les pochettes et images
    /// d'artistes. Preuve par le comportement : juste après un
    /// `rate_limit_delay()`, un `acquire` direct sur ce limiteur et cette clé
    /// doit attendre ~1 s. Un second limiteur, ou une autre clé, rendrait la
    /// main tout de suite — et deux flux parallèles doubleraient le débit.
    #[tokio::test]
    async fn la_passe_des_credits_emprunte_le_limiteur_partage_des_pochettes() {
        use super::super::musicbrainz_release::{CLE_LIMITEUR_MUSICBRAINZ, rate_limit_delay};
        rate_limit_delay().await;
        let debut = std::time::Instant::now();
        crate::http::fetch::MUSICBRAINZ
            .acquire(CLE_LIMITEUR_MUSICBRAINZ)
            .await;
        assert!(
            debut.elapsed() >= std::time::Duration::from_millis(900),
            "rate_limit_delay ne réserve pas dans le limiteur partagé : {:?}",
            debut.elapsed()
        );

        // Et les pochettes : depuis #7667, leurs requêtes MusicBrainz passent
        // par la porte `images_rapides` (plus d'`acquire` en ligne dans
        // `artwork.rs`). Le contrat déménage avec elles : la porte de
        // l'hôte MusicBrainz est CE limiteur sous CETTE clé, et `artwork.rs`
        // n'interroge MusicBrainz que par elle.
        let (limiteur, cle) = crate::library::images_rapides::Hote::MusicBrainz.limiteur();
        assert!(
            std::ptr::eq(limiteur, &*crate::http::fetch::MUSICBRAINZ)
                && cle == CLE_LIMITEUR_MUSICBRAINZ,
            "les pochettes ont changé de limiteur ou de clé : la passe des crédits ne partage plus leur créneau"
        );
        let pochettes = include_str!("../library/artwork.rs");
        assert!(
            pochettes.contains("images_rapides::json(")
                && !pochettes.contains("MUSICBRAINZ.acquire(")
                && !pochettes.contains("RateLimiter::"),
            "les pochettes interrogent MusicBrainz hors de la porte partagée"
        );
        let ici = include_str!("credits_release.rs");
        let corps = &ici[..ici.find("#[cfg(test)]").unwrap()];
        assert!(corps.contains("musicbrainz_release::rate_limit_delay().await"));
        for interdit in ["tokio::time::sleep", "RateLimiter::"] {
            assert!(
                !corps.contains(interdit),
                "second mécanisme de cadence : {interdit}"
            );
        }
    }
}
