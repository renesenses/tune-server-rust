//! La veille Bandcamp : un passage lent, en arrière-plan, jamais sur requête.
//!
//! Bandcamp ne sert pas de fil de nouveautés : il faut aller lire la page de
//! chaque artiste. C'est **un appel réseau par artiste**, là où les autres
//! services en demandent un pour tout leur catalogue.
//!
//! Deux conséquences, et elles dictent tout ce module :
//!
//! 1. **Jamais sur le chemin d'une requête.** L'accueil se charge à chaque
//!    ouverture de page ; y brancher des dizaines d'appels réseau le rendrait
//!    inutilisable. Le passage tourne en arrière-plan et *dépose* son résultat ;
//!    la route d'accueil ne fait que le lire.
//! 2. **Seulement les favoris.** Une bibliothèque compte des milliers
//!    d'artistes ; les favoris se comptent en dizaines. C'est le seul sous-
//!    ensemble dont le coût soit tenable, et c'est aussi celui dont les
//!    nouveautés intéressent vraiment.
//!
//! Le premier passage n'annonce rien — voir [`tune_core::bandcamp_veille`].

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use tune_core::bandcamp_veille::{Empreintes, comparer, ranger};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;

/// Où l'on garde ce qu'on a déjà vu.
const CLE_EMPREINTES: &str = "bandcamp_veille_empreintes";
/// Où l'on dépose ce que la route d'accueil viendra lire.
const CLE_NOUVEAUTES: &str = "bandcamp_veille_nouveautes";

/// Combien d'artistes par passage. Bandcamp est un site, pas une API : on n'y
/// tape pas cinquante fois d'affilée.
const ARTISTES_PAR_PASSAGE: usize = 12;
/// Le repos entre deux artistes, dans un même passage.
const REPOS_ENTRE_ARTISTES: Duration = Duration::from_secs(5);
/// Le repos entre deux passages.
const REPOS_ENTRE_PASSAGES: Duration = Duration::from_secs(6 * 3600);
/// Le délai avant le tout premier passage : le démarrage a mieux à faire.
const DELAI_AU_DEMARRAGE: Duration = Duration::from_secs(300);

/// Les artistes mis en favori, par leur nom d'affichage.
fn artistes_favoris(backend: &Arc<dyn DbBackend>) -> Vec<String> {
    let mut noms: Vec<String> = Vec::new();
    for sql in [
        "SELECT DISTINCT item_name FROM favorites WHERE item_type = 'artist' AND item_name IS NOT NULL AND item_name != ''",
        "SELECT DISTINCT item_artist FROM favorites WHERE item_artist IS NOT NULL AND item_artist != ''",
    ] {
        for cols in backend.query_many(sql, &[]).unwrap_or_default() {
            if let Some(n) = cols.first().and_then(|v| v.as_string())
                && !noms.iter().any(|x| x.eq_ignore_ascii_case(&n))
            {
                noms.push(n);
            }
        }
    }
    noms
}

