//! Background ReplayGain analysis.
//!
//! The scan reads ReplayGain tags straight from the file (fast, no decode) but
//! most files have none. This pass FILLS the missing values by measuring EBU
//! R128 loudness — which requires decoding the whole file, far too expensive to
//! do inline in the scan (58k-file libraries already draw "scan interminable"
//! complaints). So it runs as a throttled, resumable background task, entirely
//! separate from the scan walk: the scan stays tag-only and fast, the heavy
//! calculation lives here.
//!
//! Written to `track_metadata` as `rg_track_gain` / `rg_track_peak` (+ album
//! variants), matching the keys `metadata::read_extended_metadata` uses for
//! file-tag ReplayGain — so the two are interchangeable downstream. A file's own
//! ReplayGain tags always win: a track that already has `rg_track_gain` is never
//! recomputed.
// Code audio (décodage, analyse, traitement du signal) : les boucles indexées
// et les découpes par `chunks_exact` y sont gardées telles quelles. Les récrire
// (`as_chunks`, itérateurs, `repeat_n`) ne changerait rien au son mais toucherait
// la logique audio pour un gain de forme (clippy 1.98).
#![allow(clippy::chunks_exact_to_as_chunks)]

/// Ce que la passe dit d'elle-même pendant qu'elle travaille (#4144).
///
/// Déclaré ici plutôt que dans `audio/mod.rs` : l'avancement n'a de sens que
/// pour cette passe-ci, et rien d'autre ne doit l'écrire.
pub mod progression;

/// La mesure de la plage dynamique À LA DEMANDE (#4185) : le geste qui
/// manquait. La cascade de fond ci-dessous (ReplayGain → empreintes → plage
/// dynamique) reste la passe nominale ; ce module en est le raccourci, borné
/// au seul DR, que l'utilisateur peut lancer et suivre.
pub mod plage_dynamique;

/// Le couple « analysées / éligibles » de la BIBLIOTHÈQUE (#5597) : ce que la
/// jauge doit dire après un redémarrage, quand la campagne repart de zéro.
pub mod bibliotheque;

use crate::audio::ecretage::CompteurDEcretage;
use crate::db::backend::{DbBackend, DbTxHandle, ToSqlValue};
use crate::db::settings_repo::SettingsRepo;
use crate::db::track_metadata_repo::TrackMetadataRepo;
use crate::library::local_path::{
    LocalPath, deferral_stamp, deferral_threshold, resolve_local_path,
};
use std::sync::Arc;
use std::time::SystemTime;
use tracing::{debug, info, warn};

/// ReplayGain 2.0 reference loudness. `track_gain = REFERENCE_LUFS - measured`.
pub const REFERENCE_LUFS: f64 = -18.0;

/// Tracks analysed per wake-up before the loop sleeps again. Small so the pass
/// never monopolises the CPU on a big library — it chips away over time.
const TRACK_BATCH: usize = 25;

/// Provenance d'un `dr_track` CALCULÉ par cette passe, par opposition à celui
/// lu dans les tags du fichier au scan. Voir l'écriture dans
/// `analyze_track_batch`.
///
/// Jumeau de [`crate::metadata::DR_SOURCE_TAG`], écrit par le scan sur la
/// valeur qu'il LIT dans le fichier (#3924). Les deux producteurs de
/// `dr_track` marquent désormais la clef `dr_source` ; sans le second, une
/// valeur non marquée ne se distinguait pas d'une valeur d'avant la clef.
const DR_SOURCE_ANALYSIS: &str = "analysis";

/// #5594 (lot 2) — la clé de `track_metadata` qui dit QUEL algorithme a
/// produit la mesure ReplayGain de piste (`rg_track_gain`, `rg_track_peak`,
/// `rg_track_true_peak`).
///
/// Posée par la passe d'analyse, au moment de la mesure, et par elle seule.
/// Une mesure faite avant cette clé reste SANS version : rien ne permet de
/// dire après coup quel code l'a produite, et on n'invente pas. La clé ne se
/// lit qu'avec `rg_track_source = analysis` : un gain venu des tags du
/// fichier n'a pas de version Tune.
pub const RG_ALGO_KEY: &str = "rg_algo";

/// La version de la mesure ReplayGain de Tune : sonie intégrée EBU R128 /
/// ITU-R BS.1770 (pondération K, double porte), pic d'échantillon, et true
/// peak par suréchantillonnage 4× (#1694). À changer dès qu'une valeur
/// rendue pour le même signal change — c'est ce qui permettra de ne comparer
/// que des mesures comparables entre deux instances.
pub const RG_ALGO: &str = "bs1770-tp4x-v1";

/// #5594 (lot 2) — la clé de `track_metadata` qui dit quel algorithme a
/// produit `dr_track`. Même règle que [`RG_ALGO_KEY`] : posée à la mesure,
/// jamais rétroactivement, et lue seulement avec `dr_source = analysis`.
pub const DR_ALGO_KEY: &str = "dr_algo";

/// La version de la plage dynamique de Tune : la méthode du TT DR Meter
/// (blocs de 3 s, 20 % des blocs les plus forts, deuxième pic), arrondie à
/// l'entier.
pub const DR_ALGO: &str = "tt-dr-v1";

/// La plage calculée a-t-elle le droit de s'écrire ?
///
/// 🔴 LE TAG DU FICHIER FAIT FOI. `dr_track` a DEUX producteurs : le scan, qui
/// lit `DYNAMIC RANGE` dans le fichier (`metadata/mod.rs`), et cette passe, qui
/// le calcule. Le premier porte la valeur que le producteur du disque a
/// mesurée ; la remplacer par une estimation perdrait une donnée d'origine.
///
/// ⚠️ Une valeur VIDE n'est pas une valeur. Un tag présent mais vide
/// (`DYNAMIC RANGE=`) existe sur des fichiers mal étiquetés ; le traiter comme
/// « déjà tagué » condamnerait ces pistes à n'avoir jamais de DR, ni lu ni
/// calculé.
///
/// Fonction à part, et non un `if` dans la boucle : une garde écrite contre la
/// boucle devrait monter une base, des fichiers et un décodeur pour juger deux
/// lignes de condition. Ici elle APPELLE la décision.
pub(crate) fn peut_ecrire_le_dr(existant: Option<&str>) -> bool {
    !existant.is_some_and(|v| !v.trim().is_empty())
}

/// Poser une plage dynamique MESURÉE, seulement dans le vide.
///
/// 🔴 LE TAG DU FICHIER FAIT FOI, TOUJOURS. `dr_track` peut déjà porter la
/// valeur lue dans `DYNAMIC RANGE` au scan (`metadata/mod.rs`) : c'est celle
/// que le producteur du disque a mesurée, et l'écraser par la nôtre
/// remplacerait une donnée d'origine par une estimation. On relit donc juste
/// avant d'écrire — un scan a pu en poser un PENDANT le décodage.
///
/// `dr_source` dit d'où vient ce qui est en base : sans lui, une valeur
/// calculée s'afficherait comme une valeur du disque, ce qui serait un
/// affichage inventé.
///
/// Synchrone, et appelée HORS du fil async (#4681) : la passe de piste comme
/// le rattrapage la font partir sur le pool bloquant.
fn ecrire_le_dr_mesure(repo: &TrackMetadataRepo, track_id: i64, dr: u32) {
    // `get_all` : le dépôt n'expose pas de lecture d'UNE clé.
    let existant = repo
        .get_all(track_id)
        .ok()
        .and_then(|m| m.get("dr_track").cloned());
    if peut_ecrire_le_dr(existant.as_deref()) {
        let _ = repo.set(track_id, "dr_track", &dr.to_string());
        let _ = repo.set(track_id, "dr_source", DR_SOURCE_ANALYSIS);
        // #5594 — la version de l'algorithme, écrite avec la mesure.
        let _ = repo.set(track_id, DR_ALGO_KEY, DR_ALGO);
    }
}

/// Ce que la passe nominale doit écrire pour UNE piste, calculé pendant le
/// tour et écrit à sa fin, avec celles des autres pistes du tour (#5519).
///
/// Les champs suivent l'ordre des écritures d'avant, qui partaient une à une
/// au fil de la piste : report effacé ou posé, mesure, empreinte, témoin.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EcrituresDePiste {
    pub track_id: i64,
    /// Le fichier répond : un report (#1865) qui traînait est effacé.
    pub effacer_le_report: bool,
    /// Le fichier ne répond pas : report daté (#1865), et rien d'autre.
    pub report: Option<String>,
    /// `(lufs, crête, crête vraie, plage dynamique)`, quand la mesure a abouti.
    pub mesure: Option<(f64, f64, f64, Option<u32>)>,
    /// L'empreinte du contenu, ou sa marque « pas d'empreinte possible ».
    pub empreinte: Option<String>,
    /// Le témoin `rg_analyzed` (heure unix) : la piste quitte le balayage.
    pub temoin: Option<String>,
}

impl EcrituresDePiste {
    fn pour(track_id: i64) -> Self {
        Self {
            track_id,
            ..Self::default()
        }
    }

    fn est_vide(&self) -> bool {
        !self.effacer_le_report
            && self.report.is_none()
            && self.mesure.is_none()
            && self.empreinte.is_none()
            && self.temoin.is_none()
    }
}

/// Les mêmes écritures qu'avant, dans le même ordre, à travers `tx`.
///
/// La première erreur est rendue : dans la transaction du tour, elle l'annule
/// tout entière et le tour repasse par l'écriture piste à piste
/// ([`ecrire_le_tour`]).
fn appliquer_les_ecritures(tx: &dyn DbTxHandle, e: &EcrituresDePiste) -> Result<(), String> {
    const UPSERT: &str = "INSERT INTO track_metadata (track_id, key, value) VALUES (?, ?, ?) \
                          ON CONFLICT (track_id, key) DO UPDATE SET value = excluded.value";
    let id = e.track_id;
    let poser = |cle: &str, valeur: &str| -> Result<(), String> {
        tx.execute(
            UPSERT,
            &[
                &id as &dyn ToSqlValue,
                &cle as &dyn ToSqlValue,
                &valeur as &dyn ToSqlValue,
            ],
        )
        .map(|_| ())
    };
    if e.effacer_le_report {
        tx.execute(
            "DELETE FROM track_metadata WHERE track_id = ? AND key = ?",
            &[
                &id as &dyn ToSqlValue,
                &PATH_UNRESOLVED_KEY as &dyn ToSqlValue,
            ],
        )?;
    }
    if let Some(report) = &e.report {
        poser(PATH_UNRESOLVED_KEY, report)?;
    }
    if let Some((lufs, peak, true_peak, plage)) = e.mesure {
        poser("rg_track_gain", &format_gain(track_gain_db(lufs)))?;
        poser("rg_track_peak", &format_peak(peak))?;
        // True peak inter-échantillons 4× (#1694). Clé à part : `rg_track_peak`
        // garde sa sémantique sample-peak (compat tags, #1382) ;
        // `prevent_clipping` PRÉFÈRE celle-ci quand elle existe. Peut dépasser
        // 1.0 — c'est l'information.
        poser("rg_track_true_peak", &format_peak(true_peak))?;
        // Témoin de PROVENANCE (#1627). Sans lui, rien ne distingue en base un
        // gain MESURÉ ici d'un gain lu dans les tags du fichier : les deux
        // s'écrivent sous `rg_track_gain`, et c'est voulu (interchangeables à
        // la lecture). Mais le chemin du signal doit pouvoir dire d'où vient
        // le gain qu'il applique, et « tags du fichier » affiché sur une
        // mesure Tune serait un affichage inventé.
        poser(TRACK_SOURCE_KEY, SOURCE_ANALYSIS)?;
        // #5594 — la version de l'algorithme qui a produit ces trois
        // valeurs, écrite avec elles. Une mesure d'avant cette clé reste sans
        // version.
        poser(RG_ALGO_KEY, RG_ALGO)?;
        // ── PLAGE DYNAMIQUE ──────────────────────────────────────────────
        //
        // Elle voyage avec ce décodage-ci : la mesure la calcule sur les
        // mêmes échantillons. Le tag du fichier fait foi
        // ([`peut_ecrire_le_dr`]) : relu ICI, dans la transaction, juste
        // avant d'écrire — un scan a pu en poser un pendant le décodage.
        if let Some(dr) = plage {
            let existant = tx
                .query_one(
                    "SELECT value FROM track_metadata WHERE track_id = ? AND key = 'dr_track'",
                    &[&id as &dyn ToSqlValue],
                )?
                .and_then(|r| r.first().and_then(|v| v.as_string()));
            if peut_ecrire_le_dr(existant.as_deref()) {
                poser("dr_track", &dr.to_string())?;
                poser("dr_source", DR_SOURCE_ANALYSIS)?;
                // #5594 — la version de l'algorithme, écrite avec la mesure.
                poser(DR_ALGO_KEY, DR_ALGO)?;
            }
        }
    }
    if let Some(empreinte) = &e.empreinte {
        tx.execute(
            "UPDATE tracks SET audio_fingerprint = ? WHERE id = ?",
            &[
                &empreinte.as_str() as &dyn ToSqlValue,
                &id as &dyn ToSqlValue,
            ],
        )?;
    }
    if let Some(temoin) = &e.temoin {
        poser("rg_analyzed", temoin)?;
    }
    Ok(())
}

/// Les écritures d'un tour, hors transaction : chaque instruction part seule
/// et une erreur n'arrête pas les suivantes — exactement l'écriture d'avant
/// le regroupement (`let _ = repo.set(…)`).
struct SansTransaction<'a>(&'a dyn DbBackend);

impl DbTxHandle for SansTransaction<'_> {
    fn execute(&self, sql: &str, params: &[&dyn ToSqlValue]) -> Result<usize, String> {
        // Erreur avalée, comme avant : la suite de la piste s'écrit quand même.
        Ok(self.0.execute(sql, params).unwrap_or(0))
    }
    fn query_one(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Option<Vec<crate::db::backend::SqlValue>>, String> {
        Ok(self.0.query_one(sql, params).ok().flatten())
    }
    fn query_many(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Vec<Vec<crate::db::backend::SqlValue>>, String> {
        Ok(self.0.query_many(sql, params).unwrap_or_default())
    }
    fn last_insert_rowid(&self) -> i64 {
        self.0.last_insert_rowid()
    }
}

/// Comment se sont écrites les pistes d'un tour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EcritureDuTour {
    /// Rien à écrire.
    Rien,
    /// Une seule transaction pour tout le tour.
    Groupee,
    /// La transaction a échoué : chaque écriture est partie seule, comme
    /// avant le regroupement.
    PisteAPiste,
}

/// Écrire en UNE transaction tout ce que les pistes d'un tour ont produit
/// (#5519).
///
/// Avant, chaque piste écrivait au fil de l'eau : cinq à neuf instructions,
/// chacune sa propre transaction implicite, son `COMMIT` et sa prise du
/// verrou d'écriture — environ 200 par tour de 25 pistes. Ici, tout le calcul
/// est déjà fait : la transaction n'enchaîne que des écritures de clés,
/// quelques millisecondes pour un tour entier (mesure dans la PR).
///
/// **Le verrou d'écriture n'est pas tenu longtemps**, et c'est voulu :
/// - la transaction ne s'ouvre qu'après le dernier décodage du tour, jamais
///   pendant ; elle ne traverse aucun `await` ;
/// - sur SQLite, `write_tx` prend la connexion comme toute écriture : si un
///   lot de scan a sa transaction ouverte, elle attend sa prochaine cession
///   (#5202, #5798), puis passe en une fois au lieu de deux cents ;
/// - le `COMMIT` ne replie plus le WAL sur la connexion d'écriture : le
///   replieur s'en charge sur la sienne (#5804). Un `COMMIT` par tour au lieu
///   de deux cents, c'est aussi deux cents fois moins de pages de WAL
///   validées une à une.
///
/// Si la transaction échoue (une transaction orpheline restée ouverte sur la
/// connexion, par exemple), le tour repasse par l'écriture d'avant, piste à
/// piste, erreurs avalées : un échec du regroupement ne perd aucune mesure.
///
/// Synchrone : à appeler HORS du fil async (#4681).
pub fn ecrire_le_tour(
    backend: &Arc<dyn DbBackend>,
    ecritures: &[EcrituresDePiste],
) -> EcritureDuTour {
    let a_ecrire: Vec<&EcrituresDePiste> = ecritures.iter().filter(|e| !e.est_vide()).collect();
    if a_ecrire.is_empty() {
        return EcritureDuTour::Rien;
    }
    let groupee = backend.write_tx(&mut |tx| {
        for e in &a_ecrire {
            appliquer_les_ecritures(tx, e)?;
        }
        Ok(())
    });
    match groupee {
        Ok(()) => EcritureDuTour::Groupee,
        Err(erreur) => {
            warn!(
                error = %erreur,
                pistes = a_ecrire.len(),
                "replaygain_ecriture_groupee_echouee — repli piste a piste"
            );
            let seule = SansTransaction(backend.as_ref());
            for e in &a_ecrire {
                let _ = appliquer_les_ecritures(&seule, e);
            }
            EcritureDuTour::PisteAPiste
        }
    }
}

// #5519 — la pause fixe de 400 ms entre deux fichiers (`PER_FILE_PAUSE_MS`)
// a été RETIRÉE (décision de Bertrand, 30/09/2026) : 21 % du temps de la passe,
// mesuré sur Shrek. Ce qui garde la machine et la lecture ne dépendait pas
// d'elle : la priorité à la lecture (#1310, #2495), la garde thermique
// (#1576) et la vitesse réglée (`taches_de_fond::vitesse`).

/// How long the loop sleeps once there is nothing left to analyse.
const IDLE_SLEEP_SECS: u64 = 900;

/// Témoin de REPORT — à ne pas confondre avec `rg_analyzed`.
///
/// `rg_analyzed` veut dire « on a essayé, n'y revenons pas ». Il était posé
/// même quand le fichier était introuvable, ce qui gelait définitivement des
/// pistes parfaitement saines : chemin stocké en NFC, fichier sur le disque en
/// NFD (#1865), ou simplement partage démonté au mauvais moment. Sur .18,
/// 114 pistes portaient `rg_analyzed` pour **zéro** `rg_track_gain` calculé.
///
/// Cette clé-ci dit autre chose : « aucune graphie ne répondait à telle date ».
/// Elle écarte la piste du balayage — sans quoi les 135 pistes concernées, plus
/// nombreuses que `TRACK_BATCH`, bloqueraient la passe entière sur les mêmes
/// lignes — mais elle **périme** au bout de
/// [`crate::library::local_path::PATH_RETRY_AFTER_SECS`]. Un disque rebranché
/// est repris tout seul.
const PATH_UNRESOLVED_KEY: &str = "rg_path_unresolved";

/// Le marqueur que posait l'ancien plafond `MAX_ANALYSIS_EST_BYTES` (#1109) :
/// « piste trop grosse pour être décodée en mémoire ». Plus aucun code ne le
/// pose (plafond-analyse) ; il ne sert plus qu'à retrouver, au démarrage, les
/// pistes qu'il a écartées — voir [`reprendre_les_pistes_ecartees_pour_leur_taille`].
///
/// Ce plafond (1,2 Go estimés à 12 o par échantillon) datait d'avant
/// l'analyse par segments de 30 s : il écartait tout 24/192 stéréo de plus de
/// 4 min 20 et tout DSD64 de plus de 4 min 40. Mesuré sur Shrek le 05/10/2026
/// (`examples/banc_memoire_analyse.rs`), le pic de mémoire de l'analyse d'un
/// FLAC ou d'un WAV 24/192 ne dépend plus de la durée (≈ 390 Mio pour 5, 20 et
/// 60 min : c'est la tête de 90 s de l'empreinte). Seul le DSD croissait
/// encore, parce que chaque segment re-décodait le fichier depuis son début ;
/// c'est corrigé dans `decode_dsd_to_pcm` (reprise au bloc). Ce qui croît
/// encore avec la durée — un `f64` par bloc de 100 ms pour la sonie intégrée,
/// un par bloc de 3 s et par canal pour la plage dynamique — pèse 288 Kio pour
/// une heure : rien qui justifie un plafond.
const OVERSIZED_KEY: &str = "rg_skipped_oversized";

/// A single file must never stall the whole sweep. `measure_loudness_and_peak`
/// decodes in segments via `spawn_blocking`; a pathological file (corrupt FLAC,
/// symphonia decode loop) or a dormant NAS mount can make a segment hang and
/// never return — the pass then gets stuck on that one file forever
/// (« n'avance plus », Bilou #1155). Bound each track: on timeout we log, stamp
/// it analysed and move on. Generous vs. a normal streaming analysis (seconds,
/// up to ~2 min for a very long hi-res track), tight vs. an indefinite hang.
///
/// C'est le délai PLANCHER : [`delai_d_analyse`] y ajoute la durée de la piste.
const PER_TRACK_ANALYSIS_TIMEOUT_SECS: u64 = 180;

/// Le délai accordé à l'analyse d'UNE piste : le plancher de 180 s, plus la
/// durée de la piste elle-même (plafond-analyse).
///
/// Un délai fixe de 180 s ne tenait que parce que le plafond de taille
/// écartait les longues pistes haute résolution. Sans lui, une heure de
/// 24/192 (80 s d'analyse sur Shrek chargé) ou vingt minutes de DSD256
/// dépasseraient 180 s sur un Pi 4 — et une piste qui dépasse son délai est
/// estampillée analysée SANS gain : le même trou qu'avant, en silence. La
/// règle retenue : un hôte qui analyse au moins au temps réel — celui qu'il
/// faut déjà pour LIRE la piste — finit toujours. Un vrai blocage (#1155)
/// reste borné : à la durée de la piste plus trois minutes. Durée inconnue :
/// le plancher seul, comme avant.
fn delai_d_analyse(duration_ms: Option<i64>) -> std::time::Duration {
    let duree_s = duration_ms
        .filter(|&ms| ms > 0)
        .map_or(0, |ms| ms as u64 / 1000);
    std::time::Duration::from_secs(PER_TRACK_ANALYSIS_TIMEOUT_SECS.saturating_add(duree_s))
}

/// L'estimation de l'ancien plafond, FIGÉE en SQL : 12 o par échantillon, la
/// cadence DSD ramenée à celle du décodage, CD stéréo à défaut, au-delà de
/// 1,2 Go. Elle ne sert qu'à retrouver les pistes que le rattrapage de la
/// plage dynamique a marquées `dr_indisponible` pour leur seule taille.
const ANCIEN_PLAFOND_DEPASSE_SQL: &str = "t.duration_ms > 0 \
     AND CAST(t.duration_ms AS BIGINT) / 1000 \
       * (CASE WHEN COALESCE(t.sample_rate, 0) <= 0 THEN 44100 \
               WHEN t.sample_rate > 768000 THEN \
                 CASE WHEN t.sample_rate >= 5000000 THEN 352800 ELSE 176400 END \
               ELSE t.sample_rate END) \
       * (CASE WHEN COALESCE(t.channels, 0) > 0 THEN t.channels ELSE 2 END) \
       * 12 > 1200000000";

/// Rattrapage des pistes que l'ancien plafond de taille a écartées
/// (plafond-analyse). Rejoué à chaque démarrage de la passe, IDEMPOTENT : ce
/// ne sont que des `DELETE`, et une fois les marqueurs partis il ne trouve
/// plus rien. Rend le nombre de lignes effacées.
///
/// * le témoin `rg_analyzed` qu'il posait SANS gain sur une piste marquée
///   [`OVERSIZED_KEY`] — la piste redevient candidate de la passe nominale, qui
///   mesurera gain, crêtes ET plage dynamique ;
/// * la marque `dr_indisponible` que le rattrapage de la plage dynamique
///   posait pour la même raison (pistes au gain lu dans les tags, jamais
///   décodées) : la piste redevient candidate du rattrapage. Ce n'est pas
///   distinguable d'un vrai échec sur un long fichier haute résolution : un
///   tel fichier sera réessayé UNE fois, puis re-marqué. Le prix est borné ;
/// * le marqueur [`OVERSIZED_KEY`] lui-même, en dernier : tant qu'il reste,
///   le premier `DELETE` le retrouve au démarrage suivant.
///
/// Les gains, crêtes et plages DÉJÀ présents ne sont jamais touchés.
pub fn reprendre_les_pistes_ecartees_pour_leur_taille(
    backend: &Arc<dyn DbBackend>,
) -> Result<usize, String> {
    let temoins = backend.execute(
        &format!(
            "DELETE FROM track_metadata WHERE key = 'rg_analyzed' \
             AND track_id IN (SELECT s.track_id FROM track_metadata s WHERE s.key = '{OVERSIZED_KEY}') \
             AND track_id NOT IN (SELECT g.track_id FROM track_metadata g WHERE g.key = 'rg_track_gain')"
        ),
        &[],
    )?;
    let plages = backend.execute(
        &format!(
            "DELETE FROM track_metadata WHERE key = '{DR_INDISPONIBLE_KEY}' \
             AND track_id IN (SELECT t.id FROM tracks t WHERE {ANCIEN_PLAFOND_DEPASSE_SQL})"
        ),
        &[],
    )?;
    let marqueurs = backend.execute(
        &format!("DELETE FROM track_metadata WHERE key = '{OVERSIZED_KEY}'"),
        &[],
    )?;
    if temoins + plages + marqueurs > 0 {
        info!(
            temoins,
            plages,
            marqueurs,
            "replaygain_reprise_des_pistes_trop_grosses — l'ancien plafond de taille est levé, \
             ces pistes redeviennent candidates (plafond-analyse)"
        );
    }
    Ok(temoins + plages + marqueurs)
}

/// Attente entre deux vérifications quand la machine est trop chaude (#1576).
const THERMAL_RETRY_SECS: u64 = 120;

/// Attente entre deux relectures du drapeau quand la cascade est SUSPENDUE par
/// l'utilisateur.
///
/// Ni les 2 s du tour actif (une passe garée toute la soirée relirait deux
/// réglages par seconde pour rien) ni les 900 s du repos (« Reprendre » ne doit
/// pas mettre un quart d'heure à se voir). Cinq secondes : la reprise est
/// perçue comme immédiate, et le coût d'une nuit de pause est de quelques
/// milliers de lectures triviales.
const SIESTE_EN_PAUSE_SECS: u64 = 5;

/// Cadence à laquelle on regarde si une zone s'est mise à jouer PENDANT
/// l'analyse d'un fichier (#2495).
///
/// 250 ms est court devant le temps de démarrage qu'on protège (Thierry
/// Clemont : 6 782 ms pour résoudre une piste locale pendant le balayage,
/// contre 149 ms pour une piste Qobuz dans le même journal) et long devant le
/// coût de la vérification, un `SELECT ... LIMIT 1` sur `zones` — quatre
/// requêtes par seconde et par fichier en cours, uniquement pendant qu'un
/// fichier est effectivement en cours d'analyse.
const VEILLE_LECTURE_MS: u64 = 250;

/// Ce qu'il advient d'une analyse qui peut être abandonnée au profit de la
/// lecture.
///
/// Le point n'est pas le type, c'est le contrat qu'il impose au site d'appel :
/// il devient impossible d'écrire la suite sans dire ce qu'on fait du cas
/// « cédée ». C'est précisément ce qui manquait — une piste abandonnée ne doit
/// SURTOUT pas être estampillée `rg_analyzed`, sinon elle sort du balayage pour
/// toujours et ne sera jamais mesurée.
pub(crate) enum Issue<T> {
    Terminee(T),
    CedeeALaLecture,
}

/// Attendre qu'une zone se mette à jouer.
///
/// Vérifie AVANT de dormir : entre le garde-fou d'entrée de fichier et le
/// premier octet décodé, il y a la résolution du chemin — trois `stat` qui,
/// sur un montage réseau endormi, ne sont pas instantanés. La lecture peut
/// démarrer dans cette fenêtre-là aussi.
async fn veiller_lecture(backend: Arc<dyn DbBackend>) {
    loop {
        if let Some(zone) = playing_zone_name(&backend) {
            debug!(zone = %zone, "replaygain_veille_lecture — zone passee a playing");
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(VEILLE_LECTURE_MS)).await;
    }
}

/// Courir l'analyse d'UN fichier contre l'arrivée de la lecture.
///
/// `biased` n'est pas cosmétique : quand les deux futurs sont prêts au même
/// réveil, la lecture doit gagner. Sans lui, `select!` tirerait au sort, et le
/// garde-fou serait probabiliste.
///
/// Ce qu'on gagne, et rien de plus : `spawn_blocking` ne se rétracte pas. Le
/// segment de 30 s déjà parti dans le pool bloquant ira jusqu'à son terme dans
/// le vide. Ce qu'on cesse de faire, c'est de l'ATTENDRE — et surtout
/// d'enchaîner les segments suivants, puis le fichier suivant. La fenêtre
/// passe donc d'un fichier entier (borné à
/// [`PER_TRACK_ANALYSIS_TIMEOUT_SECS`], soit 180 s) à un segment. Le journal ne
/// doit pas laisser croire à une interruption immédiate.
pub(crate) async fn analyser_ou_ceder<T>(
    travail: impl std::future::Future<Output = T>,
    ceder: impl std::future::Future<Output = ()>,
) -> Issue<T> {
    tokio::select! {
        biased;
        () = ceder => Issue::CedeeALaLecture,
        resultat = travail => Issue::Terminee(resultat),
    }
}

/// Mesurer une piste sans jamais faire attendre la lecture (#2495).
///
/// Le site d'appel passait par un simple `tokio::time::timeout` : une fois
/// `measure_loudness_and_peak` lancée, plus rien ne pouvait la rendre avant
/// 180 s. Le garde-fou existait bien, mais il ne s'exerçait qu'ENTRE deux
/// fichiers — inutile quand un seul fichier de 4,5 Go sur partage réseau tient
/// le chemin d'E/S pendant des minutes.
pub(crate) async fn mesurer_en_cedant_a_la_lecture<T>(
    backend: &Arc<dyn DbBackend>,
    delai: std::time::Duration,
    travail: impl std::future::Future<Output = T>,
) -> Issue<Result<T, tokio::time::error::Elapsed>> {
    analyser_ou_ceder(
        tokio::time::timeout(delai, travail),
        veiller_lecture(backend.clone()),
    )
    .await
}

/// How long the sweep backs off after finding a zone actively playing. The
/// track pass fully decodes files — often over a network (SMB/NAS) mount — and
/// on a busy link that starves the same disk/network the player reads from,
/// stalling the audio pipeline (#1310, « la musique s'arrête au premier
/// morceau »). The pass yields entirely while anything plays, then rechecks.
/// Shared with the audio-embedding sweep (#1515), which obeys the same rule.
pub(crate) const PLAYBACK_BACKOFF_SECS: u64 = 30;

/// Réglage propre à la PASSE D'ANALYSE : absent/"true" ⇒ autorisée,
/// "false" ⇒ coupée. Il ne décide pas seul — voir [`analysis_enabled`].
pub const ANALYSIS_ENABLED_KEY: &str = "replaygain_analysis_enabled";

/// Format a gain the way ReplayGain tags do, e.g. `-6.50 dB`.
pub fn format_gain(db: f64) -> String {
    format!("{:.2} dB", db)
}

/// Format a linear peak (0.0–1.0), e.g. `0.988553`.
pub fn format_peak(peak: f64) -> String {
    format!("{:.6}", peak)
}

/// `track_gain = REFERENCE_LUFS - measured_lufs`.
pub fn track_gain_db(lufs: f64) -> f64 {
    REFERENCE_LUFS - lufs
}

/// La passe d'analyse a-t-elle le droit de décoder ?
///
/// DEUX réglages la commandent, et les confondre est tout le défaut #2496 :
///
/// * [`MODE_KEY`] (`replaygain_mode`) — le sélecteur dont la première valeur
///   s'affiche « Désactivé (niveau source) ». La boucle ne l'a JAMAIS lu : qui
///   choisissait « Désactivé » n'arrêtait que l'APPLICATION du gain à la
///   lecture, pendant que le balayage continuait de décoder la bibliothèque
///   entière — CPU, disque, et sur un partage réseau chargé des démarrages de
///   lecture à 6,8 s là où un flux distant partait en 0,15 s (#2495).
/// * [`ANALYSIS_ENABLED_KEY`] (`replaygain_analysis_enabled`) — la coche
///   « Analyse ReplayGain », qui coupe la passe même quand un mode est armé.
///
/// Règle : la passe tourne quand un mode est demandé ET que la coche n'a pas
/// été décochée. « Désactivé » arrête donc bien le balayage. C'est la voie A
/// de #2496 : on renonce au pré-remplissage silencieux — armer ReplayGain plus
/// tard redevient long — pour qu'un réglage nommé « Désactivé » désactive.
/// Un réglage sans effet est pire qu'un réglage absent : l'utilisateur croit
/// avoir agi.
///
/// Un mode illisible ou absent vaut `Off`, exactement comme dans
/// [`ReplayGainSettings::load`] : dans le doute on travaille MOINS, jamais plus.
///
/// Ce que cette fonction ne fait PAS, et ne doit jamais faire : effacer. Les
/// `rg_track_gain` / `rg_album_gain` déjà mesurés restent en base et resservent
/// tels quels dès qu'un mode est réarmé — les recalculer coûte des heures de
/// décodage. Couper l'analyse suspend le travail, elle ne le jette pas.
pub fn analysis_enabled(backend: &Arc<dyn DbBackend>) -> bool {
    matches!(etat_de_l_analyse(backend), EtatAnalyse::Active)
}

/// POURQUOI la passe ne décode pas — la phrase du registre, `None` quand elle
/// est active. Exposé pour la route qui lance la plage dynamique à la demande
/// (#4185) : un refus qui dit « analyse désactivée » sans dire LEQUEL des deux
/// réglages la coupe renvoie l'utilisateur chercher au hasard.
pub fn motif_d_inaction(backend: &Arc<dyn DbBackend>) -> Option<&'static str> {
    etat_de_l_analyse(backend).motif()
}

/// Pourquoi la passe décode — ou ne décode pas.
///
/// [`analysis_enabled`] rend un booléen, et un booléen ne sait pas dire
/// POURQUOI. C'est ce qui a rendu le défaut invisible : le 09/09/2026, le .18
/// portait 13 463 pistes sans plage dynamique et 19 lignes de registre disant
/// « aucune piste ni album sans ReplayGain » depuis le 29/08. La passe n'avait
/// rien fini : elle n'avait jamais démarré, parce que `replaygain_mode` était
/// ABSENT de la table des réglages — ce qui vaut `Off`. Les deux états
/// écrivaient la même phrase.
///
/// D'où cette énumération : le registre inscrit l'état, pas une interprétation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EtatAnalyse {
    /// Un mode est armé et la coche n'est pas décochée : la passe travaille.
    Active,
    /// `replaygain_analysis_enabled = false` — la coche « Analyse ReplayGain ».
    CocheDecochee,
    /// `replaygain_mode` vaut explicitement « Désactivé ».
    ModeDesactive,
    /// `replaygain_mode` est ABSENT. État d'une installation NEUVE : le défaut
    /// de l'API est `"off"` (`routes/system/config.rs`) et la clé n'est écrite
    /// qu'au premier réglage. Distinct de [`Self::ModeDesactive`] à dessein —
    /// « jamais choisi » et « choisi Désactivé » appellent des réponses
    /// opposées quand on se demande pourquoi une bibliothèque n'a pas de DR.
    ModeAbsent,
}

impl EtatAnalyse {
    /// La phrase inscrite au registre. `None` quand la passe est active : il
    /// n'y a alors pas de motif d'inaction à raconter.
    pub(crate) fn motif(self) -> Option<&'static str> {
        match self {
            Self::Active => None,
            Self::CocheDecochee => Some("analyse desactivee : replaygain_analysis_enabled = false"),
            Self::ModeDesactive => Some("analyse desactivee : replaygain_mode = off"),
            Self::ModeAbsent => {
                Some("analyse desactivee : replaygain_mode ABSENT (vaut off) — jamais regle")
            }
        }
    }

    /// La phrase du registre quand la cascade n'a plus rien à faire (#5246).
    ///
    /// ReplayGain coupé, la cascade tourne quand même pour les empreintes et
    /// la plage dynamique : son repos doit dire QUE ces deux-là sont finies,
    /// et POURQUOI le ReplayGain, lui, n'a pas été tenté.
    pub(crate) fn motif_de_repos(self) -> &'static str {
        match self {
            Self::Active => "aucune piste ni album sans ReplayGain, empreinte ni plage dynamique",
            Self::CocheDecochee => {
                "aucune piste sans empreinte ni plage dynamique ; ReplayGain non tente : \
                 replaygain_analysis_enabled = false"
            }
            Self::ModeDesactive => {
                "aucune piste sans empreinte ni plage dynamique ; ReplayGain non tente : \
                 replaygain_mode = off"
            }
            Self::ModeAbsent => {
                "aucune piste sans empreinte ni plage dynamique ; ReplayGain non tente : \
                 replaygain_mode ABSENT (vaut off) — jamais regle"
            }
        }
    }
}

pub(crate) fn etat_de_l_analyse(backend: &Arc<dyn DbBackend>) -> EtatAnalyse {
    let settings = SettingsRepo::with_backend(backend.clone());
    let opted_out = settings
        .get(ANALYSIS_ENABLED_KEY)
        .ok()
        .flatten()
        .map(|v| v == "false")
        .unwrap_or(false);
    if opted_out {
        return EtatAnalyse::CocheDecochee;
    }
    // La distinction ABSENT / « off » se joue ici, et nulle part ailleurs :
    // `unwrap_or(Off)` écrasait les deux cas en un seul.
    let Some(brut) = settings.get(MODE_KEY).ok().flatten() else {
        return EtatAnalyse::ModeAbsent;
    };
    // Illisible vaut `Off`, comme dans `ReplayGainSettings::load` : dans le
    // doute on travaille MOINS, jamais plus.
    if ReplayGainMode::from_setting(&brut) == ReplayGainMode::Off {
        EtatAnalyse::ModeDesactive
    } else {
        EtatAnalyse::Active
    }
}

