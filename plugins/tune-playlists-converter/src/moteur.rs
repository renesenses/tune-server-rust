//! Le moteur du convertisseur : préparer, apercevoir, exécuter, reprendre.
//!
//! Trois règles, et tout le reste en découle.
//!
//! 1. **Aucune écriture avant l'aperçu.** [`preparer`] n'appelle QUE des
//!    capacités de lecture, plus le carnet `kv` du greffon. Ce n'est pas une
//!    intention : le trait [`Hote`] sépare les deux familles, et l'essai
//!    `l_apercu_n_ecrit_rien` le mesure sur un hôte qui compte ses écritures.
//! 2. **Aucune écriture sans accord explicite.** [`executer`] refuse sans
//!    `confirme: true`, et refuse un transfert dont aucun aperçu n'existe dans
//!    le `kv` : le plan ne peut naître que de [`preparer`].
//! 3. **Aucune suppression, nulle part.** L'interface hôte n'en offre pas ; le
//!    greffon n'en simule pas (pas de « vider puis remplir »).
//!
//! La reprise tient dans un champ : [`Ligne::ecrite`]. Le plan est réécrit
//! dans le `kv` après chaque écriture réussie, donc un lot coupé au milieu
//! reprend là où il s'est arrêté — sans recréer la playlist déjà créée, sans
//! réécrire les titres déjà posés.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::hote::Hote;
use crate::plan::{Bloc, Cible, Comptes, Etat, Ligne, Origine, Plan, Statut, raison};

/// Combien de pistes par appel d'écriture.
///
/// Un seul appel pour trois cents titres, c'est trois cents titres perdus de
/// vue quand le service coupe au milieu : l'hôte rend un nombre, pas la liste
/// de ce qui est passé. Par tranches, une coupure ne coûte qu'une tranche, et
/// le plan est enregistré entre chaque.
const TAILLE_TRANCHE: usize = 50;

/// Le code porté par un bloc dont rien n'est appariable : aucune playlist
/// n'est créée pour lui. Comme les raisons de [`crate::plan::raison`], c'est
/// un CODE — le client web le traduit.
pub const AUCUN_TITRE_A_ECRIRE: &str = "aucun_titre_a_ecrire";

/// Ce que le client demande : des sources, une cible.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Demande {
    /// Une ou PLUSIEURS playlists — c'est le mode par lot, et c'est le même
    /// chemin de code que le transfert simple.
    pub sources: Vec<Origine>,
    pub cible: Cible,
    /// Suffixe facultatif ajouté au nom de chaque playlist créée. Vide par
    /// défaut : le transfert est « à l'identique ».
    #[serde(default)]
    pub suffixe: String,
}

// ---------------------------------------------------------------------------
// Aperçu
// ---------------------------------------------------------------------------

/// Préparer un transfert et en rendre l'APERÇU, sans rien écrire chez
/// l'utilisateur.
pub fn preparer(hote: &dyn Hote, demande: &Demande) -> Result<Plan, String> {
    if demande.sources.is_empty() {
        return Err("aucune playlist source".to_string());
    }
    verifier_la_cible(hote, &demande.cible)?;

    let mut blocs = Vec::with_capacity(demande.sources.len());
    for origine in &demande.sources {
        blocs.push(preparer_un_bloc(
            hote,
            origine,
            &demande.cible,
            &demande.suffixe,
        )?);
    }

    let transfert_id = numeroter(hote)?;
    let plan = Plan {
        transfert_id,
        etat: Etat::Apercu,
        cible: demande.cible.clone(),
        blocs,
    };
    enregistrer(hote, &plan)?;
    hote.journal(
        "info",
        &format!(
            "apercu_pret transfert={} playlists={}",
            plan.transfert_id,
            plan.blocs.len()
        ),
    );
    Ok(plan)
}