/// Un passage complet. Rend le nombre de nouveautés déposées.
pub async fn passage(backend: &Arc<dyn DbBackend>) -> usize {
    let reglages = SettingsRepo::with_backend(backend.clone());

    let mut empreintes: Empreintes = reglages
        .get(CLE_EMPREINTES)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let mut deposees: Vec<serde_json::Value> = reglages
        .get(CLE_NOUVEAUTES)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let favoris = artistes_favoris(backend);
    if favoris.is_empty() {
        return 0;
    }

    // On reprend là où le passage précédent s'est arrêté, plutôt que de
    // recommencer par le début : sans cela, seuls les douze premiers favoris
    // seraient jamais visités.
    let curseur: usize = reglages
        .get("bandcamp_veille_curseur")
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let mut vues = 0usize;
    let mut ajoutees = 0usize;

    for i in 0..ARTISTES_PAR_PASSAGE.min(favoris.len()) {
        let nom = &favoris[(curseur + i) % favoris.len()];
        vues += 1;

        let Some(racine) = tune_bandcamp::adresse_artiste(nom).await else {
            tokio::time::sleep(REPOS_ENTRE_ARTISTES).await;
            continue;
        };
        let parutions = tune_bandcamp::parutions_discographie(&racine).await;
        let adresses: Vec<String> = parutions
            .iter()
            .filter_map(|v| v.get("url").and_then(|u| u.as_str()).map(str::to_string))
            .collect();

        // Une page vide veut dire « je n'ai pas su lire », pas « cet artiste
        // n'a plus rien ». Ranger une empreinte vide ferait réannoncer toute
        // la discographie au passage suivant.
        if adresses.is_empty() {
            tokio::time::sleep(REPOS_ENTRE_ARTISTES).await;
            continue;
        }

        let cle = nom.to_lowercase();
        let p = comparer(empreintes.get(&cle), &adresses);

        if !p.nouveautes.is_empty() {
            deposees.retain(|d| d.get("artist_name").and_then(|a| a.as_str()) != Some(nom));
            // On depose les parutions COMPLETES, pas les seules adresses :
            // l'ecran a besoin du titre et de la pochette, et les rechercher
            // plus tard couterait un second appel reseau par nouveaute.
            let completes: Vec<serde_json::Value> = p
                .nouveautes
                .iter()
                .filter_map(|u| {
                    parutions
                        .iter()
                        .find(|v| v.get("url").and_then(|x| x.as_str()) == Some(u.as_str()))
                        .cloned()
                })
                .collect();
            deposees.push(serde_json::json!({
                "artist_name": nom,
                "artist_url": racine,
                "parutions": completes,
            }));
            ajoutees += p.nouveautes.len();
        }

        ranger(&mut empreintes, cle, p.empreinte);
        tokio::time::sleep(REPOS_ENTRE_ARTISTES).await;
    }

    // La table déposée ne grossit pas sans fin : l'accueil n'en montre qu'une
    // poignée de toute façon.
    if deposees.len() > 40 {
        let trop = deposees.len() - 40;
        deposees.drain(0..trop);
    }

    let _ = reglages.set(
        CLE_EMPREINTES,
        &serde_json::to_string(&empreintes).unwrap_or_default(),
    );
    let _ = reglages.set(
        CLE_NOUVEAUTES,
        &serde_json::to_string(&deposees).unwrap_or_default(),
    );
    let _ = reglages.set(
        "bandcamp_veille_curseur",
        &((curseur + vues) % favoris.len().max(1)).to_string(),
    );

    tracing::info!(
        artistes = vues,
        nouveautes = ajoutees,
        connus = empreintes.len(),
        "bandcamp_veille_passage"
    );
    ajoutees
}

/// Ce que la route d'accueil vient lire. Jamais d'appel réseau ici.
pub fn nouveautes_deposees(backend: &Arc<dyn DbBackend>) -> Vec<serde_json::Value> {
    SettingsRepo::with_backend(backend.clone())
        .get(CLE_NOUVEAUTES)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Arme la veille. Sans favori, elle ne fait rien et ne coûte rien.
pub fn spawn(backend: Arc<dyn DbBackend>) {
    tokio::spawn(async move {
        tokio::time::sleep(DELAI_AU_DEMARRAGE).await;
        loop {
            passage(&backend).await;
            tokio::time::sleep(REPOS_ENTRE_PASSAGES).await;
        }
    });
}

/// Les adresses déjà connues d'un artiste — utilisé par les tests et les
/// diagnostics.
pub fn empreinte_connue(backend: &Arc<dyn DbBackend>, nom: &str) -> BTreeSet<String> {
    let empreintes: Empreintes = SettingsRepo::with_backend(backend.clone())
        .get(CLE_EMPREINTES)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    empreintes
        .get(&nom.to_lowercase())
        .cloned()
        .unwrap_or_default()
}
