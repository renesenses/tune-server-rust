//! Les groupes de lecture rangés dans le réglage `zone_groups` (#7937).
//!
//! Fil forum 2661 : deux zones groupées ne se dégroupaient plus, et une zone
//! supprimée puis recréée revenait « avec la mémoire » du groupe. Supprimer
//! une zone n'est qu'un MASQUAGE : la ligne garde son numéro, et
//! `POST /zones` sur le même appareil la ressuscite sous ce numéro. Or rien
//! ne retirait ce numéro de `zone_groups` : la zone recréée retombait dans
//! son ancien groupe, et le relais de groupe (#6155) la refaisait suivre sa
//! meneuse. De son côté, `POST /zones/group` empilait un nouveau groupe sans
//! retirer ses zones de leurs groupes précédents : un ancien groupe survivait
//! au nouveau, et défaire l'un laissait l'autre en place.
//!
//! Les règles tiennent ici, en un seul endroit :
//! - une zone appartient à UN groupe au plus ;
//! - un groupe réduit à moins de deux zones n'est plus un groupe ;
//! - une meneuse retirée cède sa place à la première zone qui reste.
//!
//! #8680 (fil 2800) — ces règles ne valaient qu'à l'ÉCRITURE (`create_group`).
//! Les groupes enregistrés avant la rc11, que le client web empilait à chaque
//! « Appliquer », restaient en base et se chevauchaient : une zone dans 23
//! groupes, une suiveuse meneuse d'un autre groupe. Le relais partait alors en
//! cascade d'un groupe à l'autre (« playing everywhere or nowhere »).
//! [`normaliser`] remet n'importe quelle liste dans la règle ; le démarrage
//! l'applique au réglage ([`normaliser_les_groupes_enregistres`]) et le relais
//! à ce qu'il lit.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// La clé du réglage.
pub const CLE_ZONE_GROUPS: &str = "zone_groups";

