//! Témoins de [`super`] sur des réponses AcoustID enregistrées.
//!
//! Les fixtures de `tests/fixtures/acoustid/` sont **fabriquées** : albums,
//! artistes, titres et identifiants inventés, dans la forme exacte de
//! `/v2/lookup` avec `meta=recordings releasegroups releases compress`
//! (enregistrements → groupes de sortie → releases, titre de release omis
//! quand il est celui du groupe). Aucune donnée réelle, aucune donnée de
//! MetaRust.

use std::collections::BTreeMap;

use serde_json::Value;

use super::*;

/// `(nom, contenu)` de chaque album du banc.
const BANC: &[(&str, &str)] = &[
    (
        "propre",
        include_str!("../../tests/fixtures/acoustid/propre.json"),
    ),
    (
        "ambigu",
        include_str!("../../tests/fixtures/acoustid/ambigu.json"),
    ),
    (
        "duree",
        include_str!("../../tests/fixtures/acoustid/duree.json"),
    ),
    (
        "compilation",
        include_str!("../../tests/fixtures/acoustid/compilation.json"),
    ),
    (
        "moitie",
        include_str!("../../tests/fixtures/acoustid/moitie.json"),
    ),
    (
        "minorite",
        include_str!("../../tests/fixtures/acoustid/minorite.json"),
    ),
    (
        "deux_albums",
        include_str!("../../tests/fixtures/acoustid/deux_albums.json"),
    ),
    (
        "doublon",
        include_str!("../../tests/fixtures/acoustid/doublon.json"),
    ),
];

struct Album {
    titre: String,
    pistes: Vec<(PisteAIdentifier, Vec<EnregistrementAcoustid>)>,
    attendu_release: Option<String>,
    attendu_groupe: Option<String>,
    attendu_enregistrements: BTreeMap<i64, String>,
    /// Le bon enregistrement de chaque piste (`None` : AcoustID n'en a pas).
    verite: BTreeMap<i64, Option<String>>,
}

fn charger(contenu: &str) -> Album {
    let v: Value = serde_json::from_str(contenu).unwrap();
    let pistes = v["pistes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            let piste = PisteAIdentifier {
                track_id: p["track_id"].as_i64().unwrap(),
                titre: p["titre"].as_str().unwrap().to_string(),
                artiste: p["artiste"].as_str().map(str::to_string),
                duree_s: p["duree_s"].as_u64().unwrap() as u32,
            };
            (piste, lire_reponse(&p["reponse"]).unwrap())
        })
        .collect();
    let carte = |o: &Value| -> BTreeMap<i64, Option<String>> {
        o.as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.parse().unwrap(), v.as_str().map(str::to_string)))
            .collect()
    };
    Album {
        titre: v["album"]["titre"].as_str().unwrap().to_string(),
        pistes,
        attendu_release: v["attendu"]["release_id"].as_str().map(str::to_string),
        attendu_groupe: v["attendu"]["release_group_id"]
            .as_str()
            .map(str::to_string),
        attendu_enregistrements: carte(&v["attendu"]["enregistrements"])
            .into_iter()
            .map(|(k, v)| (k, v.unwrap()))
            .collect(),
        verite: carte(&v["verite"]),
    }
}

fn album(nom: &str) -> Album {
    charger(BANC.iter().find(|(n, _)| *n == nom).unwrap().1)
}

fn decision(a: &Album) -> DecisionAlbum {
    decider_l_album(Some(&a.titre), &a.pistes)
}

/// Chaque album du banc rend exactement la décision attendue : la release,
/// son groupe, et les seuls enregistrements qui y figurent.
#[test]
fn chaque_album_du_banc_rend_la_decision_attendue() {
    for (nom, contenu) in BANC {
        let a = charger(contenu);
        match (decision(&a), &a.attendu_release) {
            (
                DecisionAlbum::Retenue {
                    release,
                    enregistrements,
                    ..
                },
                Some(attendue),
            ) => {
                assert_eq!(&release.id, attendue, "{nom} : mauvaise release");
                assert_eq!(
                    release.release_group_id, a.attendu_groupe,
                    "{nom} : mauvais groupe"
                );
                let obtenus: BTreeMap<i64, String> = enregistrements.into_iter().collect();
                assert_eq!(obtenus, a.attendu_enregistrements, "{nom}");
            }
            (DecisionAlbum::SansMajorite { .. }, None) => {}
            (d, attendu) => panic!("{nom} : attendu {attendu:?}, obtenu {d:?}"),
        }
    }
}

