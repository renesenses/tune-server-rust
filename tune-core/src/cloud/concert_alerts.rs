use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::db::backend::DbBackend;

const CONCERTS_API: &str = "https://mozaiklabs.fr/api/v1/premium/concerts";

/// Le nuage n'accepte pas plus de 200 artistes par appel (`artists => max:200`).
const LOT: usize = 200;

/// Plafond de sécurité, en artistes. Une bibliothèque ordinaire en compte
/// quelques milliers (1 747 sur le serveur de référence, soit 9 appels) ; ce
/// plafond n'existe que pour qu'une bibliothèque pathologique ne parte pas en
/// centaines de requêtes. Une troncature est TOUJOURS signalée dans le journal :
/// un abonnement silencieusement amputé se lit comme « ce groupe ne joue nulle
/// part » côté utilisateur.
const PLAFOND: usize = 5_000;

/// Les artistes de la bibliothèque, prêts à être abonnés.
///
/// ⚠️ LE MBID N'EST PLUS EXIGÉ. Cette requête filtrait `musicbrainz_id IS NOT
/// NULL`, ce qui plafonnait la fonction à la part identifiée de la bibliothèque
/// — quelques pour cent sur une installation ordinaire.
///
/// Mesure du 30/08/2026 contre l'agenda Ticketmaster, sur les 1 747 artistes du
/// serveur de référence : 881 d'entre eux (50,4 %) sont reconnus par leur seul
/// NOM, mais seules 460 des attractions correspondantes portent un lien
/// MusicBrainz. Exiger le MBID écartait donc la moitié des concerts que la
/// source sait rendre, en plus de tous les artistes non identifiés localement.
///
/// Le MBID reste envoyé quand on l'a : c'est la meilleure identité disponible,
/// il a simplement cessé d'être une condition d'entrée.
///
/// `GROUP BY name` parce que la même personne peut apparaître sur plusieurs
/// lignes — l'une identifiée, l'autre non. Le nuage classe désormais par nom
/// replié : envoyer deux fois le même artiste ne ferait que gonfler la charge.
fn artistes_de_la_bibliotheque(
    backend: &Arc<dyn DbBackend>,
) -> Result<Vec<serde_json::Value>, String> {
    let rows = backend
        .query_many(
            "SELECT name, MAX(musicbrainz_id) FROM artists \
             WHERE name IS NOT NULL AND name != '' \
             GROUP BY name ORDER BY name \
             LIMIT 5000",
            &[],
        )
        .map_err(|e| format!("query: {e}"))?;

    Ok(rows
        .iter()
        .filter_map(|r| {
            let nom = r.get(0).and_then(|v| v.as_string())?;
            if nom.is_empty() {
                return None;
            }
            let mbid = r
                .get(1)
                .and_then(|v| v.as_string())
                .filter(|m| !m.is_empty());

            Some(serde_json::json!({
                "artist_name": nom,
                "musicbrainz_artist_id": mbid,
            }))
        })
        .collect())
}