/// Verrou GLOBAL des analyses lourdes : UNE seule passe décode à la fois.
///
/// ReplayGain et le sweep acoustique décodent tous deux des fichiers entiers ;
/// ensemble ils ont tenu .18 à ~450 % CPU pendant 75 minutes avant que la
/// machine ne s'éteigne net (#1576, 2e arrêt de ce type — journal coupé en
/// pleine ligne). Chaque passe reste bornée et cède déjà à la lecture ; ce
/// verrou fait qu'elles se succèdent au lieu de s'additionner : le pic de
/// charge est divisé par deux, la progression totale est identique.
///
/// Particulièrement important au premier démarrage après une mise à jour qui
/// invalide les deux analyses à la fois (échelle SACD #1638 → RG des DSD,
/// bump de modèle #1498 → tous les embeddings) : sans lui, tout le parc
/// rejouerait le scénario du crash.
pub(crate) static ANALYSIS_SLOT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// True if any zone is currently playing, per the persisted `last_play_state`
/// the orchestrator writes on every play/pause/stop. The ReplayGain track pass
/// must yield to playback (#1310): decoding whole files — often over a network
/// mount — otherwise saturates the same disk/network the player reads from and
/// stalls audio. Fails open (returns `false`) on a query error so a DB hiccup
/// can never freeze the sweep permanently. Shared with the audio-embedding
/// sweep (#1515), which must yield for the same reason and then some: its
/// batches also run multi-threaded ONNX inference on top of the decode.
pub(crate) fn any_zone_playing(backend: &Arc<dyn DbBackend>) -> bool {
    playing_zone_name(backend).is_some()
}

/// Le NOM de la zone qui joue, pour que le report des passes d'analyse soit
/// diagnosticable.
///
/// `any_zone_playing` ne disait que « oui » ou « non », et les journaux se
/// bornaient à « pausing sweep ». Face à une analyse figée alors qu'il ne jouait
/// rien, l'utilisateur ne pouvait pas savoir QUELLE zone la retenait — trois
/// signalements ont buté là-dessus (#1464, #1456, #1457), la cause étant une
/// zone restée à `playing` après un arrêt brutal. Nommer la zone rend la cause
/// lisible dans le journal, sans lire le code.
pub fn playing_zone_name(backend: &Arc<dyn DbBackend>) -> Option<String> {
    backend
        .query_one(
            "SELECT name FROM zones WHERE last_play_state = 'playing' LIMIT 1",
            &[],
        )
        .ok()
        .flatten()
        .map(|cols| {
            cols.first()
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "?".to_string())
        })
}

/// Ce qu'un tour de la cascade de fond a donné.
///
/// Un `usize` ne suffisait plus : « 0 » disait à la fois « plus rien à faire »
/// et « on ne m'a pas laissé travailler », et c'est précisément la confusion
/// qui aurait fait démarrer la plage dynamique dès qu'on met le ReplayGain en
/// pause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TourDeCascade {
    /// Un rang a travaillé : le nombre de pistes retirées du balayage.
    Travail(usize),
    /// La descente s'est arrêtée sur un rang SUSPENDU par l'utilisateur. Les
    /// rangs suivants n'ont PAS été tentés.
    Suspendue(crate::taches_de_fond::Tache),
    /// Plus aucun rang n'a de candidat : la cascade est au repos.
    Repos,
}

/// Un tour de la cascade de fond : ReplayGain, puis les empreintes, puis la
/// plage dynamique — dans l'ordre par défaut. #5169 : l'utilisateur peut
/// faire passer la plage dynamique avant les empreintes, ou avant tout
/// (`taches_de_fond::ordre`) ; la règle de descente ci-dessous vaut pour
/// chaque ordre.
///
/// Une passe à la fois (#1576) : le verrou d'analyse est pris ici, pour tout le
/// tour — si le sweep acoustique décode, on attend notre tour plutôt que
/// d'empiler. BIB-B2 : quand le ReplayGain n'a plus de piste, le même créneau
/// sert au rattrapage des empreintes, puis à celui de la plage dynamique.
/// Chacun ne voit le disque que lorsque le précédent n'a plus rien — jamais
/// deux décodages en même temps.
///
/// # 🔴 UN RANG SUSPENDU ARRÊTE LA DESCENTE
///
/// C'est le point le plus facile à casser de tout ce fichier, et il ne se voit
/// pas à la lecture du code qu'il remplace. La descente se décidait sur le
/// NOMBRE rendu par le rang précédent :
///
/// ```ignore
/// match analyze_track_batch(&backend).await {
///     0 => match empreinter_un_lot(&backend).await {
///         0 => rattraper_un_lot_de_dr(&backend).await,
/// ```
///
/// Un rang suspendu rend `0`. Avec cette forme-là, mettre le **ReplayGain** en
/// pause aurait donc fait démarrer les empreintes, puis **la plage dynamique** —
/// la passe la plus lourde des trois, lancée par le geste censé tout calmer.
///
/// La condition de rang n'est pas « le précédent a rendu 0 », c'est « le
/// précédent est AU REPOS ». Suspendu n'est pas au repos : le travail est
/// toujours devant lui. On s'arrête donc au premier rang suspendu, sans tenter
/// les suivants.
///
/// Gardé par `tune-core/tests/pause_taches_de_fond.rs`
/// (`une_pause_du_replaygain_ne_lance_pas_la_plage_dynamique`).
///
/// `pub` et non privée : le témoin de la dépendance d'ordre est une caisse
/// EXTERNE, et un test qui recopierait la cascade la répliquerait au lieu de la
/// garder.
pub async fn un_tour_de_cascade(backend: &Arc<dyn DbBackend>) -> TourDeCascade {
    use crate::taches_de_fond::ordre::{
        Rang, noter_rang_au_travail, noter_travail_dr, ordre_de_la_cascade, priorite_dr,
    };
    use crate::taches_de_fond::{Tache, est_en_pause};

    let _slot = ANALYSIS_SLOT.lock().await;

    // #5169 — l'ORDRE des rangs vient du réglage (défaut : ReplayGain, puis
    // les empreintes, puis la plage dynamique — l'ordre d'avant). La règle de
    // descente, elle, ne change pas pour le ReplayGain et les empreintes : on
    // s'arrête au premier d'entre eux qui est SUSPENDU.
    //
    // La plage dynamique suspendue est SAUTÉE, pas bloquante. Dans l'ordre par
    // défaut elle est dernière, et cela revient exactement à l'ancien
    // comportement. Avancée par l'utilisateur, elle ne doit pas prendre les
    // autres rangs en otage : « suspendre la seule plage dynamique laisse le
    // ReplayGain travailler » vaut dans tous les ordres
    // (`suspendre_la_plage_dynamique_laisse_le_replaygain_travailler`).
    let mut dr_suspendue = false;
    // #5246 — ReplayGain coupé (mode « Désactivé », absent, ou coche
    // décochée), le rang ReplayGain est SAUTÉ, pas bloquant : les empreintes
    // et la plage dynamique se calculent quand même (décision de Bertrand,
    // 27/09/2026). Sauté AVANT sa pause : une pause posée sur une passe
    // coupée ne doit pas non plus prendre les autres rangs en otage.
    let rg_armee = analysis_enabled(backend);
    for rang in ordre_de_la_cascade(priorite_dr()) {
        if rang == Rang::ReplayGain && !rg_armee {
            continue;
        }
        let tache = match rang {
            Rang::ReplayGain => Tache::ReplayGain,
            Rang::Empreintes => Tache::Empreintes,
            Rang::PlageDynamique => Tache::PlageDynamique,
        };
        if est_en_pause(tache) {
            if rang == Rang::PlageDynamique {
                noter_travail_dr(false);
                dr_suspendue = true;
                continue;
            }
            noter_rang_au_travail(None);
            return TourDeCascade::Suspendue(tache);
        }
        // #5519 / web#1828 — dire QUI décode, avant de décoder : l'écran ne
        // doit pas lire « ReplayGain en cours » pendant que la plage
        // dynamique, passée devant, tient le créneau.
        noter_rang_au_travail(Some(rang));
        let n = match rang {
            Rang::ReplayGain => analyze_track_batch(backend).await,
            Rang::Empreintes => empreinter_un_lot(backend).await,
            Rang::PlageDynamique => {
                let n = rattraper_un_lot_de_dr(backend).await;
                // Le signal que lit le CLAP pour céder son tour (#5169).
                noter_travail_dr(n > 0);
                n
            }
        };
        if n > 0 {
            return TourDeCascade::Travail(n);
        }
    }
    noter_rang_au_travail(None);
    // Une plage dynamique suspendue n'est pas au repos : le travail est
    // toujours devant elle, la campagne ne doit pas se clore.
    if dr_suspendue {
        TourDeCascade::Suspendue(Tache::PlageDynamique)
    } else {
        TourDeCascade::Repos
    }
}

/// Spawn the background ReplayGain analysis loop. Drains tracks that lack
/// ReplayGain, then idles; picks up any new tracks after later scans on its own.
pub fn spawn(backend: Arc<dyn DbBackend>) {
    tokio::spawn(async move {
        // Let startup/scan settle before touching the disk hard.
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        // plafond-analyse — les pistes que l'ancien plafond de taille a
        // écartées redeviennent candidates. Une fois par démarrage, hors des
        // fils de l'exécuteur ; un échec n'empêche pas la passe.
        {
            let b = backend.clone();
            if let Some(Err(e)) = crate::taches_de_fond::priorite::hors_du_fil_async(
                crate::taches_de_fond::Tache::ReplayGain.id(),
                move || reprendre_les_pistes_ecartees_pour_leur_taille(&b),
            )
            .await
            {
                warn!(error = %e, "replaygain_reprise_des_pistes_trop_grosses_echec");
            }
        }
        // Garde thermique de cette passe (#1576) : ReplayGain décode des
        // fichiers entiers, c'est l'autre moitié de la charge qui a éteint .18.
        let mut thermal = crate::audio::thermal::ThermalGate::new();

        // ─── Registre des exécutions automatisées (#2080) ────────────────
        //
        // L'unité inscrite n'est PAS le lot : cette boucle en enchaîne des
        // centaines, et cinquante lignes de registre seraient consommées en
        // quelques minutes sans jamais raconter autre chose que « ça avance ».
        // L'unité est la CAMPAGNE — de la première piste trouvée jusqu'au
        // retour au repos. C'est la phrase que l'utilisateur attend : « la
        // passe a tourné de 21h04 à 21h37 et a analysé 812 pistes ».
        //
        // Le repos sans campagne ouverte est inscrit AUSSI, une seule fois par
        // transition (`repos_deja_inscrit`). C'est la réponse littérale à « ça
        // n'a rien fait » : la passe a tourné, elle n'a rien trouvé. Sans
        // cette ligne, une bibliothèque déjà entièrement analysée serait
        // indistinguable d'une passe jamais lancée. Sans le MOTIF mémorisé, la
        // même ligne se réécrirait toutes les IDLE_SLEEP_SECS et chasserait
        // tout l'historique utile hors de la rétention — et, pire, un passage
        // de « fini » à « désactivée » ne s'inscrirait jamais.
        let registre = crate::db::task_run_repo::TaskRunRepo::with_backend(backend.clone());
        let mut campagne: Option<crate::db::task_run_repo::Execution> = None;
        let mut analysees: i64 = 0;
        // PAS un booléen : le DERNIER motif inscrit. Un `bool` ne sait dire que
        // « déjà au repos », si bien qu'une bibliothèque finie puis une passe
        // coupée à la main n'écrivaient qu'une seule ligne, la première — et
        // l'utilisateur qui décoche « Analyse ReplayGain » ne voyait RIEN
        // changer au registre. Comparer les motifs inscrit une ligne à chaque
        // changement d'état, et une seule.
        let mut dernier_repos: Option<&'static str> = None;

        loop {
            let etat = etat_de_l_analyse(&backend);
            // #5246 — le réglage ReplayGain ne commande PLUS la boucle entière,
            // seulement le rang ReplayGain et la passe d'albums. Avant, la
            // boucle ne lançait la cascade que ReplayGain armé : sur « Off »,
            // ni les empreintes ni la plage dynamique ne tournaient jamais,
            // alors que la carte promettait la plage dynamique « quand le
            // ReplayGain n'a plus rien à faire » (Levente Toth, 0.9.166).
            // Décision de Bertrand du 27/09/2026 : découpler.
            let rg_armee = etat == EtatAnalyse::Active;
            if !rg_armee {
                // #4144 — `analyze_track_batch` n'est plus appelé : personne
                // ne viendrait fermer l'avancement du ReplayGain, et la carte
                // afficherait « en cours » sur une passe éteinte.
                progression::au_repos();
            }
            if thermal.should_hold("replaygain") {
                crate::taches_de_fond::ordre::noter_rang_au_travail(None);
                tokio::time::sleep(std::time::Duration::from_secs(THERMAL_RETRY_SECS)).await;
                continue;
            }
            // Yield the decode-heavy track pass to playback (#1310). The
            // album pass yields too since #4681 — see `passe_d_album`.
            let playing = any_zone_playing(&backend);
            let mut suspendue: Option<crate::taches_de_fond::Tache> = None;
            let did = if playing {
                // Rien ne décode pendant la lecture (#1310).
                crate::taches_de_fond::ordre::noter_rang_au_travail(None);
                0
            } else {
                match un_tour_de_cascade(&backend).await {
                    TourDeCascade::Travail(n) => n,
                    TourDeCascade::Suspendue(tache) => {
                        suspendue = Some(tache);
                        0
                    }
                    TourDeCascade::Repos => 0,
                }
            };
            // La lecture peut avoir démarré PENDANT le lot, qui a alors
            // cédé et rendu 0 sans avoir fini son travail (#2495). Relire
            // l'état ici : sinon ce 0 se lirait « plus rien à analyser »,
            // la boucle inscrirait au registre un « rien à faire » faux et
            // dormirait 15 minutes au lieu des 30 s de report lecture.
            let playing = playing || any_zone_playing(&backend);
            // Le gain d'album EST du ReplayGain : il reste coupé avec lui.
            let albums = if rg_armee {
                passe_d_albums_du_tour(&backend, playing).await
            } else {
                0
            };

            if did > 0 || albums > 0 {
                // Du travail : ouvrir la campagne si elle ne l'est pas déjà.
                if campagne.is_none() {
                    campagne = Some(registre.ouvrir(crate::db::task_run_repo::TACHE_REPLAYGAIN));
                    analysees = 0;
                }
                analysees += did as i64 + albums as i64;
                dernier_repos = None;
            }

            if playing {
                // #4681 — céder, oui, mais le dire : le relevé de
                // `/system/background-tasks` doit pouvoir nommer la passe
                // qui s'est effacée devant la lecture.
                crate::taches_de_fond::priorite::noter_cedee(
                    crate::taches_de_fond::Tache::ReplayGain.id(),
                );
                tokio::time::sleep(std::time::Duration::from_secs(PLAYBACK_BACKOFF_SECS)).await;
            } else if let Some(tache) = suspendue {
                // ⚠️ NE PAS clore la campagne, et NE PAS inscrire « rien à
                // faire ». Une passe suspendue n'est pas une passe finie :
                // le travail est toujours devant elle, la jauge doit rester
                // là où la pause l'a laissée, et la reprise doit repartir du
                // même point. Inscrire un repos ici mentirait au registre et
                // remettrait la carte à zéro sous les yeux de l'utilisateur.
                debug!(tache = tache.id(), "cascade_suspendue");
                tokio::time::sleep(std::time::Duration::from_secs(SIESTE_EN_PAUSE_SECS)).await;
            } else if did == 0 && albums == 0 {
                clore_campagne(
                    &registre,
                    &mut campagne,
                    &mut analysees,
                    &mut dernier_repos,
                    etat.motif_de_repos(),
                );
                tokio::time::sleep(std::time::Duration::from_secs(IDLE_SLEEP_SECS)).await;
            } else {
                // More to do — loop again promptly.
                tokio::time::sleep(pause_entre_deux_tours(&backend)).await;
            }
        }
    });
}

/// La pause de la boucle de fond entre deux tours qui ont travaillé (#5519).
///
/// Elle valait 2 s pour toutes les vitesses, « the per-file pauses already
/// throttle the actual work » — or ces pauses par fichier ont été retirées
/// (30/09). Mesurée sur Shrek (banc étage F, 500 000 pistes en base, 05/10),
/// elle prenait 9 à 15 % du temps de la passe, et davantage à mesure que le
/// tour raccourcit.
///
/// La vitesse réglée dit déjà combien l'utilisateur concède à l'analyse :
/// « Discret » garde les 2 s d'avant, « Normal » et « Rapide » ne marquent
/// qu'un court répit. Le verrou d'analyse (#1576) est relâché pendant ce
/// répit, et `tokio::sync::Mutex` sert ses demandeurs dans l'ordre : le sweep
/// acoustique qui attend son tour le prend, quelle que soit la durée.
pub fn pause_entre_deux_tours(backend: &Arc<dyn DbBackend>) -> std::time::Duration {
    pause_pour_la_vitesse(crate::taches_de_fond::vitesse::vitesse(backend))
}

/// [`pause_entre_deux_tours`], pour une vitesse donnée.
pub fn pause_pour_la_vitesse(v: crate::taches_de_fond::vitesse::Vitesse) -> std::time::Duration {
    use crate::taches_de_fond::vitesse::Vitesse;
    match v {
        Vitesse::Discrete => std::time::Duration::from_secs(2),
        Vitesse::Normale | Vitesse::Rapide => std::time::Duration::from_millis(250),
    }
}

/// Albums au plus par tour de la boucle de fond (#5519).
///
/// Un tour de la passe de pistes en mesure 25, soit environ deux albums
/// complets ; la passe d'albums n'en faisait qu'UN par tour. Elle prenait donc
/// du retard pendant toute la campagne, puis le rattrapait seule, un album par
/// tour — tour de cascade, sélection et pause compris, soit près de 4 s par
/// album sur une base de 500 000 pistes (banc étage F, 05/10). Pour les 33 000
/// albums de Tades, c'étaient des heures de jauge immobile après la dernière
/// piste.
pub const ALBUMS_PAR_TOUR: usize = 4;

/// La passe d'albums d'UN tour : jusqu'à [`ALBUMS_PAR_TOUR`] albums, tant
/// qu'il y en a. Mêmes gardes que [`passe_d_album`], relues à chaque album.
pub async fn passe_d_albums_du_tour(backend: &Arc<dyn DbBackend>, en_lecture: bool) -> usize {
    albums_jusqu_a_la_borne(|| passe_d_album(backend, en_lecture)).await
}

/// La boucle de [`passe_d_albums_du_tour`], sans la base ni les gardes
/// globales (pause, lecture) : elle se garde seule, sans dépendre de l'état
/// que d'autres tests du processus posent.
async fn albums_jusqu_a_la_borne<F, Fut>(mut un_album: F) -> usize
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = usize>,
{
    let mut faits = 0;
    for _ in 0..ALBUMS_PAR_TOUR {
        let n = un_album().await;
        if n == 0 {
            break;
        }
        faits += n;
    }
    faits
}

/// Un tour de la passe d'ALBUMS, ou rien.
///
/// Le lot d'albums est du ReplayGain — le même traitement, la même carte à
/// l'écran. Il ne décode rien, mais le suspendre avec sa passe est la seule
/// lecture honnête du bouton : sinon « ReplayGain en pause » continuerait
/// d'écrire des gains d'album, et la jauge avancerait sous un badge « En
/// pause ».
///
/// # 🔴 PAS PENDANT LA LECTURE (#4681)
///
/// Cette passe était annoncée « pure arithmetic » et tournait donc en pleine
/// écoute. Elle ÉCRIT pourtant quatre clés par piste — jusqu'à ~60 écritures
/// SQLite d'affilée pour un album de 15 titres, précédées d'une sélection à
/// trois `NOT EXISTS` imbriqués sur toute la bibliothèque. Trois
/// micro-coupures de la .18 les 19 et 20/09/2026 tombent à la seconde sur une
/// ligne `replaygain_album` (#4567). #4572 l'a sortie du fil de l'exécuteur ;
/// elle prenait encore le verrou d'écriture UNIQUE de la base, que le chemin
/// de lecture attend aussi.
///
/// Elle attend donc l'arrêt, comme les passes qui décodent. Le retard est
/// sans conséquence : un album par tour, quelques millisecondes chacun, tout
/// est rattrapé dans la minute qui suit l'arrêt.
///
/// `en_lecture` est le témoin en base que la boucle vient de lire ; le témoin
/// en mémoire ([`crate::taches_de_fond::priorite::lecture_en_cours`]) compte
/// aussi — l'un ou l'autre suffit à céder.
///
/// Gardé par `tune-core/tests/priorite_lecture_taches_de_fond.rs`.
pub async fn passe_d_album(backend: &Arc<dyn DbBackend>, en_lecture: bool) -> usize {
    use crate::taches_de_fond::{Tache, est_en_pause, priorite};

    if est_en_pause(Tache::ReplayGain) {
        return 0;
    }
    if en_lecture || priorite::lecture_en_cours() {
        priorite::noter_cedee(Tache::ReplayGain.id());
        return 0;
    }
    let backend_album = backend.clone();
    priorite::hors_du_fil_async(Tache::ReplayGain.id(), move || {
        analyze_album_batch(&backend_album)
    })
    .await
    .unwrap_or(0)
}

/// Fermer la campagne ReplayGain en cours, ou inscrire un « rien à faire ».
///
/// Un seul endroit pour les deux transitions vers le repos, sinon le drapeau
/// `repos_deja_inscrit` finirait par diverger entre les deux branches et le
/// registre se remplirait de lignes vides.
fn clore_campagne(
    registre: &crate::db::task_run_repo::TaskRunRepo,
    campagne: &mut Option<crate::db::task_run_repo::Execution>,
    analysees: &mut i64,
    dernier_repos: &mut Option<&'static str>,
    motif: &'static str,
) {
    if let Some(e) = campagne.take() {
        let n = *analysees;
        e.terminer(
            crate::db::task_run_repo::Verdict::Succes,
            Some(n),
            Some(&format!("{n} elements analyses")),
        );
        *analysees = 0;
        *dernier_repos = Some(motif);
        return;
    }
    // Une ligne par CHANGEMENT de motif. Même motif qu'au tour précédent : la
    // situation n'a pas bougé, le registre n'a rien de neuf à dire et ne
    // chasse pas l'historique utile hors de la rétention.
    if *dernier_repos != Some(motif) {
        registre
            .ouvrir(crate::db::task_run_repo::TACHE_REPLAYGAIN)
            .rien_a_faire(Some(motif));
        *dernier_repos = Some(motif);
    }
}

/// Le prédicat des pistes À ANALYSER, partagé entre le balayage et son compteur
/// (#4144).
///
/// Il était écrit en toutes lettres dans [`analyze_track_batch`], et il y était
/// seul. Le sortir n'est pas de la cosmétique : la jauge de l'écran Santé a
/// besoin du DÉNOMINATEUR, et un second texte recopié à la main finirait par
/// diverger de la sélection — la carte annoncerait alors une progression vers
/// un total que la passe ne vise pas. Même montage que
/// [`CANDIDATS_EMPREINTE_WHERE`] et [`CANDIDATS_DR_WHERE`], pour la même
/// raison.
///
/// Un seul paramètre : le seuil de report (#1865).
///
/// 🔴 `t.file_path IS NOT NULL` écarte les pistes CUE, À DESSEIN. Voir le
/// commentaire de [`analyze_track_batch`] : `mesurer_intensite_et_plage` mesure
/// le fichier ENTIER, et les quinze pistes d'une image recevraient le gain du
/// disque complet.
const CANDIDATS_RG_WHERE: &str = "t.file_path IS NOT NULL AND t.file_path != '' \
           AND NOT EXISTS (SELECT 1 FROM track_metadata m \
                 WHERE m.track_id = t.id AND m.key = 'rg_analyzed') \
           AND NOT EXISTS (SELECT 1 FROM track_metadata m \
                 WHERE m.track_id = t.id AND m.key = 'rg_track_gain') \
           AND NOT EXISTS (SELECT 1 FROM track_metadata m \
                 WHERE m.track_id = t.id AND m.key = 'rg_path_unresolved' \
                   AND m.value > ?)";

/// Le prédicat de la passe ReplayGain, PÉRIMÈTRE compris (#5593) : les racines
/// exclues par l'utilisateur sortent de la sélection ET du compteur, par le
/// même texte. Sans paramètre ajouté : la clause porte des littéraux, les `?`
/// de [`CANDIDATS_RG_WHERE`] ne bougent pas. Voir
/// [`crate::taches_de_fond::perimetre`].
fn candidats_rg_where(backend: &Arc<dyn DbBackend>) -> String {
    let perimetre = crate::taches_de_fond::perimetre::clause_decodage(backend);
    format!("{CANDIDATS_RG_WHERE}{perimetre}")
}

