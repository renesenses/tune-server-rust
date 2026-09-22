//! Les essais du convertisseur, contre un hôte de banc qui **compte ses
//! écritures**.
//!
//! C'est tout l'intérêt du trait [`Hote`] : « l'aperçu n'écrit rien » n'est pas
//! une phrase de documentation, c'est un compteur à zéro — et sa contre-épreuve
//! est le même compteur au-dessus de zéro après l'exécution.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use serde_json::{Value, json};

use crate::hote::{Hote, Reponse};
use crate::moteur::{self, Demande};
use crate::plan::{Cible, Etat, Origine, Statut, raison};

const CIBLE: &str = "cible";
const SOURCE_DISTANTE: &str = "amont";

// ---------------------------------------------------------------------------
// L'hôte de banc
// ---------------------------------------------------------------------------

/// Tout ce qui a été ÉCRIT chez l'utilisateur, et rien d'autre. Le carnet `kv`
/// n'y figure pas : ce n'est pas la bibliothèque de l'utilisateur, c'est la
/// mémoire du greffon.
#[derive(Default)]
struct Journal {
    creations_locales: Vec<String>,
    ajouts_locaux: Vec<(i64, Vec<i64>)>,
    creations_service: Vec<(String, String)>,
    ajouts_service: Vec<(String, String, Vec<String>)>,
}

impl Journal {
    fn ecritures(&self) -> usize {
        self.creations_locales.len()
            + self.ajouts_locaux.len()
            + self.creations_service.len()
            + self.ajouts_service.len()
    }
}

struct Banc {
    journal: RefCell<Journal>,
    kv: RefCell<HashMap<String, Value>>,
    /// playlists locales : id → (nom, pistes)
    locales: RefCell<HashMap<i64, (String, Vec<Value>)>>,
    /// playlists de service : (service, id) → (nom, pistes)
    distantes: HashMap<(String, String), (String, Vec<Value>)>,
    /// services annoncés : nom → sait écrire
    services: Vec<(String, bool)>,
    /// ce que le service CIBLE sait apparier : titre → (id, score, approximatif)
    appariements: HashMap<String, (String, f64, bool)>,
    /// Le RANG de l'appel d'ajout qui doit échouer (1 = le premier), 0 =
    /// aucun. Un rang, et non un compte : c'est ce qui permet de couper le
    /// DEUXIÈME bloc d'un lot en laissant passer le premier.
    panne_sur_ajout: Cell<usize>,
    ajouts_tentes: Cell<usize>,
    prochain_local: Cell<i64>,
    prochain_distant: Cell<usize>,
}

impl Banc {
    fn neuf() -> Self {
        Self {
            journal: RefCell::new(Journal::default()),
            kv: RefCell::new(HashMap::new()),
            locales: RefCell::new(HashMap::new()),
            distantes: HashMap::new(),
            services: vec![(CIBLE.to_string(), true)],
            appariements: HashMap::new(),
            panne_sur_ajout: Cell::new(0),
            ajouts_tentes: Cell::new(0),
            prochain_local: Cell::new(100),
            prochain_distant: Cell::new(0),
        }
    }

    fn avec_playlist_locale(self, id: i64, nom: &str, titres: &[(i64, &str)]) -> Self {
        let pistes = titres
            .iter()
            .map(|(piste, titre)| {
                json!({
                    "track_id": piste,
                    "title": titre,
                    "artist_name": "Charles Aznavour",
                    "duration_ms": 210_000,
                    "isrc": Value::Null,
                })
            })
            .collect();
        self.locales
            .borrow_mut()
            .insert(id, (nom.to_string(), pistes));
        self
    }

    fn avec_playlist_distante(
        mut self,
        service: &str,
        id: &str,
        nom: &str,
        titres: &[&str],
    ) -> Self {
        let pistes = titres
            .iter()
            .enumerate()
            .map(|(i, titre)| {
                json!({
                    "source_id": format!("{id}-{i}"),
                    "title": titre,
                    "artist_name": "Charles Aznavour",
                    "duration_ms": 210_000,
                })
            })
            .collect();
        self.distantes.insert(
            (service.to_string(), id.to_string()),
            (nom.to_string(), pistes),
        );
        if !self.services.iter().any(|(n, _)| n == service) {
            self.services.push((service.to_string(), true));
        }
        self
    }