/// Push the library's artists as concert subscriptions to the mozaiklabs cloud.
/// Returns the number of artists subscribed.
///
/// ⚠️ ORDRE DE DÉPLOIEMENT. Cette fonction envoie désormais des artistes SANS
/// `musicbrainz_artist_id`. Le nuage ne l'accepte que depuis site-mozaiklabs#185
/// (30/08/2026) ; une version antérieure répondait 422 sur la charge entière.
/// Le nuage se déploie en continu et cette version de Tune passe par un train de
/// release, donc l'ordre est acquis en pratique — mais il faut le savoir avant
/// de rejouer ce code sur une instance pointant vers un nuage figé.
pub async fn sync_artist_subscriptions(
    backend: &Arc<dyn DbBackend>,
    http_client: &reqwest::Client,
    instance_id: &str,
) -> Result<usize, String> {
    let artistes = artistes_de_la_bibliotheque(backend)?;

    if artistes.is_empty() {
        debug!("concert_alerts_no_artists");
        return Ok(0);
    }

    if artistes.len() >= PLAFOND {
        warn!(
            plafond = PLAFOND,
            "concert_subscriptions_tronquees: bibliotheque au-dela du plafond, \
             les artistes suivants ne seront pas abonnes"
        );
    }

    // Un seul appel ne peut porter que 200 artistes : au-delà, l'ancienne
    // requête coupait à 200 sans le dire. On découpe et on additionne.
    let mut total = 0usize;
    let mut ignores = 0usize;
    let mut lots_en_echec = 0usize;
    let nombre_de_lots = artistes.len().div_ceil(LOT);

    for lot in artistes.chunks(LOT) {
        let body = serde_json::json!({
            "instance_id": instance_id,
            "artists": lot,
        });

        let resp = http_client
            .post(format!("{CONCERTS_API}/subscribe"))
            .json(&body)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await;

        // Un lot en échec ne condamne pas les autres : mieux vaut abonner
        // 1 500 artistes sur 1 747 que zéro parce que le huitième appel a
        // rencontré une coupure réseau.
        let resp = match resp {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                warn!(statut = %r.status(), "concert_subscribe_lot_refuse");
                lots_en_echec += 1;
                continue;
            }
            Err(e) => {
                warn!(error = %e, "concert_subscribe_lot_echoue");
                lots_en_echec += 1;
                continue;
            }
        };

        let result: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "concert_subscribe_lot_illisible");
                lots_en_echec += 1;
                continue;
            }
        };

        total += result["subscribed"].as_i64().unwrap_or(0) as usize;
        // Le nuage écarte les noms qui ne désignent aucun artiste
        // (« Various Artists », « Unknown »...). Les compter permet de voir
        // d'un coup d'œil si une bibliothèque est surtout faite de compilations.
        ignores += result["ignored"].as_i64().unwrap_or(0) as usize;
    }

    if lots_en_echec == nombre_de_lots {
        return Err(format!(
            "concert subscribe: {nombre_de_lots} lot(s) en echec"
        ));
    }

    info!(
        count = total,
        ignores,
        lots = nombre_de_lots,
        lots_en_echec,
        "concert_subscriptions_synced"
    );
    Ok(total)
}

/// Fetch upcoming concerts for artists that this instance has subscribed to.
pub async fn get_upcoming_concerts(
    http_client: &reqwest::Client,
    instance_id: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let resp = http_client
        .get(format!("{CONCERTS_API}/upcoming"))
        .query(&[("instance_id", instance_id)])
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("concerts: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("concerts: HTTP {}", resp.status()));
    }

    let data: serde_json::Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
    let concerts = data["concerts"].as_array().cloned().unwrap_or_default();
    info!(count = concerts.len(), "upcoming_concerts_fetched");
    Ok(concerts)
}