#[test]
fn un_score_sous_le_plancher_est_ecarte() {
    let a = album("ambigu");
    let (piste, hits) = &a.pistes[1];
    assert!(hits[0].score < PLANCHER_DE_SCORE);
    assert!(
        choisir_l_enregistrement(hits, Some(&piste.titre), None, piste.duree_s).is_none(),
        "0,42 < 0,5 : la piste ne doit rien retenir"
    );
}

#[test]
fn deux_candidats_trop_proches_ne_departagent_rien() {
    let a = album("ambigu");
    let (piste, hits) = &a.pistes[0];
    assert!((hits[0].score - hits[1].score) < MARGE_SUR_LE_SECOND);
    assert!(choisir_l_enregistrement(hits, Some(&piste.titre), None, piste.duree_s).is_none());
}

#[test]
fn une_duree_trop_eloignee_ecarte_le_meilleur_score() {
    let a = album("duree");
    let (piste, hits) = &a.pistes[0];
    let pris = choisir_l_enregistrement(hits, Some(&piste.titre), None, piste.duree_s).unwrap();
    // Le 0,97 dure 412 s pour un fichier de 201 s : c'est le 0,61 qui reste.
    assert_eq!(pris.duree_s, Some(203));
    assert_eq!(Some(&pris.recording_id), a.verite[&piste.track_id].as_ref());
}

#[test]
fn le_titre_de_la_piste_departage_avant_la_marge() {
    let hits = vec![
        EnregistrementAcoustid {
            acoustid: "a".into(),
            score: 0.92,
            recording_id: "faux".into(),
            titre: Some("Autre chanson".into()),
            artiste: None,
            duree_s: Some(180),
            releases: vec![],
        },
        EnregistrementAcoustid {
            acoustid: "a".into(),
            score: 0.91,
            recording_id: "juste".into(),
            titre: Some("Ça plane pour moi".into()),
            artiste: None,
            duree_s: Some(180),
            releases: vec![],
        },
    ];
    let pris = choisir_l_enregistrement(&hits, Some("ça plane pour moi !"), None, 180).unwrap();
    assert_eq!(pris.recording_id, "juste");
}

/// Tune : un même enregistrement sous deux empreintes ne se fait pas
/// concurrence (MetaRust aurait conclu « ambigu » sur 0,93 / 0,91).
#[test]
fn un_enregistrement_en_double_ne_se_fait_pas_concurrence() {
    let a = album("doublon");
    let (piste, hits) = &a.pistes[0];
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].recording_id, hits[1].recording_id);
    assert!(choisir_l_enregistrement(hits, None, None, piste.duree_s).is_some());
}

#[test]
fn la_moitie_des_pistes_suffit_mais_pas_moins() {
    let DecisionAlbum::Retenue { votes, pistes, .. } = decision(&album("moitie")) else {
        panic!("2 pistes sur 4 : la moitié doit suffire");
    };
    assert_eq!((votes, pistes), (2, 4));
    assert_eq!(
        decision(&album("minorite")),
        DecisionAlbum::SansMajorite {
            pistes_reconnues: 2,
            pistes: 5
        }
    );
}

/// La compilation qui reprend les morceaux ne vole pas les voix de l'album
/// (MetaRust : une piste indécise votait pour toutes ses releases), et deux
/// pressages d'un même album se départagent par la date.
#[test]
fn deux_pressages_du_meme_album_se_departagent_par_la_date() {
    let DecisionAlbum::Retenue { release, votes, .. } = decision(&album("propre")) else {
        panic!("album propre non retenu");
    };
    assert_eq!(votes, 4);
    assert_eq!(
        release.date,
        Some((2004, 3, 0)),
        "le pressage le plus ancien"
    );
    assert_eq!(
        release.titre, "Les Heures Claires",
        "titre repris du groupe"
    );
}

#[test]
fn deux_albums_differents_a_egalite_ne_donnent_rien() {
    assert!(matches!(
        decision(&album("deux_albums")),
        DecisionAlbum::SansMajorite { .. }
    ));
}

