//! L'hôte de banc : un double du serveur, pour jouer le moteur sans wasmtime,
//! sans base et **sans jamais toucher un vrai service**.
//!
//! Il compte ce qu'on lui demande. C'est ce qui permet de prouver qu'un aperçu
//! n'écrit rien : on ne lit pas le code pour s'en convaincre, on met le double
//! en refus d'écriture et on regarde l'aperçu réussir quand même.
//!
//! Depuis #4718, les playlists du banc **bougent** : une playlist créée existe
//! (vide), un ajout y apparaît, et [`HoteDeBanc::remplacer_pistes`] simule
//! l'utilisateur qui retouche une playlist chez son service. Sans cela, un
//! retour en arrière ne pourrait rien comparer. Le banc n'a, comme l'hôte réel,
//! **aucune** méthode de suppression : ce qui disparaît d'une playlist n'y
//! disparaît que par `remplacer_pistes`, c'est-à-dire par l'utilisateur.

use std::cell::RefCell;
use std::collections::HashMap;

use serde_json::{Value, json};

use crate::hote::Hote;

/// L'heure de départ du banc : 24/09/2026, pour des dates lisibles.
pub const HEURE_DU_BANC_MS: u64 = 1_790_200_000_000;

/// Une piste du banc.
#[derive(Clone, Debug)]
pub struct Piste {
    pub id: String,
    pub titre: String,
    pub artiste: String,
    pub duree_ms: u64,
}

impl Piste {
    pub fn new(id: &str, titre: &str, artiste: &str, duree_ms: u64) -> Self {
        Self {
            id: id.into(),
            titre: titre.into(),
            artiste: artiste.into(),
            duree_ms,
        }
    }
    fn json(&self) -> Value {
        json!({
            "source_id": self.id,
            "title": self.titre,
            "artist_name": self.artiste,
            "duration_ms": self.duree_ms,
            "isrc": "",
        })
    }
}

/// Ce que la cible rend pour un titre donné.
#[derive(Clone)]
pub enum Verdict {
    /// Un candidat, avec son score et le drapeau `approximate` de l'hôte.
    Candidat {
        piste: Piste,
        score: f64,
        approximatif: bool,
    },
    /// Le service n'a rien.
    Rien,
    /// L'appel échoue.
    Erreur(String),
}

impl Verdict {
    pub fn exact(piste: Piste) -> Self {
        Verdict::Candidat {
            piste,
            score: 0.95,
            approximatif: false,
        }
    }
    pub fn flou(piste: Piste, score: f64) -> Self {
        Verdict::Candidat {
            piste,
            score,
            approximatif: true,
        }
    }
}

#[derive(Default)]
pub struct Journaux {
    pub creations: Vec<(String, String)>,
    pub ajouts: Vec<(String, Vec<String>)>,
    pub ecritures_kv: usize,
    /// Toutes les opérations dans l'ordre : `kv:<clé>`, `creation:<id>`,
    /// `ajout:<id>`. C'est ce qui prouve qu'un snapshot est pris AVANT le
    /// premier ajout, et pas seulement « quelque part ».
    pub operations: Vec<String>,
}

pub struct HoteDeBanc {
    /// Playlists de départ : id → (nom, pistes).
    pub source: HashMap<String, (String, Vec<Piste>)>,
    /// Verdict de la cible, par titre source.
    pub cible: HashMap<String, Verdict>,
    /// Le contenu COURANT des playlists qui ont bougé depuis le départ
    /// (créées, versées, retouchées). Prime sur `source`.
    modifs: RefCell<HashMap<String, (String, Vec<Piste>)>>,
    /// Stockage clé/valeur.
    kv: RefCell<HashMap<String, Value>>,
    /// Ce qui a été demandé.
    pub journaux: RefCell<Journaux>,
    /// Si vrai, toute écriture CHEZ UN SERVICE échoue au lieu d'aboutir.
    /// C'est le piège qui prouve qu'un aperçu n'écrit rien.
    pub ecriture_interdite: bool,
    /// Nombre d'ajouts réussis avant que le service ne tombe. `None` = jamais.
    pub tomber_apres_ajouts: RefCell<Option<usize>>,
    compteur_creations: RefCell<usize>,
    maintenant: RefCell<u64>,
}

impl HoteDeBanc {
    pub fn new() -> Self {
        Self {
            source: HashMap::new(),
            cible: HashMap::new(),
            modifs: RefCell::new(HashMap::new()),
            kv: RefCell::new(HashMap::new()),
            journaux: RefCell::new(Journaux::default()),
            ecriture_interdite: false,
            tomber_apres_ajouts: RefCell::new(None),
            compteur_creations: RefCell::new(0),
            maintenant: RefCell::new(HEURE_DU_BANC_MS),
        }
    }

    pub fn avec_playlist(mut self, id: &str, nom: &str, pistes: Vec<Piste>) -> Self {
        self.source.insert(id.into(), (nom.into(), pistes));
        self
    }

