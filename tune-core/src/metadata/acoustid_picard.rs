//! D'après MetaRust, de Xavier Joly (code offert à Tune le 05/10/2026).
//!
//! AcoustID « à la Picard » (#4805, idée 4) : de l'empreinte d'une piste à
//! l'enregistrement MusicBrainz, puis de l'ensemble des pistes d'un album à
//! UNE release — ou à rien.
//!
//! # Ce qui vient de MetaRust
//!
//! `acoustid.rs::parse_lookup`, `acoustid.rs::pick_recording`,
//! `identify.rs::choose_release` et `identify.rs::vote_release_id`, adaptés
//! aux types de Tune :
//!
//! * on écarte les résultats de score inférieur à [`PLANCHER_DE_SCORE`] et
//!   ceux dont la durée s'écarte de plus de [`ECART_DE_DUREE_MAX_S`] ;
//! * on départage par titre, puis par artiste ;
//! * on exige [`MARGE_SUR_LE_SECOND`] d'avance sur le second ;
//! * au niveau de l'album, la release proposée par au moins la moitié des
//!   pistes l'emporte. Sinon, rien.
//!
//! # Ce que Tune y ajoute, et pourquoi
//!
//! * **Doublons d'enregistrement** : AcoustID rend souvent le même
//!   enregistrement sous deux résultats (deux empreintes soumises). Avant la
//!   marge, on ne garde que le meilleur score de chaque enregistrement :
//!   sinon un enregistrement se ferait concurrence à lui-même et la piste
//!   resterait « ambiguë » sans raison.
//! * **Vote déterministe** : MetaRust prend le maximum d'une `HashMap`, donc
//!   un ex aequo se tranche au hasard de l'ordre de hachage. Ici, un ex aequo
//!   se départage par le titre de l'album puis par le nombre de pistes ; s'il
//!   reste, il n'est accepté que si toutes les releases à égalité sont des
//!   pressages du MÊME groupe de sortie (même album, autre pays) — on prend
//!   alors le plus ancien. Sinon, rien n'est écrit.
//! * **Le statut `Official`** de `choose_release` n'est pas repris :
//!   AcoustID ne le rend pas dans `/v2/lookup`. Le nombre de pistes, qu'il
//!   rend, le remplace.
//! * **Refus et absence** sont séparés ([`RefusAcoustid`]), comme pour
//!   MusicBrainz (#4991) : une clé refusée arrête la passe, une panne compte
//!   au disjoncteur, une absence est un résultat.
//!
//! Le client HTTP, la clé et le débit sont ceux de Tune : la clé est un
//! réglage serveur, le débit est le limiteur partagé
//! [`crate::http::fetch::ACOUSTID`] (3 requêtes/s).

use std::collections::BTreeMap;

use serde_json::Value;

use super::musicbrainz_release::normalize;

/// Score AcoustID en dessous duquel un résultat est ignoré.
pub const PLANCHER_DE_SCORE: f64 = 0.5;
/// Écart maximal, en secondes, entre la durée du fichier et celle de
/// l'enregistrement (la tolérance de Picard).
pub const ECART_DE_DUREE_MAX_S: i64 = 30;
/// Avance minimale du meilleur enregistrement sur le second.
pub const MARGE_SUR_LE_SECOND: f64 = 0.05;

const ACOUSTID_API: &str = "https://api.acoustid.org/v2";

/// Ce que demande la passe : les enregistrements, leurs groupes de sortie et
/// leurs releases (avec le nombre de pistes), en réponse compressée.
/// Séparés par des espaces, comme les envoie Picard.
pub const META_LOOKUP: &str = "recordings releasegroups releases compress";

static BASE_REMPLACEE: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

/// **Tests seulement.** Fait pointer AcoustID vers une doublure locale.
#[doc(hidden)]
pub fn remplacer_la_base_acoustid(base: Option<String>) {
    if let Ok(mut b) = BASE_REMPLACEE.write() {
        *b = base;
    }
}

fn base_acoustid() -> String {
    BASE_REMPLACEE
        .read()
        .ok()
        .and_then(|b| b.clone())
        .unwrap_or_else(|| ACOUSTID_API.to_string())
}

/// Une release proposée par AcoustID pour un enregistrement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAcoustid {
    pub id: String,
    pub titre: String,
    pub release_group_id: Option<String>,
    pub nb_pistes: Option<u32>,
    /// `(année, mois, jour)`, les absents à 0 — pour prendre le plus ancien
    /// de deux pressages d'un même album.
    pub date: Option<(u32, u32, u32)>,
}