/// Combien de pistes le balayage ReplayGain a encore devant lui (#4144).
///
/// Le dénominateur de la jauge, et rien d'autre : la MÊME sélection que
/// [`analyze_track_batch`], sans `LIMIT`. `0` sur erreur de requête — une base
/// qui ne répond pas ne doit pas faire tomber la passe, et une jauge sur zéro
/// se rend comme « total inconnu » plutôt que comme une fausse certitude.
pub fn compter_les_candidats_replaygain(backend: &Arc<dyn DbBackend>) -> i64 {
    let seuil_report = deferral_threshold(now_epoch_secs() as i64);
    let predicat = candidats_rg_where(backend);
    backend
        .query_one(
            &format!("SELECT COUNT(*) FROM tracks t WHERE {predicat}"),
            &[&seuil_report as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|row| row.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
}

/// Combien de pistes la passe tient à l'écart parce que **leur fichier ne
/// répond pas** — report non expiré (#1865), même clé pour le ReplayGain et
/// la plage dynamique.
///
/// Ce n'est ni « analysé » ni « à faire » : c'est « en attente d'un disque ».
/// Sans ce chiffre, une bibliothèque entière sur un partage démonté se lisait
/// « ReplayGain terminée » — `compter_les_candidats_replaygain` exclut ces
/// pistes, à raison, et rendait `total = 0` (Benjithom, 0.9.151, #4254). Les
/// cartes de la page Santé le montrent à côté de la jauge, avec sa cause.
pub fn compter_les_reportees_par_chemin(backend: &Arc<dyn DbBackend>) -> i64 {
    let seuil_report = deferral_threshold(now_epoch_secs() as i64);
    // #5593 — une piste d'une racine EXCLUE n'attend plus son disque : elle
    // n'est plus du travail du tout. La compter ici ferait dire à la carte
    // « en attente d'un disque » pour un partage que l'utilisateur a retiré
    // des analyses — précisément le NAS démonté de l'exemple.
    let perimetre = crate::taches_de_fond::perimetre::clause_decodage(backend);
    let dans_le_perimetre = if perimetre.is_empty() {
        String::new()
    } else {
        format!(" AND EXISTS (SELECT 1 FROM tracks t WHERE t.id = m.track_id{perimetre})")
    };
    backend
        .query_one(
            &format!(
                "SELECT COUNT(DISTINCT m.track_id) FROM track_metadata m \
                 WHERE m.key = 'rg_path_unresolved' AND m.value > ?{dans_le_perimetre}"
            ),
            &[&seuil_report as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|row| row.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
}

/// Les `n` premières pistes candidates d'identifiant strictement supérieur à
/// `apres`, dans l'ordre des identifiants (#5519).
///
/// C'est la sélection de [`analyze_track_batch`], bornée par un CURSEUR. Sans
/// lui, chaque tour relisait la bibliothèque depuis la première piste : sur
/// 500 000 pistes déjà faites et des nouvelles en queue (l'ordre d'un scan),
/// deux sondes de `track_metadata` par piste déjà faite, environ 0,4 s par
/// tour sur Shrek. Avec lui, un tour ne relit que ce qui suit le dernier
/// identifiant traité.
///
/// L'index est la clé primaire de `tracks` (`t.id > ? ORDER BY t.id`), et
/// chaque `NOT EXISTS` est une sonde de la clé primaire `(track_id, key)` de
/// `track_metadata` — sur SQLite comme sur PostgreSQL. Aucune migration : les
/// plans sont relevés dans la PR.
///
/// Rendue `pub` pour le banc (`examples/banc_replaygain_5519.rs`).
pub fn selectionner_les_candidats_replaygain(
    backend: &Arc<dyn DbBackend>,
    apres: i64,
    n: usize,
) -> Result<Vec<Vec<crate::db::backend::SqlValue>>, String> {
    let seuil_report = deferral_threshold(now_epoch_secs() as i64);
    // #5593 — le PÉRIMÈTRE : les racines exclues sortent de la sélection.
    let predicat = candidats_rg_where(backend);
    backend.query_many(
        &format!(
            "SELECT t.id, t.file_path, t.duration_ms, t.sample_rate, t.channels FROM tracks t \
             WHERE {predicat} AND t.id > ? ORDER BY t.id LIMIT ?"
        ),
        &[
            &seuil_report as &dyn ToSqlValue,
            &apres as &dyn ToSqlValue,
            &(n as i64) as &dyn ToSqlValue,
        ],
    )
}

/// Le curseur de la sélection ReplayGain, PAR BASE : le dernier identifiant
/// que la passe a laissé derrière elle (#5519).
///
/// En mémoire, et c'est assez : au redémarrage il repart de 0, et le premier
/// tour paie une fois la relecture complète d'avant. Rangé à côté d'un `Weak`
/// de la base qu'il suit : tant qu'il existe, l'allocation de cette base ne
/// peut pas être réemployée par une autre, si bien que deux bases (deux
/// tests, par exemple) n'héritent jamais du curseur l'une de l'autre.
///
/// Le curseur n'est jamais une raison de manquer une piste : quand plus rien
/// ne le suit, [`analyze_track_batch`] repart de 0 avant de conclure au repos.
static CURSEURS_RG: std::sync::Mutex<Vec<(std::sync::Weak<dyn DbBackend>, i64)>> =
    std::sync::Mutex::new(Vec::new());

fn curseur_rg(backend: &Arc<dyn DbBackend>) -> i64 {
    let cible = Arc::downgrade(backend);
    let curseurs = CURSEURS_RG.lock().unwrap_or_else(|e| e.into_inner());
    curseurs
        .iter()
        .find(|(base, _)| std::sync::Weak::ptr_eq(base, &cible))
        .map(|(_, c)| *c)
        .unwrap_or(0)
}

fn poser_curseur_rg(backend: &Arc<dyn DbBackend>, valeur: i64) {
    let cible = Arc::downgrade(backend);
    let mut curseurs = CURSEURS_RG.lock().unwrap_or_else(|e| e.into_inner());
    // Les bases disparues libèrent leur place.
    curseurs.retain(|(base, _)| base.strong_count() > 0);
    match curseurs
        .iter_mut()
        .find(|(base, _)| std::sync::Weak::ptr_eq(base, &cible))
    {
        Some((_, c)) => *c = valeur,
        None => curseurs.push((cible, valeur)),
    }
}

/// Analyse up to `TRACK_BATCH` local tracks that have no ReplayGain yet. Returns
/// how many were processed (0 ⇒ nothing left, caller idles).
pub async fn analyze_track_batch(backend: &Arc<dyn DbBackend>) -> usize {
    // Local tracks with a file on disk, not yet analysed (no `rg_analyzed`
    // sentinel) and without file-tag ReplayGain (`rg_track_gain`). The two
    // NOT EXISTS keep the sweep advancing and honour the file's own tags.
    //
    // Le troisième écarte les pistes REPORTÉES trop récemment (#1865). La
    // comparaison se fait en TEXTE sur une estampille rembourrée de zéros,
    // pas via un `CAST(... AS INTEGER)` : `track_metadata.value` est partagée
    // par toutes les clés, et un CAST y ferait tomber la requête entière sur
    // PostgreSQL dès qu'une valeur non numérique existe ailleurs dans la table.
    // 🔴 PISTES CUE : ÉCARTÉES À DESSEIN, ET PAS PAR OUBLI.
    //
    // Une piste découpée par une feuille CUE porte `file_path = NULL` par
    // construction ; `t.file_path IS NOT NULL` l'exclut donc. Contrairement à
    // la pochette (`library::artwork`), à l'empreinte acoustique
    // (`audio::embedding_store::candidats_acoustiques`), au dédoublonnage et
    // aux playlists de dossier — tous corrigés — la retomber ici sur
    // `cue_media_path` FERAIT DES DÉGÂTS : `mesurer_intensite_et_plage` ne
    // prend qu'un chemin et mesure le fichier ENTIER depuis 0 s. Les quinze
    // pistes d'une même image recevraient le gain, le pic et la plage
    // dynamique de tout le disque — quinze valeurs identiques, présentées
    // comme mesurées piste par piste, et un niveau de lecture faux sur
    // chacune.
    //
    // Ce qu'il faudrait d'abord : une mesure BORNÉE
    // (`mesurer_intensite_et_plage(chemin, debut_s, fin_s)`). Sa boucle
    // décode déjà par segments de 30 s avec un `seek` — la borner est
    // mécanique — mais c'est un chantier d'analyseur, avec sa propre garde
    // sur du vrai signal. Tant qu'il n'existe pas, une piste CUE sans
    // ReplayGain vaut mieux qu'une piste CUE au mauvais ReplayGain.
    //
    // Les deux autres sélections de ce module (`CANDIDATS_EMPREINTE_WHERE`,
    // `CANDIDATS_DR_WHERE`) sont EN AVAL de celle-ci : elles exigent le témoin
    // `rg_analyzed`, que seule cette passe pose. Les élargir sans elle ne
    // sélectionnerait rien.
    let depuis = curseur_rg(backend);
    let rows = match selectionner_les_candidats_replaygain(backend, depuis, TRACK_BATCH) {
        // Rien après le curseur : un tour complet depuis le début, une fois,
        // avant de conclure. Les pistes redevenues candidates DERRIÈRE lui
        // (report expiré, rattrapage, re-scan) sont reprises ici.
        Ok(r) if r.is_empty() && depuis > 0 => {
            poser_curseur_rg(backend, 0);
            selectionner_les_candidats_replaygain(backend, 0, TRACK_BATCH)
        }
        autre => autre,
    };
    let rows = match rows {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "replaygain_candidate_query_failed");
            return 0;
        }
    };
    if rows.is_empty() {
        debug!("replaygain_no_pending_tracks");
        // #4144 — le bord « fini ». Sans lui, la carte resterait sur le dernier
        // couple annoncé et l'écran dirait « en cours » pour toujours.
        progression::au_repos();
        return 0;
    }
    // #4144 — l'ouverture de la campagne, et avec elle le DÉNOMINATEUR. Compté
    // une seule fois : voir `progression::ouvrir_si_besoin`.
    progression::ouvrir_si_besoin(|| compter_les_candidats_replaygain(backend));

    let mut done = 0usize;
    let mut deferred = 0usize;
    let mut cedees = 0usize;
    // #5519 — ce que les pistes du tour écriront ensemble, à sa fin.
    let mut tour: Vec<EcrituresDePiste> = Vec::with_capacity(rows.len());
    let mut passees: std::collections::HashSet<i64> = std::collections::HashSet::new();
    // #5519 — plusieurs fichiers à la fois, selon la vitesse réglée. Les
    // gardes (réglage, pause, lecture) restent relues avant CHAQUE fichier
    // lancé : la frontière propre est toujours « entre deux fichiers ».
    let largeur = crate::taches_de_fond::vitesse::largeur_courante(backend);
    en_parallele_borne(
        largeur,
        rows.iter(),
        || {
            // Le réglage peut basculer EN PLEIN LOT. 25 fichiers à jusqu'à
            // 180 s chacun, c'est plus d'une heure de décodage après un
            // « Désactivé » si on ne regarde qu'entre deux lots : un réglage
            // qui n'agit qu'au prochain démarrage n'est pas un réglage
            // (#2496). On relit donc avant CHAQUE fichier. Le décodage déjà
            // lancé n'est pas annulable — même contrat que le garde-fou
            // lecture ci-dessous : on s'arrête au fichier suivant, pas au
            // milieu d'un decode.
            if !analysis_enabled(backend) {
                info!("replaygain_analysis_disabled_mid_batch — réglage coupé, arrêt du balayage");
                return false;
            }
            // Pause demandée par l'utilisateur, relue au MÊME endroit et pour
            // la même raison.
            if crate::taches_de_fond::est_en_pause(crate::taches_de_fond::Tache::ReplayGain) {
                info!("replaygain_pause_utilisateur_mid_batch — arrêt à la frontière de piste");
                return false;
            }
            // Playback can start mid-batch; yield at once so a decode never
            // competes with the audio pipeline (#1310).
            if any_zone_playing(backend) {
                debug!("replaygain_yield_to_playback — zone playing, pausing sweep mid-batch");
                return false;
            }
            true
        },
        |r| {
            let id = r.first().and_then(|v| v.as_i64());
            let piste = analyser_une_piste(backend, r);
            async move { (id, piste.await) }
        },
        |(id, (suite, ecritures))| {
            if let Some(e) = ecritures {
                tour.push(e);
            }
            match suite {
                SuitePiste::Ignoree => {
                    passees.extend(id);
                    true
                }
                SuitePiste::Avancee { reportee } => {
                    passees.extend(id);
                    done += 1;
                    if reportee {
                        deferred += 1;
                    }
                    true
                }
                SuitePiste::Cedee => {
                    cedees += 1;
                    false
                }
            }
        },
    )
    .await;

    // #5519 — UNE transaction pour tout le tour, hors du fil async (#4681).
    let ecriture = if tour.is_empty() {
        EcritureDuTour::Rien
    } else {
        let b = backend.clone();
        crate::taches_de_fond::priorite::hors_du_fil_async(
            crate::taches_de_fond::Tache::ReplayGain.id(),
            move || ecrire_le_tour(&b, &tour),
        )
        .await
        .unwrap_or(EcritureDuTour::Rien)
    };
    // #4144 — l'avancement suit ce qui est ÉCRIT : la jauge ne devance pas la
    // base.
    for _ in 0..done {
        progression::avancer();
    }

    // Le curseur s'arrête AVANT la première piste du lot qui n'a pas quitté le
    // balayage — cédée à la lecture, ou jamais lancée (réglage coupé, pause,
    // lecture) : le tour suivant la reprend. Les pistes passées derrière elle
    // ne sont plus candidates, les relire ne coûte qu'une sonde chacune.
    let ids: Vec<i64> = rows
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect();
    let suivant = match ids.iter().find(|id| !passees.contains(id)) {
        Some(&id) => id - 1,
        None => ids.last().copied().unwrap_or(depuis),
    };
    poser_curseur_rg(backend, suivant);

    // `deferred` est porté par la ligne de journal : sans lui, un lot où tout
    // est introuvable ressemblerait à un lot analysé (#1865). `cedees` dit la
    // même chose pour l'abandon au profit de la lecture (#2495) : sans elle,
    // un lot rendu à zéro serait indistinguable d'une bibliothèque finie.
    info!(
        analyzed = done - deferred,
        deferred,
        cedees,
        largeur,
        ecriture = ?ecriture,
        curseur = suivant,
        "replaygain_track_batch"
    );
    done
}

/// Ce qu'est devenue UNE piste d'un lot.
enum SuitePiste {
    /// Ligne illisible : rien n'a été fait, rien n'est compté.
    Ignoree,
    /// La piste a quitté le balayage. `reportee` : fichier introuvable (#1865).
    Avancee { reportee: bool },
    /// Abandonnée au profit de la lecture, SANS témoin : elle sera reprise.
    Cedee,
}

/// Lancer au plus `largeur` travaux à la fois (#5519).
///
/// `peut_lancer` est relu avant CHAQUE lancement : c'est là que vivent les
/// gardes de réglage, de pause et de lecture. `recu` reçoit chaque issue, dans
/// l'ordre d'arrivée ; `false` arrête les lancements — ceux déjà en vol vont
/// à leur terme (leur décodage n'est pas annulable, ils cèdent eux-mêmes à la
/// lecture). À `largeur = 1`, c'est exactement la boucle séquentielle d'avant.
///
/// Les futurs ne sont PAS lancés sur l'exécuteur : ils tournent dans la tâche
/// de la passe, et le travail lourd de chacun part déjà sur le pool bloquant
/// (`spawn_blocking` du décodage) — c'est ce qui fait le parallélisme.
async fn en_parallele_borne<I, R, Fut>(
    largeur: usize,
    items: impl IntoIterator<Item = I>,
    mut peut_lancer: impl FnMut() -> bool,
    mut lancer: impl FnMut(I) -> Fut,
    mut recu: impl FnMut(R) -> bool,
) where
    Fut: std::future::Future<Output = R>,
{
    use futures_util::stream::{FuturesUnordered, StreamExt};
    let largeur = largeur.max(1);
    let mut items = items.into_iter().peekable();
    let mut en_vol = FuturesUnordered::new();
    let mut ouvert = true;
    loop {
        while ouvert && en_vol.len() < largeur {
            // Les gardes ne sont relues que s'il reste de quoi lancer : chacune
            // coûte une requête, et la veille de lecture (#2495) compte les
            // siennes.
            if items.peek().is_none() || !peut_lancer() {
                ouvert = false;
                break;
            }
            if let Some(item) = items.next() {
                en_vol.push(lancer(item));
            }
        }
        match en_vol.next().await {
            Some(r) => {
                if !recu(r) {
                    ouvert = false;
                }
            }
            None => break,
        }
    }
}

/// Mesurer UNE piste de la passe nominale : gain, pics, plage dynamique et
/// empreinte, puis le témoin `rg_analyzed`.
///
/// Le corps de l'ancienne boucle de [`analyze_track_batch`], inchangé, à trois
/// différences près (#5519) : l'empreinte est tirée du décodage de la mesure
/// quand le format le permet (`mesurer_intensite_plage_et_empreinte`, au bit
/// près), il n'y a plus de pause fixe de 400 ms après la piste, et rien n'est
/// écrit ici : la piste REND ses écritures, que [`ecrire_le_tour`] pose avec
/// celles des autres pistes du tour, en une transaction.
async fn analyser_une_piste(
    backend: &Arc<dyn DbBackend>,
    r: &[crate::db::backend::SqlValue],
) -> (SuitePiste, Option<EcrituresDePiste>) {
    let track_id = match r.first().and_then(|v| v.as_i64()) {
        Some(id) => id,
        None => return (SuitePiste::Ignoree, None),
    };
    let path = match r.get(1).and_then(|v| v.as_string()) {
        Some(p) if !p.is_empty() => p,
        _ => return (SuitePiste::Ignoree, None),
    };
    let mut ecritures = EcrituresDePiste::pour(track_id);

    // Le chemin de la base est en NFC ; le fichier, lui, peut être écrit
    // en NFD sur le disque (macOS, SMB/CIFS). On résout AVANT de décider
    // quoi que ce soit — et surtout avant de poser le moindre témoin
    // (#1865).
    let sur_disque = match resolve_local_path(&path) {
        LocalPath::Found(reel) => reel,
        LocalPath::Missing => {
            // Introuvable N'EST PAS indécodable. Aucun `rg_analyzed` ici :
            // on ne fige pas une piste que le prochain montage rendra. On
            // pose seulement un report daté, qui périme tout seul.
            warn!(
                track_id,
                path = %path,
                "replaygain_path_unresolved — aucune graphie (stockee, NFD, NFC) \
                 ne repond ; piste REPORTEE, pas marquee analysee (#1865)"
            );
            ecritures.report = Some(deferral_stamp(now_epoch_secs() as i64));
            // Comptée comme AVANCÉE : la ligne ne ressortira pas de la
            // prochaine requête. Sans cela, un lot entièrement introuvable
            // rendrait 0 et endormirait la passe 15 minutes à chaque paquet de
            // 25 lignes. #4144 — la jauge avance pour la même raison (après
            // l'écriture du tour).
            return (SuitePiste::Avancee { reportee: true }, Some(ecritures));
        }
    };
    // Un report qui traînait n'a plus lieu d'être : le fichier répond.
    ecritures.effacer_le_report = true;

    let delai = delai_d_analyse(r.get(2).and_then(|v| v.as_i64()));

    // `sur_disque`, PAS `path` : c'est la graphie que le système a
    // reconnue. Le chemin de la base reste ce qu'il est (#1865).
    //
    // La course contre la lecture est ici, pas seulement au lancement (#2495) :
    // le contrôle d'entrée ne sert à rien quand UN fichier monopolise le
    // disque pendant des minutes.
    let measured = match mesurer_en_cedant_a_la_lecture(
        backend,
        delai,
        crate::audio::analyzer::mesurer_intensite_plage_et_empreinte(&sur_disque),
    )
    .await
    {
        Issue::Terminee(m) => m,
        Issue::CedeeALaLecture => {
            // AUCUN `rg_analyzed` ici, et c'est tout l'enjeu : on n'a pas
            // essayé, on a renoncé. Estampiller sortirait la piste du
            // balayage pour toujours — le défaut #1865 exactement, mais
            // déclenché par un simple appui sur « Lecture ».
            info!(
                track_id,
                path = %path,
                "replaygain_cede_en_cours_d_analyse — lecture demarree, fichier \
                 abandonne SANS temoin (il sera repris) ; le segment deja parti \
                 finit dans le vide, il n'est pas annulable (#2495)"
            );
            // L'effacement du report, lui, reste dû : le fichier a répondu.
            return (SuitePiste::Cedee, Some(ecritures));
        }
    };
    let (mesure, empreinte) = match measured {
        Ok(m) => (Ok(m.mesure), m.empreinte),
        Err(elapsed) => (Err(elapsed), None),
    };
    match mesure {
        Ok(Some(m)) => {
            // Écrite à la fin du tour (`ecrire_le_tour`), hors des fils de
            // l'exécuteur (#4681).
            ecritures.mesure = Some(m);
        }
        // Le fichier a disparu ENTRE la résolution et le décodage — un
        // partage qui tombe pendant la passe, exactement le scénario qui a
        // déjà coûté des pistes. On ne le déclare pas indécodable : on le
        // reporte, comme un absent de la première heure.
        Ok(None) if resolve_local_path(&path).is_missing() => {
            warn!(
                track_id,
                path = %path,
                "replaygain_path_disparu_pendant_analyse — REPORTEE, pas marquee analysee (#1865)"
            );
            ecritures.effacer_le_report = false;
            ecritures.report = Some(deferral_stamp(now_epoch_secs() as i64));
            return (SuitePiste::Avancee { reportee: true }, Some(ecritures)); // #4144
        }
        Ok(None) => {
            // Le fichier est bien là et reste illisible ou silencieux :
            // là, le témoin est légitime.
            debug!(track_id, path = %path, "replaygain_measure_none");
        }
        Err(_elapsed) => {
            // The file blocked analysis (pathological decode / dormant NAS
            // mount) past the per-track bound. Stamp it analysed below so the
            // sweep ADVANCES instead of looping on it forever (#1155).
            warn!(
                track_id,
                path = %path,
                timeout_s = delai.as_secs(),
                "replaygain_measure_timeout — file stalled analysis; skipping so the sweep advances (#1155)"
            );
        }
    }
    // BIB-B2 : l'empreinte du contenu, dans la même passe. #5519 — tirée du
    // décodage de la mesure quand il l'a permis ; sinon, décodée à part comme
    // avant.
    ecritures.empreinte = match empreinte {
        Some(calcul) => Some(valeur_d_empreinte(track_id, &sur_disque, calcul)),
        None => calculer_l_empreinte(track_id, &sur_disque).await,
    };
    // Sentinel = unix seconds, so an album pass can tell a track has been
    // handled even when it produced no gain.
    ecritures.temoin = Some(now_epoch_secs().to_string());
    // #4144 — LE point d'avancement nominal, par piste (compté par le lot,
    // après l'écriture du tour).
    (SuitePiste::Avancee { reportee: false }, Some(ecritures))
}

/// BIB-B2 : la marque « pas d'empreinte possible » (silence, fichier
/// indecodable), versionnee comme une empreinte : elle sort la piste du
/// rattrapage sans jamais se comparer a rien (`deserialiser` la refuse).
fn marque_sans_empreinte() -> String {
    format!("{}:-", crate::audio::empreinte::VERSION)
}

/// Calcule et pose l'empreinte du contenu d'une piste (BIB-B2). Le decodage
/// (90 s au plus, mono 11 kHz) part en tache bloquante. Rend `true` si une
/// valeur a ete ecrite (empreinte ou marque).
async fn empreinter_la_piste(backend: &Arc<dyn DbBackend>, track_id: i64, chemin: &str) -> bool {
    match calculer_l_empreinte(track_id, chemin).await {
        Some(valeur) => ecrire_l_empreinte(backend, track_id, valeur).await,
        None => false,
    }
}

/// Décoder et calculer l'empreinte d'une piste (tâche bloquante), sans
/// l'écrire. `None` si la tâche a été interrompue : rien à poser.
async fn calculer_l_empreinte(track_id: i64, chemin: &str) -> Option<String> {
    let chemin_owned = chemin.to_string();
    let calcul = tokio::task::spawn_blocking(move || {
        crate::audio::empreinte::empreinte_du_fichier(&chemin_owned)
    })
    .await;
    match calcul {
        Ok(calcul) => Some(valeur_d_empreinte(track_id, chemin, calcul)),
        Err(e) => {
            warn!(track_id, path = %chemin, error = %e, "empreinte_tache_interrompue");
            None
        }
    }
}

/// La valeur à poser pour un calcul d'empreinte : l'empreinte sérialisée, ou
/// la marque « pas d'empreinte possible ».
fn valeur_d_empreinte(
    track_id: i64,
    chemin: &str,
    calcul: Result<Option<crate::audio::empreinte::Empreinte>, String>,
) -> String {
    match calcul {
        Ok(Some(e)) => e.serialiser(),
        Ok(None) => {
            debug!(track_id, path = %chemin, "empreinte_silence — marque posee");
            marque_sans_empreinte()
        }
        Err(e) => {
            warn!(track_id, path = %chemin, error = %e, "empreinte_decodage_echoue — marque posee");
            marque_sans_empreinte()
        }
    }
}

/// Poser l'empreinte calculée — ou la marque « pas d'empreinte possible » —
/// d'une piste. Rend `true` si une valeur a été écrite.
async fn ecrire_l_empreinte(backend: &Arc<dyn DbBackend>, track_id: i64, valeur: String) -> bool {
    // Hors du fil async (#4681) : c'est une écriture SQLite.
    let depot = crate::db::track_repo::TrackRepo::with_backend(backend.clone());
    let ecriture = crate::taches_de_fond::priorite::hors_du_fil_async(
        crate::taches_de_fond::Tache::Empreintes.id(),
        move || depot.set_audio_fingerprint(track_id, &valeur),
    )
    .await;
    match ecriture {
        Some(Ok(())) => true,
        Some(Err(e)) => {
            warn!(track_id, error = %e, "empreinte_ecriture_echouee");
            false
        }
        // Le travail a paniqué : `hors_du_fil_async` l'a déjà journalisé.
        None => false,
    }
}

/// BIB-B2 : rattrapage borne des empreintes — les pistes deja analysees par
/// le ReplayGain (donc jamais reprises par [`analyze_track_batch`]) qui n'ont
/// pas d'empreinte de la version courante. Memes gardes que le ReplayGain :
/// reglage, lecture en cours, chemin introuvable reporte (#1865), DSD ecarte
/// (le reechantillonneur DSD→PCM peut boucler sur certains rips SACD).
/// Le prédicat des pistes À EMPREINTER, partagé entre le rattrapage et son
/// compteur (BIB-B2 phase D) : deux textes finiraient par diverger, et la
/// couverture annoncée ne serait plus celle que le rattrapage traite.
/// Paramètres, dans l'ordre : motif `VERSION:%`, seuil de report.
///
/// 🔴 `t.file_path IS NOT NULL` écarte les pistes CUE, à dessein — deux fois
/// plutôt qu'une. D'abord parce que ce prédicat exige `rg_analyzed`, que seule
/// [`analyze_track_batch`] pose et qui n'atteint pas les pistes CUE : élargir
/// ici ne sélectionnerait rien. Ensuite parce qu'une empreinte prise sur
/// l'image entière serait IDENTIQUE pour toutes les pistes du disque, et que
/// `library::duplicate_detector::scan_fingerprint_duplicates` regroupe
/// justement sur l'empreinte : les quinze pistes seraient proposées à la
/// suppression les unes contre les autres. Voir le commentaire de
/// [`analyze_track_batch`].
///
/// #5246 — le témoin `rg_analyzed` n'est exigé que lorsque l'analyse
/// ReplayGain est ARMÉE : c'est alors elle qui décode la piste et pose
/// l'empreinte au passage, et le rattrapage ne doit pas décoder deux fois.
/// ReplayGain coupé, personne ne poserait jamais ce témoin : exiger le témoin
/// revenait à ne jamais empreinter. Voir [`candidats_empreinte_where`].
const CANDIDATS_EMPREINTE_WHERE: &str = "t.file_path IS NOT NULL AND t.file_path != '' \
           AND (t.audio_fingerprint IS NULL OR t.audio_fingerprint NOT LIKE ?) \
           AND NOT EXISTS (SELECT 1 FROM track_metadata m \
                 WHERE m.track_id = t.id AND m.key = 'rg_path_unresolved' \
                   AND m.value > ?) \
           AND LOWER(COALESCE(t.format, '')) NOT IN ('dsd', 'dsf', 'dff', 'dsdiff')";

/// Le témoin de la passe ReplayGain, exigé par le rattrapage des empreintes
/// quand cette passe est armée (#5246). Sans paramètre : l'ajouter ou non ne
/// décale pas les `?` de [`CANDIDATS_EMPREINTE_WHERE`].
const TEMOIN_RG_EMPREINTE: &str = " AND EXISTS (SELECT 1 FROM track_metadata m \
                 WHERE m.track_id = t.id AND m.key = 'rg_analyzed')";

/// Le prédicat du rattrapage des empreintes, selon l'état du ReplayGain
/// (#5246). Décision de Bertrand du 27/09/2026 : les empreintes et la plage
/// dynamique se calculent MÊME ReplayGain coupé ; seuls le calcul et
/// l'application du gain restent désactivés.
///
/// #5593 — et le PÉRIMÈTRE : les racines exclues sortent de la sélection et du
/// compteur. Indispensable même ReplayGain armé : le témoin seul ne suffit pas,
/// une piste analysée AVANT l'exclusion porte déjà `rg_analyzed`.
fn candidats_empreinte_where(backend: &Arc<dyn DbBackend>) -> String {
    let temoin = if analysis_enabled(backend) {
        TEMOIN_RG_EMPREINTE
    } else {
        ""
    };
    let perimetre = crate::taches_de_fond::perimetre::clause_decodage(backend);
    format!("{CANDIDATS_EMPREINTE_WHERE}{temoin}{perimetre}")
}

/// Combien de pistes le rattrapage traiterait encore. `None` : base
/// antérieure à la colonne `audio_fingerprint` (rien à compter).
pub fn compter_les_candidats_a_empreinter(backend: &Arc<dyn DbBackend>) -> Option<i64> {
    let seuil_report = deferral_threshold(now_epoch_secs() as i64);
    let motif = format!("{}:%", crate::audio::empreinte::VERSION);
    let predicat = candidats_empreinte_where(backend);
    backend
        .query_one(
            &format!("SELECT COUNT(*) FROM tracks t WHERE {predicat}"),
            &[&motif as &dyn ToSqlValue, &seuil_report as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|row| row.first().and_then(|v| v.as_i64()))
}

pub async fn empreinter_un_lot(backend: &Arc<dyn DbBackend>) -> usize {
    let seuil_report = deferral_threshold(now_epoch_secs() as i64);
    let motif = format!("{}:%", crate::audio::empreinte::VERSION);
    let predicat = candidats_empreinte_where(backend);
    let rows = match backend.query_many(
        &format!("SELECT t.id, t.file_path FROM tracks t WHERE {predicat} LIMIT ?"),
        &[
            &motif as &dyn ToSqlValue,
            &seuil_report as &dyn ToSqlValue,
            &(TRACK_BATCH as i64) as &dyn ToSqlValue,
        ],
    ) {
        Ok(r) => r,
        Err(e) => {
            // Base anterieure a la colonne : rien a rattraper, sans bruit.
            if !(e.contains("no such column") || e.contains("does not exist")) {
                warn!(error = %e, "empreinte_candidate_query_failed");
            }
            return 0;
        }
    };
    if rows.is_empty() {
        return 0;
    }
    let repo = TrackMetadataRepo::with_backend(backend.clone());
    let mut done = 0usize;
    // #5519 — la même largeur que la passe nominale, les mêmes gardes avant
    // chaque fichier, plus de pause fixe.
    let largeur = crate::taches_de_fond::vitesse::largeur_courante(backend);
    en_parallele_borne(
        largeur,
        rows.iter(),
        || {
            // #5246 : plus de garde sur le réglage ReplayGain ici. Les
            // empreintes se calculent même ReplayGain coupé ; leur propre
            // pause (ci-dessous) est le geste qui les arrête.
            if any_zone_playing(backend) {
                return false;
            }
            // Même frontière que la passe nominale : entre deux pistes.
            if crate::taches_de_fond::est_en_pause(crate::taches_de_fond::Tache::Empreintes) {
                info!("empreinte_pause_utilisateur_mid_lot — arrêt à la frontière de piste");
                return false;
            }
            true
        },
        |r| {
            let repo = &repo;
            async move {
                let Some(track_id) = r.first().and_then(|v| v.as_i64()) else {
                    return false;
                };
                let Some(path) = r
                    .get(1)
                    .and_then(|v| v.as_string())
                    .filter(|p| !p.is_empty())
                else {
                    return false;
                };
                let sur_disque = match resolve_local_path(&path) {
                    LocalPath::Found(reel) => reel,
                    LocalPath::Missing => {
                        warn!(track_id, path = %path, "empreinte_path_unresolved — piste REPORTEE (#1865)");
                        let _ = repo.set(
                            track_id,
                            PATH_UNRESOLVED_KEY,
                            &deferral_stamp(now_epoch_secs() as i64),
                        );
                        return true;
                    }
                };
                empreinter_la_piste(backend, track_id, &sur_disque).await
            }
        },
        |avancee| {
            if avancee {
                done += 1;
            }
            true
        },
    )
    .await;
    info!(empreintes = done, "empreinte_lot");
    done
}

/// La marque « pas de plage dynamique possible pour ce fichier » : estampille
/// unix, posée quand le rattrapage a VRAIMENT essayé et n'a rien obtenu
/// (fichier illisible, silencieux, décodage bloqué, taille hors de portée).
///
/// Sans elle, le rattrapage reprendrait les mêmes fichiers à chaque réveil,
/// pour toujours : c'est la boucle infinie que `rg_analyzed` évite déjà à la
/// passe nominale (#1155). Introuvable n'en fait PAS partie — un partage
/// démonté se reporte, il ne se condamne pas (#1865).
const DR_INDISPONIBLE_KEY: &str = "dr_indisponible";

/// Le prédicat des pistes SANS plage dynamique que la passe nominale ne
/// repassera JAMAIS voir.
///
/// 🔴 C'est le trou mesuré sur le .18 le 09/09/2026 : 33 414 pistes sur 46 877
/// portaient déjà `rg_analyzed` — posé avant que le DR n'existe — et
/// `analyze_track_batch` les écarte par construction. Aucune route ne remet ce
/// témoin à zéro (seule une migration DSD l'a jamais fait). Sans ce
/// rattrapage, 71 % de la bibliothèque n'aurait jamais de DR calculé, quoi que
/// fasse l'utilisateur. Même raisonnement et même forme que
/// [`CANDIDATS_EMPREINTE_WHERE`] (BIB-B2), pour la même raison.
///
/// Les deux populations visées, et elles seules :
/// * `rg_analyzed` — analysées avant l'arrivée du DR ;
/// * `rg_track_gain` — gain lu dans les tags du fichier, donc jamais décodées.
///
/// Ce qui est écarté, et pourquoi :
/// * un `dr_track` NON VIDE — le tag du disque fait foi, cf. [`peut_ecrire_le_dr`] ;
///   la comparaison sur `TRIM(...) != ''` suit exactement cette règle, un tag
///   présent mais vide n'étant pas une valeur ;
/// * [`DR_INDISPONIBLE_KEY`] — déjà essayé, en vain ;
/// * un report de chemin encore frais (#1865).
///
/// Paramètre : le seuil de report.
///
/// 🔴 Les pistes CUE sont hors de ce prédicat, à dessein : il exige
/// `rg_analyzed` ou `rg_track_gain`, et la plage dynamique se mesure sur le
/// MÊME décodage non borné que le ReplayGain. Voir [`analyze_track_batch`].
///
/// #5246 — le témoin (`rg_analyzed` ou `rg_track_gain`) n'est exigé que
/// ReplayGain ARMÉ, pour la même raison que [`TEMOIN_RG_EMPREINTE`] : armée,
/// la passe nominale mesure elle-même la plage dynamique des pistes qu'elle
/// n'a pas encore vues ; coupée, elle ne les verra jamais. Voir
/// [`candidats_dr_where`].
const CANDIDATS_DR_WHERE: &str = "t.file_path IS NOT NULL AND t.file_path != '' \
           AND NOT EXISTS (SELECT 1 FROM track_metadata m \
                 WHERE m.track_id = t.id AND m.key = 'dr_track' AND TRIM(m.value) != '') \
           AND NOT EXISTS (SELECT 1 FROM track_metadata m \
                 WHERE m.track_id = t.id AND m.key = 'dr_indisponible') \
           AND NOT EXISTS (SELECT 1 FROM track_metadata m \
                 WHERE m.track_id = t.id AND m.key = 'rg_path_unresolved' \
                   AND m.value > ?)";

/// Le témoin de la passe nominale exigé par le rattrapage de la plage
/// dynamique quand le ReplayGain est armé (#5246). Sans paramètre.
const TEMOIN_RG_DR: &str = " AND EXISTS (SELECT 1 FROM track_metadata m \
                 WHERE m.track_id = t.id AND m.key IN ('rg_analyzed', 'rg_track_gain'))";

/// Le prédicat du rattrapage de la plage dynamique, selon l'état du
/// ReplayGain (#5246) — même règle que [`candidats_empreinte_where`].
///
/// #5593 — et le PÉRIMÈTRE, pour la même raison que
/// [`candidats_empreinte_where`].
fn candidats_dr_where(backend: &Arc<dyn DbBackend>) -> String {
    let temoin = if analysis_enabled(backend) {
        TEMOIN_RG_DR
    } else {
        ""
    };
    let perimetre = crate::taches_de_fond::perimetre::clause_decodage(backend);
    format!("{CANDIDATS_DR_WHERE}{temoin}{perimetre}")
}

/// Combien de pistes le rattrapage de la plage dynamique prendrait MAINTENANT.
///
/// Même texte que la sélection de [`rattraper_un_lot_de_dr`] — c'est le
/// dénominateur de la passe à la demande (#4185), et un compte recopié à la
/// main finirait par viser une population que la passe ne traite pas (même
/// montage que [`compter_les_candidats_a_empreinter`]). Une panne de base
/// rend 0, journalisée : la route ne doit pas tomber pour une jauge.
pub fn compter_les_candidats_dr(backend: &Arc<dyn DbBackend>) -> i64 {
    let seuil_report = deferral_threshold(now_epoch_secs() as i64);
    let predicat = candidats_dr_where(backend);
    match backend.query_one(
        &format!("SELECT COUNT(*) FROM tracks t WHERE {predicat}"),
        &[&seuil_report as &dyn ToSqlValue],
    ) {
        Ok(row) => row
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap_or(0),
        Err(e) => {
            warn!(error = %e, "dr_candidate_count_failed");
            0
        }
    }
}

/// Combien de pistes SANS plage dynamique dorment dans une racine exclue des
/// analyses (#5593) — fil 2157, « bloquée à 97 % ».
///
/// Le périmètre retire ces pistes de toutes les passes qui décodent, ET de
/// leurs compteurs ([`compter_les_candidats_dr`], [`compter_les_reportees_par_chemin`]).
/// Elles n'étaient donc comptées nulle part, sauf au total de la
/// bibliothèque : l'écran Santé les rangeait « en attente » et sa jauge ne
/// finissait jamais, la passe au repos. Les compter ici, par la clause même
/// qui les écarte, ferme ce trou.
///
/// Jamais deux fois : ni une piste qui a un DR, ni une piste déjà comptée
/// ailleurs — `dr_indisponible` (`dynamic_range_unavailable`),
/// `rg_skipped_oversized` (`dynamic_range_oversized`), sans fichier
/// (`dynamic_range_without_file`). Aucune racine exclue : `0` sans requête.
pub fn compter_les_sans_dr_hors_perimetre(backend: &Arc<dyn DbBackend>) -> i64 {
    let hors = crate::taches_de_fond::perimetre::clause_hors_perimetre_decodage(backend);
    if hors.is_empty() {
        return 0;
    }
    let sql = format!(
        "SELECT COUNT(*) FROM tracks t \
         WHERE t.file_path IS NOT NULL AND t.file_path != '' \
           AND NOT EXISTS (SELECT 1 FROM track_metadata m \
                 WHERE m.track_id = t.id AND m.key = 'dr_track' AND TRIM(m.value) != '') \
           AND NOT EXISTS (SELECT 1 FROM track_metadata m \
                 WHERE m.track_id = t.id AND m.key IN ('dr_indisponible', '{OVERSIZED_KEY}')){hors}"
    );
    match backend.query_one(&sql, &[]) {
        Ok(row) => row
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap_or(0),
        Err(e) => {
            warn!(error = %e, "dr_hors_perimetre_count_failed");
            0
        }
    }
}

/// Les pistes TRAITÉES par la plage dynamique — décision du 06/10 : la jauge
/// de l'écran Santé vaut `traitees / total`, et le total est TOUTE la
/// bibliothèque. Plus rien n'est retiré du dénominateur.
///
/// Une piste est traitée quand elle a un DR (mesuré, lu dans ses tags ou
/// dans un `foo_dr.txt`), ou qu'elle est déclarée NON GÉRABLE :
/// * sans fichier propre (`file_path` vide, images CUE) ;
/// * mesure impossible pour de bon (`dr_indisponible` : format illisible,
///   silence, délai dépassé) ;
/// * trop longue pour l'analyse (`rg_skipped_oversized`, sans DR) ;
/// * dans une racine exclue des analyses (#5593).
///
/// 🔴 Une piste REPORTÉE (fichier qui ne répond pas, `rg_path_unresolved`,
/// #1865) n'est PAS traitée : elle sera reprise à l'expiration du report, et
/// ne devient traitée que mesurée ou déclarée indisponible. Le report ne
/// pose jamais `dr_indisponible`, à dessein : un partage démonté revient.
///
/// Compté sur `tracks`, une seule passe : `traitees` ne dépasse jamais le
/// total, et une piste n'est comptée qu'une fois quelle que soit la somme de
/// ses marques.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PistesTraiteesDr {
    /// Pistes avec un DR, ou déclarées non gérables.
    pub traitees: i64,
    /// Pistes de la table `tracks` qui ont un DR.
    pub avec_dr: i64,
    /// Pistes avec un fichier, sans DR, marquées `dr_indisponible`, dans le
    /// périmètre ou hors de lui : la version dédupliquée de
    /// `dynamic_range_unavailable`, qui compte aussi celles qui ont un DR.
    pub non_mesurables: i64,
}

impl PistesTraiteesDr {
    /// Les pistes non gérables : traitées, mais sans DR.
    pub fn non_gerables(&self) -> i64 {
        (self.traitees - self.avec_dr).max(0)
    }
}

/// Voir [`PistesTraiteesDr`]. `None` sur erreur de requête : la route publie
/// alors les anciens champs seuls, et le client garde son calcul d'avant.
pub fn compter_les_pistes_traitees_dr(backend: &Arc<dyn DbBackend>) -> Option<PistesTraiteesDr> {
    let hors = crate::taches_de_fond::perimetre::clause_hors_perimetre_decodage(backend);
    // Sans racine exclue, rien n'est hors périmètre : un terme toujours faux.
    let hors = if hors.is_empty() {
        " AND 1 = 0".to_string()
    } else {
        hors
    };
    let fichier = "(t.file_path IS NOT NULL AND t.file_path != '')";
    let avec_dr = "EXISTS (SELECT 1 FROM track_metadata d \
                   WHERE d.track_id = t.id AND d.key = 'dr_track' AND TRIM(d.value) != '')";
    let indisponible = "EXISTS (SELECT 1 FROM track_metadata i \
                        WHERE i.track_id = t.id AND i.key = 'dr_indisponible')";
    let sql = format!(
        "SELECT \
           COUNT(CASE WHEN {avec_dr} \
                   OR NOT {fichier} \
                   OR EXISTS (SELECT 1 FROM track_metadata x WHERE x.track_id = t.id \
                        AND x.key IN ('dr_indisponible', '{OVERSIZED_KEY}')) \
                   OR ({fichier}{hors}) THEN 1 END), \
           COUNT(CASE WHEN {avec_dr} THEN 1 END), \
           COUNT(CASE WHEN {fichier} AND NOT {avec_dr} AND {indisponible} THEN 1 END) \
         FROM tracks t"
    );
    match backend.query_one(&sql, &[]) {
        Ok(Some(row)) => {
            let get = |i: usize| row.get(i).and_then(|v| v.as_i64()).unwrap_or(0).max(0);
            Some(PistesTraiteesDr {
                traitees: get(0),
                avec_dr: get(1),
                non_mesurables: get(2),
            })
        }
        Ok(None) => None,
        Err(e) => {
            warn!(error = %e, "dr_traitees_count_failed");
            None
        }
    }
}

/// Calcule la plage dynamique d'un lot de pistes que la passe nominale a
/// laissées derrière elle. Rend combien de lignes ont AVANCÉ (0 ⇒ plus rien).
///
/// N'écrit QUE `dr_track` / `dr_source`. Surtout pas les gains : sur une piste
/// `rg_track_gain`, ils viennent des tags du fichier et les remplacer par une
/// mesure changerait le niveau de lecture sous les pieds de l'utilisateur, qui
/// n'a rien demandé de tel. Le rattrapage comble un vide, il ne révise rien.
///
/// Débit mesuré le 09/09/2026 sur le .18 (Core i7-3615QM, machine au repos) :
/// 171 ×RT sur 24 pistes réelles, dont ~4,6 % imputables au DR lui-même.
pub async fn rattraper_un_lot_de_dr(backend: &Arc<dyn DbBackend>) -> usize {
    let seuil_report = deferral_threshold(now_epoch_secs() as i64);
    let predicat = candidats_dr_where(backend);
    let rows = match backend.query_many(
        &format!(
            "SELECT t.id, t.file_path, t.duration_ms, t.sample_rate, t.channels \
             FROM tracks t WHERE {predicat} LIMIT ?"
        ),
        &[
            &seuil_report as &dyn ToSqlValue,
            &(TRACK_BATCH as i64) as &dyn ToSqlValue,
        ],
    ) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "dr_candidate_query_failed");
            return 0;
        }
    };
    if rows.is_empty() {
        debug!("dr_rattrapage_rien_a_faire");
        return 0;
    }

    let repo = TrackMetadataRepo::with_backend(backend.clone());
    let mut done = 0usize;
    let mut deferred = 0usize;
    // #5519 — la même largeur que la passe nominale, les mêmes gardes avant
    // chaque fichier, plus de pause fixe.
    let largeur = crate::taches_de_fond::vitesse::largeur_courante(backend);
    en_parallele_borne(
        largeur,
        rows.iter(),
        || {
            // Gardes relues AVANT CHAQUE fichier et pas seulement entre deux
            // lots : 25 fichiers à 180 s, c'est plus d'une heure de décodage
            // après un appui sur « Pause » ou sur « Lecture » (#1310).
            //
            // #5246 : le réglage ReplayGain ne coupe PLUS ce rattrapage ; la
            // pause de la plage dynamique est le geste qui l'arrête.
            if crate::taches_de_fond::est_en_pause(crate::taches_de_fond::Tache::PlageDynamique) {
                info!("dr_rattrapage_pause_utilisateur_mid_lot — arret a la frontiere de piste");
                return false;
            }
            if any_zone_playing(backend) {
                debug!("dr_rattrapage_cede_a_la_lecture — zone en lecture, pause");
                return false;
            }
            true
        },
        |r| rattraper_une_piste(backend, &repo, r),
        |suite| match suite {
            SuitePiste::Ignoree => true,
            SuitePiste::Avancee { reportee } => {
                done += 1;
                if reportee {
                    deferred += 1;
                }
                true
            }
            SuitePiste::Cedee => false,
        },
    )
    .await;

    info!(rattrapees = done - deferred, deferred, "dr_rattrapage_lot");
    done
}

