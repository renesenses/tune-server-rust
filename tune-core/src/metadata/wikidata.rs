//! D'après MetaRust, de Xavier Joly (code offert à Tune le 05/10/2026).
//!
//! Wikidata pour l'enrichissement : les INSTRUMENTS d'un musicien (P1303,
//! « instrument joué ») et le TEMPO d'un enregistrement (P1725, « battements
//! par minute »).
//!
//! # Repris, réécrit
//!
//! * **Repris** de MetaRust (`wikidata.rs`) : les extracteurs purs
//!   [`extract_instrument_qids`], [`extract_labels`] (français puis anglais) et
//!   [`extract_bpm`] / [`parse_bpm_amount`]. Ils lisent la forme `entities.{id}`
//!   commune à `Special:EntityData` et à `wbgetentities`.
//! * **Réécrit** : le client. MetaRust tenait son propre `reqwest::Client` et
//!   son limiteur par instance. Ici, le transport est celui de Tune (le client
//!   HTTP partagé du serveur), sous le limiteur PARTAGÉ
//!   [`crate::http::fetch::WIKIDATA`]. L'entité se lit par
//!   `Special:EntityData/{qid}.json`, comme `artwork.rs` et `bio_batch.rs` ;
//!   les libellés des instruments, en UNE requête `wbgetentities&props=labels`
//!   (une entité d'instrument complète pèse plusieurs centaines de Ko).
//! * **Nouveau** : le QID se trouve par l'identifiant MusicBrainz que Wikidata
//!   porte lui-même (`haswbstatement:P434=<mbid>` pour un artiste, P4404 pour
//!   un enregistrement, P435 pour une œuvre). Aucune requête MusicBrainz n'est
//!   consommée : le budget d'une requête par seconde reste aux passes
//!   d'identification et de crédits.
//!
//! # La réserve de qualité, écrite par Xavier lui-même
//!
//! « Les instruments sont ceux de la fiche artiste, pas la confirmation qu'ils
//! ont été joués sur CET enregistrement. » D'où la règle de Tune
//! (`credits_release.rs` : « un crédit posé sur la mauvaise piste… pire que la
//! section absente ») : les instruments Wikidata sont gardés au niveau de
//! l'ARTISTE (`artiste_instruments`), **jamais** écrits dans `track_credits`.
//! La fiche des crédits les montre à côté du nom, marqués « d'après Wikidata »,
//! et seulement pour un crédit de musicien que MusicBrainz a laissé sans
//! instrument — un instrument fourni par MusicBrainz n'est jamais remplacé.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, info};

use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::engine::Engine;

const INSTRUMENT_PROPERTY: &str = "P1303";
const BPM_PROPERTY: &str = "P1725";

/// Propriétés Wikidata qui portent un identifiant MusicBrainz.
pub const P_MBID_ARTISTE: &str = "P434";
pub const P_MBID_ENREGISTREMENT: &str = "P4404";
pub const P_MBID_OEUVRE: &str = "P435";

/// Une réponse Wikidata gardée ne se redemande pas avant ce délai : une fiche
/// d'artiste change peu, et un vide (« P1303 non renseigné ») se garde aussi.
pub const VALIDITE_JOURS: i64 = 180;

// ── Extracteurs purs (repris de MetaRust) ───────────────────────────────────

/// Les QID des instruments (P1303) d'une entité. Vide si non renseigné.
pub fn extract_instrument_qids(entity_json: &Value, qid: &str) -> Vec<String> {
    entity_json
        .get("entities")
        .and_then(|e| e.get(qid))
        .and_then(|e| e.get("claims"))
        .and_then(|c| c.get(INSTRUMENT_PROPERTY))
        .and_then(|claims| claims.as_array())
        .into_iter()
        .flatten()
        .filter_map(|claim| {
            claim
                .get("mainsnak")
                .and_then(|s| s.get("datavalue"))
                .and_then(|d| d.get("value"))
                .and_then(|v| v.get("id"))
                .and_then(|id| id.as_str())
                .map(|s| s.to_string())
        })
        .collect()
}