/// Un enregistrement MusicBrainz reconnu par AcoustID.
#[derive(Debug, Clone, PartialEq)]
pub struct EnregistrementAcoustid {
    /// L'identifiant AcoustID (de l'empreinte), pas celui de MusicBrainz.
    pub acoustid: String,
    pub score: f64,
    pub recording_id: String,
    pub titre: Option<String>,
    pub artiste: Option<String>,
    pub duree_s: Option<u32>,
    pub releases: Vec<ReleaseAcoustid>,
}

/// Pourquoi AcoustID n'a **pas répondu** — à ne pas confondre avec « aucun
/// résultat », qui est une réponse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefusAcoustid {
    /// Requête non partie ou réponse jamais arrivée.
    Transport,
    /// Statut HTTP sans corps exploitable, ou `429`/`503`.
    Statut(u16),
    /// La clé d'application est refusée. Inutile d'insister : la passe
    /// s'arrête au premier refus.
    CleRefusee,
    /// Réponse reçue, illisible.
    CorpsIllisible,
    /// AcoustID a répondu `status: error` pour une autre raison.
    Erreur(String),
}

impl std::fmt::Display for RefusAcoustid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport => write!(f, "transport"),
            Self::Statut(code) => write!(f, "statut_{code}"),
            Self::CleRefusee => write!(f, "cle_refusee"),
            Self::CorpsIllisible => write!(f, "corps_illisible"),
            Self::Erreur(m) => write!(f, "erreur: {m}"),
        }
    }
}

/// Le message d'erreur d'AcoustID dit-il « clé refusée » ?
pub fn est_une_cle_refusee(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("invalid api key") || m.contains("invalid client")
}

/// Lit une réponse `/v2/lookup` : la liste des enregistrements, ou le refus.
pub fn lire_reponse(json: &Value) -> Result<Vec<EnregistrementAcoustid>, RefusAcoustid> {
    if json.get("status").and_then(Value::as_str) == Some("ok") {
        return Ok(parse_lookup(json));
    }
    let message = json
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("erreur inconnue");
    if est_une_cle_refusee(message) {
        Err(RefusAcoustid::CleRefusee)
    } else {
        Err(RefusAcoustid::Erreur(message.to_string()))
    }
}

fn texte(v: &Value, cle: &str) -> Option<String> {
    v.get(cle)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn date_de(v: &Value) -> Option<(u32, u32, u32)> {
    let d = v.get("date")?;
    let part = |k: &str| d.get(k).and_then(Value::as_u64).unwrap_or(0) as u32;
    let annee = part("year");
    (annee > 0).then(|| (annee, part("month"), part("day")))
}

fn release_de(rel: &Value, groupe: Option<&Value>) -> Option<ReleaseAcoustid> {
    let id = texte(rel, "id")?;
    // En réponse compressée, une release qui porte le titre de son groupe ne
    // le répète pas.
    let titre = texte(rel, "title")
        .or_else(|| groupe.and_then(|g| texte(g, "title")))
        .unwrap_or_default();
    Some(ReleaseAcoustid {
        id,
        titre,
        release_group_id: groupe.and_then(|g| texte(g, "id")),
        nb_pistes: rel
            .get("track_count")
            .and_then(Value::as_u64)
            .map(|n| n as u32),
        date: date_de(rel),
    })
}

/// Les releases d'un enregistrement, sous les deux formes que rend AcoustID :
/// `recordings[].releases[]` (meta `releases`) et
/// `recordings[].releasegroups[].releases[]` (meta `releasegroups releases`).
fn releases_de(rec: &Value) -> Vec<ReleaseAcoustid> {
    let mut out: Vec<ReleaseAcoustid> = Vec::new();
    let mut pousser = |r: ReleaseAcoustid| {
        if !out.iter().any(|o| o.id == r.id) {
            out.push(r);
        }
    };
    for rel in rec
        .get("releases")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(r) = release_de(rel, None) {
            pousser(r);
        }
    }
    for groupe in rec
        .get("releasegroups")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for rel in groupe
            .get("releases")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(r) = release_de(rel, Some(groupe)) {
                pousser(r);
            }
        }
    }
    out
}