    /// Le service cible sait apparier ce titre, avec certitude.
    fn apparie(mut self, titre: &str, id: &str) -> Self {
        self.appariements
            .insert(titre.to_string(), (id.to_string(), 0.95, false));
        self
    }

    /// Le service cible trouve quelque chose, mais sans certitude.
    fn apparie_a_peu_pres(mut self, titre: &str, id: &str) -> Self {
        self.appariements
            .insert(titre.to_string(), (id.to_string(), 0.55, true));
        self
    }

    fn ecritures(&self) -> usize {
        self.journal.borrow().ecritures()
    }
}

impl Hote for Banc {
    fn journal(&self, _niveau: &str, _message: &str) {}

    fn playlists_locales(&self, _limite: i64, _decalage: i64) -> Reponse {
        let locales = self.locales.borrow();
        let mut fiches: Vec<Value> = locales
            .iter()
            .map(
                |(id, (nom, pistes))| json!({ "id": id, "name": nom, "track_count": pistes.len() }),
            )
            .collect();
        fiches.sort_by_key(|f| f["id"].as_i64().unwrap_or_default());
        Ok(json!({ "count": fiches.len(), "playlists": fiches }))
    }

    fn pistes_locales(&self, playlist_id: i64) -> Reponse {
        let locales = self.locales.borrow();
        let (nom, pistes) = locales
            .get(&playlist_id)
            .ok_or_else(|| format!("playlist introuvable : {playlist_id}"))?;
        Ok(json!({
            "playlist_id": playlist_id,
            "name": nom,
            "count": pistes.len(),
            "tracks": pistes,
        }))
    }

    fn services(&self) -> Reponse {
        let fiches: Vec<Value> = self
            .services
            .iter()
            .map(|(nom, ecrit)| {
                json!({ "name": nom, "authenticated": true, "supports_write": ecrit })
            })
            .collect();
        Ok(json!({ "count": fiches.len(), "services": fiches }))
    }

    fn playlists_du_service(&self, service: &str) -> Reponse {
        let fiches: Vec<Value> = self
            .distantes
            .iter()
            .filter(|((s, _), _)| s == service)
            .map(|((_, id), (nom, pistes))| {
                json!({ "source_id": id, "name": nom, "track_count": pistes.len() })
            })
            .collect();
        Ok(json!({ "service": service, "count": fiches.len(), "playlists": fiches }))
    }

    fn pistes_du_service(&self, service: &str, playlist_id: &str) -> Reponse {
        let (_, pistes) = self
            .distantes
            .get(&(service.to_string(), playlist_id.to_string()))
            .ok_or_else(|| format!("playlist introuvable : {playlist_id}"))?;
        Ok(json!({
            "service": service,
            "playlist_id": playlist_id,
            "count": pistes.len(),
            "tracks": pistes,
        }))
    }

    fn apparier(
        &self,
        service: &str,
        titre: &str,
        _artiste: &str,
        _isrc: &str,
        _duree_ms: u64,
    ) -> Reponse {
        match self.appariements.get(titre) {
            None => Ok(json!({ "service": service, "matched": Value::Null })),
            Some((id, score, approximatif)) => Ok(json!({
                "service": service,
                "matched": { "source_id": id, "title": titre },
                "score": score,
                "approximate": approximatif,
            })),
        }
    }

    fn kv_lire(&self, cle: &str) -> Reponse {
        Ok(match self.kv.borrow().get(cle) {
            Some(valeur) => json!({ "key": cle, "found": true, "value": valeur }),
            None => json!({ "key": cle, "found": false, "value": Value::Null }),
        })
    }

    fn kv_ecrire(&self, cle: &str, valeur: &Value) -> Reponse {
        self.kv.borrow_mut().insert(cle.to_string(), valeur.clone());
        Ok(json!({ "ok": true, "key": cle }))
    }