/// La cible est-elle utilisable ? Un service doit être AUTHENTIFIÉ et savoir
/// écrire — le dire à l'aperçu évite de le découvrir après avoir apparié trois
/// cents titres pour rien.
fn verifier_la_cible(hote: &dyn Hote, cible: &Cible) -> Result<(), String> {
    let Cible::Service { service } = cible else {
        return Ok(());
    };
    let rendu = hote.services()?;
    let fiche = rendu
        .get("services")
        .and_then(Value::as_array)
        .and_then(|liste| {
            liste
                .iter()
                .find(|s| s.get("name").and_then(Value::as_str) == Some(service.as_str()))
        })
        .ok_or_else(|| format!("service non authentifié : {service}"))?;
    if fiche.get("supports_write").and_then(Value::as_bool) != Some(true) {
        return Err(format!("ce service ne sait pas écrire : {service}"));
    }
    Ok(())
}

/// Une piste lue chez la source, réduite à ce qui sert à apparier.
struct PisteSource {
    titre: String,
    artiste: String,
    isrc: String,
    duree_ms: u64,
    /// L'identifiant local, quand la source est locale.
    piste_locale: Option<i64>,
    /// L'identifiant chez le service, quand la source est un service.
    piste_service: Option<String>,
}

fn preparer_un_bloc(
    hote: &dyn Hote,
    origine: &Origine,
    cible: &Cible,
    suffixe: &str,
) -> Result<Bloc, String> {
    let (nom, pistes) = lire_la_source(hote, origine)?;
    let nom_cible = format!("{nom}{suffixe}");
    let lignes = pistes
        .iter()
        .map(|piste| apercevoir_une_piste(hote, origine, cible, piste))
        .collect();
    Ok(Bloc {
        origine: origine.clone(),
        nom,
        nom_cible,
        cible_playlist: None,
        cible_playlist_locale: None,
        erreur: None,
        ajoutees: 0,
        lignes,
    })
}

/// Lire le nom et les pistes d'une playlist source.
fn lire_la_source(
    hote: &dyn Hote,
    origine: &Origine,
) -> Result<(String, Vec<PisteSource>), String> {
    match origine {
        Origine::Local { id } => {
            let rendu = hote.pistes_locales(*id)?;
            let nom = texte(&rendu, "name");
            Ok((nom, pistes_de(&rendu)))
        }
        Origine::Service { service, id } => {
            let rendu = hote.pistes_du_service(service, id)?;
            // Le nom de la playlist n'est pas dans la réponse des pistes : il
            // se lit dans la liste des playlists du service. À défaut, on
            // garde l'identifiant plutôt qu'un nom inventé.
            let nom = nom_chez_le_service(hote, service, id).unwrap_or_else(|| id.clone());
            Ok((nom, pistes_de(&rendu)))
        }
    }
}

fn nom_chez_le_service(hote: &dyn Hote, service: &str, playlist_id: &str) -> Option<String> {
    let rendu = hote.playlists_du_service(service).ok()?;
    rendu
        .get("playlists")?
        .as_array()?
        .iter()
        .find(|p| p.get("source_id").and_then(Value::as_str) == Some(playlist_id))
        .map(|p| texte(p, "name"))
        .filter(|n| !n.is_empty())
}