    pub fn avec_verdict(mut self, titre_source: &str, v: Verdict) -> Self {
        self.cible.insert(titre_source.into(), v);
        self
    }

    pub fn ecriture_interdite(mut self) -> Self {
        self.ecriture_interdite = true;
        self
    }

    pub fn creations(&self) -> Vec<(String, String)> {
        self.journaux.borrow().creations.clone()
    }

    pub fn ajouts(&self) -> Vec<(String, Vec<String>)> {
        self.journaux.borrow().ajouts.clone()
    }

    pub fn operations(&self) -> Vec<String> {
        self.journaux.borrow().operations.clone()
    }

    /// Faire avancer l'horloge du banc.
    pub fn avancer(&self, ms: u64) {
        *self.maintenant.borrow_mut() += ms;
    }

    /// L'utilisateur retouche une playlist chez son service : on en remplace
    /// le contenu. C'est la SEULE façon dont une piste quitte une playlist du
    /// banc — le greffon, lui, n'en a aucune.
    pub fn remplacer_pistes(&self, id: &str, pistes: Vec<Piste>) {
        let nom = self
            .contenu(id)
            .map(|(n, _)| n)
            .unwrap_or_else(|| id.to_string());
        self.modifs
            .borrow_mut()
            .insert(id.to_string(), (nom, pistes));
    }

    /// Les identifiants actuellement dans une playlist.
    pub fn ids_de(&self, id: &str) -> Vec<String> {
        self.contenu(id)
            .map(|(_, p)| p.into_iter().map(|p| p.id).collect())
            .unwrap_or_default()
    }

    /// Le contenu courant d'une playlist : ce qui a bougé, sinon le départ.
    pub fn contenu(&self, id: &str) -> Option<(String, Vec<Piste>)> {
        if let Some(c) = self.modifs.borrow().get(id) {
            return Some(c.clone());
        }
        self.source.get(id).cloned()
    }

    /// Retrouver la fiche d'une piste par son identifiant, où qu'elle soit
    /// connue du banc — pour qu'un ajout fasse apparaître une vraie piste.
    fn piste_par_id(&self, id: &str) -> Piste {
        for v in self.cible.values() {
            if let Verdict::Candidat { piste, .. } = v
                && piste.id == id
            {
                return piste.clone();
            }
        }
        for (_, pistes) in self.source.values() {
            if let Some(p) = pistes.iter().find(|p| p.id == id) {
                return p.clone();
            }
        }
        for (_, pistes) in self.modifs.borrow().values() {
            if let Some(p) = pistes.iter().find(|p| p.id == id) {
                return p.clone();
            }
        }
        Piste::new(id, id, "", 0)
    }

    fn refuser_si_lecture_seule(&self) -> Result<(), String> {
        if self.ecriture_interdite {
            Err("ECRITURE INTERDITE : le banc est en lecture seule".into())
        } else {
            Ok(())
        }
    }

    fn noter(&self, operation: String) {
        self.journaux.borrow_mut().operations.push(operation);
    }

    fn creer(&self, id: String, name: &str) {
        self.journaux
            .borrow_mut()
            .creations
            .push((id.clone(), name.to_string()));
        self.noter(format!("creation:{id}"));
        self.modifs
            .borrow_mut()
            .insert(id, (name.to_string(), Vec::new()));
    }

    fn verser(&self, playlist_id: &str, track_ids: &[String]) -> Result<(), String> {
        {
            let mut restant = self.tomber_apres_ajouts.borrow_mut();
            if let Some(n) = restant.as_mut() {
                if *n == 0 {
                    return Err("service_indisponible".into());
                }
                *n -= 1;
            }
        }
        self.journaux
            .borrow_mut()
            .ajouts
            .push((playlist_id.to_string(), track_ids.to_vec()));
        self.noter(format!("ajout:{playlist_id}"));
        let (nom, mut pistes) = self
            .contenu(playlist_id)
            .unwrap_or_else(|| (playlist_id.to_string(), Vec::new()));
        for id in track_ids {
            pistes.push(self.piste_par_id(id));
        }
        self.modifs
            .borrow_mut()
            .insert(playlist_id.to_string(), (nom, pistes));
        Ok(())
    }

    fn fiche_playlist(&self, id: &str) -> Result<(String, Vec<Piste>), String> {
        self.contenu(id)
            .ok_or_else(|| format!("playlist introuvable : {id}"))
    }
}

impl Hote for HoteDeBanc {
    fn journal(&self, _niveau: &str, _message: &str) {}

    fn maintenant_ms(&self) -> u64 {
        *self.maintenant.borrow()
    }