    fn kv_lister(&self, prefixe: &str) -> Reponse {
        let mut cles: Vec<String> = self
            .kv
            .borrow()
            .keys()
            .filter(|k| k.starts_with(prefixe))
            .cloned()
            .collect();
        cles.sort();
        Ok(json!({ "count": cles.len(), "keys": cles }))
    }

    fn creer_playlist_locale(&self, nom: &str, _description: Option<&str>) -> Reponse {
        self.journal
            .borrow_mut()
            .creations_locales
            .push(nom.to_string());
        let id = self.prochain_local.get();
        self.prochain_local.set(id + 1);
        self.locales
            .borrow_mut()
            .insert(id, (nom.to_string(), Vec::new()));
        Ok(json!({ "playlist_id": id, "name": nom }))
    }

    fn ajouter_pistes_locales(&self, playlist_id: i64, pistes: &[i64]) -> Reponse {
        self.journal
            .borrow_mut()
            .ajouts_locaux
            .push((playlist_id, pistes.to_vec()));
        Ok(json!({ "ok": true, "added": pistes.len(), "demandees": pistes.len() }))
    }

    fn creer_playlist_chez_le_service(
        &self,
        service: &str,
        nom: &str,
        _description: Option<&str>,
    ) -> Reponse {
        self.journal
            .borrow_mut()
            .creations_service
            .push((service.to_string(), nom.to_string()));
        let n = self.prochain_distant.get() + 1;
        self.prochain_distant.set(n);
        Ok(json!({ "service": service, "playlist_id": format!("neuve-{n}"), "name": nom }))
    }

    fn ajouter_pistes_chez_le_service(
        &self,
        service: &str,
        playlist_id: &str,
        pistes: &[String],
    ) -> Reponse {
        let rang = self.ajouts_tentes.get() + 1;
        self.ajouts_tentes.set(rang);
        if rang == self.panne_sur_ajout.get() {
            return Err("service injoignable".to_string());
        }
        self.journal.borrow_mut().ajouts_service.push((
            service.to_string(),
            playlist_id.to_string(),
            pistes.to_vec(),
        ));
        Ok(json!({ "ok": true, "added": pistes.len(), "demandees": pistes.len() }))
    }
}

// ---------------------------------------------------------------------------
// Socle des essais
// ---------------------------------------------------------------------------

fn vers_le_service() -> Cible {
    Cible::Service {
        service: CIBLE.to_string(),
    }
}

fn demande_locale(ids: &[i64], cible: Cible) -> Demande {
    Demande {
        sources: ids.iter().map(|id| Origine::Local { id: *id }).collect(),
        cible,
        suffixe: String::new(),
    }
}

/// Un banc courant : une playlist locale de trois titres, dont deux que le
/// service cible sait apparier.
fn banc_courant() -> Banc {
    Banc::neuf()
        .avec_playlist_locale(
            1,
            "Mes classiques",
            &[(10, "La Bohème"), (11, "Emmenez-moi"), (12, "Inédit")],
        )
        .apparie("La Bohème", "cible-1")
        .apparie("Emmenez-moi", "cible-2")
}

// ---------------------------------------------------------------------------
// 1. L'aperçu n'écrit rien
// ---------------------------------------------------------------------------

#[test]
fn l_apercu_n_ecrit_rien() {
    let banc = banc_courant();
    let plan = moteur::preparer(&banc, &demande_locale(&[1], vers_le_service())).unwrap();

    assert_eq!(
        banc.ecritures(),
        0,
        "l'aperçu ne doit toucher ni la bibliothèque ni un service — \
         journal : {} créations locales, {} ajouts locaux, {} créations de \
         service, {} ajouts de service",
        banc.journal.borrow().creations_locales.len(),
        banc.journal.borrow().ajouts_locaux.len(),
        banc.journal.borrow().creations_service.len(),
        banc.journal.borrow().ajouts_service.len(),
    );
    assert_eq!(plan.etat, Etat::Apercu);
    let comptes = plan.comptes();
    assert_eq!(comptes.total, 3);
    assert_eq!(comptes.appariees, 2);
    assert_eq!(comptes.introuvables, 1);
    assert_eq!(comptes.ecrites, 0);

    // Contre-épreuve : le même compteur, après l'accord explicite, bouge.
    moteur::executer(&banc, &plan.transfert_id, true).unwrap();
    assert!(
        banc.ecritures() > 0,
        "sans cette contre-épreuve, un hôte qui ne compte rien rendrait le \
         premier constat vide de sens"
    );
}