/// Les enregistrements d'une réponse `status: ok`. Un résultat sans
/// enregistrement (empreinte connue, jamais rattachée à MusicBrainz) ne rend
/// rien.
pub fn parse_lookup(json: &Value) -> Vec<EnregistrementAcoustid> {
    let mut out = Vec::new();
    for result in json
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let acoustid = texte(result, "id").unwrap_or_default();
        let score = result.get("score").and_then(Value::as_f64).unwrap_or(0.0);
        for rec in result
            .get("recordings")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(recording_id) = texte(rec, "id") else {
                continue;
            };
            let artiste = rec
                .get("artists")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|a| texte(a, "name"));
            out.push(EnregistrementAcoustid {
                acoustid: acoustid.clone(),
                score,
                recording_id,
                titre: texte(rec, "title"),
                artiste,
                duree_s: rec
                    .get("duration")
                    .and_then(Value::as_f64)
                    .map(|d| d.round() as u32),
                releases: releases_de(rec),
            });
        }
    }
    out
}

fn compact(s: &str) -> String {
    normalize(s)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

/// Restreint `pool` à ce qui satisfait `garde` : rend l'unique élément s'il
/// n'en reste qu'un, garde le sous-ensemble s'il n'est pas vide, sinon laisse
/// `pool` tel quel.
fn restreindre<'a, T>(pool: &mut Vec<&'a T>, garde: impl Fn(&T) -> bool) -> Option<&'a T> {
    let gardes: Vec<&'a T> = pool.iter().copied().filter(|x| garde(x)).collect();
    if gardes.len() == 1 {
        return Some(gardes[0]);
    }
    if !gardes.is_empty() {
        *pool = gardes;
    }
    None
}

/// Choisit l'enregistrement d'une piste comme Picard : plancher de score,
/// durée à ±30 s, titre puis artiste de la piste pour départager, et marge
/// sur le second. `None` = rien de sûr.
pub fn choisir_l_enregistrement<'a>(
    resultats: &'a [EnregistrementAcoustid],
    titre: Option<&str>,
    artiste: Option<&str>,
    duree_s: u32,
) -> Option<&'a EnregistrementAcoustid> {
    let mut pool: Vec<&EnregistrementAcoustid> = Vec::new();
    for r in resultats
        .iter()
        .filter(|r| r.score >= PLANCHER_DE_SCORE)
        .filter(|r| {
            r.duree_s
                .is_none_or(|d| (d as i64 - duree_s as i64).abs() <= ECART_DE_DUREE_MAX_S)
        })
    {
        // Tune : un enregistrement ne concourt qu'une fois, sous son meilleur
        // score.
        match pool.iter_mut().find(|p| p.recording_id == r.recording_id) {
            Some(p) if p.score < r.score => *p = r,
            Some(_) => {}
            None => pool.push(r),
        }
    }
    if pool.is_empty() {
        return None;
    }

    for (cle, voulu) in [("titre", titre), ("artiste", artiste)] {
        let Some(voulu) = voulu.map(normalize).filter(|v| !v.is_empty()) else {
            continue;
        };
        let champ = |r: &EnregistrementAcoustid| -> Option<String> {
            match cle {
                "titre" => r.titre.as_deref().map(normalize),
                _ => r.artiste.as_deref().map(normalize),
            }
        };
        if let Some(seul) = restreindre(&mut pool, |r| champ(r).as_deref() == Some(voulu.as_str()))
        {
            return Some(seul);
        }
    }

    pool.sort_by(|a, b| b.score.total_cmp(&a.score));
    match pool.as_slice() {
        [seul] => Some(*seul),
        [premier, second, ..] if premier.score - second.score >= MARGE_SUR_LE_SECOND => {
            Some(*premier)
        }
        _ => None,
    }
}

/// Les releases d'un enregistrement que l'album local rend plausibles : le
/// titre de l'album (forme compacte : « Mind State » = « Mindstate »), puis le
/// nombre de pistes de l'album local. Chaque critère ne restreint que s'il
/// garde au moins une release ; une seule restante est un choix.
pub fn releases_plausibles<'a>(
    releases: &'a [ReleaseAcoustid],
    album: Option<&str>,
    nb_pistes: usize,
) -> Vec<&'a ReleaseAcoustid> {
    let mut pool: Vec<&ReleaseAcoustid> = releases.iter().collect();
    if pool.len() <= 1 {
        return pool;
    }
    if let Some(voulu) = album.map(compact).filter(|v| !v.is_empty())
        && let Some(seule) = restreindre(&mut pool, |r| compact(&r.titre) == voulu)
    {
        return vec![seule];
    }
    if let Some(seule) = restreindre(&mut pool, |r| r.nb_pistes == Some(nb_pistes as u32)) {
        return vec![seule];
    }
    pool
}