/// Libellés FR puis EN, dans l'ordre de `ids`. Un libellé manquant est omis.
pub fn extract_labels(labels_json: &Value, ids: &[String]) -> Vec<String> {
    extract_libelles(labels_json, ids)
        .into_iter()
        .filter_map(|l| l.fr.or(l.en))
        .collect()
}

/// [`extract_labels`] sans choisir la langue : l'interface choisit selon la
/// sienne.
pub fn extract_libelles(labels_json: &Value, ids: &[String]) -> Vec<Instrument> {
    let entities = labels_json.get("entities");
    ids.iter()
        .filter_map(|id| {
            let labels = entities?.get(id)?.get("labels")?;
            let lire = |lang: &str| {
                labels
                    .get(lang)
                    .and_then(|l| l.get("value"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            };
            let (fr, en) = (lire("fr"), lire("en"));
            if fr.is_none() && en.is_none() {
                return None;
            }
            let canon = en
                .as_deref()
                .or(fr.as_deref())
                .map(super::instruments::canoniser_instrument);
            Some(Instrument {
                qid: id.clone(),
                fr,
                en,
                canon,
            })
        })
        .collect()
}

/// Un BPM Wikidata (`"+105.00"`) en nombre, ou rien s'il est hors de ]0, 400].
pub fn parse_bpm_amount(raw: &str) -> Option<f64> {
    let s = raw.trim().trim_start_matches('+');
    let n: f64 = s.parse().ok()?;
    if !(n > 0.0 && n <= 400.0) {
        return None;
    }
    Some(n)
}

/// Le BPM (P1725) d'une entité.
pub fn extract_bpm(entity_json: &Value, qid: &str) -> Option<f64> {
    entity_json
        .get("entities")
        .and_then(|e| e.get(qid))
        .and_then(|e| e.get("claims"))
        .and_then(|c| c.get(BPM_PROPERTY))
        .and_then(|claims| claims.as_array())
        .into_iter()
        .flatten()
        .find_map(|claim| {
            claim
                .get("mainsnak")
                .and_then(|s| s.get("datavalue"))
                .and_then(|d| d.get("value"))
                .and_then(|v| v.get("amount"))
                .and_then(|a| a.as_str())
                .and_then(parse_bpm_amount)
        })
}

/// Le QID d'une recherche `haswbstatement` : seulement s'il est UNIQUE. Deux
/// entités pour le même MBID (doublon Wikidata) : on ne choisit pas.
pub fn qid_unique_de_la_recherche(json: &Value) -> Option<String> {
    let hits = json.pointer("/query/search")?.as_array()?;
    if hits.len() != 1 {
        return None;
    }
    let t = hits[0].get("title")?.as_str()?;
    (t.starts_with('Q') && t[1..].chars().all(|c| c.is_ascii_digit())).then(|| t.to_string())
}

fn mbid_valide(mbid: &str) -> bool {
    let m = mbid.trim();
    m.len() == 36 && m.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

// ── URLs ────────────────────────────────────────────────────────────────────

pub fn url_entite(qid: &str) -> String {
    format!("https://www.wikidata.org/wiki/Special:EntityData/{qid}.json")
}

pub fn url_recherche_par_mbid(propriete: &str, mbid: &str) -> String {
    format!(
        "https://www.wikidata.org/w/api.php?action=query&list=search&format=json&srlimit=2\
         &srsearch=haswbstatement:{propriete}={}",
        mbid.trim()
    )
}

pub fn url_libelles(ids: &[String]) -> String {
    format!(
        "https://www.wikidata.org/w/api.php?action=wbgetentities&props=labels&languages=fr|en\
         &format=json&ids={}",
        ids.join("|")
    )
}

/// Le transport réel : le client HTTP PARTAGÉ du serveur, sous le limiteur
/// Wikidata partagé. `None` = pas de réponse exploitable (panne, statut non
/// 2xx, corps illisible) — jamais confondu avec « Wikidata n'a rien ».
pub async fn lire_json(client: &reqwest::Client, url: String) -> Option<Value> {
    use crate::http::fetch::{CLE_WIKIDATA, WIKIDATA};
    WIKIDATA.acquire(CLE_WIKIDATA).await;
    let r = client
        .get(&url)
        .header(
            reqwest::header::USER_AGENT,
            super::musicbrainz_release::MB_UA,
        )
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .ok()?;
    WIKIDATA.constater_reponse(CLE_WIKIDATA, &r);
    if !r.status().is_success() {
        return None;
    }
    r.json().await.ok()
}

// ── Instruments d'un artiste ────────────────────────────────────────────────

/// Un instrument joué, d'après la fiche Wikidata de l'artiste.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instrument {
    pub qid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub en: Option<String>,
    /// Le canon de Tune (`instruments::canoniser_instrument`), le même que
    /// les crédits MusicBrainz et les Smart Collections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canon: Option<String>,
}

/// Ce que Wikidata dit d'un artiste. `qid: None` = aucune entité Wikidata ne
/// porte ce MBID (le vide se garde aussi).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FicheInstruments {
    pub qid: Option<String>,
    pub instruments: Vec<Instrument>,
}

/// Les instruments d'un artiste par son MBID. `get(url)` est le transport
/// (couture des essais, qui rejouent des réponses enregistrées). `None` =
/// panne : rien ne se garde, la passe suivante réessaiera.
pub async fn instruments_de_l_artiste_par<F, Fut>(mbid: &str, get: &F) -> Option<FicheInstruments>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Option<Value>>,
{
    if !mbid_valide(mbid) {
        return Some(FicheInstruments::default());
    }
    let recherche = get(url_recherche_par_mbid(P_MBID_ARTISTE, mbid)).await?;
    let Some(qid) = qid_unique_de_la_recherche(&recherche) else {
        return Some(FicheInstruments::default());
    };
    let entite = get(url_entite(&qid)).await?;
    let ids = extract_instrument_qids(&entite, &qid);
    if ids.is_empty() {
        return Some(FicheInstruments {
            qid: Some(qid),
            instruments: Vec::new(),
        });
    }
    let libelles = get(url_libelles(&ids)).await?;
    Some(FicheInstruments {
        qid: Some(qid),
        instruments: extract_libelles(&libelles, &ids),
    })
}

/// Garde la fiche d'un artiste.
pub fn garder_instruments(backend: &Arc<dyn DbBackend>, mbid: &str, fiche: &FicheInstruments) {
    let json = serde_json::to_string(&fiche.instruments).unwrap_or_else(|_| "[]".into());
    let quand = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let qid = fiche.qid.clone();
    if let Err(e) = backend.execute(
        "INSERT INTO artiste_instruments (artist_mbid, qid, instruments, lu_le) \
         VALUES (?, ?, ?, ?) ON CONFLICT (artist_mbid) DO UPDATE SET \
         qid = excluded.qid, instruments = excluded.instruments, lu_le = excluded.lu_le",
        &[
            &mbid.trim() as &dyn ToSqlValue,
            &qid as &dyn ToSqlValue,
            &json as &dyn ToSqlValue,
            &quand as &dyn ToSqlValue,
        ],
    ) {
        tracing::warn!(erreur = %e, "artiste_instruments_ecriture_impossible");
    }
}

/// Les instruments gardés, par MBID d'artiste.
pub fn instruments_gardes(
    backend: &Arc<dyn DbBackend>,
    mbids: &[String],
) -> HashMap<String, Vec<Instrument>> {
    let mut out = HashMap::new();
    for m in mbids {
        if out.contains_key(m) {
            continue;
        }
        let Some(r) = backend
            .query_one(
                "SELECT instruments FROM artiste_instruments WHERE artist_mbid = ?",
                &[&m.as_str() as &dyn ToSqlValue],
            )
            .ok()
            .flatten()
        else {
            continue;
        };
        let liste: Vec<Instrument> = r
            .first()
            .and_then(|v| v.as_string())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        out.insert(m.clone(), liste);
    }
    out
}

/// Rôles de MUSICIEN dont l'instrument peut manquer : la relation
/// `instrument`/`performer` sans attribut (`credits_mb` les range sous
/// `performer`), et l'interprète en tête (`artist`). La voix (`vocal`) est
/// déjà son propre instrument.
pub const ROLES_SANS_INSTRUMENT: &[&str] = &["performer", "artist"];

/// Les artistes à interroger : crédités comme musiciens SANS instrument,
/// pourvus d'un MBID, et absents (ou périmés) dans `artiste_instruments`.
/// `album_id` restreint à un disque.
pub fn artistes_a_interroger(
    backend: &Arc<dyn DbBackend>,
    album_id: Option<i64>,
    limite: usize,
) -> Vec<String> {
    let limite_date = (chrono::Utc::now() - chrono::Duration::days(VALIDITE_JOURS))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let jointure = match backend.engine() {
        Engine::Postgres => "CAST(tc.track_id AS TEXT) = CAST(t.id AS TEXT)",
        Engine::Sqlite => "tc.track_id = t.id",
    };
    let filtre_album = match (album_id, backend.engine()) {
        (None, _) => String::new(),
        (Some(_), Engine::Postgres) => " AND CAST(t.album_id AS TEXT) = ?".to_string(),
        (Some(_), Engine::Sqlite) => " AND t.album_id = ?".to_string(),
    };
    let sql = format!(
        "SELECT DISTINCT tc.artist_mbid FROM track_credits tc JOIN tracks t ON {jointure} \
         WHERE tc.role IN ('performer', 'artist') \
         AND (tc.instrument IS NULL OR tc.instrument = '') \
         AND tc.artist_mbid IS NOT NULL AND tc.artist_mbid <> ''{filtre_album} \
         AND NOT EXISTS (SELECT 1 FROM artiste_instruments ai \
             WHERE ai.artist_mbid = tc.artist_mbid AND ai.lu_le >= ?) \
         ORDER BY tc.artist_mbid"
    );
    let album_txt = album_id.map(|a| a.to_string());
    let lignes = match &album_txt {
        Some(a) => {
            let a_lier: &dyn ToSqlValue = match backend.engine() {
                Engine::Postgres => a,
                Engine::Sqlite => album_id.as_ref().unwrap(),
            };
            backend.query_many(&sql, &[a_lier, &limite_date as &dyn ToSqlValue])
        }
        None => backend.query_many(&sql, &[&limite_date as &dyn ToSqlValue]),
    };
    lignes
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| r.first().and_then(|v| v.as_string()))
        .filter(|m| mbid_valide(m))
        .take(limite)
        .collect()
}