/// Rattraper la plage dynamique d'UNE piste — le corps de l'ancienne boucle de
/// [`rattraper_un_lot_de_dr`], inchangé, sans la pause fixe (#5519).
async fn rattraper_une_piste(
    backend: &Arc<dyn DbBackend>,
    repo: &TrackMetadataRepo,
    r: &[crate::db::backend::SqlValue],
) -> SuitePiste {
    let Some(track_id) = r.first().and_then(|v| v.as_i64()) else {
        return SuitePiste::Ignoree;
    };
    let Some(path) = r
        .get(1)
        .and_then(|v| v.as_string())
        .filter(|p| !p.is_empty())
    else {
        return SuitePiste::Ignoree;
    };
    // La base est en NFC, le disque peut être en NFD (macOS, SMB/CIFS).
    let sur_disque = match resolve_local_path(&path) {
        LocalPath::Found(reel) => reel,
        LocalPath::Missing => {
            // AUCUN `dr_indisponible` : un partage démonté redeviendra
            // lisible, et la marque serait définitive (#1865).
            warn!(
                track_id,
                path = %path,
                "dr_path_unresolved — piste REPORTEE, pas marquee indisponible (#1865)"
            );
            let _ = repo.set(
                track_id,
                PATH_UNRESOLVED_KEY,
                &deferral_stamp(now_epoch_secs() as i64),
            );
            // Comptée : la ligne ne ressortira pas de la prochaine requête,
            // le rattrapage a donc bel et bien avancé.
            return SuitePiste::Avancee { reportee: true };
        }
    };
    let _ = repo.delete(track_id, PATH_UNRESOLVED_KEY);

    let delai = delai_d_analyse(r.get(2).and_then(|v| v.as_i64()));

    let measured = match mesurer_en_cedant_a_la_lecture(
        backend,
        delai,
        crate::audio::analyzer::mesurer_intensite_et_plage(&sur_disque),
    )
    .await
    {
        Issue::Terminee(m) => m,
        Issue::CedeeALaLecture => {
            // Renoncé, pas essayé : aucune marque, la piste sera reprise.
            info!(
                track_id,
                path = %path,
                "dr_rattrapage_cede_en_cours — lecture demarree, fichier abandonne SANS temoin"
            );
            return SuitePiste::Cedee;
        }
    };

    let mut mesuree = false;
    match measured {
        Ok(Some((_lufs, _peak, _true_peak, Some(dr)))) => {
            mesuree = true;
            // 🔴 LE TAG DU FICHIER FAIT FOI. La requête a bien écarté les
            // pistes qui en portaient un, mais un scan a pu en poser un
            // PENDANT le décodage — jusqu'à 180 s de fenêtre. On relit,
            // hors du fil async (#4681).
            let repo_dr = TrackMetadataRepo::with_backend(backend.clone());
            crate::taches_de_fond::priorite::hors_du_fil_async(
                crate::taches_de_fond::Tache::PlageDynamique.id(),
                move || ecrire_le_dr_mesure(&repo_dr, track_id, dr),
            )
            .await;
        }
        // Le fichier a disparu ENTRE la résolution et le décodage : un
        // partage qui tombe pendant la passe. On reporte, on ne condamne pas.
        Ok(None) if resolve_local_path(&path).is_missing() => {
            warn!(
                track_id,
                path = %path,
                "dr_path_disparu_pendant_analyse — REPORTEE (#1865)"
            );
            let _ = repo.set(
                track_id,
                PATH_UNRESOLVED_KEY,
                &deferral_stamp(now_epoch_secs() as i64),
            );
            return SuitePiste::Avancee { reportee: true };
        }
        // Présent mais illisible, silencieux, ou plage non calculable.
        Ok(_) => debug!(track_id, path = %path, "dr_rattrapage_sans_plage"),
        Err(_elapsed) => warn!(
            track_id,
            path = %path,
            timeout_s = delai.as_secs(),
            "dr_rattrapage_timeout — fichier bloquant, marque pour que le lot AVANCE (#1155)"
        ),
    }
    if !mesuree {
        let _ = repo.set(track_id, DR_INDISPONIBLE_KEY, &now_epoch_secs().to_string());
    }
    SuitePiste::Avancee { reportee: false }
}

/// Compute album ReplayGain for one album whose tracks are all analysed but that
/// still lacks album gain. Returns 1 if an album was processed, else 0.
///
/// Album gain uses the duration-weighted energy mean of the tracks' loudness
/// (recovered from each `rg_track_gain`), matching how ReplayGain 2.0 shares one
/// gain across an album to preserve inter-track dynamics; album peak is the max
/// track peak. Written to the tracks of the album (ReplayGain album tags are
/// per-track).
///
/// # Deux gardes, et pourquoi
///
/// La première version prenait un album dès qu'**une** piste lui manquait un
/// gain d'album, puis moyennait les seules pistes qui avaient un
/// `rg_track_gain` **à cet instant** — parfois une sur douze — et écrivait le
/// résultat sur toutes. Ce gain-là ne se recalculait jamais : toutes les
/// pistes portant désormais un `rg_album_gain`, l'album sortait
/// définitivement de la sélection. Mesuré sur le .18 : **179 albums,
/// 2 385 pistes**, dont un album de 64 pistes dont le gain vient de 2.
///
/// * **Complétude, à la SÉLECTION** — un album n'est pris que lorsque
///   *toutes* ses pistes ont un `rg_track_gain`. Même raisonnement que
///   `true_peak_complete` plus bas, qui refuse déjà un `rg_album_true_peak`
///   tiré d'un maximum partiel : une moyenne partielle n'est pas une moyenne.
///   La garde est dans la requête, et pas dans la boucle, pour que l'album
///   incomplet ne soit pas RECHOISI à chaque tour — il affamerait tous les
///   autres.
/// * **Provenance, à l'ÉCRITURE** — voir [`peut_ecrire_le_gain_album`]. Les
///   tags du fichier et la mesure de Tune s'écrivent sous la même clé ; une
///   valeur qui n'est pas estampillée de notre main ne s'écrase pas.
pub fn analyze_album_batch(backend: &Arc<dyn DbBackend>) -> usize {
    // An album that has track gains but no album gain yet. One at a time keeps
    // it cheap (pure arithmetic, no decode) and interleaved with the track pass.
    //
    // 🔴 GARDE DE COMPLÉTUDE : le troisième `NOT EXISTS` écarte l'album dont
    // une seule piste n'a pas encore son `rg_track_gain`. Sans lui, le gain
    // d'album se calculait sur le sous-ensemble analysé à cet instant, puis se
    // figeait pour toujours.
    let album_row = backend
        .query_one(
            "SELECT t.album_id FROM tracks t \
             JOIN track_metadata g ON g.track_id = t.id AND g.key = 'rg_track_gain' \
             WHERE t.album_id IS NOT NULL \
               AND NOT EXISTS (SELECT 1 FROM track_metadata a \
                     WHERE a.track_id = t.id AND a.key = 'rg_album_gain') \
               AND NOT EXISTS (SELECT 1 FROM tracks t2 \
                     WHERE t2.album_id = t.album_id \
                       AND NOT EXISTS (SELECT 1 FROM track_metadata g2 \
                             WHERE g2.track_id = t2.id \
                               AND g2.key = 'rg_track_gain')) \
             LIMIT 1",
            &[],
        )
        .ok()
        .flatten();
    let album_id = match album_row.and_then(|r| r.first().and_then(|v| v.as_i64())) {
        Some(id) => id,
        None => return 0,
    };

    // All tracks of the album, with their gain, peak and duration.
    let rows = match backend.query_many(
        "SELECT t.id, t.duration_ms, \
                (SELECT value FROM track_metadata WHERE track_id = t.id AND key = 'rg_track_gain'), \
                (SELECT value FROM track_metadata WHERE track_id = t.id AND key = 'rg_track_peak'), \
                (SELECT value FROM track_metadata WHERE track_id = t.id AND key = 'rg_track_true_peak'), \
                (SELECT value FROM track_metadata WHERE track_id = t.id AND key = 'rg_album_gain'), \
                (SELECT value FROM track_metadata WHERE track_id = t.id AND key = 'rg_album_source') \
         FROM tracks t WHERE t.album_id = ?",
        &[&album_id as &dyn ToSqlValue],
    ) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, album_id, "replaygain_album_query_failed");
            return 0;
        }
    };

    let mut energy_sum = 0.0f64; // duration-weighted linear energy
    let mut dur_sum = 0.0f64;
    let mut peak_max = 0.0f64;
    // True peak d'album (#1694) : max des true peaks de pistes — mais
    // SEULEMENT si toutes les pistes en ont un. Un max partiel pourrait
    // rater la piste la plus chaude, et `prevent_clipping` s'y fierait.
    let mut true_peak_max = 0.0f64;
    let mut true_peak_complete = true;
    let mut n = 0usize;
    let repo = TrackMetadataRepo::with_backend(backend.clone());
    let mut track_ids: Vec<i64> = Vec::new();
    // Les pistes sur lesquelles il est LICITE d'écrire le gain d'album : les
    // autres portent une valeur venue des tags du fichier.
    let mut ecrivables: Vec<i64> = Vec::new();

    for r in &rows {
        let tid = match r.first().and_then(|v| v.as_i64()) {
            Some(id) => id,
            None => continue,
        };
        track_ids.push(tid);
        let gain_album_en_place = r.get(5).and_then(|v| v.as_string());
        let temoin_album = r.get(6).and_then(|v| v.as_string());
        if peut_ecrire_le_gain_album(gain_album_en_place.as_deref(), temoin_album.as_deref()) {
            ecrivables.push(tid);
        }
        let dur = r.get(1).and_then(|v| v.as_i64()).unwrap_or(0).max(1) as f64;
        // gain string like "-6.50 dB" → lufs = REFERENCE - gain
        if let Some(gain) = r.get(2).and_then(|v| v.as_string()).and_then(parse_gain_db) {
            let lufs = REFERENCE_LUFS - gain;
            energy_sum += dur * 10f64.powf(lufs / 10.0);
            dur_sum += dur;
            n += 1;
        }
        if let Some(p) = r
            .get(3)
            .and_then(|v| v.as_string())
            .and_then(|s| s.parse::<f64>().ok())
        {
            peak_max = peak_max.max(p);
        }
        match r
            .get(4)
            .and_then(|v| v.as_string())
            .and_then(|s| s.parse::<f64>().ok())
        {
            Some(tp) => true_peak_max = true_peak_max.max(tp),
            // Piste analysée avant #1694 : pas de true peak. L'album n'en
            // reçoit pas non plus tant qu'elle n'est pas ré-analysée.
            None => true_peak_complete = false,
        }
    }

    if n == 0 || dur_sum <= 0.0 {
        return 0;
    }
    let album_lufs = 10.0 * (energy_sum / dur_sum).log10();
    let album_gain = track_gain_db(album_lufs);
    let gain_str = format_gain(album_gain);
    let peak_str = format_peak(peak_max);

    let true_peak_str =
        (true_peak_complete && true_peak_max > 0.0).then(|| format_peak(true_peak_max));

    // 🔴 GARDE DE PROVENANCE : `ecrivables`, et non `track_ids`. Une piste dont
    // le `rg_album_gain` vient des tags du fichier garde sa valeur — et ne
    // reçoit surtout pas l'estampille `ALBUM_SOURCE_KEY`, qui la ferait passer
    // pour une mesure de Tune au tour suivant.
    if ecrivables.is_empty() {
        debug!(
            album_id,
            "replaygain_album_tout_vient_des_tags — rien à écrire"
        );
        return 0;
    }
    for tid in &ecrivables {
        let _ = repo.set(*tid, "rg_album_gain", &gain_str);
        let _ = repo.set(*tid, "rg_album_peak", &peak_str);
        if let Some(tp) = &true_peak_str {
            let _ = repo.set(*tid, "rg_album_true_peak", tp);
        }
        // Provenance (#1627) : ce gain d'album n'est dans AUCUN fichier. Il
        // vient d'être calculé ici, à partir des gains de piste — que ceux-ci
        // soient eux-mêmes des tags ou des mesures ne change rien : la valeur
        // d'album, elle, est de Tune.
        let _ = repo.set(*tid, ALBUM_SOURCE_KEY, SOURCE_ANALYSIS);
    }
    info!(
        album_id,
        tracks = track_ids.len(),
        ecrites = ecrivables.len(),
        gain = %gain_str,
        "replaygain_album"
    );
    1
}

/// Le gain d'album déjà posé sur cette piste est-il à nous ?
///
/// Jumeau album de [`peut_ecrire_le_dr`] : une valeur déjà présente ne
/// s'écrase pas. Les tags du fichier et la mesure de Tune s'écrivent sous la
/// **même** clé `rg_album_gain` — le scan y verse ce qu'il lit dans le fichier
/// (`crate::metadata`) — et seule l'estampille [`ALBUM_SOURCE_KEY`] à
/// [`SOURCE_ANALYSIS`] dit que la valeur en place vient d'ici. Sans elle, on
/// est devant un tag de l'utilisateur : on n'y touche pas.
///
/// Le refus est par PISTE, et non par album : les pistes encore vierges
/// reçoivent le gain calculé, donc l'album sort de la sélection au tour
/// suivant. Un refus par album, lui, le ferait rechoisir indéfiniment.
fn peut_ecrire_le_gain_album(gain_existant: Option<&str>, temoin: Option<&str>) -> bool {
    match gain_existant {
        None => true,
        Some(v) if v.trim().is_empty() => true,
        Some(_) => temoin.is_some_and(|t| t.trim() == SOURCE_ANALYSIS),
    }
}

/// Parse a ReplayGain gain string ("-6.50 dB", "+3.2", "-6.50dB") to dB.
pub(crate) fn parse_gain_db(s: String) -> Option<f64> {
    s.to_lowercase()
        .replace("db", "")
        .trim()
        .parse::<f64>()
        .ok()
}

// ---------------------------------------------------------------------------
// Applying the gain at playback
// ---------------------------------------------------------------------------
//
// Everything above MEASURES and STORES the gain. Nothing used to READ it back:
// a library could be fully analysed, every `rg_track_gain` in place, and not
// one decibel was ever applied. This is the consuming half.

/// Setting: `off` (default), `track` or `album`.
pub const MODE_KEY: &str = "replaygain_mode";
/// Setting: extra dB applied on top of the tag, e.g. `+3` for a quiet system.
pub const PREAMP_KEY: &str = "replaygain_preamp_db";
/// Setting: pull the gain back when the tagged peak says it would clip.
pub const PREVENT_CLIPPING_KEY: &str = "replaygain_prevent_clipping";
/// Setting (#1694) : plafond dBTP de l'anti-écrêtage — `0` (défaut, plein
/// niveau, comportement historique), `-0.5` ou `-1`. N'agit que si
/// `prevent_clipping` est actif : le facteur est tiré pour que
/// `peak × factor` ne dépasse pas `10^(plafond/20)`. Avec le true peak
/// stocké, cela laisse la marge inter-échantillons aux DAC qui la demandent.
pub const TRUE_PEAK_CEILING_KEY: &str = "replaygain_true_peak_ceiling_db";

/// Au-delà de ce pic, la valeur tagée n'est plus une amplitude normalisée.
///
/// Un pic ReplayGain vaut `1.0` à la pleine échelle. Un master écrêté monte
/// légitimement au-dessus (`1.02`, `1.1`) : le plafond est posé à `4.0`
/// (+12 dBFS) pour que ces pics-là continuent de protéger. Une valeur plus
/// haute — `32768`, `8388607` — est une échelle d'ÉCHANTILLON écrite par un
/// vieux tagueur ; l'utiliser comme pic ferait tomber le facteur sur son
/// plancher, soit 60 dB de trop peu.
pub const PEAK_MAX_PLAUSIBLE: f64 = 4.0;

/// Témoin de provenance du gain de PISTE (#1627) : posé par la passe d'analyse
/// à côté de `rg_track_gain`. Absent ⇒ la valeur vient des tags du fichier.
pub const TRACK_SOURCE_KEY: &str = "rg_track_source";
/// Idem pour le gain d'ALBUM, posé par `analyze_album_batch`.
pub const ALBUM_SOURCE_KEY: &str = "rg_album_source";
/// Seule valeur écrite dans ces deux témoins : la mesure vient de Tune.
/// « Tags du fichier » ne s'écrit pas — c'est l'ABSENCE de témoin.
pub const SOURCE_ANALYSIS: &str = "analysis";
/// Sentinelle « on a essayé d'analyser cette piste » (voir `spawn`).
const ANALYZED_KEY: &str = "rg_analyzed";

/// D'où vient le gain qui s'applique réellement à une piste (#1627).
///
/// Les deux sources s'écrivent sous les MÊMES clés (`rg_track_gain`…) et sont
/// volontairement interchangeables à la lecture. Cet enum ne change rien à ce
/// qui est appliqué : il permet seulement de le DIRE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GainSource {
    /// Lu tel quel dans les tags du fichier au scan (rsgain, foobar, …).
    /// Ceux-ci priment toujours : ils ne sont jamais recalculés (#1382).
    FileTags,
    /// Mesuré par la passe EBU R128 de Tune, ou calculé par elle
    /// (gain d'album dérivé des gains de piste).
    Analysis,
}

impl GainSource {
    /// Valeur stable pour l'API et l'affichage — jamais traduite.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FileTags => "file_tags",
            Self::Analysis => "analysis",
        }
    }

    /// Libellé français court, tel qu'il apparaît dans le chemin du signal.
    pub fn label_fr(self) -> &'static str {
        match self {
            Self::FileTags => "tags du fichier",
            Self::Analysis => "analyse Tune",
        }
    }
}

/// Les TROIS modes de la demande initiale (#1627), vus comme UN seul choix.
///
/// Le serveur n'a jamais eu de réglage à trois valeurs, et n'en gagne pas un
/// ici : c'est une LECTURE dérivée des deux axes existants
/// (`replaygain_mode` × `replaygain_analysis_enabled`), sans migration et sans
/// changement de sémantique. Un enum unique en base coûterait soit la
/// distinction piste/album, soit cinq valeurs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayGainSourceMode {
    /// « 1- néant » : aucun gain appliqué, et depuis #2496 aucune analyse.
    Off,
    /// « 2- fichier » : on applique ce que les fichiers portent, rien d'autre.
    FileTagsOnly,
    /// « 3- calcul » : les tags priment, l'analyse ne COMBLE que les manques.
    /// Jamais d'écrasement — c'est la réponse à #1382.
    TagsThenAnalysis,
}

impl ReplayGainSourceMode {
    /// Valeur stable pour l'API — jamais traduite, jamais persistée.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::FileTagsOnly => "file_tags",
            Self::TagsThenAnalysis => "tags_then_analysis",
        }
    }

    /// Relit une des trois valeurs de [`as_str`](Self::as_str).
    ///
    /// Rend `None` — et JAMAIS un repli silencieux — sur tout le reste. Un
    /// `_ => Self::Off` en fin de `match` serait ici la faute classique : un
    /// mode mal orthographié couperait le ReplayGain sans le dire, en
    /// répondant « c'est fait ». L'appelant doit refuser la demande.
    pub fn from_setting(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "off" => Some(Self::Off),
            "file_tags" => Some(Self::FileTagsOnly),
            "tags_then_analysis" => Some(Self::TagsThenAnalysis),
            _ => None,
        }
    }
}

/// Les écritures que le mode à trois valeurs de #1627 REPRÉSENTE.
///
/// C'est la moitié écriture de [`active_source_mode`], et sa réciproque :
/// aucune clé nouvelle n'est persistée, les deux axes existants restent la
/// seule vérité en base. On traduit seulement « néant / tags du fichier /
/// calcul » vers les `settings` que tout le reste du serveur lit déjà
/// ([`ReplayGainSettings::load`], [`analysis_enabled`]) — donc les chemins
/// d'application du gain ne changent pas d'un octet.
///
/// `granularite` est l'axe piste/album, que les trois modes ne portent PAS :
/// il est conservé tel qu'il est, jamais deviné.
pub fn source_mode_settings(
    mode: ReplayGainSourceMode,
    granularite: ReplayGainMode,
) -> Vec<(&'static str, &'static str)> {
    // `Off` ne peut pas servir de granularité : réécrire `off` en réponse à
    // « tags du fichier » ne changerait RIEN tout en répondant « ok ». Le
    // repli est `track`, la granularité par défaut de l'interface.
    let granularite = match granularite {
        ReplayGainMode::Album => "album",
        _ => "track",
    };
    match mode {
        // « Néant » ne touche PAS à la coche d'analyse : `analysis_enabled`
        // rend déjà `false` quand le mode est `off` (#2496), et l'écraser
        // détruirait le choix de l'utilisateur pour le jour où il rallume.
        ReplayGainSourceMode::Off => vec![(MODE_KEY, "off")],
        ReplayGainSourceMode::FileTagsOnly => {
            vec![(MODE_KEY, granularite), (ANALYSIS_ENABLED_KEY, "false")]
        }
        ReplayGainSourceMode::TagsThenAnalysis => {
            vec![(MODE_KEY, granularite), (ANALYSIS_ENABLED_KEY, "true")]
        }
    }
}

/// Lequel des trois modes est ACTIF, dérivé des deux réglages existants.
///
/// Rien n'est lu ni écrit d'autre que ce que lisaient déjà
/// [`ReplayGainSettings::load`] et [`analysis_enabled`] : les deux axes restent
/// la seule vérité, cette fonction ne fait que les nommer ensemble.
pub fn active_source_mode(backend: &Arc<dyn DbBackend>) -> ReplayGainSourceMode {
    let mode = ReplayGainSettings::load(backend).mode;
    if mode == ReplayGainMode::Off {
        // `analysis_enabled` rend déjà `false` dans ce cas (#2496) ; on ne
        // dépend pas de cette coïncidence, on l'énonce.
        return ReplayGainSourceMode::Off;
    }
    if analysis_enabled(backend) {
        ReplayGainSourceMode::TagsThenAnalysis
    } else {
        ReplayGainSourceMode::FileTagsOnly
    }
}

/// How the gain is chosen for a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayGainMode {
    /// No gain is applied — the stream stays bit-identical to the source.
    Off,
    /// Per-track gain: every track plays at the same loudness.
    Track,
    /// Per-album gain: the relative dynamics between tracks of an album are
    /// preserved, which is what a classical or concept album needs.
    Album,
}

impl ReplayGainMode {
    pub fn from_setting(raw: &str) -> Self {
        match raw.trim().to_lowercase().as_str() {
            "track" => Self::Track,
            "album" => Self::Album,
            _ => Self::Off,
        }
    }
}

/// The gain to apply to one track, in dB, plus the peak it was tagged with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrackGain {
    pub gain_db: f64,
    pub peak: Option<f64>,
}

/// Resolved playback settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReplayGainSettings {
    pub mode: ReplayGainMode,
    pub preamp_db: f64,
    pub prevent_clipping: bool,
    /// Plafond dBTP de l'anti-écrêtage (#1694) : 0 (défaut) = pleine
    /// échelle, comportement historique ; −0.5 ou −1 laissent une marge.
    /// Toujours ≤ 0 — un plafond positif serait une demande d'écrêter.
    pub true_peak_ceiling_db: f64,
}

impl Default for ReplayGainSettings {
    fn default() -> Self {
        Self {
            mode: ReplayGainMode::Off,
            preamp_db: 0.0,
            prevent_clipping: true,
            true_peak_ceiling_db: 0.0,
        }
    }
}

impl ReplayGainSettings {
    /// Read the three settings. Anything unreadable falls back to the default,
    /// which is `Off` — a broken setting must never silently alter the sound.
    pub fn load(backend: &Arc<dyn DbBackend>) -> Self {
        let settings = SettingsRepo::with_backend(backend.clone());
        let get = |k: &str| settings.get(k).ok().flatten();
        Self {
            mode: get(MODE_KEY)
                .map(|v| ReplayGainMode::from_setting(&v))
                .unwrap_or(ReplayGainMode::Off),
            preamp_db: get(PREAMP_KEY)
                .and_then(|v| v.trim().parse::<f64>().ok())
                .unwrap_or(0.0)
                .clamp(-15.0, 15.0),
            prevent_clipping: get(PREVENT_CLIPPING_KEY)
                .map(|v| v != "false")
                .unwrap_or(true),
            // Borné à [−1, 0] : l'UI ne propose que 0 / −0.5 / −1, et une
            // valeur cassée en base ne doit jamais creuser le niveau.
            true_peak_ceiling_db: get(TRUE_PEAK_CEILING_KEY)
                .and_then(|v| v.trim().parse::<f64>().ok())
                .unwrap_or(0.0)
                .clamp(-1.0, 0.0),
        }
    }
}

/// Read the gain stored for `track_id`, honouring the mode.
///
/// Album mode falls back to the track gain when the album values are missing —
/// half an album's worth of gain is still better than a jump in level.
pub fn stored_gain_for(
    backend: &Arc<dyn DbBackend>,
    track_id: i64,
    mode: ReplayGainMode,
) -> Option<TrackGain> {
    stored_gain_detail(backend, track_id, mode).map(|(gain, _)| gain)
}

/// Comme [`stored_gain_for`], mais dit AUSSI quelle granularité a fourni la
/// valeur : en mode album, une piste sans tags d'album retombe sur le gain de
/// piste, et un affichage (chemin du signal) doit nommer ce qui s'applique
/// VRAIMENT, pas le réglage demandé.
pub fn stored_gain_detail(
    backend: &Arc<dyn DbBackend>,
    track_id: i64,
    mode: ReplayGainMode,
) -> Option<(TrackGain, ReplayGainMode)> {
    stored_gain_with_peak(backend, track_id, mode).map(|(gain, mode, _)| (gain, mode))
}

/// Nature du pic utilisé, indépendante de la provenance du gain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeakKind {
    None,
    SamplePeak,
    TruePeak,
}

impl PeakKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::SamplePeak => "sample_peak",
            Self::TruePeak => "true_peak",
        }
    }

    /// Réserve de repli, pas une mesure ni une garantie universelle de crête vraie.
    pub fn headroom_db(self, settings: ReplayGainSettings) -> f64 {
        if self == Self::SamplePeak
            && settings.prevent_clipping
            && settings.mode != ReplayGainMode::Off
        {
            3.0
        } else {
            0.0
        }
    }
}

/// Gain brut, granularité effective et nature du pic (#4074).
/// Un pic d'échantillon ne devient jamais une crête vraie par changement de nom.
pub fn stored_gain_with_peak(
    backend: &Arc<dyn DbBackend>,
    track_id: i64,
    mode: ReplayGainMode,
) -> Option<(TrackGain, ReplayGainMode, PeakKind)> {
    if mode == ReplayGainMode::Off {
        return None;
    }
    let meta = TrackMetadataRepo::with_backend(backend.clone())
        .get_all(track_id)
        .ok()?;
    let pick =
        |gain_key: &str, peak_key: &str, true_peak_key: &str| -> Option<(TrackGain, PeakKind)> {
            let gain_db = meta.get(gain_key).cloned().and_then(parse_gain_db)?;
            // Un pic ReplayGain est une AMPLITUDE NORMALISÉE, pas une valeur
            // d'échantillon. Le tag est importé VERBATIM du fichier
            // (`metadata/mod.rs`, `ItemKey::ReplayGainTrackPeak` → `rg_track_peak`)
            // et certains vieux tagueurs y écrivent l'échelle de l'échantillon :
            // `32768` en 16 bits, `8388607` en 24 bits. `gain_factor` calcule alors
            // `plafond / pic`, tombe sur le plancher `0.001` de son `clamp` final,
            // et la piste sort 60 dB trop bas — inaudible, sans qu'aucun message ne
            // le dise.
            //
            // Le plafond est `PEAK_MAX_PLAUSIBLE` = 4 (+12 dBFS) et non 1 : un
            // master écrêté dépasse légitimement la pleine échelle (1,02 ; 1,1) et
            // son pic doit continuer de protéger. Au-delà, la valeur n'est plus un
            // pic normalisé mais une autre échelle ; l'IGNORER fait retomber
            // `prevent_clipping` sur le cas « aucun pic tagué », ce qui vaut mieux
            // que d'éteindre le son.
            let read_peak = |key: &str| {
                meta.get(key)
                    .and_then(|p| p.trim().parse::<f64>().ok())
                    .filter(|p| *p > 0.0 && *p <= PEAK_MAX_PLAUSIBLE)
            };
            // Le true peak (inter-échantillons, #1694) PRIME quand il existe :
            // c'est lui qui voit les overs que le sample peak rate, et c'est
            // contre lui que `prevent_clipping` et le plafond dBTP doivent tirer.
            let (peak, kind) = if let Some(peak) = read_peak(true_peak_key) {
                (Some(peak), PeakKind::TruePeak)
            } else if let Some(peak) = read_peak(peak_key) {
                (Some(peak), PeakKind::SamplePeak)
            } else {
                (None, PeakKind::None)
            };
            Some((TrackGain { gain_db, peak }, kind))
        };
    match mode {
        ReplayGainMode::Album => pick("rg_album_gain", "rg_album_peak", "rg_album_true_peak")
            .map(|(g, kind)| (g, ReplayGainMode::Album, kind))
            .or_else(|| {
                pick("rg_track_gain", "rg_track_peak", "rg_track_true_peak")
                    .map(|(g, kind)| (g, ReplayGainMode::Track, kind))
            }),
        _ => pick("rg_track_gain", "rg_track_peak", "rg_track_true_peak")
            .map(|(g, kind)| (g, ReplayGainMode::Track, kind)),
    }
}

/// D'où vient le gain que [`stored_gain_detail`] vient de rendre (#1627).
///
/// `granularity` est celle qui a effectivement FOURNI la valeur — la seconde
/// composante de [`stored_gain_detail`], pas le mode demandé : en mode album
/// sans tags d'album, c'est la provenance du gain de PISTE qu'il faut nommer.
///
/// Rend `None` quand la piste n'a aucun gain à cette granularité : ne rien
/// afficher vaut mieux qu'affirmer une origine pour une valeur absente.
pub fn stored_gain_source(
    backend: &Arc<dyn DbBackend>,
    track_id: i64,
    granularity: ReplayGainMode,
) -> Option<GainSource> {
    let meta = TrackMetadataRepo::with_backend(backend.clone())
        .get_all(track_id)
        .ok()?;
    gain_source_from_meta(&meta, granularity)
}

/// Cœur testable de [`stored_gain_source`], sur une carte déjà lue.
fn gain_source_from_meta(
    meta: &std::collections::HashMap<String, String>,
    granularity: ReplayGainMode,
) -> Option<GainSource> {
    let (gain_key, source_key) = match granularity {
        ReplayGainMode::Album => ("rg_album_gain", ALBUM_SOURCE_KEY),
        // `Off` n'atteint pas ce point via `stored_gain_detail`, qui rend
        // `None` avant. Le traiter comme `Track` évite un cas mort.
        _ => ("rg_track_gain", TRACK_SOURCE_KEY),
    };
    meta.get(gain_key)?;
    if meta
        .get(source_key)
        .is_some_and(|v| v.trim() == SOURCE_ANALYSIS)
    {
        return Some(GainSource::Analysis);
    }
    // Repli pour les bibliothèques analysées AVANT que ce témoin existe — la
    // quasi-totalité du parc installé. La passe n'analyse QUE les pistes
    // dépourvues de `rg_track_gain` (double `NOT EXISTS` du balayage) : un
    // gain présent sur une piste estampillée `rg_analyzed` a donc été mesuré
    // ici. Le seul cas où ce repli se trompe est celui d'un fichier analysé,
    // puis retagué et rescané depuis (et, en granularité album, celui d'un
    // fichier portant des tags d'album sans tags de piste) — le témoin
    // explicite ci-dessus tranche pour tout ce qui sera analysé désormais.
    if meta.contains_key(ANALYZED_KEY) {
        return Some(GainSource::Analysis);
    }
    Some(GainSource::FileTags)
}

/// Ce que l'anti-écrêtage a retenu sur le facteur demandé, et POURQUOI
/// (#4072).
///
/// Un simple `f64` ne permettait pas au chemin du signal de distinguer « le
/// gain demandé s'applique » de « le gain demandé a été refusé » : dans les
/// deux cas le facteur peut valoir 1,0, et l'étape disparaissait alors du
/// panneau. L'auditeur voyait ReplayGain armé, ses tags lus, et rien ne
/// bougeait — sans un mot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetenueAntiEcretage {
    /// Rien retenu : le facteur demandé multiplie les échantillons tel quel.
    Aucune,
    /// Le pic TAGUÉ a borné le facteur à `plafond / pic` — le cas nominal,
    /// inchangé depuis toujours.
    ParLePicTague,
    /// Aucun pic tagué (ni `rg_*_true_peak`, ni `rg_*_peak` plausible) et un
    /// gain POSITIF demandé : il est refusé en entier. Voir [`gain_factor`].
    GainPositifRefuseSansPic,
}

impl RetenueAntiEcretage {
    /// Clé stable pour le JSON du chemin du signal — jamais du français.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Aucune => "none",
            Self::ParLePicTague => "tagged_peak",
            Self::GainPositifRefuseSansPic => "refused_no_peak",
        }
    }
}

/// The scalar linear factor: `1.0` means "leave the audio alone".
/// This low-level API cannot infer peak provenance. For stored metadata use
/// [`playback_factor`] or [`gain_factor_with_peak`] so sample peaks get headroom.
///
/// Clipping prevention is not cosmetic. A loudness-war master tagged at
/// `peak = 1.0` with a positive gain would be pushed past full scale and
/// crunch on every peak — the listener would blame Tune, rightly. When the
/// tagged peak says the result would exceed full scale, the factor is pulled
/// back to exactly what fits.
///
/// 🔴 **#4072 — sans pic tagué, le facteur ne dépasse JAMAIS l'unité.** Le
/// garde-fou ci-dessus ne s'armait que si `gain.peak` existait ; sans pic il
/// ne retenait RIEN, et `prevent_clipping` — armé par défaut — ne prévenait
/// rien du tout. Mesuré par le banc T9 (#2218,
/// `docs/mesures/2218-marge-ecretage-crete-vraie.md`, Q1) : +6 dB sur un sinus
/// à −0,1 dBFS, pic non tagué, garde-fou armé ⇒ **29 174 / 44 100 échantillons
/// écrêtés dur (66,2 %)**, excès maximal 31 866 LSB.
///
/// Le choix est le **refus**, pas le déclenchement de l'analyse : cette
/// fonction est pure et synchrone, appelée au démarrage d'une piste et à
/// chaque construction du chemin du signal, tandis que mesurer un pic exige de
/// décoder le fichier entier (`measure_loudness_and_peak`, des secondes à des
/// minutes, borné à `PER_TRACK_ANALYSIS_TIMEOUT_SECS` = 180 s). Le déclenchement
/// existe déjà, ailleurs et au bon endroit : la passe de fond de ce module
/// remplit `rg_track_peak`, et la piste retrouve son gain positif dès qu'elle
/// est mesurée. Refuser en attendant ne coûte qu'un gain non appliqué ; le
/// contraire coûte 66 % d'échantillons mutilés.
///
/// **Le plafond dBTP n'entre PAS dans cette borne.** Sans pic, la borne sûre
/// est l'unité — le signal source tient déjà sous le rail, le laisser tel quel
/// ne peut rien faire déborder. Descendre à `ceiling` (−0,5 / −1 dBTP)
/// atténuerait silencieusement toute piste non taguée, y compris celles dont
/// le gain demandé est nul ; la marge inter-échantillons est une autre
/// question, tenue par le pic vrai (issue C de T9).
///
/// L'atténuation n'est jamais touchée : un gain négatif ne peut pas écrêter,
/// et c'est la grande majorité des valeurs ReplayGain réelles.
pub fn gain_factor(gain: TrackGain, settings: ReplayGainSettings) -> f64 {
    gain_factor_detail(gain, settings).0
}

/// [`gain_factor`], qui dit AUSSI ce que l'anti-écrêtage a retenu.
///
/// Même calcul, même résultat au bit près : `gain_factor` délègue ici. Le
/// second membre sert au chemin du signal, qui doit nommer un gain refusé
/// plutôt que de faire disparaître l'étape.
pub fn gain_factor_detail(
    gain: TrackGain,
    settings: ReplayGainSettings,
) -> (f64, RetenueAntiEcretage) {
    if settings.mode == ReplayGainMode::Off {
        return (1.0, RetenueAntiEcretage::Aucune);
    }
    let total_db = (gain.gain_db + settings.preamp_db).clamp(-30.0, 30.0);
    let mut factor = 10f64.powf(total_db / 20.0);
    let mut retenue = RetenueAntiEcretage::Aucune;
    if settings.prevent_clipping {
        // Plafond dBTP (#1694) : 0 dB = pleine échelle (comportement
        // historique, à l'identique) ; −0.5 / −1 laissent une marge
        // inter-échantillons. Le peak stocké est le true peak quand
        // l'analyse l'a mesuré (`stored_gain_detail` le préfère).
        let ceiling = 10f64.powf(settings.true_peak_ceiling_db.min(0.0) / 20.0);
        match gain.peak {
            // Un pic tagué : la borne exacte, celle d'avant.
            Some(peak) if peak > 0.0 => {
                if factor * peak > ceiling {
                    factor = ceiling / peak;
                    retenue = RetenueAntiEcretage::ParLePicTague;
                }
            }
            // Aucun pic exploitable — absent, nul, négatif, ou hors échelle
            // écarté par `stored_gain_detail` (`PEAK_MAX_PLAUSIBLE`). Le pic
            // réel peut valoir 1,0 : tout facteur au-dessus de l'unité porte
            // alors des échantillons au-delà du rail.
            _ => {
                if factor > 1.0 {
                    factor = 1.0;
                    retenue = RetenueAntiEcretage::GainPositifRefuseSansPic;
                }
            }
        }
    }
    // A factor below this is inaudible attenuation of a signal to nothing; a
    // factor above is a bug, not a preference.
    (factor.clamp(0.001, 4.0), retenue)
}

/// Facteur de lecture tenant compte de la nature du pic.
/// La réserve de 3 dB borne le gain quand seul un sample peak est disponible.
/// Elle ne s'ajoute pas à une atténuation déjà suffisante et ne modifie pas
/// les tags. Une crête vraie exploitable remplace cette estimation.
/// Les helpers historiques sans nature de pic restent des calculs scalaires ;
/// les consommateurs de métadonnées doivent utiliser cette porte.
pub fn gain_factor_with_peak(
    mut gain: TrackGain,
    settings: ReplayGainSettings,
    kind: PeakKind,
) -> (f64, RetenueAntiEcretage) {
    if let Some(peak) = gain.peak {
        gain.peak = Some(peak * 10f64.powf(kind.headroom_db(settings) / 20.0));
    }
    gain_factor_detail(gain, settings)
}