#[test]
fn executer_sans_apercu_refuse() {
    let banc = banc_courant();
    let erreur = moteur::executer(&banc, "t404", true)
        .expect_err("un transfert sans aperçu ne doit pas s'exécuter");
    assert!(erreur.contains("aucun aperçu"), "{erreur}");
    assert_eq!(banc.ecritures(), 0);
}

#[test]
fn executer_sans_accord_explicite_refuse() {
    let banc = banc_courant();
    let plan = moteur::preparer(&banc, &demande_locale(&[1], vers_le_service())).unwrap();
    let erreur = moteur::executer(&banc, &plan.transfert_id, false)
        .expect_err("sans `confirme`, rien ne doit partir");
    assert!(erreur.contains("accord explicite"), "{erreur}");
    assert_eq!(banc.ecritures(), 0);
}

// ---------------------------------------------------------------------------
// 2. La reprise ne duplique pas
// ---------------------------------------------------------------------------

#[test]
fn un_lot_rejoue_ne_duplique_rien() {
    let banc = banc_courant();
    let plan = moteur::preparer(&banc, &demande_locale(&[1], vers_le_service())).unwrap();
    let premier = moteur::executer(&banc, &plan.transfert_id, true).unwrap();
    assert_eq!(premier.etat, Etat::Termine);
    let apres_le_premier = banc.ecritures();
    assert_eq!(banc.journal.borrow().creations_service.len(), 1);
    assert_eq!(banc.journal.borrow().ajouts_service.len(), 1);

    // Rejouer : le plan relu dit que tout est écrit, donc plus AUCUN appel.
    let second = moteur::executer(&banc, &plan.transfert_id, true).unwrap();
    assert_eq!(second.etat, Etat::Termine);
    assert_eq!(
        banc.ecritures(),
        apres_le_premier,
        "un second passage ne doit ni recréer la playlist ni réécrire les titres"
    );
    assert_eq!(banc.journal.borrow().creations_service.len(), 1);
    assert_eq!(banc.journal.borrow().ajouts_service.len(), 1);
}

#[test]
fn un_lot_interrompu_reprend_sans_recreer_la_cible() {
    let banc = banc_courant();
    banc.panne_sur_ajout.set(1); // le service coupe juste après la création
    let plan = moteur::preparer(&banc, &demande_locale(&[1], vers_le_service())).unwrap();

    let coupe = moteur::executer(&banc, &plan.transfert_id, true).unwrap();
    assert_eq!(coupe.etat, Etat::Partiel, "la coupure doit se voir");
    assert_eq!(banc.journal.borrow().creations_service.len(), 1);
    assert_eq!(
        banc.journal.borrow().ajouts_service.len(),
        0,
        "rien n'est passé : aucune ligne ne doit être marquée écrite"
    );
    assert_eq!(coupe.comptes().ecrites, 0);

    // La reprise : la playlist existe déjà, on ne la recrée pas.
    let repris = moteur::executer(&banc, &plan.transfert_id, true).unwrap();
    assert_eq!(repris.etat, Etat::Termine);
    assert_eq!(
        banc.journal.borrow().creations_service.len(),
        1,
        "la playlist cible ne doit être créée qu'UNE fois"
    );
    assert_eq!(banc.journal.borrow().ajouts_service.len(), 1);
    assert_eq!(repris.comptes().ecrites, 2);
    // Et c'est bien la même playlist qu'au premier passage.
    assert_eq!(
        repris.blocs[0].cible_playlist.as_deref(),
        Some("neuve-1"),
        "la reprise doit viser la playlist déjà créée"
    );
}

// ---------------------------------------------------------------------------
// 3. Ce qu'on ne trouve pas, on le DIT
// ---------------------------------------------------------------------------