fn ids_du_groupe(groupe: &Value) -> Vec<i64> {
    groupe
        .get("zone_ids")
        .and_then(Value::as_array)
        .map(|l| l.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

/// Retire `zones` de tous les groupes de `groupes`. Un groupe qui tombe sous
/// deux zones disparaît ; une meneuse retirée est remplacée par la première
/// zone restante. Rend `true` si quelque chose a changé.
pub fn retirer_les_zones(groupes: &mut Vec<Value>, zones: &[i64]) -> bool {
    if zones.is_empty() {
        return false;
    }
    let avant = groupes.clone();
    groupes.retain_mut(|groupe| {
        let ids = ids_du_groupe(groupe);
        if !ids.iter().any(|id| zones.contains(id)) {
            return true;
        }
        let restants: Vec<i64> = ids.into_iter().filter(|id| !zones.contains(id)).collect();
        if restants.len() < 2 {
            return false;
        }
        let meneuse = groupe
            .get("leader_id")
            .and_then(Value::as_i64)
            .filter(|id| restants.contains(id))
            .unwrap_or(restants[0]);
        groupe["zone_ids"] = json!(restants);
        if groupe.get("leader_id").is_some() {
            groupe["leader_id"] = json!(meneuse);
        }
        true
    });
    *groupes != avant
}

fn poser_la_meneuse(groupe: &mut Value, cle: &str, restants: &[i64]) {
    if let Some(meneuse) = groupe.get(cle).and_then(Value::as_i64)
        && !restants.contains(&meneuse)
    {
        groupe[cle] = json!(restants[0]);
    }
}

/// #8680 — met `groupes` dans la règle « une zone, un groupe au plus ».
///
/// Règle déterministe : le groupe le plus RÉCENT gagne. Les groupes sont
/// rangés dans l'ordre où ils ont été formés (création et recomposition les
/// ajoutent en fin de liste) : on parcourt donc la liste de la fin vers le
/// début, et une zone déjà prise par un groupe plus récent quitte les plus
/// anciens. Une zone en double dans un même groupe n'y compte qu'une fois. Un
/// groupe réduit à moins de deux zones disparaît (ses zones ne sont alors
/// prises par personne et restent libres pour un groupe plus ancien). Une
/// meneuse (`leader_id`, `leader_zone_id`) qui n'est plus dans les zones de
/// son groupe cède sa place à la première zone qui reste.
///
/// Rend `true` si la liste a changé. Idempotent : une liste déjà dans la
/// règle n'est pas touchée.
pub fn normaliser(groupes: &mut Vec<Value>) -> bool {
    let avant = groupes.clone();
    let mut prises: Vec<i64> = Vec::new();
    let mut garder = vec![false; groupes.len()];
    for (i, groupe) in groupes.iter_mut().enumerate().rev() {
        let mut restants: Vec<i64> = Vec::new();
        for id in ids_du_groupe(groupe) {
            if !prises.contains(&id) && !restants.contains(&id) {
                restants.push(id);
            }
        }
        if restants.len() < 2 || !groupe.is_object() {
            continue;
        }
        if ids_du_groupe(groupe) != restants {
            groupe["zone_ids"] = json!(restants);
        }
        poser_la_meneuse(groupe, "leader_id", &restants);
        poser_la_meneuse(groupe, "leader_zone_id", &restants);
        prises.extend_from_slice(&restants);
        garder[i] = true;
    }
    let mut garde = garder.into_iter();
    groupes.retain(|_| garde.next().unwrap_or(false));
    *groupes != avant
}

/// #8680 — `zones` deviennent la composition du groupe rangé à `index` :
/// elles quittent tous les AUTRES groupes ([`retirer_les_zones`]), et le
/// groupe, recomposé, passe en fin de liste (il devient le plus récent, voir
/// [`normaliser`]). Sa meneuse reste si elle est toujours dans ses zones,
/// sinon c'est la première. Rend le nouvel index du groupe.
///
/// `zones` doit compter au moins deux zones distinctes : c'est à l'appelant
/// de refuser une composition plus courte.
pub fn recomposer_le_groupe(groupes: &mut Vec<Value>, index: usize, zones: &[i64]) -> usize {
    let mut uniques: Vec<i64> = Vec::new();
    for id in zones {
        if !uniques.contains(id) {
            uniques.push(*id);
        }
    }
    let mut groupe = groupes.remove(index);
    retirer_les_zones(groupes, &uniques);
    groupe["zone_ids"] = json!(uniques);
    if !uniques.is_empty() {
        poser_la_meneuse(&mut groupe, "leader_id", &uniques);
        poser_la_meneuse(&mut groupe, "leader_zone_id", &uniques);
    }
    groupes.push(groupe);
    groupes.len() - 1
}

/// Le numéro du prochain groupe : un de plus que le plus grand. Le compter
/// (`len() + 1`) redonnait le numéro d'un groupe encore là dès qu'un groupe
/// plus ancien avait été défait.
pub fn prochain_numero(groupes: &[Value]) -> i64 {
    groupes
        .iter()
        .filter_map(|g| g.get("id").and_then(Value::as_i64))
        .max()
        .unwrap_or(0)
        + 1
}

/// #8107 — les AUTRES zones du groupe de `zone` (vide si elle joue seule).
pub fn zones_compagnes(groupes: &[Value], zone: i64) -> Vec<i64> {
    groupes
        .iter()
        .map(ids_du_groupe)
        .find(|ids| ids.contains(&zone))
        .map(|ids| ids.into_iter().filter(|id| *id != zone).collect())
        .unwrap_or_default()
}

/// Lit `zone_groups` (vide s'il est absent ou illisible).
pub fn lire(settings: &SettingsRepo) -> Vec<Value> {
    settings
        .get(CLE_ZONE_GROUPS)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Retire `zones` des groupes enregistrés, et réécrit le réglage seulement
/// s'il change. Rend `true` si un groupe a été modifié ou défait.
///
/// Un réglage illisible n'est pas réécrit : on ne remplace pas ce qu'on ne
/// comprend pas par une liste vide.
pub fn retirer_des_groupes_enregistres(
    backend: &Arc<dyn DbBackend>,
    zones: &[i64],
) -> Result<bool, String> {
    let settings = SettingsRepo::with_backend(backend.clone());
    let Some(brut) = settings.get(CLE_ZONE_GROUPS)? else {
        return Ok(false);
    };
    let Ok(mut groupes) = serde_json::from_str::<Vec<Value>>(&brut) else {
        return Ok(false);
    };
    if !retirer_les_zones(&mut groupes, zones) {
        return Ok(false);
    }
    settings.set(
        CLE_ZONE_GROUPS,
        &serde_json::to_string(&groupes).map_err(|e| e.to_string())?,
    )?;
    tracing::info!(zones = ?zones, restants = groupes.len(), "zone_groups_zones_retirees");
    Ok(true)
}

/// #8680 — ce que la normalisation du réglage a changé.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupesNormalises {
    /// Les groupes tels qu'ils étaient enregistrés.
    pub avant: Vec<Value>,
    /// Les groupes réécrits, dans la règle.
    pub apres: Vec<Value>,
}

/// #8680 — applique [`normaliser`] au réglage `zone_groups` et le réécrit
/// seulement s'il change ; journalise ce qui a changé. Rend `None` si rien
/// n'a changé (réglage absent, illisible ou déjà dans la règle).
///
/// Passe par [`SettingsRepo`] : même chemin sur SQLite et sur PostgreSQL. Un
/// réglage illisible n'est pas réécrit : on ne remplace pas ce qu'on ne
/// comprend pas.
pub fn normaliser_les_groupes_enregistres(
    backend: &Arc<dyn DbBackend>,
) -> Result<Option<GroupesNormalises>, String> {
    let settings = SettingsRepo::with_backend(backend.clone());
    let Some(brut) = settings.get(CLE_ZONE_GROUPS)? else {
        return Ok(None);
    };
    let Ok(avant) = serde_json::from_str::<Vec<Value>>(&brut) else {
        return Ok(None);
    };
    let mut apres = avant.clone();
    if !normaliser(&mut apres) {
        return Ok(None);
    }
    settings.set(
        CLE_ZONE_GROUPS,
        &serde_json::to_string(&apres).map_err(|e| e.to_string())?,
    )?;
    let appartenances = |groupes: &[Value]| {
        groupes
            .iter()
            .map(|g| ids_du_groupe(g).len())
            .sum::<usize>()
    };
    tracing::warn!(
        groupes_avant = avant.len(),
        groupes_apres = apres.len(),
        appartenances_avant = appartenances(&avant),
        appartenances_apres = appartenances(&apres),
        "zone_groups_normalises — une zone n'appartient qu'à un groupe (#8680)"
    );
    Ok(Some(GroupesNormalises { avant, apres }))
}

/// Vide `zone_groups` (toutes les zones supprimées). Rend `true` s'il y
/// avait au moins un groupe.
pub fn vider_les_groupes_enregistres(backend: &Arc<dyn DbBackend>) -> Result<bool, String> {
    let settings = SettingsRepo::with_backend(backend.clone());
    if lire(&settings).is_empty() {
        return Ok(false);
    }
    settings.set(CLE_ZONE_GROUPS, "[]")?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn groupe(id: i64, meneuse: i64, zones: &[i64]) -> Value {
        json!({ "id": id, "group_id": id.to_string(), "leader_id": meneuse, "zone_ids": zones })
    }

    #[test]
    fn un_groupe_de_deux_disparait_quand_une_zone_part() {
        let mut g = vec![groupe(1, 1, &[1, 2])];
        assert!(retirer_les_zones(&mut g, &[2]));
        assert!(g.is_empty());
    }

    #[test]
    fn un_groupe_de_trois_garde_les_deux_autres_et_change_de_meneuse() {
        let mut g = vec![groupe(1, 1, &[1, 2, 3])];
        assert!(retirer_les_zones(&mut g, &[1]));
        assert_eq!(g[0]["zone_ids"], json!([2, 3]));
        assert_eq!(g[0]["leader_id"], json!(2));
    }

    #[test]
    fn un_groupe_sans_ces_zones_reste_intact() {
        let mut g = vec![groupe(1, 1, &[1, 2])];
        assert!(!retirer_les_zones(&mut g, &[5]));
        assert_eq!(g, vec![groupe(1, 1, &[1, 2])]);
    }

    /// Toutes les zones d'une liste de groupes, avec leurs répétitions.
    fn appartenances(groupes: &[Value]) -> Vec<i64> {
        groupes.iter().flat_map(ids_du_groupe).collect()
    }

    fn une_zone_un_groupe(groupes: &[Value]) -> bool {
        let toutes = appartenances(groupes);
        let mut uniques = toutes.clone();
        uniques.sort_unstable();
        uniques.dedup();
        uniques.len() == toutes.len()
    }

    /// Le réglage du testeur du fil 2800, en petit : la zone 10 dans de
    /// nombreux groupes empilés par « Appliquer », la zone 11 suiveuse de 10
    /// ET meneuse d'un groupe (16, 23, 9), la zone 9 dans les deux.
    fn reglage_du_fil_2800() -> Vec<Value> {
        let mut g = Vec::new();
        for id in 1..=20 {
            g.push(groupe(id, 10, &[10, 9]));
        }
        g.push(groupe(21, 11, &[11, 16, 23, 9]));
        g.push(groupe(22, 10, &[10, 9, 11]));
        g
    }

    #[test]
    fn normaliser_le_reglage_du_fil_2800_8680() {
        let mut g = reglage_du_fil_2800();
        assert!(!une_zone_un_groupe(&g), "témoin : le réglage se chevauche");
        assert!(normaliser(&mut g));
        assert!(une_zone_un_groupe(&g), "{g:?}");
        // Le plus récent (22) garde 10, 9 et 11 ; le 21 garde 16 et 23 et
        // change de meneuse ; les vingt groupes {10, 9} disparaissent.
        assert_eq!(
            g,
            vec![
                json!({ "id": 21, "group_id": "21", "leader_id": 16, "zone_ids": [16, 23] }),
                groupe(22, 10, &[10, 9, 11]),
            ]
        );
    }

    #[test]
    fn normaliser_est_idempotent_8680() {
        let mut g = reglage_du_fil_2800();
        assert!(normaliser(&mut g));
        let une_fois = g.clone();
        assert!(
            !normaliser(&mut g),
            "une liste dans la règle n'est pas touchée"
        );
        assert_eq!(g, une_fois);
    }

    #[test]
    fn normaliser_retire_les_doublons_et_remet_la_meneuse_dans_ses_zones_8680() {
        let mut g = vec![
            json!({ "id": 1, "leader_id": 7, "zone_ids": [3, 4, 3] }),
            json!({ "id": 2, "leader_zone_id": 9, "zone_ids": [5, 6] }),
            json!({ "id": 3, "zone_ids": [8] }),
            json!("illisible"),
        ];
        assert!(normaliser(&mut g));
        assert_eq!(
            g,
            vec![
                json!({ "id": 1, "leader_id": 3, "zone_ids": [3, 4] }),
                json!({ "id": 2, "leader_zone_id": 5, "zone_ids": [5, 6] }),
            ]
        );
    }

    #[test]
    fn un_groupe_recent_defait_libere_ses_zones_pour_un_plus_ancien_8680() {
        // 2 tombe à {10} (9 est pris par 3) : 10 n'est prise par personne,
        // le groupe 1 (10, 12) reste entier.
        let mut g = vec![
            groupe(1, 10, &[10, 12]),
            groupe(2, 10, &[10, 9]),
            groupe(3, 9, &[9, 11]),
        ];
        assert!(normaliser(&mut g));
        assert_eq!(g, vec![groupe(1, 10, &[10, 12]), groupe(3, 9, &[9, 11])]);
    }

    #[test]
    fn recomposer_retire_les_zones_des_autres_groupes_8680() {
        let mut g = vec![
            groupe(1, 1, &[1, 2]),
            groupe(2, 3, &[3, 4, 5]),
            groupe(3, 6, &[6, 7]),
        ];
        // Le groupe 1 prend 4 et 6, et perd sa meneuse.
        let i = recomposer_le_groupe(&mut g, 0, &[2, 4, 6, 4]);
        assert_eq!(i, 1);
        assert_eq!(
            g,
            vec![groupe(2, 3, &[3, 5]), groupe(1, 2, &[2, 4, 6])],
            "le 3 (6, 7) tombe sous deux zones, le 1 passe en fin de liste"
        );
        assert!(une_zone_un_groupe(&g));
    }

    #[test]
    fn le_prochain_numero_ne_reprend_pas_celui_d_un_groupe_restant() {
        // Le groupe 1 a été défait, le 2 reste : compter donnait 2.
        assert_eq!(prochain_numero(&[groupe(2, 3, &[3, 4])]), 3);
        assert_eq!(prochain_numero(&[]), 1);
    }

    fn verifier_le_reglage_8680(backend: Arc<dyn DbBackend>) {
        let settings = SettingsRepo::with_backend(backend.clone());
        settings.delete(CLE_ZONE_GROUPS).unwrap();
        assert_eq!(
            normaliser_les_groupes_enregistres(&backend).unwrap(),
            None,
            "réglage absent"
        );
        settings.set(CLE_ZONE_GROUPS, "pas du json").unwrap();
        assert_eq!(normaliser_les_groupes_enregistres(&backend).unwrap(), None);
        assert_eq!(
            settings.get(CLE_ZONE_GROUPS).unwrap().as_deref(),
            Some("pas du json"),
            "un réglage illisible n'est pas réécrit"
        );

        settings
            .set(
                CLE_ZONE_GROUPS,
                &serde_json::to_string(&reglage_du_fil_2800()).unwrap(),
            )
            .unwrap();
        let fait = normaliser_les_groupes_enregistres(&backend)
            .unwrap()
            .expect("le réglage se chevauchait : il est réécrit");
        assert_eq!(fait.avant, reglage_du_fil_2800());
        let relu = lire(&settings);
        assert_eq!(relu, fait.apres, "le réglage relu est celui normalisé");
        assert!(une_zone_un_groupe(&relu), "{relu:?}");
        assert_eq!(relu.len(), 2);
        assert_eq!(
            normaliser_les_groupes_enregistres(&backend).unwrap(),
            None,
            "idempotent : au démarrage suivant, rien n'est réécrit"
        );
    }

    #[test]
    fn normaliser_le_reglage_sur_sqlite_8680() {
        verifier_le_reglage_8680(Arc::new(
            crate::db::sqlite::SqliteDb::open_in_memory().unwrap(),
        ));
    }

    #[cfg(feature = "postgres")]
    #[tokio::test(flavor = "multi_thread")]
    async fn pg_8680_normaliser_le_reglage_postgres() {
        let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
            eprintln!("SAUT: TUNE_TEST_PG_URL absent");
            return;
        };
        // Une seule connexion : la table temporaire masque celle de la base
        // de test et disparaît avec elle.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        sqlx::raw_sql(
            "CREATE TEMPORARY TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL, \
             updated_at TEXT)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let db: Arc<dyn DbBackend> =
            Arc::new(crate::db::backend::PostgresBackend::new(pool.clone()));
        verifier_le_reglage_8680(db);
        pool.close().await;
    }
}