/// Les deux sources — locale et service — rendent les mêmes noms de champs
/// (`title`, `artist_name`, `duration_ms`, `isrc`) : une seule lecture suffit.
fn pistes_de(rendu: &Value) -> Vec<PisteSource> {
    rendu
        .get("tracks")
        .and_then(Value::as_array)
        .map(|liste| {
            liste
                .iter()
                .map(|t| PisteSource {
                    titre: texte(t, "title"),
                    artiste: texte(t, "artist_name"),
                    isrc: texte(t, "isrc"),
                    duree_ms: t
                        .get("duration_ms")
                        .and_then(Value::as_u64)
                        .unwrap_or_default(),
                    piste_locale: t.get("track_id").and_then(Value::as_i64),
                    piste_service: t
                        .get("source_id")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Le verdict d'un titre. **Lecture seule** : rien ici n'écrit.
fn apercevoir_une_piste(
    hote: &dyn Hote,
    origine: &Origine,
    cible: &Cible,
    piste: &PisteSource,
) -> Ligne {
    let mut ligne = Ligne {
        titre: piste.titre.clone(),
        artiste: piste.artiste.clone(),
        isrc: piste.isrc.clone(),
        duree_ms: piste.duree_ms,
        statut: Statut::Introuvable,
        raison: None,
        detail: None,
        score: None,
        cible_piste: None,
        cible_piste_locale: None,
        ecrite: false,
    };

    match cible {
        Cible::Local => {
            // Une piste locale se recopie telle quelle : c'est le même
            // identifiant, il n'y a rien à apparier.
            if let Some(id) = piste.piste_locale {
                ligne.statut = Statut::Appariee;
                ligne.cible_piste_locale = Some(id);
            } else {
                // Venue d'un service : l'interface hôte de la tranche 1 ne
                // sait pas chercher dans le catalogue local. On le DIT.
                ligne.raison = Some(raison::RECHERCHE_LOCALE_INDISPONIBLE.to_string());
            }
        }
        Cible::Service { service } => {
            // Même service de part et d'autre : la piste est déjà là-bas, son
            // identifiant suffit. Rechercher son propre catalogue pour se
            // retrouver soi-même n'ajoute que du risque.
            if let Origine::Service {
                service: depuis,
                id: _,
            } = origine
                && depuis == service
                && let Some(id) = piste.piste_service.clone()
            {
                ligne.statut = Statut::Appariee;
                ligne.cible_piste = Some(id);
                return ligne;
            }
            match hote.apparier(
                service,
                &piste.titre,
                &piste.artiste,
                &piste.isrc,
                piste.duree_ms,
            ) {
                Err(e) => {
                    ligne.raison = Some(raison::ERREUR_SERVICE.to_string());
                    ligne.detail = Some(e);
                }
                Ok(rendu) => {
                    let trouvee = rendu.get("matched").filter(|m| !m.is_null());
                    let score = rendu.get("score").and_then(Value::as_f64);
                    ligne.score = score;
                    match trouvee {
                        None => ligne.raison = Some(raison::AUCUN_RESULTAT.to_string()),
                        Some(piste_cible) => {
                            let approximative = rendu
                                .get("approximate")
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                            let id = texte(piste_cible, "source_id");
                            if id.is_empty() {
                                ligne.raison = Some(raison::AUCUN_RESULTAT.to_string());
                            } else if approximative {
                                // 🔴 Trouvé n'est pas apparié. Une écriture
                                // silencieuse ici pose chez l'utilisateur un
                                // titre qu'il n'a pas demandé, et qu'aucune
                                // capacité ne saurait retirer.
                                ligne.statut = Statut::Approximative;
                                ligne.cible_piste = Some(id);
                                ligne.raison = Some(raison::APPARIEMENT_APPROXIMATIF.to_string());
                            } else {
                                ligne.statut = Statut::Appariee;
                                ligne.cible_piste = Some(id);
                            }
                        }
                    }
                }
            }
        }
    }
    ligne
}

// ---------------------------------------------------------------------------
// Exécution, et reprise
// ---------------------------------------------------------------------------

/// Exécuter — ou REPRENDRE — un transfert déjà prévu.
///
/// `confirme` est l'accord explicite : sans lui, rien ne part. Et sans plan
/// dans le `kv`, il n'y a rien à exécuter : c'est ce qui rend l'aperçu
/// obligatoire, plutôt que recommandé.
pub fn executer(hote: &dyn Hote, transfert_id: &str, confirme: bool) -> Result<Plan, String> {
    if !confirme {
        return Err("accord explicite requis : `confirme` doit valoir true".to_string());
    }
    let mut plan = charger(hote, transfert_id)?;

    for i in 0..plan.blocs.len() {
        plan.blocs[i].erreur = None;
        // 0. Un bloc dont AUCUN titre n'est apparié ne fait rien créer.
        //
        //    C'est le cas d'un service vers la bibliothèque locale : la
        //    tranche 1 ne sait pas chercher dans le catalogue local, tous les
        //    titres sont rapportés introuvables — créer quand même une
        //    playlist vide chez l'utilisateur serait une écriture qu'il n'a
        //    pas demandée. Un bloc déjà créé lors d'un passage précédent,
        //    lui, garde sa cible : on ne défait rien.
        if plan.blocs[i].cible_playlist.is_none()
            && plan.blocs[i].cible_playlist_locale.is_none()
            && !plan.blocs[i].lignes.iter().any(Ligne::reste_a_ecrire)
        {
            plan.blocs[i].erreur = Some(AUCUN_TITRE_A_ECRIRE.to_string());
            enregistrer(hote, &plan)?;
            continue;
        }
        // 1. La playlist cible, si elle n'existe pas encore. Elle est
        //    enregistrée AUSSITÔT créée : une coupure juste après ne doit pas
        //    laisser une playlist orpheline qu'une reprise recréerait.
        if let Err(e) = creer_la_cible(hote, &plan.cible, &mut plan.blocs[i]) {
            plan.blocs[i].erreur = Some(e);
            enregistrer(hote, &plan)?;
            continue;
        }
        enregistrer(hote, &plan)?;

        // 2. Les titres qui restent à écrire, par tranches, en enregistrant
        //    entre chacune.
        loop {
            let tranche = prochaine_tranche(&plan.blocs[i]);
            if tranche.is_empty() {
                break;
            }
            match ecrire_une_tranche(hote, &plan.cible, &plan.blocs[i], &tranche) {
                Ok(ajoutees) => {
                    for index in &tranche {
                        plan.blocs[i].lignes[*index].ecrite = true;
                    }
                    plan.blocs[i].ajoutees += ajoutees;
                    enregistrer(hote, &plan)?;
                }
                Err(e) => {
                    // Rien n'est marqué écrit : la reprise recommencera cette
                    // tranche, et elle seule.
                    plan.blocs[i].erreur = Some(e);
                    enregistrer(hote, &plan)?;
                    break;
                }
            }
        }
    }

    plan.etat = if plan.reste_du_travail() {
        Etat::Partiel
    } else {
        Etat::Termine
    };
    enregistrer(hote, &plan)?;
    hote.journal(
        "info",
        &format!(
            "transfert_execute id={} etat={:?}",
            plan.transfert_id, plan.etat
        ),
    );
    Ok(plan)
}

/// Créer la playlist cible du bloc, si elle n'existe pas DÉJÀ.
///
/// C'est le cœur de la reprise : un bloc qui porte déjà son identifiant de
/// cible sort sans rien appeler. Aucun doublon ne peut donc naître d'un second
/// passage.
fn creer_la_cible(hote: &dyn Hote, cible: &Cible, bloc: &mut Bloc) -> Result<(), String> {
    match cible {
        Cible::Local => {
            if bloc.cible_playlist_locale.is_some() {
                return Ok(());
            }
            let rendu = hote.creer_playlist_locale(&bloc.nom_cible, None)?;
            let id = rendu
                .get("playlist_id")
                .and_then(Value::as_i64)
                .ok_or_else(|| "la playlist créée n'a pas d'identifiant".to_string())?;
            bloc.cible_playlist_locale = Some(id);
        }
        Cible::Service { service } => {
            if bloc.cible_playlist.is_some() {
                return Ok(());
            }
            let rendu = hote.creer_playlist_chez_le_service(service, &bloc.nom_cible, None)?;
            let id = texte(&rendu, "playlist_id");
            if id.is_empty() {
                return Err("la playlist créée n'a pas d'identifiant".to_string());
            }
            bloc.cible_playlist = Some(id);
        }
    }
    Ok(())
}

/// Les index des lignes de la prochaine tranche à écrire.
fn prochaine_tranche(bloc: &Bloc) -> Vec<usize> {
    bloc.lignes
        .iter()
        .enumerate()
        .filter(|(_, l)| l.reste_a_ecrire())
        .map(|(i, _)| i)
        .take(TAILLE_TRANCHE)
        .collect()
}

fn ecrire_une_tranche(
    hote: &dyn Hote,
    cible: &Cible,
    bloc: &Bloc,
    tranche: &[usize],
) -> Result<usize, String> {
    match cible {
        Cible::Local => {
            let playlist = bloc
                .cible_playlist_locale
                .ok_or_else(|| "playlist locale cible absente".to_string())?;
            let pistes: Vec<i64> = tranche
                .iter()
                .filter_map(|i| bloc.lignes[*i].cible_piste_locale)
                .collect();
            if pistes.is_empty() {
                return Ok(0);
            }
            let rendu = hote.ajouter_pistes_locales(playlist, &pistes)?;
            Ok(rendu
                .get("added")
                .and_then(Value::as_u64)
                .unwrap_or_default() as usize)
        }
        Cible::Service { service } => {
            let playlist = bloc
                .cible_playlist
                .clone()
                .ok_or_else(|| "playlist cible absente chez le service".to_string())?;
            let pistes: Vec<String> = tranche
                .iter()
                .filter_map(|i| bloc.lignes[*i].cible_piste.clone())
                .collect();
            if pistes.is_empty() {
                return Ok(0);
            }
            let rendu = hote.ajouter_pistes_chez_le_service(service, &playlist, &pistes)?;
            Ok(rendu
                .get("added")
                .and_then(Value::as_u64)
                .unwrap_or_default() as usize)
        }
    }
}

// ---------------------------------------------------------------------------
// Le carnet du greffon (`kv`)
// ---------------------------------------------------------------------------

/// Charger un plan. Son absence est le refus qui rend l'aperçu OBLIGATOIRE.
pub fn charger(hote: &dyn Hote, transfert_id: &str) -> Result<Plan, String> {
    let rendu = hote.kv_lire(&Plan::cle(transfert_id))?;
    if rendu.get("found").and_then(Value::as_bool) != Some(true) {
        return Err(format!(
            "aucun aperçu pour ce transfert : {transfert_id} — préparez-le d'abord"
        ));
    }
    let valeur = rendu
        .get("value")
        .cloned()
        .ok_or_else(|| "plan illisible".to_string())?;
    serde_json::from_value(valeur).map_err(|e| format!("plan illisible : {e}"))
}

fn enregistrer(hote: &dyn Hote, plan: &Plan) -> Result<(), String> {
    let valeur = serde_json::to_value(plan).map_err(|e| format!("plan insérialisable : {e}"))?;
    hote.kv_ecrire(&Plan::cle(&plan.transfert_id), &valeur)?;
    Ok(())
}

/// Le numéro du prochain transfert. Un greffon wasm n'a ni horloge ni aléa :
/// le compteur vit dans le `kv`, ce qui rend les identifiants reproductibles.
fn numeroter(hote: &dyn Hote) -> Result<String, String> {
    let precedent = hote
        .kv_lire(crate::plan::CLE_COMPTEUR)
        .ok()
        .and_then(|v| v.get("value").and_then(Value::as_u64))
        .unwrap_or(0);
    let suivant = precedent + 1;
    hote.kv_ecrire(crate::plan::CLE_COMPTEUR, &json!(suivant))?;
    Ok(format!("t{suivant}"))
}

/// Les transferts connus, du plus ancien au plus récent.
pub fn lister(hote: &dyn Hote) -> Result<Vec<String>, String> {
    let rendu = hote.kv_lister(crate::plan::PREFIXE_TRANSFERT)?;
    Ok(rendu
        .get("keys")
        .and_then(Value::as_array)
        .map(|liste| {
            liste
                .iter()
                .filter_map(Value::as_str)
                .filter_map(|k| k.strip_prefix(crate::plan::PREFIXE_TRANSFERT))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Rendu
// ---------------------------------------------------------------------------

/// Le plan tel que le client web le lit : les comptes en tête, le détail
/// ensuite. `ecritures` vaut 0 tant que rien n'est parti — c'est l'énoncé que
/// l'écran affiche avant de demander l'accord.
pub fn rendre(plan: &Plan) -> Value {
    let total = plan.comptes();
    json!({
        "transfert_id": plan.transfert_id,
        "etat": plan.etat,
        "cible": plan.cible,
        "comptes": total,
        "ecritures": total.ecrites,
        "reste_du_travail": plan.reste_du_travail(),
        "playlists": plan.blocs.iter().map(rendre_un_bloc).collect::<Vec<_>>(),
    })
}

fn rendre_un_bloc(bloc: &Bloc) -> Value {
    let comptes: Comptes = bloc.comptes();
    json!({
        "origine": bloc.origine,
        "nom": bloc.nom,
        "nom_cible": bloc.nom_cible,
        "cible_playlist": bloc.cible_playlist,
        "cible_playlist_locale": bloc.cible_playlist_locale,
        "erreur": bloc.erreur,
        "ajoutees": bloc.ajoutees,
        "comptes": comptes,
        "titres": bloc.lignes,
    })
}

fn texte(valeur: &Value, cle: &str) -> String {
    valeur
        .get(cle)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}