#[test]
fn la_cle_refusee_se_reconnait() {
    let v: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/acoustid/cle_refusee.json"
    ))
    .unwrap();
    assert_eq!(lire_reponse(&v), Err(RefusAcoustid::CleRefusee));
    let autre = serde_json::json!({"status": "error", "error": {"code": 3, "message": "invalid fingerprint"}});
    assert_eq!(
        lire_reponse(&autre),
        Err(RefusAcoustid::Erreur("invalid fingerprint".into()))
    );
}

#[test]
fn les_releases_se_lisent_sous_les_deux_formes() {
    let v = serde_json::json!({"status": "ok", "results": [{"id": "x", "score": 0.9, "recordings": [{
        "id": "r", "duration": 100.4,
        "releases": [{"id": "plat", "title": "A plat", "track_count": 3}],
        "releasegroups": [{"id": "g", "title": "Groupe", "releases": [{"id": "niche"}, {"id": "plat"}]}]
    }]}]});
    let hits = lire_reponse(&v).unwrap();
    assert_eq!(hits[0].duree_s, Some(100));
    let ids: Vec<&str> = hits[0].releases.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["plat", "niche"], "sans doublon");
    assert_eq!(hits[0].releases[1].titre, "Groupe");
    assert_eq!(hits[0].releases[1].release_group_id.as_deref(), Some("g"));
}

/// **Mesure avant/après** sur le banc, piste par piste.
///
/// *Avant* : ce que faisait `POST /library/identify` (`tracks.rs`) — le
/// premier résultat trié par score, sans plancher, sans durée, sans marge.
/// *Après* : [`decider_l_album`]. On compte, pour chaque piste, un
/// enregistrement juste, faux, ou rien.
#[test]
fn mesure_avant_apres_sur_le_banc() {
    let (mut naif_juste, mut naif_faux, mut naif_rien) = (0, 0, 0);
    let (mut juste, mut rien) = (0, 0);
    let mut albums_ecrits = 0;
    for (nom, contenu) in BANC {
        let a = charger(contenu);
        let ecrits: BTreeMap<i64, String> = match decision(&a) {
            DecisionAlbum::Retenue {
                enregistrements, ..
            } => {
                albums_ecrits += 1;
                enregistrements.into_iter().collect()
            }
            DecisionAlbum::SansMajorite { .. } => BTreeMap::new(),
        };
        for (piste, hits) in &a.pistes {
            let verite = a.verite[&piste.track_id].as_deref();
            let mut tries = hits.clone();
            tries.sort_by(|x, y| y.score.total_cmp(&x.score));
            match (tries.first().map(|h| h.recording_id.as_str()), verite) {
                (None, _) => naif_rien += 1,
                (Some(r), Some(v)) if r == v => naif_juste += 1,
                _ => naif_faux += 1,
            }
            match (ecrits.get(&piste.track_id).map(String::as_str), verite) {
                (None, _) => rien += 1,
                (Some(r), Some(v)) if r == v => juste += 1,
                (Some(r), _) => panic!("{nom} : piste {} faussement écrite ({r})", piste.track_id),
            }
        }
    }
    println!(
        "banc AcoustID ({} albums) — avant : {naif_juste} justes / {naif_faux} fausses / {naif_rien} sans ; \
         après : {juste} justes / 0 fausse / {rien} sans ; {albums_ecrits} albums écrits",
        BANC.len()
    );
    assert!(
        naif_faux > 0,
        "le banc doit contenir des pièges que l'avant tombe"
    );
    assert_eq!(albums_ecrits, 5);
}

#[test]
fn l_empreinte_complete_l_appariement_sans_le_contredire() {
    use crate::metadata::musicbrainz_release::MBTrack;
    let piste = |pos: u32, rid: &str| MBTrack {
        position: pos,
        disc: 1,
        number: None,
        title: format!("P{pos}"),
        length_ms: None,
        recording_id: Some(rid.into()),
        artist: None,
    };
    let release = [piste(1, "r1"), piste(2, "r2"), piste(3, "r3")];
    let par_place = vec![(10, "r1".to_string())];
    let par_empreinte = vec![
        (10, "r2".to_string()), // la place a déjà parlé : on garde r1
        (11, "r1".to_string()), // r1 est déjà porté par la piste 10
        (12, "r3".to_string()), // complète
        (13, "hors-release".to_string()),
    ];
    assert_eq!(
        completer_par_l_empreinte(par_place, &par_empreinte, &release),
        vec![(10, "r1".to_string()), (12, "r3".to_string())]
    );
}