    fn playlist_tracks(&self, playlist_id: i64) -> Result<Value, String> {
        let (nom, pistes) = self.fiche_playlist(&playlist_id.to_string())?;
        Ok(json!({
            "playlist_id": playlist_id,
            "name": nom,
            "count": pistes.len(),
            "tracks": pistes
                .iter()
                .map(|p| {
                    let mut v = p.json();
                    // Une piste locale porte son identifiant ENTIER sous
                    // `track_id`, comme la fiche de l'hôte réel.
                    if let Ok(n) = p.id.parse::<i64>() {
                        v["track_id"] = json!(n);
                    }
                    v
                })
                .collect::<Vec<_>>(),
        }))
    }

    fn playlist_create(&self, name: &str, _description: Option<&str>) -> Result<Value, String> {
        self.refuser_si_lecture_seule()?;
        let n = {
            let mut n = self.compteur_creations.borrow_mut();
            *n += 1;
            *n
        };
        let id = 1000 + n as i64;
        self.creer(id.to_string(), name);
        Ok(json!({ "playlist_id": id, "name": name }))
    }

    fn playlist_add_tracks(&self, playlist_id: i64, track_ids: &[i64]) -> Result<Value, String> {
        self.refuser_si_lecture_seule()?;
        let ids: Vec<String> = track_ids.iter().map(i64::to_string).collect();
        self.verser(&playlist_id.to_string(), &ids)?;
        Ok(json!({ "ok": true, "added": ids.len(), "demandees": ids.len() }))
    }

    fn streaming_playlists(&self, service: &str) -> Result<Value, String> {
        let mut ids: Vec<String> = self.source.keys().cloned().collect();
        for id in self.modifs.borrow().keys() {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        ids.sort();
        let playlists: Vec<Value> = ids
            .into_iter()
            .filter_map(|id| {
                let (nom, pistes) = self.contenu(&id)?;
                Some(json!({ "source_id": id, "name": nom, "track_count": pistes.len() }))
            })
            .collect();
        Ok(json!({ "service": service, "count": playlists.len(), "playlists": playlists }))
    }

    fn streaming_playlist_tracks(&self, service: &str, playlist_id: &str) -> Result<Value, String> {
        let (_, pistes) = self.fiche_playlist(playlist_id)?;
        Ok(json!({
            "service": service,
            "playlist_id": playlist_id,
            "count": pistes.len(),
            "tracks": pistes.iter().map(Piste::json).collect::<Vec<_>>(),
        }))
    }

    fn streaming_playlist_create(
        &self,
        service: &str,
        name: &str,
        _description: Option<&str>,
    ) -> Result<Value, String> {
        self.refuser_si_lecture_seule()?;
        let n = {
            let mut n = self.compteur_creations.borrow_mut();
            *n += 1;
            *n
        };
        let id = format!("cible-{n}");
        self.creer(id.clone(), name);
        Ok(json!({ "service": service, "playlist_id": id, "name": name }))
    }

    fn streaming_playlist_add_tracks(
        &self,
        _service: &str,
        playlist_id: &str,
        track_ids: &[String],
    ) -> Result<Value, String> {
        self.refuser_si_lecture_seule()?;
        self.verser(playlist_id, track_ids)?;
        Ok(json!({ "ok": true, "added": track_ids.len(), "demandees": track_ids.len() }))
    }

    fn streaming_match_track(
        &self,
        service: &str,
        title: &str,
        _artist: &str,
        _isrc: &str,
        _duration_ms: u64,
    ) -> Result<Value, String> {
        match self.cible.get(title) {
            None | Some(Verdict::Rien) => Ok(json!({ "service": service, "matched": Value::Null })),
            Some(Verdict::Erreur(e)) => Err(e.clone()),
            Some(Verdict::Candidat {
                piste,
                score,
                approximatif,
            }) => Ok(json!({
                "service": service,
                "matched": piste.json(),
                "score": score,
                "approximate": approximatif,
            })),
        }
    }

    fn kv_get(&self, key: &str) -> Result<Value, String> {
        Ok(match self.kv.borrow().get(key) {
            Some(v) => json!({ "key": key, "found": true, "value": v }),
            None => json!({ "key": key, "found": false, "value": Value::Null }),
        })
    }

    fn kv_set(&self, key: &str, value: &Value) -> Result<Value, String> {
        // Le stockage de l'hôte borne une valeur à 256 Kio ; le banc applique
        // la même borne, sans quoi un essai vert ici échouerait en production.
        let taille = serde_json::to_string(value).map_or(0, |s| s.len());
        if taille > 256 * 1024 {
            return Err(format!("kv_set: valeur trop grande ({taille} octets)"));
        }
        self.kv.borrow_mut().insert(key.to_string(), value.clone());
        self.journaux.borrow_mut().ecritures_kv += 1;
        self.noter(format!("kv:{key}"));
        Ok(json!({ "ok": true, "key": key, "bytes": taille }))
    }

    fn kv_list(&self, prefix: &str) -> Result<Value, String> {
        let mut cles: Vec<String> = self
            .kv
            .borrow()
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        cles.sort();
        Ok(json!({ "count": cles.len(), "keys": cles }))
    }
}
