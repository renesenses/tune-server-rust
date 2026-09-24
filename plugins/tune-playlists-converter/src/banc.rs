//! L'hôte de banc : un double du serveur, pour jouer le moteur sans wasmtime,
//! sans base et **sans jamais toucher un vrai service**.
//!
//! Il compte ce qu'on lui demande. C'est ce qui permet de prouver qu'un aperçu
//! n'écrit rien : on ne lit pas le code pour s'en convaincre, on met le double
//! en refus d'écriture et on regarde l'aperçu réussir quand même.

use std::cell::RefCell;
use std::collections::HashMap;

use serde_json::{Value, json};

use crate::hote::Hote;

/// Une piste du banc.
#[derive(Clone)]
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
}

pub struct HoteDeBanc {
    /// Playlists de la source : id → (nom, pistes).
    pub source: HashMap<String, (String, Vec<Piste>)>,
    /// Verdict de la cible, par titre source.
    pub cible: HashMap<String, Verdict>,
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
}

impl HoteDeBanc {
    pub fn new() -> Self {
        Self {
            source: HashMap::new(),
            cible: HashMap::new(),
            kv: RefCell::new(HashMap::new()),
            journaux: RefCell::new(Journaux::default()),
            ecriture_interdite: false,
            tomber_apres_ajouts: RefCell::new(None),
            compteur_creations: RefCell::new(0),
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

    fn refuser_si_lecture_seule(&self) -> Result<(), String> {
        if self.ecriture_interdite {
            Err("ECRITURE INTERDITE : le banc est en lecture seule".into())
        } else {
            Ok(())
        }
    }
}

impl Hote for HoteDeBanc {
    fn journal(&self, _niveau: &str, _message: &str) {}

    fn playlist_tracks(&self, playlist_id: i64) -> Result<Value, String> {
        let cle = playlist_id.to_string();
        let (nom, pistes) = self
            .source
            .get(&cle)
            .ok_or_else(|| format!("playlist introuvable : {cle}"))?;
        Ok(json!({
            "playlist_id": playlist_id,
            "name": nom,
            "count": pistes.len(),
            "tracks": pistes.iter().map(Piste::json).collect::<Vec<_>>(),
        }))
    }

    fn streaming_playlists(&self, service: &str) -> Result<Value, String> {
        let mut ids: Vec<&String> = self.source.keys().collect();
        ids.sort();
        let playlists: Vec<Value> = ids
            .into_iter()
            .map(|id| json!({ "source_id": id, "name": self.source[id].0, "track_count": self.source[id].1.len() }))
            .collect();
        Ok(json!({ "service": service, "count": playlists.len(), "playlists": playlists }))
    }

    fn streaming_playlist_tracks(&self, service: &str, playlist_id: &str) -> Result<Value, String> {
        let (_, pistes) = self
            .source
            .get(playlist_id)
            .ok_or_else(|| format!("playlist introuvable : {playlist_id}"))?;
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
        let mut n = self.compteur_creations.borrow_mut();
        *n += 1;
        let id = format!("cible-{n}");
        self.journaux
            .borrow_mut()
            .creations
            .push((id.clone(), name.to_string()));
        Ok(json!({ "service": service, "playlist_id": id, "name": name }))
    }

    fn streaming_playlist_add_tracks(
        &self,
        _service: &str,
        playlist_id: &str,
        track_ids: &[String],
    ) -> Result<Value, String> {
        self.refuser_si_lecture_seule()?;
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