/// Bilan de la sous-passe des instruments.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct BilanInstruments {
    pub interroges: usize,
    /// Artistes dont la fiche Wikidata porte au moins un instrument.
    pub avec_instruments: usize,
    pub sans_fiche: usize,
    pub pannes: usize,
}

/// La sous-passe des instruments (étape E de #4805) : pour chaque artiste à
/// interroger, la fiche Wikidata, gardée en base. Transport en paramètre.
pub async fn completer_instruments_par<F, Fut>(
    backend: &Arc<dyn DbBackend>,
    album_id: Option<i64>,
    limite: usize,
    get: F,
) -> BilanInstruments
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Option<Value>>,
{
    let mut bilan = BilanInstruments::default();
    for mbid in artistes_a_interroger(backend, album_id, limite) {
        crate::taches_de_fond::attendre_la_reprise(crate::taches_de_fond::Tache::Enrichissement)
            .await;
        bilan.interroges += 1;
        match instruments_de_l_artiste_par(&mbid, &get).await {
            Some(fiche) => {
                if fiche.qid.is_none() {
                    bilan.sans_fiche += 1;
                } else if !fiche.instruments.is_empty() {
                    bilan.avec_instruments += 1;
                }
                garder_instruments(backend, &mbid, &fiche);
            }
            None => {
                bilan.pannes += 1;
                debug!(artist_mbid = %mbid, "wikidata_instruments_panne");
            }
        }
    }
    if bilan.interroges > 0 {
        info!(?bilan, "wikidata_instruments_passe");
    }
    bilan
}