/// Spawn a periodic background task that syncs artist subscriptions every
/// 24 hours and is gated behind the `community_sync_enabled` setting
/// (piggy-backs on the same toggle as community metadata sync).
pub fn spawn(backend: Arc<dyn DbBackend>) {
    let client = match crate::http::client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("Tune/2.0 (https://mozaiklabs.fr)")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "concert_alerts_client_build_failed");
            return;
        }
    };

    tokio::spawn(async move {
        // Wait 2 minutes after startup before the first sync
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;

        loop {
            let settings = crate::db::settings_repo::SettingsRepo::with_backend(backend.clone());
            let enabled = settings
                .get("community_sync_enabled")
                .ok()
                .flatten()
                .map(|v| v == "true")
                .unwrap_or(false);

            if enabled {
                let instance_id = settings
                    .get("instance_id")
                    .ok()
                    .flatten()
                    .unwrap_or_default();

                if !instance_id.is_empty() {
                    if let Err(e) = sync_artist_subscriptions(&backend, &client, &instance_id).await
                    {
                        warn!(error = %e, "concert_subscriptions_sync_failed");
                    }
                } else {
                    debug!("concert_alerts_skipped_no_instance_id");
                }
            }

            // Every 24 hours
            tokio::time::sleep(std::time::Duration::from_secs(86400)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;

    fn base_avec_artistes(artistes: &[(&str, Option<&str>)]) -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        for (nom, mbid) in artistes {
            db.execute(
                "INSERT INTO artists (name, musicbrainz_id) VALUES (?, ?)",
                &[nom, mbid],
            )
            .unwrap();
        }

        Arc::new(db)
    }

    fn noms(artistes: &[serde_json::Value]) -> Vec<String> {
        artistes
            .iter()
            .map(|a| a["artist_name"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// CONTRE-ÉPREUVE DU LOT. Avec l'ancien filtre
    /// `WHERE musicbrainz_id IS NOT NULL`, Melissa Laveaux n'était jamais
    /// abonnée — et c'est justement une artiste que Ticketmaster reconnaît par
    /// son nom, avec des dates à La Ferté-Bernard et Grasse (mesuré le
    /// 30/08/2026). Ce test échoue si le filtre revient.
    #[test]
    fn un_artiste_sans_mbid_est_abonne() {
        let backend = base_avec_artistes(&[
            ("Melissa Laveaux", None),
            (
                "Bernard Lavilliers",
                Some("8bef9bae-a250-4c4e-8e5e-b2f81607db2a"),
            ),
        ]);

        let artistes = artistes_de_la_bibliotheque(&backend).unwrap();

        assert_eq!(
            noms(&artistes),
            vec!["Bernard Lavilliers", "Melissa Laveaux"],
            "un artiste sans MBID doit partir comme les autres"
        );
        assert!(
            artistes[1]["musicbrainz_artist_id"].is_null(),
            "l'absence de MBID s'envoie comme nulle, pas comme chaine vide"
        );
    }

    #[test]
    fn le_mbid_est_conserve_quand_on_l_a() {
        let backend = base_avec_artistes(&[(
            "Fatoumata Diawara",
            Some("6f5064bb-7dbb-4a44-bac5-04c467394817"),
        )]);

        let artistes = artistes_de_la_bibliotheque(&backend).unwrap();

        assert_eq!(
            artistes[0]["musicbrainz_artist_id"], "6f5064bb-7dbb-4a44-bac5-04c467394817",
            "le MBID reste la meilleure identite disponible : il cesse d'etre \
             obligatoire, il ne disparait pas"
        );
    }

    /// La même personne apparaît souvent deux fois : une ligne identifiée par un
    /// scan enrichi, une autre pas. Le nuage classant par nom replié, envoyer les
    /// deux ne ferait que gonfler la charge utile.
    #[test]
    fn un_artiste_present_deux_fois_ne_part_qu_une_fois_avec_son_mbid() {
        let backend = base_avec_artistes(&[
            ("Yael Naim", None),
            ("Yael Naim", Some("11111111-1111-4111-8111-111111111111")),
        ]);

        let artistes = artistes_de_la_bibliotheque(&backend).unwrap();

        assert_eq!(artistes.len(), 1, "un seul envoi pour un seul artiste");
        assert_eq!(
            artistes[0]["musicbrainz_artist_id"], "11111111-1111-4111-8111-111111111111",
            "entre une ligne identifiee et une ligne nue, on garde l'identite"
        );
    }

    #[test]
    fn un_nom_vide_ne_part_pas() {
        let backend = base_avec_artistes(&[("", None), ("Superbus", None)]);

        assert_eq!(
            noms(&artistes_de_la_bibliotheque(&backend).unwrap()),
            vec!["Superbus"]
        );
    }

    /// Le nuage refuse plus de 200 artistes par appel. L'ancienne requête
    /// coupait à 200 SANS LE DIRE : sur une bibliothèque de 1 747 artistes,
    /// 1 547 d'entre eux n'étaient jamais abonnés et personne ne pouvait le
    /// savoir. Le découpage est ce qui rend le lot utile.
    #[test]
    fn au_dela_de_200_artistes_le_decoupage_les_emmene_tous() {
        let mut artistes: Vec<(String, Option<&str>)> = Vec::new();
        for i in 0..450 {
            artistes.push((format!("Artiste {i:04}"), None));
        }
        let refs: Vec<(&str, Option<&str>)> =
            artistes.iter().map(|(n, m)| (n.as_str(), *m)).collect();

        let backend = base_avec_artistes(&refs);
        let tous = artistes_de_la_bibliotheque(&backend).unwrap();

        assert_eq!(tous.len(), 450, "aucun artiste ne doit etre perdu en amont");

        let lots: Vec<_> = tous.chunks(LOT).collect();
        assert_eq!(lots.len(), 3, "450 artistes = 3 appels de 200 au plus");
        assert_eq!(lots[0].len(), 200);
        assert_eq!(lots[2].len(), 50);

        let envoyes: usize = lots.iter().map(|l| l.len()).sum();
        assert_eq!(
            envoyes, 450,
            "la somme des lots doit rendre la bibliotheque entiere"
        );
    }
}