/// Retient UNE release parmi celles d'un enregistrement, sinon `None`
/// (`choose_release` de MetaRust).
pub fn choisir_la_release<'a>(
    releases: &'a [ReleaseAcoustid],
    album: Option<&str>,
    nb_pistes: usize,
) -> Option<&'a ReleaseAcoustid> {
    match releases_plausibles(releases, album, nb_pistes).as_slice() {
        [seule] => Some(*seule),
        _ => None,
    }
}

/// Le vote d'album (le « cluster » de Picard) : chaque piste reconnue vote
/// pour ses [`releases_plausibles`] — une seule si elle en retient une. (MetaRust
/// fait voter une piste indécise pour TOUTES ses releases : une compilation
/// qui reprend le morceau gagnait alors autant de voix que l'album.) La
/// release qui réunit **au moins la moitié** des pistes de l'album l'emporte — le dénominateur compte TOUTES les pistes, reconnues ou
/// non. Sinon `None`.
pub fn voter_la_release(
    par_piste: &[Vec<ReleaseAcoustid>],
    album: Option<&str>,
    nb_pistes: usize,
) -> Option<ReleaseAcoustid> {
    let total = par_piste.len().max(nb_pistes);
    if total == 0 {
        return None;
    }
    // BTreeMap : l'ordre d'itération ne dépend pas du hachage.
    let mut voix: BTreeMap<&str, (usize, &ReleaseAcoustid)> = BTreeMap::new();
    for releases in par_piste {
        for r in releases_plausibles(releases, album, nb_pistes) {
            voix.entry(r.id.as_str()).or_insert((0, r)).0 += 1;
        }
    }
    let meilleur = voix.values().map(|(n, _)| *n).max()?;
    if meilleur * 2 < total {
        return None;
    }
    let tete: Vec<ReleaseAcoustid> = voix
        .values()
        .filter(|(n, _)| *n == meilleur)
        .map(|(_, r)| (*r).clone())
        .collect();
    if let [seule] = tete.as_slice() {
        return Some(seule.clone());
    }
    if let Some(r) = choisir_la_release(&tete, album, nb_pistes) {
        return Some(r.clone());
    }
    // Ex aequo restant : acceptable seulement entre pressages d'un MÊME
    // groupe de sortie. On prend le plus ancien, puis le plus petit id.
    let groupe = tete[0].release_group_id.as_deref()?;
    if tete
        .iter()
        .any(|r| r.release_group_id.as_deref() != Some(groupe))
    {
        return None;
    }
    tete.into_iter().min_by(|a, b| {
        let cle = |r: &ReleaseAcoustid| r.date.unwrap_or((u32::MAX, 0, 0));
        cle(a).cmp(&cle(b)).then_with(|| a.id.cmp(&b.id))
    })
}

/// Une piste locale soumise à la passe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PisteAIdentifier {
    pub track_id: i64,
    pub titre: String,
    pub artiste: Option<String>,
    pub duree_s: u32,
}

/// Ce que la passe conclut pour un album.
#[derive(Debug, Clone, PartialEq)]
pub enum DecisionAlbum {
    /// Une release réunit au moins la moitié des pistes.
    Retenue {
        release: ReleaseAcoustid,
        votes: usize,
        pistes: usize,
        /// `(track_id, recording_id)` des pistes dont l'enregistrement
        /// retenu figure sur cette release — et seulement elles.
        enregistrements: Vec<(i64, String)>,
    },
    /// Pas de majorité : rien ne sera écrit.
    SansMajorite {
        pistes_reconnues: usize,
        pistes: usize,
    },
}