#[test]
fn un_titre_introuvable_est_rapporte_et_jamais_invente() {
    let banc = banc_courant();
    let plan = moteur::preparer(&banc, &demande_locale(&[1], vers_le_service())).unwrap();

    let introuvable = plan.blocs[0]
        .lignes
        .iter()
        .find(|l| l.titre == "Inédit")
        .expect("le titre doit figurer au rapport");
    assert_eq!(introuvable.statut, Statut::Introuvable);
    assert_eq!(introuvable.raison.as_deref(), Some(raison::AUCUN_RESULTAT));
    assert!(
        introuvable.cible_piste.is_none(),
        "aucun identifiant ne doit être inventé pour un titre qu'on n'a pas trouvé"
    );

    moteur::executer(&banc, &plan.transfert_id, true).unwrap();
    let ecrits = &banc.journal.borrow().ajouts_service[0].2;
    assert_eq!(
        ecrits.as_slice(),
        &["cible-1".to_string(), "cible-2".to_string()]
    );
}

#[test]
fn un_appariement_approximatif_n_est_jamais_ecrit() {
    // 🔴 Trouvé n'est pas apparié. Écrire un « à peu près » pose chez
    // l'utilisateur un titre qu'il n'a pas demandé — et aucune capacité de
    // l'interface hôte ne saurait l'en retirer.
    let banc = banc_courant().apparie_a_peu_pres("Inédit", "cible-3");
    let plan = moteur::preparer(&banc, &demande_locale(&[1], vers_le_service())).unwrap();

    let doute = plan.blocs[0]
        .lignes
        .iter()
        .find(|l| l.titre == "Inédit")
        .unwrap();
    assert_eq!(doute.statut, Statut::Approximative);
    assert_eq!(
        doute.raison.as_deref(),
        Some(raison::APPARIEMENT_APPROXIMATIF)
    );
    assert_eq!(doute.cible_piste.as_deref(), Some("cible-3"));

    moteur::executer(&banc, &plan.transfert_id, true).unwrap();
    let ecrits = &banc.journal.borrow().ajouts_service[0].2;
    assert!(
        !ecrits.contains(&"cible-3".to_string()),
        "l'approximatif est RAPPORTÉ, jamais écrit — vu {ecrits:?}"
    );
}

#[test]
fn vers_la_bibliotheque_locale_une_source_distante_ne_s_invente_pas() {
    // La tranche 1 n'expose aucune recherche dans le catalogue local : on le
    // dit, plutôt que d'apparier au hasard. Et surtout : on ne crée alors
    // AUCUNE playlist locale vide.
    let banc = Banc::neuf().avec_playlist_distante(
        SOURCE_DISTANTE,
        "pl-1",
        "Découvertes",
        &["La Bohème", "Emmenez-moi"],
    );
    let demande = Demande {
        sources: vec![Origine::Service {
            service: SOURCE_DISTANTE.to_string(),
            id: "pl-1".to_string(),
        }],
        cible: Cible::Local,
        suffixe: String::new(),
    };
    let plan = moteur::preparer(&banc, &demande).unwrap();
    assert_eq!(plan.blocs[0].nom, "Découvertes");
    assert!(
        plan.blocs[0]
            .lignes
            .iter()
            .all(|l| l.statut == Statut::Introuvable
                && l.raison.as_deref() == Some(raison::RECHERCHE_LOCALE_INDISPONIBLE))
    );

    let execute = moteur::executer(&banc, &plan.transfert_id, true).unwrap();
    assert_eq!(
        banc.ecritures(),
        0,
        "rien n'étant appariable, rien ne doit être créé"
    );
    assert_eq!(
        execute.blocs[0].erreur.as_deref(),
        Some(moteur::AUCUN_TITRE_A_ECRIRE)
    );
    assert_eq!(execute.etat, Etat::Termine);
}

// ---------------------------------------------------------------------------
// 4. Le lot
// ---------------------------------------------------------------------------