/// Scale interleaved PCM in place by a linear factor.
///
/// For outputs that receive an encoded stream rather than rendering samples
/// themselves: the gain has to be baked in here or it never happens.
/// Saturating on the way out — a sample pushed past full scale wraps around
/// into a loud click if it is simply truncated.
///
/// #2218 (T9, défaut A) : cette porte ne connaît ni la piste ni la zone — le
/// bras progressif lui passe un `f64` nu, bloc par bloc. Elle COMPTE donc ses
/// écrêtés par appel dans le registre du processus
/// (`audio::ecretage::REGISTRE`, section `dsp_ecretage` du rapport) et ne
/// dit qu'UNE ligne `dsp_ecretage`, au premier bloc du processus qui écrête :
/// une ligne par bloc noierait le journal. Les lignes par piste (premier
/// écrêtage, fin) sont portées par [`GainReplay`], qui sait où une piste
/// commence et finit.
///
/// #4076 : les échantillons produits ne sont PLUS ceux d'avant — le dither a
/// remplacé la troncature vers zéro, c'est tout l'objet du correctif. Les
/// empreintes de `tune-core/tests/ecretage_compte_2218.rs` ont été relevées à
/// neuf ; les COMPTEURS d'écrêtage, eux, sont inchangés (ils comparent la
/// valeur idéale au rail, en amont du bruit et de l'arrondi).
pub fn apply_gain_pcm(pcm: &mut [u8], bit_depth: u16, factor: f64) {
    let mut compteur = CompteurDEcretage::default();
    apply_gain_pcm_compte(pcm, bit_depth, factor, &mut compteur);
    let premier_du_processus = crate::audio::ecretage::REGISTRE
        .replaygain
        .absorber(&CompteurDEcretage::default(), &compteur);
    if premier_du_processus {
        crate::audio::ecretage::dire_premier(
            crate::audio::ecretage::EtageEcretant::ReplayGain,
            crate::audio::ecretage::Portee::Processus,
            &compteur,
        );
    }
}

/// [`apply_gain_pcm`] qui COMPTE dans `compteur` chaque échantillon que le
/// clamp ramène au rail — la condition du clamp, ni plus ni moins.
///
/// #4076 — l'écriture ne tronque plus vers zéro. Le produit passe par
/// [`crate::audio::dither::Dither::quantifier`] : bruit TPDF ±1 LSB, puis
/// arrondi au plus proche, puis saturation. Ce qui était mesuré et qui
/// disparaît : un facteur de 1 − 10⁻⁷ (−0,000001 dB, inaudible **en tant que
/// gain**) déplaçait 44 098 échantillons non nuls sur 44 098 d'un LSB **vers
/// zéro**, et à −1 dB l'erreur portait le signe du signal — une distorsion,
/// pas un bruit.
///
/// Deux invariants tiennent ce changement :
///
/// * **Facteur ENTIER ⇒ aucun dither** (`Dither::pour_facteur`) : 1, 2, 0
///   envoient un entier sur un entier, sans rien perdre. Le retour immédiat
///   sur facteur unitaire ci-dessous reste, et le couvre d'avance.
/// * **Le comptage d'écrêtage ne change pas** : il compare la valeur IDÉALE
///   (produit avant saturation) au rail, donc ni le bruit ni l'arrondi
///   n'entrent dans sa condition.
///
/// Zéro allocation : le générateur est un `u64` sur la pile, et sa graine
/// vient du contenu du bloc — même bloc, même facteur, **même bruit**, ce qui
/// garde le cache de transcodage et la reprise par `Range` exacts à l'octet
/// (voir la note « le dither est DÉTERMINISTE » de [`crate::audio::dither`]).
pub fn apply_gain_pcm_compte(
    pcm: &mut [u8],
    bit_depth: u16,
    factor: f64,
    compteur: &mut CompteurDEcretage,
) {
    if pcm.is_empty() || (factor - 1.0).abs() < 1e-9 {
        return;
    }
    // Pas de requantification, pas de dither : `pour_facteur` rend `None` pour
    // tout facteur ENTIER, qui envoie un entier sur un entier sans rien perdre.
    let mut dither = crate::audio::dither::Dither::pour_facteur(
        crate::audio::dither::Etage::ReplayGain,
        pcm,
        factor,
    );
    let base = compteur.echantillons_vus;
    match bit_depth {
        16 => {
            const MAX: f64 = i16::MAX as f64;
            const MIN: f64 = i16::MIN as f64;
            const PLEINE_ECHELLE: f64 = 32_768.0;
            for (i, s) in pcm.chunks_exact_mut(2).enumerate() {
                let v = i16::from_le_bytes([s[0], s[1]]) as f64 * factor;
                if v > MAX {
                    compteur.noter_ecrete(base + i as u64, v - MAX, v / PLEINE_ECHELLE);
                } else if v < MIN {
                    compteur.noter_ecrete(base + i as u64, MIN - v, -v / PLEINE_ECHELLE);
                }
                let bruit = dither.as_mut().map_or(0.0, |d| d.tirer());
                let sortie = crate::audio::dither::quantifier_avec(v, bruit, MIN, MAX) as i16;
                s.copy_from_slice(&sortie.to_le_bytes());
            }
            compteur.noter_vus((pcm.len() / 2) as u64);
        }
        24 => {
            const MAX: f64 = 8_388_607.0;
            const MIN: f64 = -8_388_608.0;
            const PLEINE_ECHELLE: f64 = 8_388_608.0;
            for (i, s) in pcm.chunks_exact_mut(3).enumerate() {
                // Sign-extend the 24-bit little-endian sample into an i32.
                let raw = ((s[2] as i32) << 24 | (s[1] as i32) << 16 | (s[0] as i32) << 8) >> 8;
                let ideal = raw as f64 * factor;
                if ideal > MAX {
                    compteur.noter_ecrete(base + i as u64, ideal - MAX, ideal / PLEINE_ECHELLE);
                } else if ideal < MIN {
                    compteur.noter_ecrete(base + i as u64, MIN - ideal, -ideal / PLEINE_ECHELLE);
                }
                let bruit = dither.as_mut().map_or(0.0, |d| d.tirer());
                let v = crate::audio::dither::quantifier_avec(ideal, bruit, MIN, MAX) as i32;
                s[0] = (v & 0xFF) as u8;
                s[1] = ((v >> 8) & 0xFF) as u8;
                s[2] = ((v >> 16) & 0xFF) as u8;
            }
            compteur.noter_vus((pcm.len() / 3) as u64);
        }
        32 => {
            const MAX: f64 = i32::MAX as f64;
            const MIN: f64 = i32::MIN as f64;
            const PLEINE_ECHELLE: f64 = 2_147_483_648.0;
            for (i, s) in pcm.chunks_exact_mut(4).enumerate() {
                let raw = i32::from_le_bytes([s[0], s[1], s[2], s[3]]);
                let ideal = raw as f64 * factor;
                if ideal > MAX {
                    compteur.noter_ecrete(base + i as u64, ideal - MAX, ideal / PLEINE_ECHELLE);
                } else if ideal < MIN {
                    compteur.noter_ecrete(base + i as u64, MIN - ideal, -ideal / PLEINE_ECHELLE);
                }
                let bruit = dither.as_mut().map_or(0.0, |d| d.tirer());
                let v = crate::audio::dither::quantifier_avec(ideal, bruit, MIN, MAX) as i32;
                s.copy_from_slice(&v.to_le_bytes());
            }
            compteur.noter_vus((pcm.len() / 4) as u64);
        }
        // 8-bit and anything exotic: leave the audio strictly alone rather
        // than guess at its encoding.
        _ => {}
    }
}

/// Le ReplayGain d'UNE piste, avec son compteur d'écrêtage (#2218, T9 A).
///
/// C'est ce qu'un porteur DSP devrait tenir à la place d'un `f64` nu : il
/// sait où la piste commence (sa construction) et où elle finit (sa
/// destruction), donc il peut dire `dsp_ecretage` UNE fois au premier
/// écrêtage et UNE fois à la fin avec le total — jamais par bloc. Les
/// échantillons sortent de [`apply_gain_pcm_compte`], à l'octet près ceux de
/// [`apply_gain_pcm`]. Le bras progressif (`StreamingDsp.replaygain:
/// Option<f64>`, `orchestrator.rs`) ne le porte pas encore : c'est l'écrivain
/// de l'orchestrateur qui branche.
pub struct GainReplay {
    facteur: f64,
    ecretage: CompteurDEcretage,
    premier_dit: bool,
    fin_dite: bool,
}

impl GainReplay {
    /// Un facteur linéaire, tel que [`gain_factor`] le rend.
    pub fn new(facteur: f64) -> Self {
        Self {
            facteur,
            ecretage: CompteurDEcretage::default(),
            premier_dit: false,
            fin_dite: false,
        }
    }

    pub fn facteur(&self) -> f64 {
        self.facteur
    }

    /// Le compteur de la piste, tel qu'il est.
    pub fn ecretage(&self) -> CompteurDEcretage {
        self.ecretage
    }

    /// Un bloc de la piste, en place. Compte, et dit le premier écrêtage
    /// UNE fois — après le bloc, jamais dans la boucle d'échantillons.
    pub fn process(&mut self, pcm: &mut [u8], bit_depth: u16) {
        let avant = self.ecretage;
        apply_gain_pcm_compte(pcm, bit_depth, self.facteur, &mut self.ecretage);
        crate::audio::ecretage::REGISTRE
            .replaygain
            .absorber(&avant, &self.ecretage);
        if !self.premier_dit && self.ecretage.echantillons_ecretes > 0 {
            self.premier_dit = true;
            crate::audio::ecretage::dire_premier(
                crate::audio::ecretage::EtageEcretant::ReplayGain,
                crate::audio::ecretage::Portee::Piste,
                &self.ecretage,
            );
        }
    }
}

impl Drop for GainReplay {
    fn drop(&mut self) {
        if self.fin_dite {
            return;
        }
        self.fin_dite = true;
        if self.ecretage.echantillons_ecretes > 0 {
            crate::audio::ecretage::REGISTRE.replaygain.piste_close();
        }
        crate::audio::ecretage::dire_fin(
            crate::audio::ecretage::EtageEcretant::ReplayGain,
            crate::audio::ecretage::Portee::Piste,
            &self.ecretage,
        );
    }
}