/// La décision d'album, pure : choix de l'enregistrement piste par piste,
/// puis vote. `pistes` porte, pour chaque piste locale, la réponse AcoustID
/// lue (vide si l'empreinte n'a rien donné ou n'a pas pu être calculée).
pub fn decider_l_album(
    album: Option<&str>,
    pistes: &[(PisteAIdentifier, Vec<EnregistrementAcoustid>)],
) -> DecisionAlbum {
    let mut par_piste: Vec<Vec<ReleaseAcoustid>> = Vec::with_capacity(pistes.len());
    let mut choisis: Vec<(i64, &EnregistrementAcoustid)> = Vec::new();
    for (piste, resultats) in pistes {
        match choisir_l_enregistrement(
            resultats,
            Some(&piste.titre),
            piste.artiste.as_deref(),
            piste.duree_s,
        ) {
            Some(e) => {
                par_piste.push(e.releases.clone());
                choisis.push((piste.track_id, e));
            }
            None => par_piste.push(Vec::new()),
        }
    }
    let Some(release) = voter_la_release(&par_piste, album, pistes.len()) else {
        return DecisionAlbum::SansMajorite {
            pistes_reconnues: choisis.len(),
            pistes: pistes.len(),
        };
    };
    let enregistrements: Vec<(i64, String)> = choisis
        .iter()
        .filter(|(_, e)| e.releases.iter().any(|r| r.id == release.id))
        .map(|(id, e)| (*id, e.recording_id.clone()))
        .collect();
    DecisionAlbum::Retenue {
        votes: enregistrements.len(),
        pistes: pistes.len(),
        release,
        enregistrements,
    }
}

/// Complète l'appariement fichier ↔ piste de la release (`recordings`, tiré
/// de [`crate::metadata::reidentify::map_recording_ids`]) par ce que
/// l'empreinte a reconnu : une piste restée sans enregistrement reçoit celui
/// qu'AcoustID lui a trouvé, **si** cet enregistrement figure dans la liste
/// des pistes de la release et qu'aucune autre piste ne le porte déjà.
/// L'appariement par place et par titre garde la priorité : il ne se contredit
/// jamais ici, il se complète.
pub fn completer_par_l_empreinte(
    mut recordings: Vec<(i64, String)>,
    par_empreinte: &[(i64, String)],
    pistes_de_la_release: &[crate::metadata::musicbrainz_release::MBTrack],
) -> Vec<(i64, String)> {
    for (track_id, recording_id) in par_empreinte {
        let sur_la_release = pistes_de_la_release
            .iter()
            .any(|t| t.recording_id.as_deref() == Some(recording_id.as_str()));
        let piste_prise = recordings.iter().any(|(t, _)| t == track_id);
        let enregistrement_pris = recordings.iter().any(|(_, r)| r == recording_id);
        if sur_la_release && !piste_prise && !enregistrement_pris {
            recordings.push((*track_id, recording_id.clone()));
        }
    }
    recordings.sort_by_key(|(id, _)| *id);
    recordings
}

/// Interroge `/v2/lookup` pour UNE empreinte, au débit du limiteur partagé
/// [`crate::http::fetch::ACOUSTID`] (3 requêtes/s), qui lit aussi la réponse
/// (un `503` ralentit, `Retry-After` est respecté).
pub async fn interroger(
    cle: &str,
    empreinte: &str,
    duree_s: u32,
) -> Result<Vec<EnregistrementAcoustid>, RefusAcoustid> {
    use crate::http::fetch::{ACOUSTID, CLE_ACOUSTID};
    ACOUSTID.acquire(CLE_ACOUSTID).await;
    let duree = duree_s.to_string();
    let reponse = crate::http::client::shared()
        .post(format!("{}/lookup", base_acoustid()))
        .form(&[
            ("client", cle),
            ("format", "json"),
            ("meta", META_LOOKUP),
            ("duration", duree.as_str()),
            ("fingerprint", empreinte),
        ])
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .inspect(|r| ACOUSTID.constater_reponse(CLE_ACOUSTID, r))
        .map_err(|_| RefusAcoustid::Transport)?;
    let statut = reponse.status().as_u16();
    if statut == 429 || statut == 503 {
        return Err(RefusAcoustid::Statut(statut));
    }
    // Une clé refusée arrive en `400` AVEC un corps JSON : on lit le corps
    // avant de juger le statut.
    match reponse.json::<Value>().await {
        Ok(json) => lire_reponse(&json),
        Err(_) if !(200..300).contains(&statut) => Err(RefusAcoustid::Statut(statut)),
        Err(_) => Err(RefusAcoustid::CorpsIllisible),
    }
}

#[cfg(test)]
#[path = "acoustid_picard_tests.rs"]
mod tests;