#[test]
fn le_lot_traite_plusieurs_playlists_en_une_passe() {
    let banc = banc_courant()
        .avec_playlist_locale(2, "Soirée", &[(20, "La Bohème")])
        .avec_playlist_locale(3, "Route", &[(30, "Emmenez-moi")]);
    let plan = moteur::preparer(&banc, &demande_locale(&[1, 2, 3], vers_le_service())).unwrap();
    assert_eq!(plan.blocs.len(), 3);
    assert_eq!(banc.ecritures(), 0);

    let execute = moteur::executer(&banc, &plan.transfert_id, true).unwrap();
    assert_eq!(execute.etat, Etat::Termine);
    let noms: Vec<String> = banc
        .journal
        .borrow()
        .creations_service
        .iter()
        .map(|(_, nom)| nom.clone())
        .collect();
    assert_eq!(noms, vec!["Mes classiques", "Soirée", "Route"]);
    assert_eq!(execute.comptes().ecrites, 4);
}

#[test]
fn un_lot_est_repris_bloc_par_bloc_sans_toucher_au_premier() {
    // La coupure frappe la DEUXIÈME playlist : la première est déjà passée et
    // ne doit pas être rejouée.
    let banc = banc_courant().avec_playlist_locale(2, "Soirée", &[(20, "La Bohème")]);
    let plan = moteur::preparer(&banc, &demande_locale(&[1, 2], vers_le_service())).unwrap();
    // Le DEUXIÈME appel d'ajout échoue : le premier bloc passe, le second non.
    banc.panne_sur_ajout.set(2);

    let coupe = moteur::executer(&banc, &plan.transfert_id, true).unwrap();
    assert_eq!(coupe.etat, Etat::Partiel);
    assert_eq!(
        coupe.blocs[0].comptes().ecrites,
        2,
        "le premier bloc est passé"
    );
    assert_eq!(coupe.blocs[1].comptes().ecrites, 0, "le second a coupé");
    assert_eq!(banc.journal.borrow().creations_service.len(), 2);
    assert_eq!(banc.journal.borrow().ajouts_service.len(), 1);

    let repris = moteur::executer(&banc, &plan.transfert_id, true).unwrap();
    assert_eq!(repris.etat, Etat::Termine);
    assert_eq!(
        banc.journal.borrow().creations_service.len(),
        2,
        "aucune playlist ne doit être recréée"
    );
    let ajouts = banc.journal.borrow().ajouts_service.clone();
    assert_eq!(ajouts.len(), 2, "seul le bloc coupé est rejoué");
    assert_eq!(ajouts[1].1, "neuve-2");
    assert_eq!(ajouts[1].2, vec!["cible-1".to_string()]);
}

#[test]
fn le_transfert_dans_le_meme_service_reprend_les_identifiants_sans_rechercher() {
    // Dupliquer une playlist chez le même service : la piste est déjà là-bas.
    // La rechercher n'ajouterait que du risque.
    let banc =
        Banc::neuf().avec_playlist_distante(CIBLE, "pl-1", "Mes classiques", &["Un", "Deux"]);
    let demande = Demande {
        sources: vec![Origine::Service {
            service: CIBLE.to_string(),
            id: "pl-1".to_string(),
        }],
        cible: vers_le_service(),
        suffixe: " (copie)".to_string(),
    };
    let plan = moteur::preparer(&banc, &demande).unwrap();
    assert_eq!(plan.blocs[0].nom_cible, "Mes classiques (copie)");
    assert!(
        plan.blocs[0]
            .lignes
            .iter()
            .all(|l| l.statut == Statut::Appariee)
    );
    assert_eq!(
        plan.blocs[0].lignes[0].cible_piste.as_deref(),
        Some("pl-1-0")
    );
    assert_eq!(banc.ecritures(), 0);
}

// ---------------------------------------------------------------------------
// 5. La cible
// ---------------------------------------------------------------------------

#[test]
fn une_cible_non_authentifiee_est_refusee_avant_tout_appariement() {
    let banc = banc_courant();
    let demande = demande_locale(
        &[1],
        Cible::Service {
            service: "inconnu".to_string(),
        },
    );
    let erreur = moteur::preparer(&banc, &demande).expect_err("service inconnu");
    assert!(erreur.contains("non authentifié"), "{erreur}");
    assert_eq!(banc.ecritures(), 0);
}