/// Convenience: the factor to apply for `track_id`, or `1.0` when ReplayGain is
/// off, unmeasured, or unreadable.
pub fn playback_factor(backend: &Arc<dyn DbBackend>, track_id: i64) -> f64 {
    let settings = ReplayGainSettings::load(backend);
    match stored_gain_with_peak(backend, track_id, settings.mode) {
        Some((gain, _, kind)) => gain_factor_with_peak(gain, settings, kind).0,
        None => 1.0,
    }
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod registre_tests {
    use super::peut_ecrire_le_dr;
    use std::sync::Arc;

    use crate::db::backend::DbBackend;
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;
    use crate::db::task_run_repo::{Execution, TACHE_REPLAYGAIN, TaskRunRepo};

    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    // ── PLAGE DYNAMIQUE : le tag du fichier fait foi ──────────────────────

    /// 🔴 UN DR LU DANS LE FICHIER N'EST JAMAIS ÉCRASÉ PAR LE CALCUL.
    ///
    /// Le tag porte la valeur que le producteur du disque a mesurée ; la
    /// remplacer par notre estimation perdrait une donnée d'origine.
    ///
    /// Ce témoin APPELLE la décision. Une première version recopiait la
    /// condition dans le test et l'affirmait sur elle-même — un `assert` qui
    /// se prouvait tout seul, vert quoi que fasse le code.
    #[test]
    fn le_dr_du_fichier_prime_sur_le_dr_calcule() {
        assert!(
            !peut_ecrire_le_dr(Some("14")),
            "le calcul écraserait le tag du fichier"
        );
        assert!(
            !peut_ecrire_le_dr(Some("0")),
            "DR 0 est une VRAIE valeur, pas une absence"
        );
    }

    /// LA CONTRE-ÉPREUVE — sans elle, un code qui n'écrirait JAMAIS serait vert.
    #[test]
    fn une_piste_sans_dr_recoit_le_dr_calcule() {
        assert!(
            peut_ecrire_le_dr(None),
            "aucune piste ne recevrait jamais de DR calculé"
        );
    }

    /// Une valeur VIDE n'est pas une valeur.
    ///
    /// Un tag présent mais vide (`DYNAMIC RANGE=`) existe sur des fichiers mal
    /// étiquetés. Le traiter comme « déjà tagué » condamnerait ces pistes à
    /// n'avoir jamais de DR, ni lu ni calculé.
    #[test]
    fn un_dr_vide_ou_blanc_ne_bloque_pas_le_calcul() {
        for vide in ["", "   ", "\t", "\n"] {
            assert!(
                peut_ecrire_le_dr(Some(vide)),
                "un tag vide ({vide:?}) passe pour une vraie valeur"
            );
        }
    }

    /// Le motif inscrit quand ces tests-ci ferment une campagne : ils
    /// éprouvent le COMPTE de lignes, pas leur texte.
    const MOTIF: &str = "rien a faire";

    /// Le registre inscrit UNE ligne par changement d'état — et il en inscrit
    /// bien une quand on passe de « plus rien à faire » à « désactivée ».
    #[test]
    fn le_registre_inscrit_le_changement_de_motif_et_pas_la_repetition() {
        let registre = TaskRunRepo::with_backend(base());

        let mut campagne: Option<Execution> = None;
        let mut analysees = 0i64;
        let mut dernier: Option<&'static str> = None;
        let lignes = || {
            registre
                .lister(Some(TACHE_REPLAYGAIN), 50)
                .unwrap()
                .into_iter()
                .filter_map(|r| r.detail)
                .collect::<Vec<_>>()
        };

        super::clore_campagne(
            &registre,
            &mut campagne,
            &mut analysees,
            &mut dernier,
            "fini",
        );
        assert_eq!(lignes().len(), 1);
        // Répétition : rien de neuf, on ne chasse pas l'historique utile hors
        // de la rétention.
        super::clore_campagne(
            &registre,
            &mut campagne,
            &mut analysees,
            &mut dernier,
            "fini",
        );
        assert_eq!(lignes().len(), 1);

        // 🔴 LE DÉFAUT : avec l'ancien drapeau booléen, ce passage n'écrivait
        // RIEN. L'utilisateur décochait « Analyse ReplayGain » et le registre
        // continuait d'affirmer qu'il n'y avait « rien à faire ».
        super::clore_campagne(
            &registre,
            &mut campagne,
            &mut analysees,
            &mut dernier,
            "desactivee",
        );
        let l = lignes();
        assert_eq!(l.len(), 2, "le changement d'état doit s'inscrire : {l:?}");
        assert!(l.iter().any(|d| d.contains("desactivee")), "{l:?}");
        assert!(l.iter().any(|d| d.contains("fini")), "{l:?}");
    }

    /// Une campagne, c'est du premier lot trouvé jusqu'au retour au repos —
    /// PAS un lot. La boucle en enchaîne des centaines : une ligne par lot
    /// consommerait toute la rétention en quelques minutes sans jamais rien
    /// raconter d'autre que « ça avance ».
    #[test]
    fn une_campagne_couvre_toute_la_serie_de_lots_et_pas_un_lot() {
        let db = base();
        let registre = TaskRunRepo::with_backend(db.clone());
        let mut campagne: Option<Execution> = None;
        let mut analysees: i64 = 0;
        let mut repos: Option<&'static str> = None;

        // Trois lots successifs, comme la boucle les enchaîne.
        for lot in [12i64, 30, 8] {
            if campagne.is_none() {
                campagne = Some(registre.ouvrir(TACHE_REPLAYGAIN));
                analysees = 0;
            }
            analysees += lot;
            repos = None;
        }
        assert_eq!(
            registre.lister(Some(TACHE_REPLAYGAIN), 10).unwrap().len(),
            1,
            "trois lots, UNE ligne"
        );

        super::clore_campagne(&registre, &mut campagne, &mut analysees, &mut repos, MOTIF);

        let l = registre.lister(Some(TACHE_REPLAYGAIN), 10).unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].outcome, "succes");
        assert_eq!(l[0].items, Some(50), "12 + 30 + 8, la campagne entière");
    }

    /// Le repos sans campagne s'inscrit UNE fois. C'est la réponse à « ça n'a
    /// rien fait » — sans elle, une bibliothèque déjà entièrement analysée
    /// serait indistinguable d'une passe jamais lancée.
    #[test]
    fn le_repos_s_inscrit_une_seule_fois_par_transition() {
        let db = base();
        let registre = TaskRunRepo::with_backend(db.clone());
        let mut campagne: Option<Execution> = None;
        let mut analysees: i64 = 0;
        let mut repos: Option<&'static str> = None;

        for _ in 0..5 {
            super::clore_campagne(&registre, &mut campagne, &mut analysees, &mut repos, MOTIF);
        }

        let l = registre.lister(Some(TACHE_REPLAYGAIN), 10).unwrap();
        assert_eq!(
            l.len(),
            1,
            "cinq tours de boucle au repos, une seule ligne — sinon la \
             rétention serait remplie de lignes vides et l'historique utile \
             chassé"
        );
        assert_eq!(l[0].outcome, "rien_a_faire");
        assert_eq!(l[0].items, Some(0));
    }

    /// Fermer une campagne ne doit PAS écrire en plus une ligne « rien à
    /// faire » : la campagne est déjà la trace du passage. Deux lignes pour un
    /// seul retour au repos rendraient le registre illisible.
    #[test]
    fn fermer_une_campagne_n_ajoute_pas_une_ligne_de_repos() {
        let db = base();
        let registre = TaskRunRepo::with_backend(db.clone());
        let mut campagne: Option<Execution> = None;
        let mut analysees: i64 = 0;
        let mut repos: Option<&'static str> = None;

        super::clore_campagne(&registre, &mut campagne, &mut analysees, &mut repos, MOTIF);
        campagne = Some(registre.ouvrir(TACHE_REPLAYGAIN));
        analysees = 7;
        repos = None;
        super::clore_campagne(&registre, &mut campagne, &mut analysees, &mut repos, MOTIF);
        super::clore_campagne(&registre, &mut campagne, &mut analysees, &mut repos, MOTIF);

        let l = registre.lister(Some(TACHE_REPLAYGAIN), 10).unwrap();
        assert_eq!(l.len(), 2, "un repos, puis une campagne — et rien de plus");
        let verdicts: Vec<&str> = l.iter().map(|x| x.outcome.as_str()).collect();
        assert!(verdicts.contains(&"rien_a_faire"), "{verdicts:?}");
        assert!(verdicts.contains(&"succes"), "{verdicts:?}");
        let campagne_close = l.iter().find(|x| x.outcome == "succes").unwrap();
        assert_eq!(campagne_close.items, Some(7));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le garde-fou lecture partagé par les deux sweeps (#1310, #1515) : il lit
    /// `zones.last_play_state` et doit tomber en panne OUVERTE (false) si la
    /// table manque, pour ne jamais geler l'analyse sur un hoquet de base.
    #[test]
    fn any_zone_playing_reads_last_play_state_and_fails_open() {
        use crate::db::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db.clone());

        // Table absente → erreur de requête → false (panne ouverte).
        assert!(!any_zone_playing(&backend));

        db.execute_batch(
            "CREATE TABLE zones (id INTEGER PRIMARY KEY, name TEXT, last_play_state TEXT);
             INSERT INTO zones (id, name, last_play_state)
                 VALUES (1, 'Salon', 'stopped'), (2, 'Bureau', 'paused');",
        )
        .unwrap();
        assert!(!any_zone_playing(&backend));

        db.execute_batch("UPDATE zones SET last_play_state = 'playing' WHERE id = 2;")
            .unwrap();
        assert!(any_zone_playing(&backend));

        // La garde doit NOMMER la zone qui bloque : sans ce nom, une analyse
        // figée par une zone restée à `playing` après un arrêt brutal est
        // indiagnosticable depuis les journaux (#1464, #1456, #1457).
        assert_eq!(playing_zone_name(&backend).as_deref(), Some("Bureau"));

        db.execute_batch("UPDATE zones SET last_play_state = 'stopped';")
            .unwrap();
        assert_eq!(playing_zone_name(&backend), None);
    }

    // ---------------------------------------------------------------------
    // #1865 — chemin stocké en NFC, fichier en NFD sur le disque.
    // ---------------------------------------------------------------------

    /// Base en mémoire avec le schéma complet, une piste, et rien d'autre.
    /// `zones` existe (via les migrations) et reste vide : `any_zone_playing`
    /// rend donc false.
    ///
    /// Le mode est armé explicitement : depuis #2496, un `replaygain_mode`
    /// ABSENT vaut « Désactivé » et [`analyze_track_batch`] rend 0 sans rien
    /// lire. Sans cette ligne, les tests #1865 passeraient au vert en ne
    /// testant plus rien.
    pub(super) fn base_avec_piste(
        chemin: &str,
    ) -> (crate::db::sqlite::SqliteDb, Arc<dyn DbBackend>) {
        use crate::db::sqlite::SqliteDb;
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        db.execute("INSERT INTO artists (id, name) VALUES (1, 'Bjork')", &[])
            .unwrap();
        db.execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Homogenic', 1)",
            &[],
        )
        .unwrap();
        db.execute(
            "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, \
             sample_rate, channels) VALUES (42, 'Joga', 1, 1, ?, 300000, 44100, 2)",
            &[&chemin],
        )
        .unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db.clone());
        SettingsRepo::with_backend(backend.clone())
            .set(MODE_KEY, "track")
            .unwrap();
        (db, backend)
    }

    pub(super) fn temoins(
        db: &crate::db::sqlite::SqliteDb,
    ) -> std::collections::HashMap<String, String> {
        TrackMetadataRepo::new(db.clone()).get_all(42).unwrap()
    }

    fn empreinte_de(db: &crate::db::sqlite::SqliteDb, id: i64) -> Option<String> {
        db.query_one(
            &format!("SELECT audio_fingerprint FROM tracks WHERE id = {id}"),
            &[],
        )
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_string()))
    }

    /// BIB-B2 : la passe ReplayGain pose aussi l'empreinte du contenu, dans
    /// la foulée du décodage.
    #[tokio::test]
    async fn la_passe_replaygain_pose_aussi_l_empreinte() {
        let fixture = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/ape/sine_16s_c3000.wav"
        );
        let (db, backend) = base_avec_piste(fixture);
        assert_eq!(analyze_track_batch(&backend).await, 1);
        let e = empreinte_de(&db, 42).expect("l'empreinte est posée");
        assert!(e.starts_with("env100ms-v1:") && e.len() > 20, "{e}");
        assert!(temoins(&db).contains_key("rg_analyzed"));
        // Rien à rattraper ensuite : la piste porte déjà la version courante.
        assert_eq!(empreinter_un_lot(&backend).await, 0);
    }

    // ---------------------------------------------------------------------
    // Rattrapage de la PLAGE DYNAMIQUE — les pistes que la passe nominale
    // n'ira jamais revoir (33 414 sur 46 877 mesurées sur le .18 le
    // 09/09/2026).
    // ---------------------------------------------------------------------

    /// Un WAV de 9 s dont la PLAGE DYNAMIQUE est connue d'avance.
    ///
    /// La fixture `sine_16s_c3000.wav` ne pouvait pas servir : elle dure 1 s,
    /// soit UN bloc de 3 s. L'algorithme TT compare le DEUXIÈME pic au RMS des
    /// 20 % de blocs les plus forts — avec un seul bloc il n'y a pas de second
    /// pic, et `finish()` rend `None` à dessein. Un test bâti dessus
    /// n'éprouvait pas le rattrapage : il constatait l'absence de plage.
    ///
    /// Signal : 3 blocs de 3 s, chacun 132 cycles de sinus pleine échelle à
    /// 441 Hz (0,2993 s) puis du silence. Par canal et par bloc :
    ///   pic  = 1,0  (441 Hz à 44 100 Hz = 100 éch./cycle, l'éch. 25 vaut 1,0)
    ///   RMS  = √(2 · 0,09977 · 0,5) = 0,3159   (RMS RÉFÉRENCÉ SINUS)
    /// Les trois blocs étant identiques : pic₂ = 1,0, et les 20 % les plus
    /// forts de 3 blocs font 1 bloc, donc RMS₂₀ = 0,3159.
    ///   DR = 20 · log₁₀(1,0 / 0,3159) = 10,01 → arrondi à 10.
    pub(super) fn wav_de_plage_connue(chemin: &std::path::Path) {
        use std::io::Write;
        const SR: u32 = 44_100;
        const BLOCS: u32 = 3;
        const CYCLES: u32 = 132; // 13 200 éch. sur les 132 300 du bloc
        let frames = (SR * 3 * BLOCS) as usize;
        let mut pcm: Vec<u8> = Vec::with_capacity(frames * 4);
        for i in 0..frames {
            let dans_le_bloc = i % (SR * 3) as usize;
            let v: i16 = if dans_le_bloc < (CYCLES * 100) as usize {
                let phase = (dans_le_bloc % 100) as f64 / 100.0;
                ((phase * std::f64::consts::TAU).sin() * 32_767.0).round() as i16
            } else {
                0
            };
            pcm.extend_from_slice(&v.to_le_bytes());
            pcm.extend_from_slice(&v.to_le_bytes());
        }
        let n = pcm.len() as u32;
        let mut v: Vec<u8> = Vec::with_capacity(n as usize + 44);
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + n).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // PCM
        v.extend_from_slice(&2u16.to_le_bytes()); // stéréo
        v.extend_from_slice(&SR.to_le_bytes());
        v.extend_from_slice(&(SR * 4).to_le_bytes());
        v.extend_from_slice(&4u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&n.to_le_bytes());
        v.extend_from_slice(&pcm);
        let mut f = std::fs::File::create(chemin).unwrap();
        f.write_all(&v).unwrap();
        f.flush().unwrap();
    }

    /// Le cas du .18, reproduit : une piste porte `rg_analyzed` (posé avant
    /// que le DR n'existe) et aucune plage. `analyze_track_batch` l'écarte
    /// pour toujours ; le rattrapage doit la reprendre, calculer, écrire — et
    /// ne jamais y revenir.
    #[tokio::test]
    async fn le_rattrapage_dr_reprend_les_pistes_que_la_passe_ecarte_a_jamais() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("plage.wav");
        wav_de_plage_connue(&f);
        let (db, backend) = base_avec_piste(f.to_string_lossy().as_ref());
        let repo = TrackMetadataRepo::new(db.clone());

        // L'état hérité : analysée, mais sans plage. C'est exactement ce que
        // la base du .18 contenait sur 71 % de ses pistes.
        repo.set(42, "rg_analyzed", "1700000000").unwrap();
        repo.set(42, "rg_track_gain", "-6.50 dB").unwrap();

        // CONTRE-ÉPREUVE de l'utilité même du rattrapage : la passe nominale
        // ne rend rien sur cette piste, et ne lui donnera donc JAMAIS de DR.
        assert_eq!(
            analyze_track_batch(&backend).await,
            0,
            "la passe nominale doit bien écarter cette piste — sans cela le \
             rattrapage ne défendrait rien"
        );
        assert!(!temoins(&db).contains_key("dr_track"));

        assert_eq!(rattraper_un_lot_de_dr(&backend).await, 1);
        let t = temoins(&db);
        assert_eq!(
            t.get("dr_track").map(String::as_str),
            Some("10"),
            "la plage du signal construit vaut 10 dB, cf. wav_de_plage_connue"
        );
        assert_eq!(t.get("dr_source").map(String::as_str), Some("analysis"));
        // Le gain des tags n'a PAS bougé : le rattrapage comble un vide, il ne
        // révise pas le niveau de lecture sous les pieds de l'utilisateur.
        assert_eq!(t.get("rg_track_gain").map(String::as_str), Some("-6.50 dB"));

        // Et il ne boucle pas : la piste porte désormais une plage.
        assert_eq!(rattraper_un_lot_de_dr(&backend).await, 0);
    }

    /// 🔴 LE TAG DU FICHIER FAIT FOI. Une plage lue au scan n'est jamais
    /// remplacée par une estimation — mais un tag VIDE n'est pas une valeur.
    #[tokio::test]
    async fn le_rattrapage_dr_respecte_le_tag_du_fichier_mais_pas_un_tag_vide() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("plage.wav");
        wav_de_plage_connue(&f);
        let (db, backend) = base_avec_piste(f.to_string_lossy().as_ref());
        let repo = TrackMetadataRepo::new(db.clone());
        repo.set(42, "rg_analyzed", "1700000000").unwrap();

        // Plage venue du disque : intouchable, et même pas candidate. La
        // valeur 14 est DIFFÉRENTE des 10 que le calcul rendrait — sans cet
        // écart, le test serait vert même si le calcul écrasait le tag.
        repo.set(42, "dr_track", "14").unwrap();
        repo.set(42, "dr_source", "tag").unwrap();
        assert_eq!(rattraper_un_lot_de_dr(&backend).await, 0);
        let t = temoins(&db);
        assert_eq!(t.get("dr_track").map(String::as_str), Some("14"));
        assert_eq!(t.get("dr_source").map(String::as_str), Some("tag"));

        // CONTRE-ÉPREUVE : un tag présent mais VIDE (`DYNAMIC RANGE=`) existe
        // sur des fichiers mal étiquetés. Le traiter comme « déjà tagué »
        // condamnerait ces pistes à n'avoir jamais de plage, ni lue ni
        // calculée — le rattrapage doit les reprendre.
        repo.set(42, "dr_track", "   ").unwrap();
        assert_eq!(rattraper_un_lot_de_dr(&backend).await, 1);
        let t = temoins(&db);
        assert_eq!(t.get("dr_track").map(String::as_str), Some("10"));
        assert_eq!(t.get("dr_source").map(String::as_str), Some("analysis"));
    }

    // ---------------------------------------------------------------------
    // #5594 (lot 2) — la VERSION de l'algorithme, écrite avec la mesure.
    // ---------------------------------------------------------------------

    /// La passe nominale mesure le gain ET la plage : les deux versions
    /// s'écrivent avec les valeurs, sous leurs libellés exacts — ce sont eux
    /// que deux instances compareront.
    #[tokio::test]
    async fn la_mesure_ecrit_la_version_de_ses_algorithmes_5594() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("plage.wav");
        wav_de_plage_connue(&f);
        let (db, backend) = base_avec_piste(f.to_string_lossy().as_ref());

        assert_eq!(analyze_track_batch(&backend).await, 1);
        let t = temoins(&db);
        assert!(t.contains_key("rg_track_gain"), "{t:?}");
        assert_eq!(
            t.get(TRACK_SOURCE_KEY).map(String::as_str),
            Some("analysis")
        );
        assert_eq!(
            t.get("rg_algo").map(String::as_str),
            Some("bs1770-tp4x-v1"),
            "la mesure ReplayGain doit porter la version de son algorithme : {t:?}"
        );
        assert_eq!(t.get("dr_track").map(String::as_str), Some("10"));
        assert_eq!(t.get("dr_source").map(String::as_str), Some("analysis"));
        assert_eq!(
            t.get("dr_algo").map(String::as_str),
            Some("tt-dr-v1"),
            "la plage dynamique mesurée doit porter la version de son algorithme : {t:?}"
        );
    }

    /// Le rattrapage de la plage dynamique écrit `dr_algo` avec la plage — et
    /// JAMAIS `rg_algo` : le gain en place vient des tags, pas de Tune.
    #[tokio::test]
    async fn le_rattrapage_dr_ecrit_dr_algo_sans_inventer_rg_algo_5594() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("plage.wav");
        wav_de_plage_connue(&f);
        let (db, backend) = base_avec_piste(f.to_string_lossy().as_ref());
        let repo = TrackMetadataRepo::new(db.clone());
        repo.set(42, "rg_analyzed", "1700000000").unwrap();
        repo.set(42, "rg_track_gain", "-6.50 dB").unwrap();

        assert_eq!(rattraper_un_lot_de_dr(&backend).await, 1);
        let t = temoins(&db);
        assert_eq!(t.get("dr_track").map(String::as_str), Some("10"));
        assert_eq!(t.get(DR_ALGO_KEY).map(String::as_str), Some(DR_ALGO));
        assert!(
            !t.contains_key(RG_ALGO_KEY),
            "un gain lu dans les tags n'a pas de version Tune : {t:?}"
        );
    }

    /// Les mesures déjà en base, faites sans version, RESTENT sans version :
    /// aucune passe ne la pose après coup, et une valeur venue du disque n'en
    /// reçoit jamais.
    #[tokio::test]
    async fn une_mesure_d_avant_la_version_reste_sans_version_5594() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("plage.wav");
        wav_de_plage_connue(&f);
        let (db, backend) = base_avec_piste(f.to_string_lossy().as_ref());
        let repo = TrackMetadataRepo::new(db.clone());
        // L'état d'une base d'avant #5594 : gain et plage mesurés par Tune,
        // provenance posée, mais aucune version.
        repo.set(42, "rg_analyzed", "1700000000").unwrap();
        repo.set(42, "rg_track_gain", "-6.12 dB").unwrap();
        repo.set(42, "rg_track_peak", "0.912345").unwrap();
        repo.set(42, TRACK_SOURCE_KEY, SOURCE_ANALYSIS).unwrap();
        repo.set(42, "dr_track", "11").unwrap();
        repo.set(42, "dr_source", "analysis").unwrap();

        assert_eq!(analyze_track_batch(&backend).await, 0);
        assert_eq!(rattraper_un_lot_de_dr(&backend).await, 0);
        let t = temoins(&db);
        assert!(!t.contains_key(RG_ALGO_KEY), "{t:?}");
        assert!(!t.contains_key(DR_ALGO_KEY), "{t:?}");
        assert_eq!(t.get("rg_track_gain").map(String::as_str), Some("-6.12 dB"));
        assert_eq!(t.get("dr_track").map(String::as_str), Some("11"));

        // Une plage lue dans les tags du fichier : ni écrasée, ni versionnée.
        repo.set(42, "dr_track", "14").unwrap();
        repo.set(42, "dr_source", "tag").unwrap();
        assert_eq!(rattraper_un_lot_de_dr(&backend).await, 0);
        assert!(!temoins(&db).contains_key(DR_ALGO_KEY));
    }

    /// Un fichier introuvable se REPORTE, un fichier illisible se MARQUE. Sans
    /// la marque, le rattrapage reprendrait éternellement les mêmes pistes.
    #[tokio::test]
    async fn le_rattrapage_dr_reporte_les_absents_et_marque_les_illisibles() {
        let tmp = tempfile::TempDir::new().unwrap();
        let absent = tmp.path().join("absente.flac");
        let (db, backend) = base_avec_piste(absent.to_string_lossy().as_ref());
        let repo = TrackMetadataRepo::new(db.clone());
        repo.set(42, "rg_analyzed", "1700000000").unwrap();

        // Introuvable : reporté, JAMAIS marqué indisponible (#1865) — le
        // partage remonté doit rendre la piste au rattrapage.
        assert_eq!(rattraper_un_lot_de_dr(&backend).await, 1);
        let t = temoins(&db);
        assert!(t.contains_key("rg_path_unresolved"));
        assert!(
            !t.contains_key("dr_indisponible"),
            "un absent ne se condamne pas : il se reporte (#1865)"
        );

        // Présent mais illisible : là, la marque est légitime, et elle sort la
        // piste du rattrapage pour de bon.
        std::fs::write(&absent, b"ceci n'est pas de l'audio").unwrap();
        repo.delete(42, "rg_path_unresolved").unwrap();
        assert_eq!(rattraper_un_lot_de_dr(&backend).await, 1);
        assert!(temoins(&db).contains_key("dr_indisponible"));
        assert_eq!(
            rattraper_un_lot_de_dr(&backend).await,
            0,
            "sans la marque, ce fichier reviendrait à chaque réveil, pour toujours"
        );
    }

    // ---------------------------------------------------------------------
    // Le registre disait la même phrase pour deux états opposés.
    // ---------------------------------------------------------------------

    /// Le 09/09/2026, le .18 portait 19 lignes « aucune piste ni album sans
    /// ReplayGain » depuis le 29/08 pendant que 13 463 pistes attendaient. La
    /// passe n'avait pas fini : `replaygain_mode` était ABSENT, ce qui vaut
    /// `off`. Les deux états s'écrivaient pareil, donc rien n'était visible.
    #[test]
    fn l_etat_de_l_analyse_distingue_absent_desactive_et_decoche() {
        use crate::db::sqlite::SqliteDb;
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db.clone());
        let reglages = SettingsRepo::with_backend(backend.clone());

        // Installation NEUVE : la clé n'existe pas. C'est le cas du .18.
        assert_eq!(etat_de_l_analyse(&backend), EtatAnalyse::ModeAbsent);
        assert!(!analysis_enabled(&backend));

        reglages.set(MODE_KEY, "off").unwrap();
        assert_eq!(etat_de_l_analyse(&backend), EtatAnalyse::ModeDesactive);

        reglages.set(MODE_KEY, "track").unwrap();
        assert_eq!(etat_de_l_analyse(&backend), EtatAnalyse::Active);
        assert!(analysis_enabled(&backend));

        reglages.set(ANALYSIS_ENABLED_KEY, "false").unwrap();
        assert_eq!(etat_de_l_analyse(&backend), EtatAnalyse::CocheDecochee);

        // Le point de tout l'exercice : TROIS phrases distinctes, aucune vide.
        let motifs: Vec<&str> = [
            EtatAnalyse::ModeAbsent,
            EtatAnalyse::ModeDesactive,
            EtatAnalyse::CocheDecochee,
        ]
        .iter()
        .map(|e| e.motif().expect("un motif d'inaction"))
        .collect();
        assert_eq!(
            motifs
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            3,
            "deux états opposés ne doivent plus écrire la même ligne : {motifs:?}"
        );
        assert_eq!(EtatAnalyse::Active.motif(), None);
    }

    /// BIB-B2 : le rattrapage traite les pistes déjà analysées par le
    /// ReplayGain (jamais reprises par la passe), reporte les chemins
    /// introuvables sans marque, et ne repasse pas sur ce qu'il a fait.
    #[tokio::test]
    async fn le_rattrapage_empreinte_les_pistes_analysees_et_reporte_les_absentes() {
        let fixture = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/ape/sine_16s_c3000.wav"
        );
        let (db, backend) = base_avec_piste(fixture);
        let tmp = tempfile::TempDir::new().unwrap();
        let absent = tmp
            .path()
            .join("absente.flac")
            .to_string_lossy()
            .to_string();
        db.execute(
            "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, \
             sample_rate, channels) VALUES (43, 'Absente', 1, 1, ?, 300000, 44100, 2)",
            &[&absent],
        )
        .unwrap();
        let meta = TrackMetadataRepo::new(db.clone());
        meta.set(42, "rg_analyzed", "1").unwrap();
        meta.set(43, "rg_analyzed", "1").unwrap();
        assert_eq!(empreinte_de(&db, 42), None, "avant : rien");

        assert_eq!(
            empreinter_un_lot(&backend).await,
            2,
            "une empreinte, un report"
        );
        let e = empreinte_de(&db, 42).expect("la piste analysée reçoit son empreinte");
        assert!(e.starts_with("env100ms-v1:"), "{e}");
        assert_eq!(
            empreinte_de(&db, 43),
            None,
            "un chemin introuvable ne reçoit ni empreinte ni marque"
        );
        let m43 = TrackMetadataRepo::new(db.clone()).get_all(43).unwrap();
        assert!(
            m43.contains_key(PATH_UNRESOLVED_KEY),
            "le report est daté : {m43:?}"
        );

        assert_eq!(
            empreinter_un_lot(&backend).await,
            0,
            "rien à refaire : faite, ou reportée"
        );
        // Une version périmée est reprise.
        crate::db::track_repo::TrackRepo::with_backend(backend.clone())
            .set_audio_fingerprint(42, "vieille-v0:abcd")
            .unwrap();
        assert_eq!(empreinter_un_lot(&backend).await, 1);
        assert!(empreinte_de(&db, 42).unwrap().starts_with("env100ms-v1:"));
    }

    /// LE défaut. Un fichier introuvable N'EST PAS un fichier indécodable :
    /// aucun `rg_analyzed` ne doit être posé, sans quoi la piste sort du
    /// balayage POUR TOUJOURS — c'est ce qui a figé 114 pistes sur .18 pour
    /// zéro gain calculé.
    #[tokio::test]
    async fn un_fichier_introuvable_est_reporte_jamais_marque_analyse() {
        let tmp = tempfile::TempDir::new().unwrap();
        let absent = tmp
            .path()
            .join("Bj\u{00f6}rk - J\u{00f3}ga.flac")
            .to_string_lossy()
            .to_string();
        let (db, backend) = base_avec_piste(&absent);

        let traites = analyze_track_batch(&backend).await;

        let m = temoins(&db);
        assert!(
            !m.contains_key("rg_analyzed"),
            "un ENOENT ne doit PAS poser le temoin d'analyse ; temoins = {m:?}"
        );
        assert!(
            m.contains_key(PATH_UNRESOLVED_KEY),
            "un report date doit etre pose ; temoins = {m:?}"
        );
        // Le report compte comme progrès : sinon 135 pistes introuvables — plus
        // que TRACK_BATCH — bloqueraient la passe sur les mêmes lignes.
        assert_eq!(traites, 1, "le balayage doit avoir AVANCE");
    }

    /// Un report frais écarte la piste du lot suivant ; passé la fenêtre, elle
    /// redevient candidate. C'est ce qui empêche le report d'être, à son tour,
    /// un état définitif — un disque rebranché est repris tout seul.
    #[tokio::test]
    async fn le_report_ecarte_puis_perime() {
        let tmp = tempfile::TempDir::new().unwrap();
        let absent = tmp
            .path()
            .join("N\u{00fa}\u{00f1}ez.flac")
            .to_string_lossy()
            .to_string();
        let (db, backend) = base_avec_piste(&absent);
        let repo = TrackMetadataRepo::new(db.clone());
        let maintenant = now_epoch_secs() as i64;

        // Report tout frais → la piste n'est même pas sélectionnée.
        repo.set(42, PATH_UNRESOLVED_KEY, &deferral_stamp(maintenant))
            .unwrap();
        assert_eq!(
            analyze_track_batch(&backend).await,
            0,
            "une piste reportee a l'instant ne doit pas ressortir"
        );

        // Report périmé → elle repasse candidate, et se fait re-reporter avec
        // une estampille fraîche.
        repo.set(
            42,
            PATH_UNRESOLVED_KEY,
            &deferral_stamp(maintenant - crate::library::local_path::PATH_RETRY_AFTER_SECS - 60),
        )
        .unwrap();
        assert_eq!(
            analyze_track_batch(&backend).await,
            1,
            "passe la fenetre, la piste doit etre reessayee"
        );
        let m = temoins(&db);
        assert!(!m.contains_key("rg_analyzed"));
        assert!(m[PATH_UNRESOLVED_KEY] >= deferral_stamp(maintenant));
    }

    /// La base tient le chemin en NFC, le disque le porte en NFD : la passe
    /// doit TROUVER le fichier. Il est ici volontairement illisible (des
    /// octets quelconques), donc `rg_analyzed` est légitime — mais AUCUN
    /// report ne doit être posé, ce qui prouve que la résolution a abouti.
    ///
    /// Sans le repli NFC→NFD, cette piste serait reportée : c'est la mutation
    /// qui met ce test au rouge.
    #[tokio::test]
    async fn le_disque_en_nfd_est_retrouve_depuis_le_chemin_nfc_de_la_base() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Sur le disque : NFD (graphie d'un Mac ou d'un partage SMB).
        let nfd = tmp.path().join("Bjo\u{0308}rk - Jo\u{0301}ga.flac");
        std::fs::write(&nfd, b"pas du flac, mais bien present").unwrap();
        // En base : NFC, comme le scanner l'enregistre.
        let nfc = tmp
            .path()
            .join("Bj\u{00f6}rk - J\u{00f3}ga.flac")
            .to_string_lossy()
            .to_string();
        assert_ne!(
            nfc,
            nfd.to_string_lossy(),
            "les deux graphies doivent differer octet a octet"
        );

        let (db, backend) = base_avec_piste(&nfc);
        assert_eq!(analyze_track_batch(&backend).await, 1);

        let m = temoins(&db);
        assert!(
            !m.contains_key(PATH_UNRESOLVED_KEY),
            "le fichier a ete TROUVE : aucun report ne doit etre pose ; temoins = {m:?}"
        );
        assert!(
            m.contains_key("rg_analyzed"),
            "fichier present mais indecodable : le temoin d'analyse est legitime"
        );
    }

    /// La contrepartie du correctif : la base n'est JAMAIS réécrite. Le repli
    /// sert à ouvrir, pas à stocker — un chemin normalisé par nos soins peut
    /// être introuvable sur un montage sensible à la forme.
    #[tokio::test]
    async fn la_passe_ne_reecrit_jamais_le_chemin_stocke() {
        let tmp = tempfile::TempDir::new().unwrap();
        let nfd = tmp.path().join("E\u{0301}tienne.flac");
        std::fs::write(&nfd, b"x").unwrap();
        let nfc = tmp
            .path()
            .join("\u{00c9}tienne.flac")
            .to_string_lossy()
            .to_string();

        let (db, backend) = base_avec_piste(&nfc);
        analyze_track_batch(&backend).await;

        let apres = db
            .query_one("SELECT file_path FROM tracks WHERE id = 42", &[])
            .unwrap()
            .unwrap()
            .first()
            .and_then(|v| v.as_string())
            .unwrap();
        assert_eq!(
            apres, nfc,
            "le chemin en base doit rester EXACTEMENT celui du scanner (NFC)"
        );
    }

    /// plafond-analyse — le délai d'une piste suit sa durée : un hôte qui
    /// analyse au temps réel finit toujours, un blocage reste borné.
    #[test]
    fn le_delai_d_analyse_suit_la_duree_de_la_piste() {
        let s = |d: std::time::Duration| d.as_secs();
        assert_eq!(s(delai_d_analyse(None)), PER_TRACK_ANALYSIS_TIMEOUT_SECS);
        assert_eq!(s(delai_d_analyse(Some(0))), PER_TRACK_ANALYSIS_TIMEOUT_SECS);
        assert_eq!(
            s(delai_d_analyse(Some(-5))),
            PER_TRACK_ANALYSIS_TIMEOUT_SECS
        );
        assert_eq!(s(delai_d_analyse(Some(240_000))), 180 + 240);
        assert_eq!(s(delai_d_analyse(Some(3_600_000))), 180 + 3600);
    }

    /// Ce que l'ancien plafond écartait : la taille ESTIMÉE depuis les
    /// colonnes de la base. 30 min de 24/192 stéréo (8,3 Go estimés) étaient
    /// estampillées `rg_analyzed` + `rg_skipped_oversized`, sans gain ni plage.
    fn declarer_trente_minutes_de_24_192(db: &crate::db::sqlite::SqliteDb) {
        db.execute(
            "UPDATE tracks SET duration_ms = 1800000, sample_rate = 192000, channels = 2 \
             WHERE id = 42",
            &[],
        )
        .unwrap();
    }

    /// plafond-analyse — la piste que le plafond écartait est désormais
    /// MESURÉE : gain, crête et plage dynamique.
    #[tokio::test]
    async fn la_piste_anciennement_ecartee_pour_sa_taille_est_maintenant_mesuree() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("longue.wav");
        wav_de_plage_connue(&f);
        let (db, backend) = base_avec_piste(f.to_string_lossy().as_ref());
        declarer_trente_minutes_de_24_192(&db);

        assert_eq!(analyze_track_batch(&backend).await, 1);
        let t = temoins(&db);
        assert!(
            !t.contains_key(OVERSIZED_KEY),
            "aucune piste ne doit plus être écartée pour sa taille estimée : {t:?}"
        );
        assert!(
            t.contains_key("rg_track_gain"),
            "la piste doit être mesurée, pas seulement estampillée : {t:?}"
        );
        assert_eq!(t.get("dr_track").map(String::as_str), Some("10"), "{t:?}");
    }

    /// plafond-analyse — le rattrapage efface les marques de l'ancien
    /// plafond, et elles seules ; la passe reprend ensuite la piste.
    #[tokio::test]
    async fn le_rattrapage_rend_a_la_passe_les_pistes_ecartees_pour_leur_taille() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("longue.wav");
        wav_de_plage_connue(&f);
        let (db, backend) = base_avec_piste(f.to_string_lossy().as_ref());
        declarer_trente_minutes_de_24_192(&db);
        let repo = TrackMetadataRepo::new(db.clone());
        // 42 : ce que posait l'ancienne passe nominale.
        repo.set(42, "rg_analyzed", "1700000000").unwrap();
        repo.set(42, OVERSIZED_KEY, "1").unwrap();
        for (id, duree, cadence) in [
            (43, 300_000, 44_100),
            (44, 600_000, 2_822_400),
            (45, 1_800_000, 192_000),
        ] {
            db.execute(
                &format!(
                    "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, \
                     sample_rate, channels) VALUES ({id}, 'p{id}', 1, 1, '/nulle/part/{id}.flac', \
                     {duree}, {cadence}, 2)"
                ),
                &[],
            )
            .unwrap();
        }
        // 43 : un vrai échec de plage sur une piste CD — il reste.
        repo.set(43, DR_INDISPONIBLE_KEY, "1700000000").unwrap();
        // 44 : 10 min de DSD64 au gain lu dans les tags, que le rattrapage DR
        // avait marquée indisponible pour sa taille (4,2 Go estimés).
        repo.set(44, "rg_track_gain", "-3.00 dB").unwrap();
        repo.set(44, DR_INDISPONIBLE_KEY, "1700000000").unwrap();
        // 45 : marquée, mais déjà dotée d'un gain : son témoin ne bouge pas.
        repo.set(45, "rg_track_gain", "-1.00 dB").unwrap();
        repo.set(45, "rg_analyzed", "1700000000").unwrap();
        repo.set(45, OVERSIZED_KEY, "1").unwrap();

        let de = |id: i64| repo.get_all(id).unwrap();
        // CONTRE-ÉPREUVE de l'utilité du rattrapage : sans lui, la piste 42
        // reste hors du balayage pour toujours (les autres, introuvables,
        // sont seulement reportées).
        analyze_track_batch(&backend).await;
        assert!(!de(42).contains_key("rg_track_gain"), "{:?}", de(42));

        // Quatre lignes : le témoin de 42, la marque DR de 44, les deux
        // marqueurs (42 et 45).
        assert_eq!(
            reprendre_les_pistes_ecartees_pour_leur_taille(&backend),
            Ok(4)
        );
        assert!(!de(42).contains_key("rg_analyzed"), "{:?}", de(42));
        assert!(!de(42).contains_key(OVERSIZED_KEY), "{:?}", de(42));
        assert!(de(43).contains_key(DR_INDISPONIBLE_KEY), "{:?}", de(43));
        assert!(!de(44).contains_key(DR_INDISPONIBLE_KEY), "{:?}", de(44));
        assert_eq!(
            de(44).get("rg_track_gain").map(String::as_str),
            Some("-3.00 dB")
        );
        assert!(de(45).contains_key("rg_analyzed"), "{:?}", de(45));
        assert!(!de(45).contains_key(OVERSIZED_KEY), "{:?}", de(45));
        // Idempotent.
        assert_eq!(
            reprendre_les_pistes_ecartees_pour_leur_taille(&backend),
            Ok(0)
        );

        // La passe reprend la piste 42 et la mesure.
        analyze_track_batch(&backend).await;
        assert!(de(42).contains_key("rg_track_gain"), "{:?}", de(42));
        assert_eq!(de(42).get("dr_track").map(String::as_str), Some("10"));
    }

    #[test]
    fn gain_is_reference_minus_lufs() {
        // A track at -12 LUFS (louder than the -18 reference) attenuates by 6 dB.
        assert!((track_gain_db(-12.0) - (-6.0)).abs() < 1e-9);
        // A track at -23 LUFS (quieter) is boosted by +5 dB.
        assert!((track_gain_db(-23.0) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn format_roundtrip() {
        assert_eq!(format_gain(-6.5), "-6.50 dB");
        assert_eq!(format_peak(0.9885534), "0.988553");
        assert_eq!(parse_gain_db("-6.50 dB".into()), Some(-6.5));
        assert_eq!(parse_gain_db("3.20".into()), Some(3.2));
    }

    #[test]
    fn factor_is_one_when_off() {
        let g = TrackGain {
            gain_db: -6.0,
            peak: Some(0.9),
        };
        let s = ReplayGainSettings::default();
        assert_eq!(s.mode, ReplayGainMode::Off);
        assert_eq!(gain_factor(g, s), 1.0);
    }

    #[test]
    fn minus_six_db_halves_amplitude() {
        let s = ReplayGainSettings {
            mode: ReplayGainMode::Track,
            ..Default::default()
        };
        let f = gain_factor(
            TrackGain {
                gain_db: -6.0206,
                peak: None,
            },
            s,
        );
        assert!((f - 0.5).abs() < 1e-4, "{f}");
    }

    #[test]
    fn preamp_adds_to_the_tag() {
        let s = ReplayGainSettings {
            mode: ReplayGainMode::Track,
            preamp_db: 6.0206,
            prevent_clipping: false,
            ..Default::default()
        };
        // -6 dB tag + 6 dB pre-amp ⇒ unity.
        let f = gain_factor(
            TrackGain {
                gain_db: -6.0206,
                peak: None,
            },
            s,
        );
        assert!((f - 1.0).abs() < 1e-4, "{f}");
    }

    #[test]
    fn clipping_prevention_caps_at_the_peak() {
        let s = ReplayGainSettings {
            mode: ReplayGainMode::Track,
            preamp_db: 0.0,
            prevent_clipping: true,
            ..Default::default()
        };
        // +6 dB on a track already peaking at 0.95 would reach 1.9 — clipped.
        let f = gain_factor(
            TrackGain {
                gain_db: 6.0,
                peak: Some(0.95),
            },
            s,
        );
        assert!((f - 1.0 / 0.95).abs() < 1e-9, "{f}");
        assert!(f * 0.95 <= 1.0 + 1e-9);
        // Turned off, the same track is allowed to overshoot.
        let loose = ReplayGainSettings {
            prevent_clipping: false,
            ..s
        };
        assert!(
            gain_factor(
                TrackGain {
                    gain_db: 6.0,
                    peak: Some(0.95)
                },
                loose
            ) > 1.9
        );
    }

    // ------------------------------------------------------------------
    // #4072 — sans pic tagué, `prevent_clipping` refuse le gain positif.
    // ------------------------------------------------------------------

    /// Le défaut mesuré par T9 : +6 dB, aucun pic tagué, garde-fou ARMÉ.
    /// Avant, le facteur sortait à ×1,9953 et 66,2 % des échantillons d'un
    /// sinus à −0,1 dBFS étaient écrêtés dur. Il ne dépasse plus l'unité.
    #[test]
    fn sans_pic_tague_le_gain_positif_est_refuse_pas_rabote_sur_le_plafond() {
        let s = ReplayGainSettings {
            mode: ReplayGainMode::Track,
            preamp_db: 0.0,
            prevent_clipping: true,
            true_peak_ceiling_db: 0.0,
        };
        let g = TrackGain {
            gain_db: 6.0,
            peak: None,
        };
        let (f, retenue) = gain_factor_detail(g, s);
        assert_eq!(
            f, 1.0,
            "aucun pic tagué ⇒ le facteur ne dépasse pas l'unité"
        );
        assert_eq!(retenue, RetenueAntiEcretage::GainPositifRefuseSansPic);
        assert_eq!(retenue.as_str(), "refused_no_peak");
        // Le clamp ×4 n'est plus la seule borne : même +30 dB reste à l'unité.
        assert_eq!(
            gain_factor(
                TrackGain {
                    gain_db: 30.0,
                    peak: None
                },
                s
            ),
            1.0
        );
    }

    /// Un plafond dBTP n'entre PAS dans la borne « sans pic » : il
    /// atténuerait toute piste non taguée, gain nul compris. La borne est
    /// l'unité, et rien d'autre.
    #[test]
    fn sans_pic_tague_le_plafond_dbtp_n_attenue_pas_le_signal() {
        let s = ReplayGainSettings {
            mode: ReplayGainMode::Track,
            preamp_db: 0.0,
            prevent_clipping: true,
            true_peak_ceiling_db: -1.0,
        };
        let (f, retenue) = gain_factor_detail(
            TrackGain {
                gain_db: 0.0,
                peak: None,
            },
            s,
        );
        assert_eq!(f, 1.0, "un gain nul sans pic reste l'identité");
        assert_eq!(retenue, RetenueAntiEcretage::Aucune);
    }

    /// L'ATTÉNUATION traverse le garde-fou sans être touchée : un gain
    /// négatif ne peut pas écrêter, et c'est la majorité des tags réels.
    #[test]
    fn sans_pic_tague_l_attenuation_passe_intacte() {
        let s = ReplayGainSettings {
            mode: ReplayGainMode::Track,
            preamp_db: 0.0,
            prevent_clipping: true,
            true_peak_ceiling_db: 0.0,
        };
        let (f, retenue) = gain_factor_detail(
            TrackGain {
                gain_db: -6.0206,
                peak: None,
            },
            s,
        );
        assert!((f - 0.5).abs() < 1e-4, "{f}");
        assert_eq!(retenue, RetenueAntiEcretage::Aucune);
    }

    /// Un pic ABSURDE (nul ou négatif) ne doit pas rouvrir la porte : il n'est
    /// pas un pic, il retombe donc sur le refus, pas sur une division.
    #[test]
    fn un_pic_nul_ou_negatif_retombe_sur_le_refus_pas_sur_une_division() {
        let s = ReplayGainSettings {
            mode: ReplayGainMode::Track,
            preamp_db: 0.0,
            prevent_clipping: true,
            true_peak_ceiling_db: 0.0,
        };
        for pic in [0.0, -0.5] {
            let (f, retenue) = gain_factor_detail(
                TrackGain {
                    gain_db: 6.0,
                    peak: Some(pic),
                },
                s,
            );
            assert_eq!(f, 1.0, "pic {pic}");
            assert_eq!(retenue, RetenueAntiEcretage::GainPositifRefuseSansPic);
        }
    }

    /// Garde-fou DÉSARMÉ : le refus disparaît avec lui. L'auditeur qui a
    /// décoché la case garde exactement le comportement d'avant, écrêtage
    /// compris — c'est ce qu'il a demandé.
    #[test]
    fn garde_fou_desarme_le_gain_positif_sans_pic_passe_comme_avant() {
        let s = ReplayGainSettings {
            mode: ReplayGainMode::Track,
            preamp_db: 0.0,
            prevent_clipping: false,
            true_peak_ceiling_db: 0.0,
        };
        let (f, retenue) = gain_factor_detail(
            TrackGain {
                gain_db: 6.0,
                peak: None,
            },
            s,
        );
        assert!((f - 1.9953).abs() < 1e-3, "{f}");
        assert_eq!(retenue, RetenueAntiEcretage::Aucune);
    }

    /// Un pic tagué garde sa retenue nommée — le cas nominal n'a pas bougé.
    #[test]
    fn un_pic_tague_nomme_sa_retenue() {
        let s = ReplayGainSettings {
            mode: ReplayGainMode::Track,
            preamp_db: 0.0,
            prevent_clipping: true,
            true_peak_ceiling_db: 0.0,
        };
        let (f, retenue) = gain_factor_detail(
            TrackGain {
                gain_db: 6.0,
                peak: Some(0.95),
            },
            s,
        );
        assert!((f - 1.0 / 0.95).abs() < 1e-9, "{f}");
        assert_eq!(retenue, RetenueAntiEcretage::ParLePicTague);
        assert_eq!(retenue.as_str(), "tagged_peak");
        assert_eq!(RetenueAntiEcretage::Aucune.as_str(), "none");
    }

    // ------------------------------------------------------------------
    // #1694 — plafond dBTP optionnel + préférence au true peak stocké.
    // ------------------------------------------------------------------

    /// Plafond à 0 dBTP (défaut) : STRICTEMENT le comportement historique.
    /// Un réglage absent ne doit changer aucun facteur déjà en production.
    #[test]
    fn ceiling_zero_is_the_historical_behavior() {
        let s = ReplayGainSettings {
            mode: ReplayGainMode::Track,
            preamp_db: 0.0,
            prevent_clipping: true,
            true_peak_ceiling_db: 0.0,
        };
        let g = TrackGain {
            gain_db: 6.0,
            peak: Some(0.95),
        };
        let f = gain_factor(g, s);
        assert!((f - 1.0 / 0.95).abs() < 1e-9, "{f}");
    }

    /// Plafond −1 dBTP : le facteur est tiré pour que `peak × factor` ne
    /// dépasse pas 10^(−1/20) — la marge inter-échantillons demandée.
    #[test]
    fn dbtp_ceiling_pulls_the_factor_under_the_ceiling() {
        let s = ReplayGainSettings {
            mode: ReplayGainMode::Track,
            preamp_db: 0.0,
            prevent_clipping: true,
            true_peak_ceiling_db: -1.0,
        };
        let ceiling = 10f64.powf(-1.0 / 20.0);
        // Un master loudness-war : true peak au-delà de la pleine échelle.
        let g = TrackGain {
            gain_db: 2.0,
            peak: Some(1.20),
        };
        let f = gain_factor(g, s);
        assert!((f * 1.20 - ceiling).abs() < 1e-9, "{f}");

        // Sans `prevent_clipping`, le plafond n'agit pas : c'est un mode de
        // l'anti-écrêtage, pas un limiteur indépendant.
        let loose = ReplayGainSettings {
            prevent_clipping: false,
            ..s
        };
        assert!(gain_factor(g, loose) * 1.20 > 1.0);
    }

    /// Le plafond ne touche jamais une piste qui reste dessous : ce n'est pas
    /// une normalisation vers −1 dBTP, c'est un plafond.
    #[test]
    fn dbtp_ceiling_leaves_quiet_results_alone() {
        let s = ReplayGainSettings {
            mode: ReplayGainMode::Track,
            preamp_db: 0.0,
            prevent_clipping: true,
            true_peak_ceiling_db: -1.0,
        };
        let g = TrackGain {
            gain_db: -6.0,
            peak: Some(0.9),
        };
        let f = gain_factor(g, s);
        // -6 dB sur un pic 0.9 : résultat ~0.45, loin sous 10^(-1/20).
        assert!((f - 10f64.powf(-6.0 / 20.0)).abs() < 1e-9, "{f}");
    }

    /// Le réglage relu de la base est borné à [−1, 0] : une valeur cassée ne
    /// doit ni creuser le niveau ni écrêter.
    #[test]
    fn ceiling_setting_is_parsed_and_clamped() {
        let (_db, backend) = sweep_db(1);
        let settings = SettingsRepo::with_backend(backend.clone());
        assert_eq!(ReplayGainSettings::load(&backend).true_peak_ceiling_db, 0.0);

        settings.set(TRUE_PEAK_CEILING_KEY, "-0.5").unwrap();
        assert_eq!(
            ReplayGainSettings::load(&backend).true_peak_ceiling_db,
            -0.5
        );

        settings.set(TRUE_PEAK_CEILING_KEY, "-12").unwrap();
        assert_eq!(
            ReplayGainSettings::load(&backend).true_peak_ceiling_db,
            -1.0,
            "borne basse : jamais plus d'un dB de marge"
        );

        settings.set(TRUE_PEAK_CEILING_KEY, "3").unwrap();
        assert_eq!(
            ReplayGainSettings::load(&backend).true_peak_ceiling_db,
            0.0,
            "borne haute : un plafond positif n'existe pas"
        );

        settings.set(TRUE_PEAK_CEILING_KEY, "junk").unwrap();
        assert_eq!(
            ReplayGainSettings::load(&backend).true_peak_ceiling_db,
            0.0,
            "illisible ⇒ défaut, jamais une surprise sonore"
        );
    }

    /// `prevent_clipping` PRÉFÈRE le true peak stocké : c'est lui qui voit
    /// les overs inter-échantillons. Le sample peak reste le repli des
    /// bibliothèques analysées avant #1694.
    #[test]
    fn stored_gain_prefers_the_true_peak_when_present() {
        let (_db, backend) = sweep_db(1);
        let meta = TrackMetadataRepo::with_backend(backend.clone());
        meta.set(1, "rg_track_gain", "-3.00 dB").unwrap();
        meta.set(1, "rg_track_peak", "0.950000").unwrap();

        // Sans true peak : repli sur le sample peak (compat, #1382).
        let g = stored_gain_for(&backend, 1, ReplayGainMode::Track).unwrap();
        assert_eq!(g.peak, Some(0.95));

        // Avec true peak : c'est LUI qui arme l'anti-écrêtage.
        meta.set(1, "rg_track_true_peak", "1.230000").unwrap();
        let g = stored_gain_for(&backend, 1, ReplayGainMode::Track).unwrap();
        assert_eq!(g.peak, Some(1.23));

        // Mode album : même préférence sur les clés d'album.
        meta.set(1, "rg_album_gain", "-2.00 dB").unwrap();
        meta.set(1, "rg_album_peak", "0.900000").unwrap();
        meta.set(1, "rg_album_true_peak", "1.100000").unwrap();
        let g = stored_gain_for(&backend, 1, ReplayGainMode::Album).unwrap();
        assert_eq!(g.peak, Some(1.10));
    }

    // ---- #1627 : les trois modes, et d'où vient le gain ---------------------

    /// Les TROIS modes de la demande, lus sur les deux réglages existants.
    ///
    /// Aucune valeur nouvelle n'est persistée : le test écrit `replaygain_mode`
    /// et `replaygain_analysis_enabled`, rien d'autre. Si un jour quelqu'un
    /// introduit un troisième réglage, ce test le dira.
    #[test]
    fn les_trois_modes_se_lisent_sur_les_deux_reglages_existants() {
        let (_db, backend) = sweep_db(1);
        let settings = SettingsRepo::with_backend(backend.clone());

        // 1- néant : c'est le défaut, sans qu'aucune clé n'ait été écrite.
        assert_eq!(active_source_mode(&backend), ReplayGainSourceMode::Off);

        // 3- calcul : mode armé, analyse au défaut (activée).
        settings.set(MODE_KEY, "track").unwrap();
        assert_eq!(
            active_source_mode(&backend),
            ReplayGainSourceMode::TagsThenAnalysis
        );

        // 2- fichier : mode armé, analyse explicitement coupée.
        settings.set(ANALYSIS_ENABLED_KEY, "false").unwrap();
        assert_eq!(
            active_source_mode(&backend),
            ReplayGainSourceMode::FileTagsOnly
        );

        // La granularité album ne change pas de MODE : c'est l'autre axe.
        settings.set(MODE_KEY, "album").unwrap();
        assert_eq!(
            active_source_mode(&backend),
            ReplayGainSourceMode::FileTagsOnly
        );
        settings.set(ANALYSIS_ENABLED_KEY, "true").unwrap();
        assert_eq!(
            active_source_mode(&backend),
            ReplayGainSourceMode::TagsThenAnalysis
        );

        // Retour à « Désactivé » : la coche seule ne rouvre rien (#2496).
        settings.set(MODE_KEY, "off").unwrap();
        assert_eq!(
            active_source_mode(&backend),
            ReplayGainSourceMode::Off,
            "mode off ⇒ néant, quelle que soit la coche d'analyse"
        );

        // Valeurs stables de l'API : elles voyagent dans GET /config.
        assert_eq!(ReplayGainSourceMode::Off.as_str(), "off");
        assert_eq!(ReplayGainSourceMode::FileTagsOnly.as_str(), "file_tags");
        assert_eq!(
            ReplayGainSourceMode::TagsThenAnalysis.as_str(),
            "tags_then_analysis"
        );
    }

    /// La réciproque : ÉCRIRE l'un des trois modes, et le relire tel quel.
    ///
    /// C'est le tour complet — `source_mode_settings` pose les deux réglages,
    /// `active_source_mode` les relit — donc aucune des deux moitiés ne peut
    /// dériver sans que ce test le dise.
    #[test]
    fn ecrire_un_des_trois_modes_le_rend_actif_et_preserve_la_granularite() {
        let (_db, backend) = sweep_db(1);
        let settings = SettingsRepo::with_backend(backend.clone());
        let poser = |mode: ReplayGainSourceMode| {
            let granularite = ReplayGainSettings::load(&backend).mode;
            for (cle, valeur) in source_mode_settings(mode, granularite) {
                settings.set(cle, valeur).unwrap();
            }
        };

        // 2- fichier depuis « néant » : la granularité par défaut est `track`.
        // Le piège évité ici : repartir de la granularité persistée (`off`)
        // écrirait `replaygain_mode = off`, donc RIEN, en répondant « ok ».
        poser(ReplayGainSourceMode::FileTagsOnly);
        assert_eq!(settings.get(MODE_KEY).unwrap().as_deref(), Some("track"));
        assert_eq!(
            active_source_mode(&backend),
            ReplayGainSourceMode::FileTagsOnly
        );

        // La granularité album est CONSERVÉE quand on change de source.
        settings.set(MODE_KEY, "album").unwrap();
        poser(ReplayGainSourceMode::TagsThenAnalysis);
        assert_eq!(
            settings.get(MODE_KEY).unwrap().as_deref(),
            Some("album"),
            "changer de source ne doit jamais reculer l'album vers la piste"
        );
        assert_eq!(
            active_source_mode(&backend),
            ReplayGainSourceMode::TagsThenAnalysis
        );

        // 1- néant coupe le gain SANS écraser la coche d'analyse : c'est le
        // choix de l'utilisateur pour le jour où il rallume.
        poser(ReplayGainSourceMode::Off);
        assert_eq!(settings.get(MODE_KEY).unwrap().as_deref(), Some("off"));
        assert_eq!(
            settings.get(ANALYSIS_ENABLED_KEY).unwrap().as_deref(),
            Some("true"),
            "« néant » ne doit toucher qu'un seul des deux axes"
        );
        assert_eq!(active_source_mode(&backend), ReplayGainSourceMode::Off);

        // Aucune clé nouvelle en base : les deux axes restent la seule vérité.
        for mode in [
            ReplayGainSourceMode::Off,
            ReplayGainSourceMode::FileTagsOnly,
            ReplayGainSourceMode::TagsThenAnalysis,
        ] {
            for (cle, _) in source_mode_settings(mode, ReplayGainMode::Track) {
                assert!(
                    cle == MODE_KEY || cle == ANALYSIS_ENABLED_KEY,
                    "clé persistée inattendue : {cle}"
                );
            }
        }
    }

    /// Un mode inconnu n'est pas un mode : il ne doit rien rendre du tout.
    ///
    /// Le repli `_ => Off` serait ici la faute qui coûte cher — une faute de
    /// frappe couperait le ReplayGain en silence, ou pire, l'allumerait.
    #[test]
    fn un_mode_inconnu_ne_se_replie_sur_rien() {
        assert_eq!(
            ReplayGainSourceMode::from_setting("off"),
            Some(ReplayGainSourceMode::Off)
        );
        assert_eq!(
            ReplayGainSourceMode::from_setting("  FILE_TAGS  "),
            Some(ReplayGainSourceMode::FileTagsOnly)
        );
        assert_eq!(
            ReplayGainSourceMode::from_setting("tags_then_analysis"),
            Some(ReplayGainSourceMode::TagsThenAnalysis)
        );
        for inconnu in ["", "calcul", "track", "album", "tags", "analysis", "true"] {
            assert_eq!(
                ReplayGainSourceMode::from_setting(inconnu),
                None,
                "« {inconnu} » doit être refusé, jamais interprété"
            );
        }
    }

    /// La provenance du gain, sur les trois configurations qui existent en base.
    #[test]
    fn la_provenance_distingue_un_tag_de_fichier_d_une_mesure_tune() {
        let (_db, backend) = sweep_db(3);
        let meta = TrackMetadataRepo::with_backend(backend.clone());

        // Piste 1 — tags du fichier seuls (le cas rsgain de #1382).
        meta.set(1, "rg_track_gain", "-4.20 dB").unwrap();
        assert_eq!(
            stored_gain_source(&backend, 1, ReplayGainMode::Track),
            Some(GainSource::FileTags)
        );

        // Piste 2 — mesurée ici, témoin explicite.
        meta.set(2, "rg_track_gain", "-6.50 dB").unwrap();
        meta.set(2, TRACK_SOURCE_KEY, SOURCE_ANALYSIS).unwrap();
        assert_eq!(
            stored_gain_source(&backend, 2, ReplayGainMode::Track),
            Some(GainSource::Analysis)
        );

        // Piste 3 — bibliothèque analysée AVANT que le témoin existe : le
        // repli sur `rg_analyzed` tranche, parce que le balayage n'analyse que
        // les pistes dépourvues de `rg_track_gain`.
        meta.set(3, "rg_track_gain", "-2.00 dB").unwrap();
        meta.set(3, ANALYZED_KEY, "1700000000").unwrap();
        assert_eq!(
            stored_gain_source(&backend, 3, ReplayGainMode::Track),
            Some(GainSource::Analysis)
        );

        // Aucun gain à cette granularité ⇒ rien à dire, surtout pas une
        // origine inventée. La piste 1 n'a pas de gain d'album.
        assert_eq!(stored_gain_source(&backend, 1, ReplayGainMode::Album), None);

        // Un gain d'album lu dans le fichier reste « tags du fichier », même
        // sur une piste dont le gain de PISTE, lui, a été mesuré ici : les deux
        // granularités ont leur propre témoin et ne décident pas l'une pour
        // l'autre.
        meta.set(2, "rg_album_gain", "-3.00 dB").unwrap();
        assert_eq!(
            stored_gain_source(&backend, 2, ReplayGainMode::Album),
            Some(GainSource::FileTags),
            "le témoin de piste ne doit pas décider de l'origine du gain d'album"
        );
        meta.set(2, ALBUM_SOURCE_KEY, SOURCE_ANALYSIS).unwrap();
        assert_eq!(
            stored_gain_source(&backend, 2, ReplayGainMode::Album),
            Some(GainSource::Analysis)
        );

        // Valeurs stables de l'API.
        assert_eq!(GainSource::FileTags.as_str(), "file_tags");
        assert_eq!(GainSource::Analysis.as_str(), "analysis");
    }

    #[test]
    fn clipping_prevention_never_boosts_a_quiet_track() {
        // A track peaking at 0.2 with a -3 dB tag must still be attenuated:
        // the peak cap is a ceiling, never a floor.
        let s = ReplayGainSettings {
            mode: ReplayGainMode::Track,
            preamp_db: 0.0,
            prevent_clipping: true,
            ..Default::default()
        };
        let f = gain_factor(
            TrackGain {
                gain_db: -3.0,
                peak: Some(0.2),
            },
            s,
        );
        assert!(f < 1.0, "{f}");
    }

    #[test]
    fn apply_gain_halves_16bit_samples() {
        let mut pcm = Vec::new();
        for v in [1000i16, -1000, 32767, -32768] {
            pcm.extend_from_slice(&v.to_le_bytes());
        }
        apply_gain_pcm(&mut pcm, 16, 0.5);
        let got: Vec<i16> = pcm
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(got, vec![500, -500, 16383, -16384]);
    }

    #[test]
    fn apply_gain_saturates_instead_of_wrapping() {
        // Without the clamp this wraps to a large negative value — an audible
        // click on every peak, which is worse than the clipping it replaces.
        let mut pcm = 30000i16.to_le_bytes().to_vec();
        apply_gain_pcm(&mut pcm, 16, 2.0);
        assert_eq!(i16::from_le_bytes([pcm[0], pcm[1]]), i16::MAX);
    }

    #[test]
    fn apply_gain_handles_24bit_sign_extension() {
        // -1000 as a 24-bit little-endian sample.
        let v: i32 = -1000;
        let mut pcm = vec![
            (v & 0xFF) as u8,
            ((v >> 8) & 0xFF) as u8,
            ((v >> 16) & 0xFF) as u8,
        ];
        apply_gain_pcm(&mut pcm, 24, 0.5);
        let raw = ((pcm[2] as i32) << 24 | (pcm[1] as i32) << 16 | (pcm[0] as i32) << 8) >> 8;
        assert_eq!(raw, -500);
    }

    #[test]
    fn apply_gain_of_one_is_a_no_op() {
        let original = vec![1u8, 2, 3, 4, 5, 6];
        let mut pcm = original.clone();
        apply_gain_pcm(&mut pcm, 16, 1.0);
        assert_eq!(pcm, original);
    }

    #[test]
    fn mode_parsing_defaults_to_off() {
        assert_eq!(ReplayGainMode::from_setting("track"), ReplayGainMode::Track);
        assert_eq!(ReplayGainMode::from_setting("ALBUM"), ReplayGainMode::Album);
        assert_eq!(ReplayGainMode::from_setting(""), ReplayGainMode::Off);
        assert_eq!(
            ReplayGainMode::from_setting("yes please"),
            ReplayGainMode::Off
        );
    }

    #[test]
    fn album_energy_mean_between_track_extremes() {
        // Duration-weighted energy mean of -12 and -18 LUFS must land between them.
        let e = (10f64.powf(-12.0 / 10.0) + 10f64.powf(-18.0 / 10.0)) / 2.0;
        let album_lufs = 10.0 * e.log10();
        assert!(album_lufs < -12.0 && album_lufs > -18.0);
    }

    // -----------------------------------------------------------------------
    // Le gain d'album, figé sur un sous-ensemble et écrasant les tags
    // -----------------------------------------------------------------------

    /// Une base minimale avec de VRAIS albums : des pistes, leurs gains de
    /// piste, et rien d'autre. Aucune de ces valeurs n'est produite par la
    /// passe elle-même — c'est l'état de départ d'une bibliothèque à moitié
    /// analysée, la forme exacte du .18.
    fn base_albums() -> (crate::db::sqlite::SqliteDb, Arc<dyn DbBackend>) {
        use crate::db::sqlite::SqliteDb;
        let db = SqliteDb::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE tracks (id INTEGER PRIMARY KEY, album_id INTEGER, file_path TEXT,
                                  duration_ms INTEGER, sample_rate INTEGER, channels INTEGER);
             CREATE TABLE track_metadata (track_id INTEGER NOT NULL, key TEXT NOT NULL,
                                          value TEXT NOT NULL, PRIMARY KEY (track_id, key));",
        )
        .unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db.clone());
        (db, backend)
    }

    fn piste(db: &crate::db::sqlite::SqliteDb, id: i64, album: i64) {
        let chemin = format!("/album{album}/{id}.flac");
        db.execute(
            "INSERT INTO tracks (id, album_id, file_path, duration_ms, sample_rate, channels) \
             VALUES (?, ?, ?, 300000, 44100, 2)",
            &[&id, &album, &chemin],
        )
        .unwrap();
    }

    fn gain_album(meta: &TrackMetadataRepo, id: i64) -> Option<String> {
        meta.get_all(id).unwrap().get("rg_album_gain").cloned()
    }

    /// 🔴 LE DÉFAUT №1 — la sélection prenait un album dès qu'UNE piste lui
    /// manquait un gain d'album, puis moyennait les seules pistes qui avaient
    /// un `rg_track_gain` à cet instant. Le résultat, écrit sur toutes les
    /// pistes, sortait l'album de la sélection : il ne se recalculait plus
    /// jamais. Mesuré sur la base du .18 le 12/09/2026 : **179 albums,
    /// 2 385 pistes**, dont un album de 64 pistes dont le gain vient de 2.
    ///
    /// La garde est dans la REQUÊTE, et le témoin le vérifie deux fois : un
    /// album incomplet ne doit pas recevoir de gain, et il ne doit pas non
    /// plus affamer les albums complets en se faisant rechoisir sans fin.
    #[test]
    fn le_gain_dalbum_attend_que_toutes_les_pistes_soient_analysees() {
        let (db, backend) = base_albums();
        let meta = TrackMetadataRepo::with_backend(backend.clone());

        // Album 1 : trois pistes, UNE SEULE analysée.
        for id in 1..=3 {
            piste(&db, id, 1);
        }
        meta.set(1, "rg_track_gain", "-6.00 dB").unwrap();
        meta.set(1, "rg_track_peak", "0.98").unwrap();

        // Album 2 : deux pistes, toutes deux analysées.
        for id in 11..=12 {
            piste(&db, id, 2);
        }
        meta.set(11, "rg_track_gain", "-9.00 dB").unwrap();
        meta.set(11, "rg_track_peak", "0.90").unwrap();
        meta.set(12, "rg_track_gain", "-5.00 dB").unwrap();
        meta.set(12, "rg_track_peak", "0.99").unwrap();

        // Plusieurs tours : l'ordre que rend la requête ne doit rien changer.
        let mut traites = 0;
        for _ in 0..4 {
            traites += analyze_album_batch(&backend);
        }

        for id in 1..=3 {
            assert_eq!(
                gain_album(&meta, id),
                None,
                "piste {id} : l'album 1 est incomplet (1 piste analysée sur 3), \
                 il ne doit porter AUCUN gain d'album"
            );
        }
        assert_eq!(
            traites, 1,
            "l'album 2 est complet : l'album incomplet ne doit pas l'affamer"
        );
        for id in 11..=12 {
            assert!(
                gain_album(&meta, id).is_some(),
                "piste {id} : album complet, gain d'album attendu"
            );
        }
    }

    /// 🔴 LE DÉFAUT №2 — `rg_album_gain` et `rg_album_peak` étaient écrits
    /// INCONDITIONNELLEMENT sur toutes les pistes de l'album. Or le scan
    /// verse sous ces MÊMES clés ce qu'il lit dans les tags du fichier : la
    /// valeur de l'utilisateur disparaissait sans un mot, et recevait même
    /// l'estampille `rg_album_source = analysis` qui la faisait passer pour
    /// une mesure de Tune au tour suivant.
    #[test]
    fn le_gain_dalbum_venu_des_tags_nest_pas_ecrase() {
        let (db, backend) = base_albums();
        let meta = TrackMetadataRepo::with_backend(backend.clone());
        for id in 1..=3 {
            piste(&db, id, 1);
        }
        for (id, g, p) in [
            (1i64, "-6.00 dB", "0.98"),
            (2, "-8.00 dB", "0.91"),
            (3, "-4.00 dB", "0.99"),
        ] {
            meta.set(id, "rg_track_gain", g).unwrap();
            meta.set(id, "rg_track_peak", p).unwrap();
        }
        // La piste 1 porte un gain d'album VENU DU FICHIER. Ce qui le dit :
        // l'ABSENCE de témoin `rg_album_source` à côté.
        meta.set(1, "rg_album_gain", "-9.99 dB").unwrap();
        meta.set(1, "rg_album_peak", "0.55").unwrap();

        assert_eq!(analyze_album_batch(&backend), 1, "l'album est complet");

        let m1 = meta.get_all(1).unwrap();
        assert_eq!(
            m1.get("rg_album_gain").map(String::as_str),
            Some("-9.99 dB"),
            "le gain d'album lu dans les tags du fichier ne doit PAS être écrasé"
        );
        assert_eq!(
            m1.get("rg_album_peak").map(String::as_str),
            Some("0.55"),
            "son pic d'album non plus"
        );
        assert!(
            !m1.contains_key(ALBUM_SOURCE_KEY),
            "et il ne doit surtout pas être estampillé comme une mesure de Tune"
        );

        // Les pistes vierges, elles, reçoivent le calcul : sans quoi l'album
        // serait rechoisi à chaque tour, indéfiniment.
        for id in 2..=3 {
            let m = meta.get_all(id).unwrap();
            assert!(
                m.get("rg_album_gain").is_some_and(|v| v != "-9.99 dB"),
                "piste {id} : gain d'album calculé attendu, vu {:?}",
                m.get("rg_album_gain")
            );
            assert_eq!(
                m.get(ALBUM_SOURCE_KEY).map(String::as_str),
                Some(SOURCE_ANALYSIS),
                "piste {id} : ce que Tune écrit, Tune l'estampille"
            );
        }
    }

    /// La garde de provenance ne doit pas geler NOS propres valeurs : une
    /// piste estampillée `rg_album_source = analysis` reste reprenable, sans
    /// quoi aucun gain d'album ne pourrait plus jamais être corrigé.
    #[test]
    fn notre_propre_gain_dalbum_reste_recalculable() {
        let (db, backend) = base_albums();
        let meta = TrackMetadataRepo::with_backend(backend.clone());
        for id in 1..=3 {
            piste(&db, id, 1);
        }
        for (id, g, p) in [
            (1i64, "-6.00 dB", "0.98"),
            (2, "-8.00 dB", "0.91"),
            (3, "-4.00 dB", "0.99"),
        ] {
            meta.set(id, "rg_track_gain", g).unwrap();
            meta.set(id, "rg_track_peak", p).unwrap();
        }
        meta.set(1, "rg_album_gain", "-9.99 dB").unwrap();
        meta.set(1, ALBUM_SOURCE_KEY, SOURCE_ANALYSIS).unwrap();

        assert_eq!(analyze_album_batch(&backend), 1);
        assert!(
            gain_album(&meta, 1).is_some_and(|v| v != "-9.99 dB"),
            "une valeur que Tune a écrite lui-même doit rester reprenable"
        );
    }

    /// Le cœur de la garde de provenance, isolé.
    #[test]
    fn peut_ecrire_le_gain_album_ne_cede_quau_temoin() {
        assert!(peut_ecrire_le_gain_album(None, None), "rien en place");
        assert!(peut_ecrire_le_gain_album(Some("  "), None), "valeur vide");
        assert!(
            !peut_ecrire_le_gain_album(Some("-9.99 dB"), None),
            "sans témoin, la valeur vient des tags du fichier"
        );
        assert!(
            !peut_ecrire_le_gain_album(Some("-9.99 dB"), Some("tag")),
            "un témoin qui ne dit pas « analysis » ne nous autorise rien"
        );
        assert!(
            peut_ecrire_le_gain_album(Some("-9.99 dB"), Some(SOURCE_ANALYSIS)),
            "notre propre mesure se reprend"
        );
    }
    // -----------------------------------------------------------------------
    // #2496 — « Désactivé » doit désactiver
    // -----------------------------------------------------------------------

    /// Les tables que la passe touche, plus `n` pistes locales candidates.
    ///
    /// Les chemins n'existent pas : `measure_loudness_and_peak` rend `None`
    /// aussitôt, mais la passe estampille quand même `rg_analyzed` et compte le
    /// fichier — c'est ce compteur qui dit combien de fichiers ont VRAIMENT été
    /// pris en charge, sans avoir à embarquer de l'audio dans le dépôt.
    /// #5519 — un tour de la boucle fait PLUSIEURS albums, au plus
    /// [`ALBUMS_PAR_TOUR`]. À un album par tour, la passe d'albums prenait du
    /// retard sur la passe de pistes (deux albums complets par lot de 25) et
    /// le rattrapait seule, à près de 4 s l'album sur une grande base.
    ///
    /// La BOUCLE est éprouvée sans `passe_d_album` : celle-ci lit des états
    /// globaux du processus (pause de la tâche, zones qui jouent) que d'autres
    /// tests de la suite posent, et rendait 0 dans la suite complète.
    #[tokio::test]
    async fn un_tour_fait_plusieurs_albums_jusqu_a_sa_borne() {
        assert_eq!(ALBUMS_PAR_TOUR, 4);
        // Six albums en attente : un tour en fait quatre, le suivant deux.
        let mut restants = 6usize;
        let mut un = || {
            let n = usize::from(restants > 0);
            restants -= n;
            std::future::ready(n)
        };
        assert_eq!(
            albums_jusqu_a_la_borne(&mut un).await,
            4,
            "premier tour : quatre albums, pas un seul"
        );
        assert_eq!(albums_jusqu_a_la_borne(&mut un).await, 2);
        assert_eq!(albums_jusqu_a_la_borne(&mut un).await, 0);
        // Et elle s'arrête au premier « rien » (pause, lecture, plus d'album).
        let mut appels = 0usize;
        let rien = || {
            appels += 1;
            std::future::ready(0usize)
        };
        assert_eq!(albums_jusqu_a_la_borne(rien).await, 0);
        assert_eq!(appels, 1, "un seul essai quand il n'y a rien");
    }

    /// #5519 — la pause entre deux tours suit la vitesse réglée : « Discret »
    /// garde les 2 s d'avant, « Normal » et « Rapide » un court répit.
    #[test]
    fn la_pause_entre_deux_tours_suit_la_vitesse() {
        use crate::taches_de_fond::vitesse::Vitesse;
        use std::time::Duration;
        assert_eq!(
            pause_pour_la_vitesse(Vitesse::Discrete),
            Duration::from_secs(2)
        );
        assert_eq!(
            pause_pour_la_vitesse(Vitesse::Normale),
            Duration::from_millis(250)
        );
        assert_eq!(
            pause_pour_la_vitesse(Vitesse::Rapide),
            Duration::from_millis(250)
        );
        // Le réglage absent vaut « Normal ».
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL,
                                    updated_at TEXT NOT NULL DEFAULT '');",
        )
        .unwrap();
        let b: Arc<dyn DbBackend> = Arc::new(db);
        assert_eq!(pause_entre_deux_tours(&b), Duration::from_millis(250));
        SettingsRepo::with_backend(b.clone())
            .set(crate::taches_de_fond::vitesse::CLE_REGLAGE, "discreet")
            .unwrap();
        assert_eq!(pause_entre_deux_tours(&b), Duration::from_secs(2));
    }

    fn sweep_db(n: i64) -> (crate::db::sqlite::SqliteDb, Arc<dyn DbBackend>) {
        sweep_db_avec(n, None)
    }

    /// Comme [`sweep_db`], mais les fichiers EXISTENT réellement sur le disque.
    ///
    /// Nécessaire depuis #1865 : un chemin introuvable quitte la piste AVANT
    /// tout témoin `rg_analyzed`. Des fichiers présents mais indécodables
    /// empruntent la voie normale — témoin posé — et c'est précisément la voie
    /// que les tests de coupure doivent pouvoir interrompre.
    ///
    /// Le `TempDir` est rendu à l'appelant : le lâcher effacerait les fichiers
    /// sous les pieds de la passe.
    fn sweep_db_fichiers_presents(
        n: i64,
    ) -> (
        tempfile::TempDir,
        crate::db::sqlite::SqliteDb,
        Arc<dyn DbBackend>,
    ) {
        let tmp = tempfile::TempDir::new().unwrap();
        let (db, backend) = sweep_db_avec(n, Some(tmp.path()));
        (tmp, db, backend)
    }

    fn sweep_db_avec(
        n: i64,
        dossier: Option<&std::path::Path>,
    ) -> (crate::db::sqlite::SqliteDb, Arc<dyn DbBackend>) {
        use crate::db::sqlite::SqliteDb;
        let db = SqliteDb::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE zones (id INTEGER PRIMARY KEY, name TEXT, last_play_state TEXT);
             CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL,
                                    updated_at TEXT NOT NULL DEFAULT '');
             CREATE TABLE tracks (id INTEGER PRIMARY KEY, album_id INTEGER, file_path TEXT,
                                  duration_ms INTEGER, sample_rate INTEGER, channels INTEGER);
             CREATE TABLE track_metadata (track_id INTEGER NOT NULL, key TEXT NOT NULL,
                                          value TEXT NOT NULL, PRIMARY KEY (track_id, key));",
        )
        .unwrap();
        for i in 1..=n {
            let chemin = match dossier {
                // Un octet suffit : le fichier RÉPOND, et reste indécodable.
                Some(d) => {
                    let f = d.join(format!("{i}.flac"));
                    std::fs::write(&f, b"pas du flac").unwrap();
                    f.to_string_lossy().to_string()
                }
                None => format!("/i2496-inexistant/{i}.flac"),
            };
            db.execute(
                "INSERT INTO tracks (id, album_id, file_path, duration_ms, sample_rate, channels) \
                 VALUES (?, NULL, ?, 300000, 44100, 2)",
                &[&i, &chemin],
            )
            .unwrap();
        }
        let backend: Arc<dyn DbBackend> = Arc::new(db.clone());
        (db, backend)
    }

    /// #2496 : la boucle ne lisait QUE `replaygain_analysis_enabled`, jamais
    /// `replaygain_mode`. L'utilisateur qui choisissait « Désactivé (niveau
    /// source) » n'arrêtait que l'application du gain à la lecture ; le
    /// balayage continuait de décoder sa bibliothèque entière.
    #[test]
    fn the_off_mode_gates_the_analysis_sweep() {
        let (_db, backend) = sweep_db(1);
        let settings = SettingsRepo::with_backend(backend.clone());

        // Installation neuve : aucun mode écrit ⇒ Off ⇒ rien à balayer.
        assert!(
            !analysis_enabled(&backend),
            "un mode absent vaut Désactivé : la passe ne doit pas démarrer"
        );
        settings.set(MODE_KEY, "off").unwrap();
        assert!(
            !analysis_enabled(&backend),
            "« Désactivé » doit arrêter le balayage d'analyse (#2496)"
        );

        // Un mode réellement demandé la relance.
        settings.set(MODE_KEY, "track").unwrap();
        assert!(analysis_enabled(&backend), "mode piste ⇒ balayage autorisé");
        settings.set(MODE_KEY, "album").unwrap();
        assert!(analysis_enabled(&backend), "mode album ⇒ balayage autorisé");

        // La coche « Analyse ReplayGain » reste un veto indépendant.
        settings.set(ANALYSIS_ENABLED_KEY, "false").unwrap();
        assert!(
            !analysis_enabled(&backend),
            "la coche décochée coupe la passe même avec un mode armé"
        );
        settings.set(ANALYSIS_ENABLED_KEY, "true").unwrap();
        assert!(analysis_enabled(&backend));
        settings.set(MODE_KEY, "off").unwrap();
        assert!(
            !analysis_enabled(&backend),
            "la coche seule ne ressuscite pas la passe quand le mode est Désactivé"
        );
    }

    /// Les trois comportements que #2496 demande de distinguer, sur une même
    /// base : le balayage s'arrête, le gain DÉJÀ mesuré est conservé, et
    /// l'application à la lecture relève d'un autre réglage.
    #[tokio::test]
    async fn disabling_stops_the_pass_without_losing_a_single_measured_gain() {
        let (_db, backend) = sweep_db(2);
        let settings = SettingsRepo::with_backend(backend.clone());
        let meta = TrackMetadataRepo::with_backend(backend.clone());

        // Piste 1 : déjà mesurée. Des heures de décodage derrière cette valeur.
        meta.set(1, "rg_track_gain", "-6.50 dB").unwrap();
        meta.set(1, "rg_track_peak", "0.988553").unwrap();
        meta.set(1, "rg_analyzed", "1700000000").unwrap();

        // Contre-épreuve intégrée : mode armé, la passe travaille pour de bon.
        settings.set(MODE_KEY, "track").unwrap();
        assert_eq!(
            analyze_track_batch(&backend).await,
            1,
            "mode armé : la piste 2 devait être prise en charge"
        );

        // Même lot rejoué, réglage sur « Désactivé ».
        meta.delete(2, "rg_analyzed").unwrap();
        settings.set(MODE_KEY, "off").unwrap();
        assert_eq!(
            analyze_track_batch(&backend).await,
            0,
            "« Désactivé » : la passe ne doit décoder aucun fichier"
        );
        assert!(
            !meta.get_all(2).unwrap().contains_key("rg_analyzed"),
            "aucune piste ne doit avoir été touchée pendant que le réglage est coupé"
        );

        // AUCUNE donnée perdue : couper l'analyse suspend, n'efface pas.
        let kept = meta.get_all(1).unwrap();
        assert_eq!(
            kept.get("rg_track_gain").map(String::as_str),
            Some("-6.50 dB")
        );
        assert_eq!(
            kept.get("rg_track_peak").map(String::as_str),
            Some("0.988553")
        );

        // L'application à la lecture est un AUTRE réglage, une AUTRE décision :
        // `Off` n'applique rien, et réarmer un mode retrouve le gain intact.
        assert!(stored_gain_for(&backend, 1, ReplayGainMode::Off).is_none());
        let back = stored_gain_for(&backend, 1, ReplayGainMode::Track)
            .expect("le gain mesuré doit resservir tel quel une fois le mode réarmé");
        assert!((back.gain_db - (-6.5)).abs() < 1e-9, "{back:?}");
    }

    /// #2496, point 4 : un réglage qui n'agit qu'au prochain démarrage n'est pas
    /// un réglage. Un lot vaut 25 fichiers à jusqu'à 180 s chacun — sans
    /// relecture par fichier, « Désactivé » laissait tourner plus d'une heure de
    /// décodage.
    #[tokio::test]
    async fn switching_off_mid_batch_interrupts_the_running_sweep() {
        let (_tmp, _db, interne) = sweep_db_fichiers_presents(4);
        SettingsRepo::with_backend(interne.clone())
            .set(MODE_KEY, "track")
            .unwrap();
        // Un fichier à la fois : la frontière « entre deux fichiers » est alors
        // exactement celle d'avant #5519, et le compte se lit au fichier près.
        SettingsRepo::with_backend(interne.clone())
            .set(crate::taches_de_fond::vitesse::CLE_REGLAGE, "discreet")
            .unwrap();

        // #5519 — la pause fixe de 400 ms, qui laissait le temps à un fil de
        // basculer le réglage, n'existe plus, et les témoins ne s'écrivent
        // plus qu'à la fin du tour. On bascule donc AU MOMENT MÊME où la
        // garde relit le réglage pour la DEUXIÈME fois, c'est-à-dire après le
        // premier fichier : plus aucune course, le test dit la même chose à
        // chaque passage.
        let backend = CoupeALaGarde::poser(interne.clone(), 1);
        let done = analyze_track_batch(&backend).await;
        assert!(
            SettingsRepo::with_backend(interne.clone())
                .get(MODE_KEY)
                .unwrap()
                .as_deref()
                == Some("off"),
            "le test n'a jamais réussi à couper le réglage — il ne prouve rien"
        );
        assert_eq!(
            done, 1,
            "un fichier à la fois : seule la première piste devait être traitée, done={done}"
        );
        let meta = TrackMetadataRepo::with_backend(interne.clone());
        assert!(
            !meta.get_all(4).unwrap().contains_key("rg_analyzed"),
            "la dernière piste du lot ne devait jamais être décodée après la coupure"
        );
    }

    /// Même coupure à la vitesse par défaut (deux fichiers à la fois) : ce qui
    /// est déjà en vol finit, mais RIEN de neuf ne part après la coupure.
    ///
    /// La coupure tombe au PREMIER témoin posé, quelle que soit la piste : à
    /// deux fichiers à la fois, l'ordre d'arrivée n'est pas celui du lot (la
    /// piste 2 peut finir avant la piste 1). Couper sur « la piste 1 » laissait
    /// partir les pistes 3, 4, 5 tant que la 1 traînait sur le pool bloquant —
    /// des lancements légitimes, réglage encore armé, que le test prenait pour
    /// un défaut (done=5 en CI, 02/10). Au premier témoin, aucun fichier n'est
    /// encore rendu : seuls les `largeur` déjà en vol peuvent finir.
    #[tokio::test]
    async fn switching_off_mid_batch_stops_launching_at_normal_speed() {
        let (_tmp, _db, interne) = sweep_db_fichiers_presents(6);
        SettingsRepo::with_backend(interne.clone())
            .set(MODE_KEY, "track")
            .unwrap();
        let largeur = crate::taches_de_fond::vitesse::largeur_courante(&interne);
        assert!(
            largeur >= 2 && largeur < 6,
            "le test parle de plusieurs fichiers en vol sur un lot plus large : largeur={largeur}"
        );
        // La garde qui suit les `largeur` premiers lancements est la première
        // relue APRÈS qu'un fichier a fini : c'est là que tombe la coupure.
        let backend = CoupeALaGarde::poser(interne.clone(), largeur);
        let done = analyze_track_batch(&backend).await;
        assert!(
            SettingsRepo::with_backend(interne.clone())
                .get(MODE_KEY)
                .unwrap()
                .as_deref()
                == Some("off"),
            "le test n'a jamais réussi à couper le réglage — il ne prouve rien"
        );
        assert!(
            done >= 1 && done <= largeur,
            "au plus les fichiers déjà en vol à la coupure : done={done}, largeur={largeur}"
        );
        let meta = TrackMetadataRepo::with_backend(interne.clone());
        assert!(!meta.get_all(6).unwrap().contains_key("rg_analyzed"));
    }

    /// Une base qui passe le mode ReplayGain à « off » quand la garde de
    /// lancement le relit pour la `(apres + 1)`-ième fois (#2496, #5519).
    ///
    /// La garde (`analysis_enabled`, dans `peut_lancer`) relit le mode avant
    /// CHAQUE fichier lancé, et rien d'autre ne le relit pendant un lot. Les
    /// `apres` premières lectures lancent les fichiers du premier créneau ; la
    /// suivante n'a lieu qu'après la fin d'un fichier. Couper là, c'est couper
    /// « en plein lot », entre deux fichiers.
    ///
    /// Avant #5519, la coupure tombait dans l'écriture du premier témoin
    /// `rg_analyzed`. Les témoins s'écrivent maintenant ensemble, à la fin du
    /// tour : une coupure accrochée à eux tomberait après le lot, et le test
    /// ne prouverait plus rien.
    ///
    /// La coupure est ATOMIQUE : « off » est écrit sous le verrou du compteur,
    /// AVANT que la lecture qui le déclenche ne soit servie. La garde qui la
    /// déclenche lit donc déjà « off ».
    struct CoupeALaGarde {
        interne: Arc<dyn DbBackend>,
        apres: usize,
        lectures: std::sync::Mutex<usize>,
    }

    impl CoupeALaGarde {
        fn poser(interne: Arc<dyn DbBackend>, apres: usize) -> Arc<dyn DbBackend> {
            Arc::new(Self {
                interne,
                apres,
                lectures: std::sync::Mutex::new(0),
            })
        }
        fn avant_lecture(&self, p: &[&dyn ToSqlValue]) {
            let lit_le_mode = p.first().is_some_and(|v| {
                matches!(v.to_sql_value(), crate::db::backend::SqlValue::Text(ref k) if k == MODE_KEY)
            });
            if !lit_le_mode {
                return;
            }
            let mut n = self.lectures.lock().unwrap();
            *n += 1;
            if *n == self.apres + 1 {
                SettingsRepo::with_backend(self.interne.clone())
                    .set(MODE_KEY, "off")
                    .unwrap();
            }
        }
    }

    impl DbBackend for CoupeALaGarde {
        fn engine(&self) -> crate::db::engine::Engine {
            self.interne.engine()
        }
        fn execute(&self, sql: &str, p: &[&dyn ToSqlValue]) -> Result<usize, String> {
            self.interne.execute(sql, p)
        }
        fn last_insert_rowid(&self) -> i64 {
            self.interne.last_insert_rowid()
        }
        fn query_one(
            &self,
            sql: &str,
            p: &[&dyn ToSqlValue],
        ) -> Result<Option<Vec<crate::db::backend::SqlValue>>, String> {
            self.avant_lecture(p);
            self.interne.query_one(sql, p)
        }
        fn query_many(
            &self,
            sql: &str,
            p: &[&dyn ToSqlValue],
        ) -> Result<Vec<Vec<crate::db::backend::SqlValue>>, String> {
            self.interne.query_many(sql, p)
        }
        fn write_tx(
            &self,
            f: &mut dyn FnMut(&dyn crate::db::backend::DbTxHandle) -> Result<(), String>,
        ) -> Result<(), String> {
            self.interne.write_tx(f)
        }
        fn execute_batch(&self, sql: &str) -> Result<(), String> {
            self.interne.execute_batch(sql)
        }
        fn query_one_strong(
            &self,
            sql: &str,
            p: &[&dyn ToSqlValue],
        ) -> Result<Option<Vec<crate::db::backend::SqlValue>>, String> {
            self.avant_lecture(p);
            self.interne.query_one_strong(sql, p)
        }
        fn query_many_strong(
            &self,
            sql: &str,
            p: &[&dyn ToSqlValue],
        ) -> Result<Vec<Vec<crate::db::backend::SqlValue>>, String> {
            self.interne.query_many_strong(sql, p)
        }
    }

    // ---------------------------------------------------------------------
    // #5519, suite — le curseur de la sélection et l'écriture groupée du tour.
    // ---------------------------------------------------------------------

    /// Une base qui note ce que la passe lui demande : le curseur de chaque
    /// sélection de candidats, et chaque écriture, groupée ou non.
    struct BaseQuiNote {
        interne: Arc<dyn DbBackend>,
        curseurs: std::sync::Mutex<Vec<i64>>,
        transactions: std::sync::atomic::AtomicUsize,
        ecritures_seules: std::sync::atomic::AtomicUsize,
        refuser_les_transactions: bool,
    }

    impl BaseQuiNote {
        fn poser(interne: Arc<dyn DbBackend>, refuser_les_transactions: bool) -> Arc<Self> {
            Arc::new(Self {
                interne,
                curseurs: std::sync::Mutex::new(Vec::new()),
                transactions: std::sync::atomic::AtomicUsize::new(0),
                ecritures_seules: std::sync::atomic::AtomicUsize::new(0),
                refuser_les_transactions,
            })
        }
        fn curseurs(&self) -> Vec<i64> {
            self.curseurs.lock().unwrap().clone()
        }
        fn transactions(&self) -> usize {
            self.transactions.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn ecritures_seules(&self) -> usize {
            self.ecritures_seules
                .load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl DbBackend for BaseQuiNote {
        fn engine(&self) -> crate::db::engine::Engine {
            self.interne.engine()
        }
        fn execute(&self, sql: &str, p: &[&dyn ToSqlValue]) -> Result<usize, String> {
            if sql.contains("track_metadata") || sql.contains("UPDATE tracks") {
                self.ecritures_seules
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            self.interne.execute(sql, p)
        }
        fn last_insert_rowid(&self) -> i64 {
            self.interne.last_insert_rowid()
        }
        fn query_one(
            &self,
            sql: &str,
            p: &[&dyn ToSqlValue],
        ) -> Result<Option<Vec<crate::db::backend::SqlValue>>, String> {
            self.interne.query_one(sql, p)
        }
        fn query_many(
            &self,
            sql: &str,
            p: &[&dyn ToSqlValue],
        ) -> Result<Vec<Vec<crate::db::backend::SqlValue>>, String> {
            if sql.contains("AND t.id > ? ORDER BY t.id LIMIT ?") {
                // Paramètres : seuil de report, curseur, borne.
                if let Some(c) = p.get(1).and_then(|v| v.to_sql_value().as_i64()) {
                    self.curseurs.lock().unwrap().push(c);
                }
            }
            self.interne.query_many(sql, p)
        }
        fn write_tx(
            &self,
            f: &mut dyn FnMut(&dyn crate::db::backend::DbTxHandle) -> Result<(), String>,
        ) -> Result<(), String> {
            if self.refuser_les_transactions {
                return Err("begin tx: cannot start a transaction within a transaction".into());
            }
            self.transactions
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.interne.write_tx(f)
        }
        fn execute_batch(&self, sql: &str) -> Result<(), String> {
            self.interne.execute_batch(sql)
        }
        fn query_one_strong(
            &self,
            sql: &str,
            p: &[&dyn ToSqlValue],
        ) -> Result<Option<Vec<crate::db::backend::SqlValue>>, String> {
            self.interne.query_one_strong(sql, p)
        }
        fn query_many_strong(
            &self,
            sql: &str,
            p: &[&dyn ToSqlValue],
        ) -> Result<Vec<Vec<crate::db::backend::SqlValue>>, String> {
            self.interne.query_many_strong(sql, p)
        }
    }

    /// #5519 — un tour ne relit plus la bibliothèque depuis la première piste :
    /// il reprend APRÈS la dernière piste que le tour précédent a laissée
    /// derrière lui, et ne repart de 0 qu'une fois, quand plus rien ne suit.
    #[tokio::test]
    async fn le_tour_suivant_reprend_apres_le_curseur() {
        let (_tmp, _db, interne) = sweep_db_fichiers_presents(30);
        SettingsRepo::with_backend(interne.clone())
            .set(MODE_KEY, "track")
            .unwrap();
        let note = BaseQuiNote::poser(interne.clone(), false);
        let backend: Arc<dyn DbBackend> = note.clone();
        assert_eq!(analyze_track_batch(&backend).await, 25);
        assert_eq!(analyze_track_batch(&backend).await, 5);
        assert_eq!(analyze_track_batch(&backend).await, 0);
        assert_eq!(
            note.curseurs(),
            vec![0, 25, 30, 0],
            "premier tour depuis 0 ; le deuxième reprend après la 25e piste, pas \
             depuis le début ; le troisième ne trouve rien après la 30e et repart \
             UNE fois de 0 avant de conclure"
        );
    }

    /// Le curseur n'est jamais une raison de manquer une piste : celle qui
    /// redevient candidate DERRIÈRE lui (report expiré, re-scan, rattrapage)
    /// est reprise au tour qui ne trouve plus rien devant lui.
    #[tokio::test]
    async fn une_piste_redevenue_candidate_derriere_le_curseur_est_reprise() {
        let (_tmp, _db, backend) = sweep_db_fichiers_presents(3);
        SettingsRepo::with_backend(backend.clone())
            .set(MODE_KEY, "track")
            .unwrap();
        assert_eq!(analyze_track_batch(&backend).await, 3);
        TrackMetadataRepo::with_backend(backend.clone())
            .delete(1, "rg_analyzed")
            .unwrap();
        assert_eq!(
            analyze_track_batch(&backend).await,
            1,
            "la piste 1, redevenue candidate derrière le curseur (posé sur 3), \
             devait être reprise"
        );
        assert!(
            TrackMetadataRepo::with_backend(backend.clone())
                .get_all(1)
                .unwrap()
                .contains_key("rg_analyzed")
        );
    }

    /// Une piste qui n'a pas quitté le balayage (lot arrêté par le réglage,
    /// la pause ou la lecture) reste DEVANT le curseur : le tour suivant la
    /// reprend en premier, sans attendre la reprise depuis 0.
    #[tokio::test]
    async fn le_curseur_s_arrete_avant_la_premiere_piste_non_traitee() {
        let (_tmp, _db, interne) = sweep_db_fichiers_presents(4);
        SettingsRepo::with_backend(interne.clone())
            .set(MODE_KEY, "track")
            .unwrap();
        SettingsRepo::with_backend(interne.clone())
            .set(crate::taches_de_fond::vitesse::CLE_REGLAGE, "discreet")
            .unwrap();
        let note = BaseQuiNote::poser(CoupeALaGarde::poser(interne.clone(), 1), false);
        let backend: Arc<dyn DbBackend> = note.clone();
        assert_eq!(analyze_track_batch(&backend).await, 1);
        SettingsRepo::with_backend(interne.clone())
            .set(MODE_KEY, "track")
            .unwrap();
        assert_eq!(analyze_track_batch(&backend).await, 3);
        assert_eq!(
            note.curseurs(),
            vec![0, 1],
            "coupé après la piste 1, le lot laisse le curseur sur 1 : les pistes \
             2 à 4 sont reprises au tour suivant"
        );
    }

    /// #5519 — les écritures d'un tour partent ENSEMBLE, en une transaction,
    /// et plus une à une : avant, chaque piste en enchaînait cinq à neuf,
    /// chacune avec son `COMMIT` et sa prise du verrou d'écriture.
    #[tokio::test]
    async fn un_tour_s_ecrit_en_une_seule_transaction() {
        let (_tmp, db, interne) = sweep_db_fichiers_presents(6);
        db.execute("ALTER TABLE tracks ADD COLUMN audio_fingerprint TEXT", &[])
            .unwrap();
        SettingsRepo::with_backend(interne.clone())
            .set(MODE_KEY, "track")
            .unwrap();
        let note = BaseQuiNote::poser(interne.clone(), false);
        let backend: Arc<dyn DbBackend> = note.clone();
        assert_eq!(analyze_track_batch(&backend).await, 6);
        assert_eq!(
            (note.transactions(), note.ecritures_seules()),
            (1, 0),
            "six pistes : une transaction pour le tour, aucune écriture isolée"
        );
        for id in 1..=6 {
            let t = TrackMetadataRepo::with_backend(interne.clone())
                .get_all(id)
                .unwrap();
            assert!(t.contains_key("rg_analyzed"), "piste {id} : {t:?}");
        }
    }

    /// Les écritures d'une piste, pour les deux épreuves ci-dessous : toutes
    /// les branches (report effacé ou posé, mesure avec ou sans plage, plage
    /// du fichier présente, vide, absente, empreinte, témoin).
    fn ecritures_variees() -> Vec<EcrituresDePiste> {
        vec![
            EcrituresDePiste {
                track_id: 1,
                effacer_le_report: true,
                mesure: Some((-14.25, 0.9876, 1.0123, Some(9))),
                empreinte: Some("env100ms-v1:abc".into()),
                temoin: Some("1790000001".into()),
                ..Default::default()
            },
            EcrituresDePiste {
                track_id: 2,
                effacer_le_report: true,
                mesure: Some((-9.5, 0.5, 0.51, Some(7))),
                empreinte: Some("env100ms-v1:-".into()),
                temoin: Some("1790000002".into()),
                ..Default::default()
            },
            EcrituresDePiste {
                track_id: 3,
                effacer_le_report: true,
                mesure: Some((-20.0, 0.25, 0.26, Some(12))),
                temoin: Some("1790000003".into()),
                ..Default::default()
            },
            EcrituresDePiste {
                track_id: 4,
                report: Some("00000001790000004".into()),
                ..Default::default()
            },
            EcrituresDePiste {
                track_id: 5,
                effacer_le_report: true,
                mesure: Some((-30.0, 0.1, 0.1, None)),
                temoin: Some("1790000005".into()),
                ..Default::default()
            },
        ]
    }

    /// L'état de départ des deux épreuves : un report à effacer, un DR du
    /// fichier qui fait foi, un DR vide qui ne bloque pas.
    fn base_ecritures() -> (crate::db::sqlite::SqliteDb, Arc<dyn DbBackend>) {
        let (db, backend) = sweep_db(5);
        db.execute("ALTER TABLE tracks ADD COLUMN audio_fingerprint TEXT", &[])
            .unwrap();
        let meta = TrackMetadataRepo::with_backend(backend.clone());
        meta.set(1, PATH_UNRESOLVED_KEY, "00000001700000000")
            .unwrap();
        meta.set(2, "dr_track", "14").unwrap();
        meta.set(3, "dr_track", "  ").unwrap();
        (db, backend)
    }

    fn tout_lire(db: &crate::db::sqlite::SqliteDb) -> Vec<String> {
        let mut v: Vec<String> = db
            .query_many(
                "SELECT track_id || ' ' || key || '=' || value FROM track_metadata \
                 UNION ALL SELECT id || ' empreinte=' || COALESCE(audio_fingerprint, '∅') FROM tracks",
                &[],
            )
            .unwrap()
            .into_iter()
            .map(|r| r[0].as_string().unwrap_or_default())
            .collect();
        v.sort();
        v
    }

    /// Le regroupement ne change RIEN à ce qui est écrit : mêmes clés, mêmes
    /// valeurs que l'écriture piste à piste — qui est aussi le repli quand la
    /// transaction échoue. Le tag du fichier fait toujours foi (piste 2), un
    /// tag vide ne bloque pas (piste 3).
    #[test]
    fn l_ecriture_groupee_pose_les_memes_lignes_que_piste_a_piste() {
        let (db_groupee, groupee) = base_ecritures();
        let (db_seule, seule) = base_ecritures();
        let refus = BaseQuiNote::poser(seule.clone(), true);
        let refus_dyn: Arc<dyn DbBackend> = refus.clone();
        let lot = ecritures_variees();
        assert_eq!(ecrire_le_tour(&groupee, &lot), EcritureDuTour::Groupee);
        assert_eq!(
            ecrire_le_tour(&refus_dyn, &lot),
            EcritureDuTour::PisteAPiste,
            "transaction refusée : le tour repasse par l'écriture piste à piste"
        );
        assert!(refus.ecritures_seules() > 0);
        let attendu = tout_lire(&db_seule);
        assert_eq!(tout_lire(&db_groupee), attendu);
        // Et ce qui est écrit est bien ce qu'on attend, pas seulement égal.
        for ligne in [
            "1 rg_track_gain=-3.75 dB",
            "1 dr_track=9",
            "1 dr_source=analysis",
            "1 rg_analyzed=1790000001",
            "1 empreinte=env100ms-v1:abc",
            "2 dr_track=14",
            "3 dr_track=12",
            "4 rg_path_unresolved=00000001790000004",
        ] {
            assert!(
                attendu.iter().any(|l| l == ligne),
                "{ligne} absent de {attendu:?}"
            );
        }
        for absente in [
            "1 rg_path_unresolved=",
            "2 dr_source=",
            "4 rg_analyzed=",
            "5 dr_track=",
        ] {
            assert!(
                !attendu.iter().any(|l| l.starts_with(absente)),
                "{absente} ne devait pas être écrit : {attendu:?}"
            );
        }
    }

    /// #5798 / #5202 — l'écriture du tour respecte un lot de scan qui tient sa
    /// transaction : elle attend sa CESSION, passe en une fois, et ne s'écrit
    /// pas dans la transaction du lot (le `ROLLBACK` du lot ne l'emporte pas).
    #[test]
    fn l_ecriture_du_tour_passe_a_la_cession_d_un_lot_de_scan() {
        use crate::db::sqlite::SqliteDb;
        let dossier = crate::test_scratch::scratch_dir("rg-ecriture-du-tour-pendant-un-lot");
        let chemin = dossier.join("tune.db");
        let db = SqliteDb::open(&chemin.to_string_lossy()).unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        for id in 1..=5i64 {
            db.execute(
                &format!(
                    "INSERT INTO tracks (id, title, file_path) VALUES ({id}, 't', '/x/{id}.flac')"
                ),
                &[],
            )
            .unwrap();
        }
        let backend: Arc<dyn DbBackend> = Arc::new(db.clone());
        const LOT: std::time::Duration = std::time::Duration::from_secs(3);
        let (ouvert, lot_ouvert) = std::sync::mpsc::channel::<()>();
        let b_lot = backend.clone();
        let lot = std::thread::spawn(move || {
            b_lot.execute_batch("BEGIN IMMEDIATE").unwrap();
            b_lot
                .execute(
                    "INSERT INTO tracks (title, file_path) VALUES ('du lot', '/lot.flac')",
                    &[],
                )
                .unwrap();
            ouvert.send(()).unwrap();
            let debut = std::time::Instant::now();
            while debut.elapsed() < LOT {
                std::thread::sleep(std::time::Duration::from_millis(20));
                // Le point de cession du scan, entre deux fichiers.
                b_lot.ceder_aux_ecrivains();
            }
            b_lot.execute_batch("ROLLBACK").unwrap();
        });
        lot_ouvert.recv().unwrap();
        let debut = std::time::Instant::now();
        let issue = ecrire_le_tour(&backend, &ecritures_variees());
        let attente = debut.elapsed();
        lot.join().unwrap();
        assert_eq!(issue, EcritureDuTour::Groupee);
        assert!(
            attente < std::time::Duration::from_secs(1),
            "l'écriture du tour a attendu {attente:?} : elle devait passer à la \
             première cession du lot, pas à sa fin ({LOT:?})"
        );
        let meta = TrackMetadataRepo::with_backend(backend.clone());
        assert_eq!(
            meta.get_all(1)
                .unwrap()
                .get("rg_analyzed")
                .map(String::as_str),
            Some("1790000001"),
            "le ROLLBACK du lot a emporté l'écriture du tour"
        );
    }

    /// La même sélection et la même écriture sur PostgreSQL : le curseur, son
    /// ordre, l'écriture groupée et le tag du fichier qui fait foi.
    /// `TUNE_TEST_PG_URL` absent : sauté. Tables temporaires sur une seule
    /// connexion, comme `i3924_album_provenance_postgres`.
    #[cfg(feature = "postgres")]
    #[tokio::test(flavor = "multi_thread")]
    async fn pg_5519_curseur_et_ecriture_groupee() {
        let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
            eprintln!("SAUT: TUNE_TEST_PG_URL absent");
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        let pg: Arc<dyn DbBackend> =
            Arc::new(crate::db::backend::PostgresBackend::new(pool.clone()));
        pg.execute_batch(
            "CREATE TEMP TABLE tracks (id BIGINT PRIMARY KEY, file_path TEXT, duration_ms BIGINT,
                sample_rate BIGINT, channels BIGINT, audio_fingerprint TEXT);
             CREATE TEMP TABLE track_metadata (track_id BIGINT NOT NULL, key TEXT NOT NULL,
                value TEXT NOT NULL, PRIMARY KEY (track_id, key));",
        )
        .unwrap();
        // Insérées dans le désordre : l'ordre rendu doit venir de `ORDER BY`.
        for id in (1..=30i64).rev() {
            pg.execute(
                "INSERT INTO tracks (id, file_path, duration_ms, sample_rate, channels) \
                 VALUES (?, ?, 240000, 44100, 2)",
                &[
                    &id as &dyn ToSqlValue,
                    &format!("/p/{id}.flac") as &dyn ToSqlValue,
                ],
            )
            .unwrap();
            if id <= 20 {
                pg.execute(
                    "INSERT INTO track_metadata (track_id, key, value) VALUES (?, 'rg_analyzed', '1')",
                    &[&id as &dyn ToSqlValue],
                )
                .unwrap();
            }
        }
        let ids = |apres: i64| -> Vec<i64> {
            selectionner_les_candidats_replaygain(&pg, apres, 25)
                .unwrap()
                .iter()
                .map(|r| r[0].as_i64().unwrap())
                .collect()
        };
        assert_eq!(ids(0), (21..=30).collect::<Vec<_>>());
        assert_eq!(ids(25), (26..=30).collect::<Vec<_>>());
        assert!(ids(30).is_empty());

        pg.execute(
            "INSERT INTO track_metadata (track_id, key, value) VALUES (22, 'dr_track', '14')",
            &[],
        )
        .unwrap();
        let lot: Vec<EcrituresDePiste> = (21..=23)
            .map(|id| EcrituresDePiste {
                track_id: id,
                effacer_le_report: true,
                mesure: Some((-14.25, 0.9876, 1.0123, Some(9))),
                empreinte: Some(format!("env100ms-v1:{id}")),
                temoin: Some("1790000000".into()),
                ..Default::default()
            })
            .collect();
        assert_eq!(ecrire_le_tour(&pg, &lot), EcritureDuTour::Groupee);
        let cle = |id: i64, k: &str| -> Option<String> {
            pg.query_one(
                "SELECT value FROM track_metadata WHERE track_id = ? AND key = ?",
                &[&id as &dyn ToSqlValue, &k as &dyn ToSqlValue],
            )
            .unwrap()
            .and_then(|r| r[0].as_string())
        };
        assert_eq!(cle(21, "rg_track_gain").as_deref(), Some("-3.75 dB"));
        assert_eq!(cle(21, "dr_track").as_deref(), Some("9"));
        assert_eq!(
            cle(22, "dr_track").as_deref(),
            Some("14"),
            "le tag du fichier fait foi"
        );
        assert_eq!(cle(22, "dr_source"), None);
        assert_eq!(ids(0), (24..=30).collect::<Vec<_>>());
        pool.close().await;
    }

    /// #5519 — plus de pause fixe entre deux fichiers. Dix fichiers présents
    /// mais indécodables passaient en ≥ 4 s (10 × 400 ms) ; ils doivent passer
    /// en bien moins d'une seconde, même un à la fois.
    #[tokio::test]
    async fn plus_de_pause_fixe_entre_deux_fichiers() {
        let (_tmp, _db, backend) = sweep_db_fichiers_presents(10);
        SettingsRepo::with_backend(backend.clone())
            .set(MODE_KEY, "track")
            .unwrap();
        SettingsRepo::with_backend(backend.clone())
            .set(crate::taches_de_fond::vitesse::CLE_REGLAGE, "discreet")
            .unwrap();
        let t = std::time::Instant::now();
        let done = analyze_track_batch(&backend).await;
        let duree = t.elapsed();
        assert_eq!(done, 10);
        assert!(
            duree < std::time::Duration::from_millis(1_500),
            "10 fichiers en {duree:?} : une pause fixe entre deux fichiers est revenue"
        );
    }

    /// #5519 — le lanceur borné tient EXACTEMENT sa largeur : jamais plus de
    /// `largeur` travaux en vol, et il la remplit quand il le peut.
    #[tokio::test]
    async fn le_lanceur_borne_tient_sa_largeur() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for largeur in [1usize, 2, 4] {
            let en_vol = AtomicUsize::new(0);
            let max = AtomicUsize::new(0);
            let mut recus = 0;
            en_parallele_borne(
                largeur,
                0..10,
                || true,
                |_| {
                    let (en_vol, max) = (&en_vol, &max);
                    async move {
                        let n = en_vol.fetch_add(1, Ordering::SeqCst) + 1;
                        max.fetch_max(n, Ordering::SeqCst);
                        for _ in 0..5 {
                            tokio::task::yield_now().await;
                        }
                        en_vol.fetch_sub(1, Ordering::SeqCst);
                    }
                },
                |()| {
                    recus += 1;
                    true
                },
            )
            .await;
            assert_eq!(
                recus, 10,
                "largeur {largeur} : tous les travaux doivent finir"
            );
            assert_eq!(
                max.load(Ordering::SeqCst),
                largeur,
                "largeur {largeur} : nombre maximal de travaux simultanés"
            );
        }

        // Une garde fausse, ou un `recu` qui dit stop : plus aucun lancement.
        let mut lances = 0;
        let mut autorises = 3;
        en_parallele_borne(
            2,
            0..10,
            || {
                autorises -= 1;
                autorises >= 0
            },
            |_| {
                lances += 1;
                async {}
            },
            |()| true,
        )
        .await;
        assert_eq!(lances, 3);
        let mut lances = 0;
        en_parallele_borne(
            1,
            0..10,
            || true,
            |_| {
                lances += 1;
                async {}
            },
            |()| false,
        )
        .await;
        assert_eq!(lances, 1, "un `recu` qui dit stop arrête les lancements");
    }

    // ---------------------------------------------------------------------
    // #2495 — céder PENDANT le décodage, pas seulement entre deux fichiers.
    // ---------------------------------------------------------------------

    /// Base de test dont la zone se met à jouer d'elle-même, à un rang de
    /// lecture CHOISI.
    ///
    /// Le déclencheur n'est pas une horloge mais un compteur de requêtes : le
    /// garde-fou d'entrée de fichier et la veille interrogent la même table,
    /// et faire dépendre le test d'un `sleep` bien placé le rendrait
    /// intermittent — donc muet le jour où il aurait quelque chose à dire.
    /// Ici, « la lecture démarre juste après la N-ième vérification » est un
    /// fait, pas une chance.
    struct ZoneQuiDemarre {
        interne: Arc<dyn DbBackend>,
        lectures: std::sync::atomic::AtomicUsize,
        demarre_apres: usize,
    }

    impl ZoneQuiDemarre {
        fn poser(interne: Arc<dyn DbBackend>, demarre_apres: usize) -> Arc<dyn DbBackend> {
            Arc::new(Self {
                interne,
                lectures: std::sync::atomic::AtomicUsize::new(0),
                demarre_apres,
            })
        }
        fn lectures(&self) -> usize {
            self.lectures.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl DbBackend for ZoneQuiDemarre {
        fn engine(&self) -> crate::db::engine::Engine {
            self.interne.engine()
        }
        fn execute(&self, sql: &str, p: &[&dyn ToSqlValue]) -> Result<usize, String> {
            self.interne.execute(sql, p)
        }
        fn last_insert_rowid(&self) -> i64 {
            self.interne.last_insert_rowid()
        }
        fn query_one(
            &self,
            sql: &str,
            p: &[&dyn ToSqlValue],
        ) -> Result<Option<Vec<crate::db::backend::SqlValue>>, String> {
            if sql.contains("last_play_state = 'playing'") {
                let rang = self
                    .lectures
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    + 1;
                return Ok((rang > self.demarre_apres)
                    .then(|| vec![crate::db::backend::SqlValue::Text("Salon".to_string())]));
            }
            self.interne.query_one(sql, p)
        }
        fn query_many(
            &self,
            sql: &str,
            p: &[&dyn ToSqlValue],
        ) -> Result<Vec<Vec<crate::db::backend::SqlValue>>, String> {
            self.interne.query_many(sql, p)
        }
        fn write_tx(
            &self,
            f: &mut dyn FnMut(&dyn crate::db::backend::DbTxHandle) -> Result<(), String>,
        ) -> Result<(), String> {
            self.interne.write_tx(f)
        }
        fn execute_batch(&self, sql: &str) -> Result<(), String> {
            self.interne.execute_batch(sql)
        }
        // Les variantes « strong » de SQLite lisent par la connexion
        // d'écriture. Laisser l'implémentation par défaut les rabattrait sur
        // la lecture faible et ferait mentir le test sur autre chose que ce
        // qu'il examine.
        fn query_one_strong(
            &self,
            sql: &str,
            p: &[&dyn ToSqlValue],
        ) -> Result<Option<Vec<crate::db::backend::SqlValue>>, String> {
            if sql.contains("last_play_state = 'playing'") {
                return self.query_one(sql, p);
            }
            self.interne.query_one_strong(sql, p)
        }
        fn query_many_strong(
            &self,
            sql: &str,
            p: &[&dyn ToSqlValue],
        ) -> Result<Vec<Vec<crate::db::backend::SqlValue>>, String> {
            self.interne.query_many_strong(sql, p)
        }
    }

    /// LE défaut de #2495, mesuré. Le garde-fou ne s'exerçait qu'entre deux
    /// fichiers : une fois `measure_loudness_and_peak` lancée, la seule borne
    /// était le délai par piste — 180 s. Un décodage de 4,5 Go sur partage
    /// réseau tenait donc le chemin d'E/S bien après l'appui sur « Lecture »
    /// (6 782 ms pour démarrer, journal de Thierry Clemont).
    ///
    /// La contre-épreuve est DANS le test : le même travail est chronométré
    /// avec l'ancienne forme (un simple `timeout`) puis avec la nouvelle. Si le
    /// correctif est retiré, les deux durées se rejoignent et le test tombe.
    #[tokio::test]
    async fn l_analyse_cede_pendant_le_travail_pas_seulement_entre_deux_fichiers() {
        /// Tient lieu du décodage interminable. Court devant les 180 s réelles,
        /// long devant la veille (250 ms) : le rapport est ce qu'on mesure.
        const TRAVAIL_LONG: std::time::Duration = std::time::Duration::from_secs(3);

        let (_db, interne) = sweep_db(1);

        // AVANT — l'ancien site d'appel, tel quel : rien ne peut le rendre.
        let t0 = std::time::Instant::now();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(PER_TRACK_ANALYSIS_TIMEOUT_SECS),
            tokio::time::sleep(TRAVAIL_LONG),
        )
        .await;
        let avant = t0.elapsed();

        // APRÈS — la même attente, mise en course avec la lecture. La zone
        // passe à `playing` juste après la première vérification de la veille,
        // c'est-à-dire ALORS QUE le travail est déjà en cours.
        let backend = ZoneQuiDemarre::poser(interne, 1);
        let t0 = std::time::Instant::now();
        let issue = mesurer_en_cedant_a_la_lecture(
            &backend,
            delai_d_analyse(None),
            tokio::time::sleep(TRAVAIL_LONG),
        )
        .await;
        let apres = t0.elapsed();

        assert!(
            matches!(issue, Issue::CedeeALaLecture),
            "l'analyse devait être abandonnée au profit de la lecture (avant={avant:?}, après={apres:?})"
        );
        assert!(
            avant >= TRAVAIL_LONG,
            "la mesure « avant » ne prouve rien si elle n'a pas attendu : {avant:?}"
        );
        // Une tolérance large : ce qui est démontré est l'ordre de grandeur,
        // pas la milliseconde. La veille rend la main au premier réveil après
        // VEILLE_LECTURE_MS.
        assert!(
            apres < std::time::Duration::from_millis(1_000),
            "la lecture a attendu la fin du travail : avant={avant:?}, après={apres:?}"
        );
        assert!(
            apres * 3 < avant,
            "aucun effondrement du temps de démarrage : avant={avant:?}, après={apres:?}"
        );
    }

    /// L'autre moitié du contrat, et la plus dangereuse à rater : une piste
    /// ABANDONNÉE n'a pas été analysée. L'estampiller `rg_analyzed` la sortirait
    /// du balayage pour toujours — 114 pistes avaient déjà été gelées ainsi
    /// (#1865), et un simple appui sur « Lecture » suffirait à en geler d'autres.
    ///
    /// Contre-épreuve intégrée : le MÊME lot, sur le MÊME fichier, sans zone qui
    /// démarre — la piste y est bien estampillée. L'absence de témoin dans le
    /// premier cas vient donc de la cession, pas d'un fichier écarté pour une
    /// autre raison.
    #[tokio::test]
    async fn une_piste_cedee_n_est_pas_estampillee_analysee() {
        let (_tmp, _db, interne) = sweep_db_fichiers_presents(1);
        SettingsRepo::with_backend(interne.clone())
            .set(MODE_KEY, "track")
            .unwrap();

        // La zone démarre après le garde-fou d'entrée de fichier (1re lecture) :
        // la passe est donc engagée sur ce fichier quand la lecture arrive.
        let veilleur = Arc::new(ZoneQuiDemarre {
            interne: interne.clone(),
            lectures: std::sync::atomic::AtomicUsize::new(0),
            demarre_apres: 1,
        });
        let backend: Arc<dyn DbBackend> = veilleur.clone();

        assert_eq!(
            analyze_track_batch(&backend).await,
            0,
            "le lot a cédé sur son premier fichier : il n'a rien accompli"
        );
        assert!(
            veilleur.lectures() >= 2,
            "le test ne prouve rien si la veille n'a jamais interrogé les zones"
        );

        let meta = TrackMetadataRepo::with_backend(interne.clone());
        let temoins = meta.get_all(1).unwrap();
        assert!(
            !temoins.contains_key("rg_analyzed"),
            "une piste cédée doit rester CANDIDATE ; témoins = {temoins:?}"
        );
        assert!(
            !temoins.contains_key(PATH_UNRESOLVED_KEY),
            "le fichier répond : céder n'est pas le reporter ; témoins = {temoins:?}"
        );

        // Contre-épreuve : sans zone qui démarre, ce même fichier est traité.
        assert_eq!(
            analyze_track_batch(&interne).await,
            1,
            "la piste devait rester candidate et être reprise au lot suivant"
        );
        assert!(
            meta.get_all(1).unwrap().contains_key("rg_analyzed"),
            "sans cession, ce fichier est bel et bien estampillé — c'est ce qui \
             donne son sens à l'absence de témoin ci-dessus"
        );
    }

    /// La démonstration en vraie grandeur : un VRAI décodage, déjà en vol,
    /// abandonné en cours de route.
    ///
    /// Les deux tests précédents isolent chacun une moitié du contrat sur un
    /// travail simulé. Celui-ci ne simule rien : le fichier est un WAV assez
    /// long pour que `measure_loudness_and_peak` enchaîne plusieurs segments,
    /// et la zone se met à jouer APRÈS la première vérification de la veille —
    /// donc alors que le décodeur tourne déjà dans le pool bloquant. C'est
    /// exactement la situation de #2495, en petit.
    #[tokio::test]
    async fn un_decodage_reel_deja_en_vol_est_abandonne_en_cours_de_route() {
        let (tmp, _db, interne) = sweep_db_fichiers_presents(1);
        SettingsRepo::with_backend(interne.clone())
            .set(MODE_KEY, "track")
            .unwrap();
        // Un fichier réellement décodable, et assez long pour occuper le
        // décodeur plus d'un tour de veille.
        // 60 s de 96 kHz/24 bits : ~3,4 s d'analyse mesurees sur la machine de
        // compilation, soit plus de dix tours de veille de marge. Le test ne
        // depend donc pas de la vitesse de la machine — et s'il finissait quand
        // meme avant la cession, le premier `assert_eq!` le dirait.
        let wav = tmp.path().join("i2495.wav");
        ecrire_wav_long(&wav, 60);
        interne
            .execute(
                "UPDATE tracks SET file_path = ?, duration_ms = 60000, sample_rate = 96000 \
                 WHERE id = 1",
                &[&wav.to_string_lossy().to_string() as &dyn ToSqlValue],
            )
            .unwrap();

        // 2 lectures avant que ça joue : le garde-fou d'entrée de fichier, puis
        // la première vérification de la veille. La suivante n'arrive qu'un
        // tour de VEILLE_LECTURE_MS plus tard — le décodage est parti.
        let veilleur = Arc::new(ZoneQuiDemarre {
            interne: interne.clone(),
            lectures: std::sync::atomic::AtomicUsize::new(0),
            demarre_apres: 2,
        });
        let backend: Arc<dyn DbBackend> = veilleur.clone();

        let t0 = std::time::Instant::now();
        let traites = analyze_track_batch(&backend).await;
        let cession = t0.elapsed();

        assert_eq!(traites, 0, "le lot a cédé sur son unique fichier");
        assert!(
            cession >= std::time::Duration::from_millis(VEILLE_LECTURE_MS),
            "la cession est arrivée avant le premier tour de veille : le décodage              n'avait pas commencé, le test ne prouve pas ce qu'il annonce ({cession:?})"
        );
        let temoins = TrackMetadataRepo::with_backend(interne.clone())
            .get_all(1)
            .unwrap();
        assert!(
            !temoins.contains_key("rg_analyzed"),
            "décodage abandonné ⇒ piste toujours candidate ; témoins = {temoins:?}"
        );
        assert!(
            !temoins.contains_key("rg_track_gain"),
            "aucun gain ne peut sortir d'une analyse abandonnée ; témoins = {temoins:?}"
        );
    }

    /// #1627 — une analyse RÉELLE laisse derrière elle de quoi dire qu'elle a
    /// eu lieu.
    ///
    /// Le test ne pose aucun témoin lui-même : il fait décoder un vrai fichier
    /// par `analyze_track_batch`, puis relit la base par l'API publique
    /// [`stored_gain_source`]. Sans le `repo.set(TRACK_SOURCE_KEY, …)` de la
    /// passe, le gain mesuré serait indiscernable d'un tag rsgain et le chemin
    /// du signal afficherait « tags du fichier » sur une mesure Tune.
    #[tokio::test]
    async fn une_analyse_reelle_laisse_le_temoin_de_provenance() {
        let (tmp, _db, backend) = sweep_db_fichiers_presents(1);
        SettingsRepo::with_backend(backend.clone())
            .set(MODE_KEY, "track")
            .unwrap();
        let wav = tmp.path().join("i1627.wav");
        ecrire_wav_long(&wav, 2);
        backend
            .execute(
                "UPDATE tracks SET file_path = ?, duration_ms = 2000, sample_rate = 96000 \
                 WHERE id = 1",
                &[&wav.to_string_lossy().to_string() as &dyn ToSqlValue],
            )
            .unwrap();

        assert_eq!(
            analyze_track_batch(&backend).await,
            1,
            "le fichier devait être analysé pour de bon"
        );

        let temoins = TrackMetadataRepo::with_backend(backend.clone())
            .get_all(1)
            .unwrap();
        assert!(
            temoins.contains_key("rg_track_gain"),
            "sans gain mesuré, le test ne prouverait rien ; témoins = {temoins:?}"
        );
        // Le témoin EXPLICITE, et pas seulement la relecture : `rg_analyzed`
        // suffirait à faire passer l'assertion suivante par le repli, et le
        // test ne dirait alors plus rien de la ligne qu'il est censé garder.
        assert_eq!(
            temoins.get(TRACK_SOURCE_KEY).map(String::as_str),
            Some(SOURCE_ANALYSIS),
            "la passe doit estampiller la provenance ; témoins = {temoins:?}"
        );
        assert_eq!(
            stored_gain_source(&backend, 1, ReplayGainMode::Track),
            Some(GainSource::Analysis),
            "un gain mesuré ici doit se relire comme mesuré ici ; témoins = {temoins:?}"
        );
    }

    /// Un WAV 96 kHz / 24 bits / stéréo de `secondes` secondes — assez de
    /// matière pour que l'analyse enchaîne plusieurs segments de 30 s au lieu
    /// de rendre la main en une poignée de millisecondes.
    fn ecrire_wav_long(path: &std::path::Path, secondes: usize) {
        const HZ: usize = 96_000;
        let frames = HZ * secondes;
        let mut donnees = Vec::with_capacity(frames * 6);
        for frame in 0..frames {
            let g = ((frame as i32 % 200) - 100) * 40_000;
            for e in [g, g / 2] {
                donnees.extend_from_slice(&e.to_le_bytes()[..3]);
            }
        }
        let mut w = Vec::with_capacity(donnees.len() + 44);
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36u32 + donnees.len() as u32).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&2u16.to_le_bytes());
        w.extend_from_slice(&(HZ as u32).to_le_bytes());
        w.extend_from_slice(&((HZ * 6) as u32).to_le_bytes());
        w.extend_from_slice(&6u16.to_le_bytes());
        w.extend_from_slice(&24u16.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&(donnees.len() as u32).to_le_bytes());
        w.extend_from_slice(&donnees);
        std::fs::write(path, w).unwrap();
    }
}