// ── BPM d'un enregistrement ─────────────────────────────────────────────────

/// Le BPM Wikidata d'un enregistrement : l'entité qui porte son MBID
/// (P4404), sinon celle de l'œuvre (P435). `None` = rien de connu ou panne.
pub async fn bpm_de_l_enregistrement_par<F, Fut>(
    recording_mbid: &str,
    work_mbids: &[String],
    get: &F,
) -> Option<f64>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Option<Value>>,
{
    let mut cibles: Vec<(&str, &str)> = Vec::new();
    if mbid_valide(recording_mbid) {
        cibles.push((P_MBID_ENREGISTREMENT, recording_mbid));
    }
    for w in work_mbids.iter().filter(|w| mbid_valide(w)) {
        cibles.push((P_MBID_OEUVRE, w));
    }
    for (prop, mbid) in cibles {
        let Some(recherche) = get(url_recherche_par_mbid(prop, mbid)).await else {
            continue;
        };
        let Some(qid) = qid_unique_de_la_recherche(&recherche) else {
            continue;
        };
        if let Some(entite) = get(url_entite(&qid)).await
            && let Some(bpm) = extract_bpm(&entite, &qid)
        {
            return Some(bpm);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const MILES: &str = "561d854a-6a28-4aa7-8c99-323e6ce46c2a";

    fn fixture(nom: &str) -> Value {
        let chemin = format!(
            "{}/tests/fixtures/wikidata/{nom}",
            env!("CARGO_MANIFEST_DIR")
        );
        serde_json::from_str(&std::fs::read_to_string(chemin).unwrap()).unwrap()
    }

    /// Le transport rejoue les réponses RÉELLES enregistrées (12/10/2026) ;
    /// toute autre URL est une panne — et donc un échec visible du test.
    fn transport_enregistre(url: String) -> std::future::Ready<Option<Value>> {
        let v = if url.contains("haswbstatement:P434=561d854a") {
            Some(fixture("recherche_P434_miles_davis.json"))
        } else if url.ends_with("Special:EntityData/Q93341.json") {
            Some(fixture("entite_miles_davis_Q93341.json"))
        } else if url.contains("wbgetentities") && url.contains("Q8338") {
            Some(fixture("libelles_instruments_miles_davis.json"))
        } else if url.contains("haswbstatement:P4404=1a13c710") {
            Some(serde_json::json!({"query": {"search": [{"title": "Q57828843"}]}}))
        } else if url.ends_with("Special:EntityData/Q57828843.json") {
            Some(fixture("entite_rolling_in_the_deep_Q57828843.json"))
        } else {
            None
        };
        std::future::ready(v)
    }

    #[test]
    fn les_instruments_sont_extraits_d_un_json_wikidata_reel() {
        let entite = fixture("entite_miles_davis_Q93341.json");
        let ids = extract_instrument_qids(&entite, "Q93341");
        assert_eq!(ids, vec!["Q8338", "Q163829", "Q1444", "Q382616"]);
        let libelles = fixture("libelles_instruments_miles_davis.json");
        assert_eq!(
            extract_labels(&libelles, &ids),
            vec!["trompette", "synthétiseur", "orgue", "bugle"],
            "le français d'abord, comme MetaRust"
        );
        let detail = extract_libelles(&libelles, &ids);
        assert_eq!(detail[0].en.as_deref(), Some("trumpet"));
        assert_eq!(detail[2].canon.as_deref(), Some("organ"));
    }

    #[test]
    fn le_qid_se_trouve_par_le_mbid_et_seulement_s_il_est_unique() {
        let r = fixture("recherche_P434_miles_davis.json");
        assert_eq!(qid_unique_de_la_recherche(&r).as_deref(), Some("Q93341"));
        let deux = serde_json::json!({"query": {"search": [{"title": "Q1"}, {"title": "Q2"}]}});
        assert!(qid_unique_de_la_recherche(&deux).is_none());
        let rien = serde_json::json!({"query": {"search": []}});
        assert!(qid_unique_de_la_recherche(&rien).is_none());
    }

    #[tokio::test]
    async fn la_fiche_d_un_artiste_se_lit_sans_reseau() {
        let fiche = instruments_de_l_artiste_par(MILES, &transport_enregistre)
            .await
            .unwrap();
        assert_eq!(fiche.qid.as_deref(), Some("Q93341"));
        assert_eq!(fiche.instruments.len(), 4);
        // Un MBID inconnu de Wikidata : une PANNE ici (transport muet), donc
        // rien à garder.
        assert!(
            instruments_de_l_artiste_par(
                "00000000-0000-0000-0000-000000000000",
                &transport_enregistre
            )
            .await
            .is_none()
        );
    }

    #[test]
    fn un_bpm_wikidata_reel_se_lit() {
        let e = fixture("entite_rolling_in_the_deep_Q57828843.json");
        assert_eq!(extract_bpm(&e, "Q57828843"), Some(105.0));
        assert_eq!(parse_bpm_amount("+0"), None);
        assert_eq!(parse_bpm_amount("+401"), None);
    }

    #[tokio::test]
    async fn le_bpm_d_un_enregistrement_passe_par_son_mbid() {
        let bpm = bpm_de_l_enregistrement_par(
            "1a13c710-4b7e-4701-8968-cd61f2e58110",
            &[],
            &transport_enregistre,
        )
        .await;
        assert_eq!(bpm, Some(105.0));
    }

    fn base() -> Arc<dyn DbBackend> {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    /// La sous-passe ne touche JAMAIS `track_credits` : l'instrument Wikidata
    /// est gardé au niveau de l'artiste, et un instrument fourni par
    /// MusicBrainz exclut l'artiste de la passe.
    #[tokio::test]
    async fn la_passe_garde_au_niveau_de_l_artiste_sans_toucher_aux_credits() {
        let b = base();
        b.execute(
            "INSERT INTO albums (id, title) VALUES (1, 'Kind of Blue')",
            &[],
        )
        .unwrap();
        b.execute(
            "INSERT INTO tracks (id, title, album_id, file_path) VALUES (10, 'So What', 1, '/m/1.flac')",
            &[],
        )
        .unwrap();
        b.execute(
            "INSERT INTO track_credits (track_id, artist_name, role, instrument, artist_mbid) \
             VALUES (10, 'Miles Davis', 'performer', NULL, ?), \
                    (10, 'Bill Evans', 'performer', 'piano', 'f3c1a6f4-7f0a-4b8f-9a77-8a0b1f2e3d4c')",
            &[&MILES as &dyn ToSqlValue],
        )
        .unwrap();
        let bilan = completer_instruments_par(&b, Some(1), 50, transport_enregistre).await;
        assert_eq!(
            bilan.interroges, 1,
            "Bill Evans a déjà son instrument : {bilan:?}"
        );
        assert_eq!(bilan.avec_instruments, 1);
        let gardes = instruments_gardes(&b, &[MILES.to_string()]);
        assert_eq!(gardes[MILES].len(), 4);
        // track_credits inchangé.
        let r = b
            .query_one(
                "SELECT instrument FROM track_credits WHERE artist_name = 'Miles Davis'",
                &[],
            )
            .unwrap()
            .unwrap();
        assert!(r.first().and_then(|v| v.as_string()).is_none());
        // Déjà gardé : la passe suivante ne réinterroge pas.
        let encore = completer_instruments_par(&b, Some(1), 50, transport_enregistre).await;
        assert_eq!(encore.interroges, 0);
    }
}