#[test]
fn une_cible_en_lecture_seule_est_refusee() {
    let mut banc = banc_courant();
    banc.services = vec![("lecture-seule".to_string(), false)];
    let demande = demande_locale(
        &[1],
        Cible::Service {
            service: "lecture-seule".to_string(),
        },
    );
    let erreur = moteur::preparer(&banc, &demande).expect_err("service en lecture seule");
    assert!(erreur.contains("ne sait pas écrire"), "{erreur}");
}

#[test]
fn une_demande_sans_source_est_refusee() {
    let banc = banc_courant();
    let erreur = moteur::preparer(&banc, &demande_locale(&[], vers_le_service()))
        .expect_err("aucune source");
    assert!(erreur.contains("aucune playlist source"), "{erreur}");
}

// ---------------------------------------------------------------------------
// 6. Les routes
// ---------------------------------------------------------------------------

#[test]
fn les_routes_repondent_le_contrat_attendu() {
    let banc = banc_courant();

    let sources = crate::routage::repondre(
        &banc,
        &json!({ "method": "GET", "path": "/sources", "query": "", "body": Value::Null }),
    );
    assert_eq!(sources["status"], 200);
    assert_eq!(sources["body"]["locales"][0]["name"], "Mes classiques");

    let apercu = crate::routage::repondre(
        &banc,
        &json!({
            "method": "POST",
            "path": "/apercu",
            "query": "",
            "body": { "sources": [{ "kind": "local", "id": 1 }], "cible": { "kind": "service", "service": CIBLE } },
        }),
    );
    assert_eq!(apercu["status"], 200);
    assert_eq!(apercu["body"]["ecritures"], 0);
    let id = apercu["body"]["transfert_id"].as_str().unwrap().to_string();
    assert_eq!(banc.ecritures(), 0);

    // Sans accord : refus, et toujours rien d'écrit.
    let refus = crate::routage::repondre(
        &banc,
        &json!({ "method": "POST", "path": "/executer", "query": "",
                 "body": { "transfert_id": id } }),
    );
    assert_eq!(refus["status"], 400);
    assert_eq!(banc.ecritures(), 0);

    let execute = crate::routage::repondre(
        &banc,
        &json!({ "method": "POST", "path": "/executer", "query": "",
                 "body": { "transfert_id": id, "confirme": true } }),
    );
    assert_eq!(execute["status"], 200);
    assert_eq!(execute["body"]["etat"], "termine");
    assert_eq!(execute["body"]["ecritures"], 2);

    let rapport = crate::routage::repondre(
        &banc,
        &json!({ "method": "GET", "path": "/transfert", "query": format!("transfert_id={id}"), "body": Value::Null }),
    );
    assert_eq!(rapport["status"], 200);
    assert_eq!(rapport["body"]["transfert_id"], id);

    let liste = crate::routage::repondre(
        &banc,
        &json!({ "method": "GET", "path": "/transferts", "query": "", "body": Value::Null }),
    );
    assert_eq!(liste["status"], 200);
    assert_eq!(liste["body"]["count"], 1);

    let inconnue = crate::routage::repondre(
        &banc,
        &json!({ "method": "GET", "path": "/ailleurs", "query": "", "body": Value::Null }),
    );
    assert_eq!(inconnue["status"], 404);
}

#[test]
fn aucune_route_ne_supprime() {
    // Une garde de surface : le greffon ne doit offrir AUCUN geste destructeur,
    // même si l'interface hôte venait un jour à en exposer un.
    let banc = banc_courant();
    for (methode, chemin) in [
        ("DELETE", "/transfert"),
        ("POST", "/supprimer"),
        ("DELETE", "/apercu"),
        ("POST", "/annuler"),
    ] {
        let rendu = crate::routage::repondre(
            &banc,
            &json!({ "method": methode, "path": chemin, "query": "", "body": Value::Null }),
        );
        assert_eq!(
            rendu["status"], 404,
            "{methode} {chemin} ne doit exister sous aucune forme"
        );
    }
    assert_eq!(banc.ecritures(), 0);
}