/// Le pic tagué dans la mauvaise échelle éteignait le son (Refs #2157).
///
/// Les témoins passent par `playback_factor`, le SITE D'APPEL de production :
/// c'est lui qu'appellent `orchestrator/resolve_local.rs:1523` (sortie locale),
/// `orchestrator/transport.rs:1373` et `orchestrator/dsp.rs:1123`. Ils ne
/// supposent aucun périphérique audio — seulement une base et la décision de
/// gain.
#[cfg(test)]
mod garde_pic_hors_echelle {
    use super::*;
    use crate::db::sqlite::SqliteDb;

    /// Une piste, ReplayGain en mode piste, anti-écrêtage à son défaut d'usine.
    fn base_replaygain(gain_db: &str, pic: Option<&str>) -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        db.execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Ella Fitzgerald')",
            &[],
        )
        .unwrap();
        db.execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Clap Hands', 1)",
            &[],
        )
        .unwrap();
        db.execute(
            "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, \
             sample_rate, channels) VALUES (42, 'Autumn Leaves', 1, 1, '/x/42.flac', 300000, \
             44100, 2)",
            &[],
        )
        .unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db.clone());
        SettingsRepo::with_backend(backend.clone())
            .set(MODE_KEY, "track")
            .unwrap();
        let meta = TrackMetadataRepo::with_backend(backend.clone());
        meta.set(42, "rg_track_gain", gain_db).unwrap();
        if let Some(p) = pic {
            meta.set(42, "rg_track_peak", p).unwrap();
        }
        backend
    }

    #[test]
    fn sample_peak_headroom_tracks_metadata_and_playback() {
        let backend = base_replaygain("+6", Some("0.95"));
        let meta = TrackMetadataRepo::with_backend(backend.clone());
        let settings = SettingsRepo::with_backend(backend.clone());
        let expected = 10f64.powf(-3.0 / 20.0) / 0.95;
        let (raw, mode, kind) = stored_gain_with_peak(&backend, 42, ReplayGainMode::Album).unwrap();
        assert_eq!(raw.peak, Some(0.95));
        assert_eq!(mode, ReplayGainMode::Track);
        assert_eq!(kind, PeakKind::SamplePeak);
        assert!((playback_factor(&backend, 42) - expected).abs() < 1e-12);
        meta.set(42, "rg_track_true_peak", "1.2").unwrap();
        assert_eq!(
            stored_gain_with_peak(&backend, 42, ReplayGainMode::Track)
                .unwrap()
                .2,
            PeakKind::TruePeak
        );
        assert!((playback_factor(&backend, 42) - 1.0 / 1.2).abs() < 1e-12);
        // Invalid true peak falls back to the sample peak, retaining its reserve.
        meta.set(42, "rg_track_true_peak", "NaN").unwrap();
        assert!((playback_factor(&backend, 42) - expected).abs() < 1e-12);
        meta.set(42, "rg_album_gain", "+6").unwrap();
        meta.set(42, "rg_album_peak", "0.8").unwrap();
        settings.set(MODE_KEY, "album").unwrap();
        assert_eq!(
            stored_gain_with_peak(&backend, 42, ReplayGainMode::Album)
                .unwrap()
                .1,
            ReplayGainMode::Album
        );
        assert!((playback_factor(&backend, 42) - 10f64.powf(-3.0 / 20.0) / 0.8).abs() < 1e-12);
        meta.set(42, "rg_album_true_peak", "1.1").unwrap();
        assert!((playback_factor(&backend, 42) - 1.0 / 1.1).abs() < 1e-12);
        settings.set(PREVENT_CLIPPING_KEY, "false").unwrap();
        assert!((playback_factor(&backend, 42) - 10f64.powf(6.0 / 20.0)).abs() < 1e-12);
        settings.set(MODE_KEY, "off").unwrap();
        assert_eq!(playback_factor(&backend, 42), 1.0);
    }

    #[test]
    fn sample_peak_headroom_does_not_stack_on_sufficient_attenuation() {
        let backend = base_replaygain("-6", Some("1.0"));
        assert!((playback_factor(&backend, 42) - 10f64.powf(-6.0 / 20.0)).abs() < 1e-12);
    }

    /// LE défaut : `32768` (16 bits) et `8388607` (24 bits) sont des échelles
    /// d'échantillon, pas des amplitudes. `gain_factor` en tirait
    /// `plafond / pic`, tombait sur le plancher `0.001` du `clamp` final, et la
    /// piste sortait 60 dB trop bas — inaudible, sans message.
    #[test]
    fn un_pic_tague_en_echelle_d_echantillon_n_eteint_plus_la_piste() {
        for pic in ["32768", "32767.0", "8388607", "2147483647"] {
            let backend = base_replaygain("-6.0 dB", Some(pic));
            let f = playback_factor(&backend, 42);
            assert!(
                f > 0.001,
                "pic {pic} : facteur {f} — le plancher du clamp, soit -60 dB"
            );
            // Le pic est ignoré : il ne reste que le gain tagué, -6 dB.
            let attendu = 10f64.powf(-6.0 / 20.0);
            assert!(
                (f - attendu).abs() < 1e-9,
                "pic {pic} : facteur {f}, attendu {attendu} (le gain seul)"
            );
        }
    }

    /// Contre-épreuve, l'autre moitié : un pic PLAUSIBLE doit continuer de
    /// protéger. Un master écrêté à `1.1` avec un gain de +6 dB dépasserait la
    /// pleine échelle ; le pic d'échantillon garde désormais 3 dB de réserve.
    #[test]
    fn un_pic_plausible_protege_toujours_de_l_ecretage() {
        let backend = base_replaygain("+6.0 dB", Some("1.1"));
        let f = playback_factor(&backend, 42);
        let attendu = 10f64.powf(-3.0 / 20.0) / 1.1;
        assert!(
            (f - attendu).abs() < 1e-9,
            "facteur {f}, attendu {attendu} : l'anti-écrêtage ne tire plus"
        );
        assert!(
            f < 10f64.powf(6.0 / 20.0),
            "le gain brut est passé tel quel"
        );

        // Et la borne elle-même : `PEAK_MAX_PLAUSIBLE` est retenu, pas rejeté.
        let backend = base_replaygain("+12.0 dB", Some("4.0"));
        let f = playback_factor(&backend, 42);
        assert!(
            (f - 0.25 * 10f64.powf(-3.0 / 20.0)).abs() < 1e-9,
            "facteur {f} : le pic 4.0 doit encore protéger avec sa réserve"
        );
    }

    /// Un pic hors échelle ne doit pas se distinguer d'un pic ABSENT : c'est
    /// tout le sens du repli. Sans ce témoin, le premier test passerait au vert
    /// sur un `factor` mis à 1.0 en dur.
    #[test]
    fn un_pic_hors_echelle_vaut_exactement_un_pic_absent() {
        let sans = playback_factor(&base_replaygain("-6.0 dB", None), 42);
        let ferraille = playback_factor(&base_replaygain("-6.0 dB", Some("32768")), 42);
        assert!(
            (sans - ferraille).abs() < 1e-12,
            "sans pic {sans} != pic hors échelle {ferraille}"
        );
    }
}

/// #4254 — les pistes que la passe REPORTE (fichier qui ne répond pas) sont
/// comptées à part : ni « faites », ni « à faire ». Un report expiré ne
/// compte plus — il redevient un candidat.
#[cfg(test)]
mod tests_reportees_par_chemin_4254 {
    use std::sync::Arc;

    use crate::db::backend::{DbBackend, ToSqlValue};
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;
    use crate::library::local_path::{PATH_RETRY_AFTER_SECS, deferral_stamp};

    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        db.execute("INSERT INTO artists (id, name) VALUES (1, 'Bjork')", &[])
            .unwrap();
        db.execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Homogenic', 1)",
            &[],
        )
        .unwrap();
        for id in 1..=3 {
            db.execute(
                &format!(
                    "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, \
                     sample_rate, channels) VALUES ({id}, 'Piste {id}', 1, 1, \
                     '/media/music/absent-{id}.flac', 300000, 44100, 2)"
                ),
                &[],
            )
            .unwrap();
        }
        Arc::new(db)
    }

    fn reporter(backend: &Arc<dyn DbBackend>, track_id: i64, epoch: i64) {
        let date = deferral_stamp(epoch);
        backend
            .execute(
                "INSERT INTO track_metadata (track_id, key, value) VALUES (?, 'rg_path_unresolved', ?)",
                &[&track_id as &dyn ToSqlValue, &date as &dyn ToSqlValue],
            )
            .unwrap();
    }

    #[test]
    fn compte_les_reports_vivants_et_oublie_les_expires() {
        let backend = base();
        assert_eq!(super::compter_les_reportees_par_chemin(&backend), 0);
        let now = super::now_epoch_secs() as i64;
        reporter(&backend, 1, now);
        reporter(&backend, 2, now - 60);
        // Expiré d'une seconde : la passe le retentera, il n'est plus « reporté ».
        reporter(&backend, 3, now - PATH_RETRY_AFTER_SECS - 1);
        assert_eq!(
            super::compter_les_reportees_par_chemin(&backend),
            2,
            "deux reports vivants, un expiré"
        );
    }
}
