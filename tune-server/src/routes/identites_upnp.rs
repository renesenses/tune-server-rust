//! Rapprochement d'un instantané avec les lignes durables déjà indexées.
//! Les ObjectID/URL sont des indices contextuels, jamais des clés primaires.
use super::*;

#[derive(Clone)]
pub(super) struct Ancienne {
    pub id: i64,
    pub cle: String,
    album_id: Option<i64>,
    empreinte: String,
    objet: String,
    url: String,
    duree: u64,
    taille: u64,
    format: Option<String>,
}

pub(super) struct Plan {
    pub pistes: Vec<Result<Option<Ancienne>, String>>,
    pub albums: HashMap<String, i64>,
    cles: HashSet<String>,
}

pub(super) fn empreinte(udn: &str, p: &PisteDistante) -> String {
    cle_d_identite(
        udn,
        &p.titre,
        p.artiste.as_deref(),
        p.album.as_deref(),
        p.duree_ms,
        p.taille,
    )
}

impl Plan {
    pub fn nouvelle_cle(&mut self, udn: &str, empreinte: &str) -> String {
        // Une ancienne empreinte peut désormais appartenir à une AUTRE piste.
        // La clé d'une ligne existante ne change jamais pour libérer ce nom.
        let cle = if self.cles.contains(empreinte) {
            format!("{udn}|{}", uuid::Uuid::new_v4())
        } else {
            empreinte.to_string()
        };
        self.cles.insert(cle.clone());
        cle
    }
}

pub(super) fn preparer(
    state: &AppState,
    udn: &str,
    pistes: &[PisteDistante],
) -> Result<Plan, String> {
    // Une seule lecture par passe ; pas de recherche quadratique par piste.
    let filtre = format!(
        "{}|%",
        udn.replace('!', "!!").replace('%', "!%").replace('_', "!_")
    );
    let param = match state.backend.engine() {
        tune_core::db::engine::Engine::Sqlite => "?",
        tune_core::db::engine::Engine::Postgres => "$1",
    };
    let lignes = state.backend.query_many(&format!(
        "SELECT t.id, t.source_id, t.album_id, t.title, ar.name, al.title, t.duration_ms, t.file_size, t.format, \
         obj.value, res.value FROM tracks t \
         LEFT JOIN artists ar ON ar.id = t.artist_id LEFT JOIN albums al ON al.id = t.album_id \
         LEFT JOIN track_metadata obj ON obj.track_id = t.id AND obj.key = 'upnp_object_id' \
         LEFT JOIN track_metadata res ON res.track_id = t.id AND res.key = 'upnp_res_url' \
         WHERE t.source = 'upnp' AND t.source_id LIKE {param} ESCAPE '!'"), &[&filtre])?;
    let prefixe = format!("{udn}|");
    let anciennes: Vec<Ancienne> = lignes
        .iter()
        .filter(|r| r[1].as_str().is_some_and(|cle| cle.starts_with(&prefixe)))
        .map(|r| {
            let duree = r[6].as_i64().unwrap_or(0).max(0) as u64;
            let taille = r[7].as_i64().unwrap_or(0).max(0) as u64;
            Ok(Ancienne {
                id: r[0].as_i64().ok_or("piste UPnP sans identifiant")?,
                cle: r[1].as_string().ok_or("piste UPnP sans clé")?,
                album_id: r[2].as_i64(),
                empreinte: cle_d_identite(
                    udn,
                    r[3].as_str().unwrap_or_default(),
                    r[4].as_str(),
                    r[5].as_str(),
                    Some(duree),
                    Some(taille),
                ),
                objet: r[9].as_string().unwrap_or_default(),
                url: r[10].as_string().unwrap_or_default(),
                duree: duree / 1000,
                taille,
                format: r[8].as_string(),
            })
        })
        .collect::<Result<_, String>>()?;
    let mut empreintes: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut objets: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut urls: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, a) in anciennes.iter().enumerate() {
        empreintes.entry(&a.empreinte).or_default().push(i);
        if !a.objet.is_empty() {
            objets.entry(&a.objet).or_default().push(i);
        }
        if !a.url.is_empty() {
            urls.entry(&a.url).or_default().push(i);
        }
    }
    let mut correspondances = Vec::new();
    for p in pistes {
        let cle = empreinte(udn, p);
        let candidats = if let Some(exacts) = empreintes.get(cle.as_str()) {
            exacts.clone()
        } else {
            let mut indices = HashSet::new();
            if let Some(ids) = objets.get(p.object_id.as_str()) {
                indices.extend(ids.iter().copied());
            }
            if let Some(ids) = p.url_de_lecture.as_deref().and_then(|url| urls.get(url)) {
                indices.extend(ids.iter().copied());
            }
            if indices.is_empty() {
                correspondances.push(Ok(None));
                continue;
            }
            // Une concordance de contexte doit être corroborée. En cas de
            // contradiction, on ne réattribue ni les liens ni les compteurs.
            let candidats: Vec<_> = indices
                .iter()
                .copied()
                .filter(|i| {
                    let a = &anciennes[*i];
                    let deux_indices = !a.objet.is_empty()
                        && a.objet == p.object_id
                        && !a.url.is_empty()
                        && Some(a.url.as_str()) == p.url_de_lecture.as_deref();
                    let taille = a.taille > 0 && Some(a.taille) == p.taille;
                    a.duree > 0
                        && Some(a.duree) == p.duree_ms.map(|d| d / 1000)
                        && a.format.is_some()
                        && a.format == p.format()
                        && (deux_indices || taille)
                })
                .collect();
            // Deux indices désignent des pistes différentes : même si une
            // seule signature convient, ne pas départager silencieusement.
            if indices.len() != 1 || candidats.len() != 1 {
                correspondances.push(Err(format!(
                    "identité UPnP ambiguë pour « {} » ; aucun retrait autorisé",
                    p.titre
                )));
                continue;
            }
            candidats
        };
        if candidats.len() == 1 {
            correspondances.push(Ok(Some(candidats[0])));
        } else {
            correspondances.push(Err(format!(
                "plusieurs identités UPnP pour « {} » ; aucun retrait autorisé",
                p.titre
            )));
        }
    }
    // Décider avant toute écriture : le résultat ne dépend pas de l'ordre
    // du DIDL. Deux pistes entrantes ne peuvent pas hériter de la même ligne.
    let mut compte = HashMap::new();
    for i in correspondances
        .iter()
        .filter_map(|r| r.as_ref().ok().copied().flatten())
    {
        *compte.entry(i).or_insert(0) += 1;
    }
    for r in &mut correspondances {
        if r.as_ref()
            .ok()
            .copied()
            .flatten()
            .is_some_and(|i| compte[&i] > 1)
        {
            *r = Err(
                "plusieurs pistes réclament la même identité UPnP ; aucun retrait autorisé".into(),
            );
        }
    }

    let mut groupes: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, p) in pistes.iter().enumerate() {
        if let Some(titre) = &p.album {
            groupes
                .entry(cle_d_identite_album(udn, titre, p.artiste.as_deref()))
                .or_default()
                .push(i);
        }
    }
    let mut albums = HashMap::new();
    let lignes = state.backend.query_many(&format!(
        "SELECT al.id, al.title, ar.name, al.source_id, (SELECT COUNT(*) FROM tracks t WHERE t.album_id = al.id) FROM albums al LEFT JOIN artists ar ON ar.id = al.artist_id \
         WHERE al.source = 'upnp' AND al.source_id LIKE {param} ESCAPE '!'"), &[&filtre])?;
    let mut albums_autorises = HashMap::new();
    for r in lignes {
        if !r[3].as_str().is_some_and(|cle| cle.starts_with(&prefixe)) {
            continue;
        }
        albums_autorises.insert(
            r[0].as_i64().ok_or("album sans identifiant")?,
            r[4].as_i64().unwrap_or(0),
        );
        let cle = cle_d_identite_album(udn, r[1].as_str().unwrap_or_default(), r[2].as_str());
        if groupes.contains_key(&cle) {
            albums.insert(cle, r[0].as_i64().ok_or("album sans identifiant")?);
        }
    }
    let mut membres: HashMap<i64, HashSet<i64>> = HashMap::new();
    for a in &anciennes {
        if let Some(id) = a.album_id {
            membres.entry(id).or_default().insert(a.id);
        }
    }
    let mut albums_repris: HashSet<i64> = albums.values().copied().collect();
    for (cle, indices) in groupes {
        if albums.contains_key(&cle) {
            continue;
        }
        let liens: Option<Vec<_>> = indices
            .iter()
            .map(|i| {
                correspondances[*i]
                    .as_ref()
                    .ok()
                    .copied()
                    .flatten()
                    .map(|a| &anciennes[a])
            })
            .collect();
        let Some(liens) = liens else {
            continue;
        };
        let Some(id) = liens.first().and_then(|a| a.album_id) else {
            continue;
        };
        // Un renommage complet, pas un déplacement d'une seule piste ni une
        // fusion/scission d'albums. Les favoris d'album gardent ainsi leur ID.
        if albums_autorises.get(&id) == Some(&(liens.len() as i64))
            && liens.iter().all(|a| a.album_id == Some(id))
            && membres.get(&id) == Some(&liens.iter().map(|a| a.id).collect())
            && !albums_repris.contains(&id)
        {
            albums.insert(cle, id);
            albums_repris.insert(id);
        }
    }
    Ok(Plan {
        pistes: correspondances
            .into_iter()
            .map(|r| r.map(|i| i.map(|i| anciennes[i].clone())))
            .collect(),
        albums,
        cles: anciennes.into_iter().map(|a| a.cle).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tune_core::db::playlist_repo::PlaylistRepo;

    fn piste(titre: &str, objet: &str) -> PisteDistante {
        PisteDistante {
            titre: titre.into(),
            object_id: objet.into(),
            artiste: Some("Artiste".into()),
            album: Some("Album".into()),
            url_de_lecture: Some(format!("http://nas/{objet}.flac")),
            pochette: None,
            duree_ms: Some(61000),
            sample_rate: Some(44100),
            bit_depth: Some(16),
            channels: Some(2),
            taille: Some(123456),
            protocol_info: Some("http-get:*:audio/flac:*".into()),
        }
    }

    fn indexer(state: &AppState, udn: &str, pistes: &[PisteDistante]) -> Bilan {
        let mut bilan = Bilan::default();
        ecrire(state, udn, "NAS", pistes, &mut bilan, &HashMap::new());
        bilan
    }

    #[test]
    fn identites_upnp_tags_puis_adresses_changent_sans_perdre_les_liens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bibliotheque.db");
        let state = AppState::new(path.to_str().unwrap(), 0, Default::default()).unwrap();
        let mut pistes = vec![piste("A", "a"), piste("B", "b")];
        let initial = indexer(&state, "uuid:nas", &pistes);
        assert!(initial.erreurs.is_empty());
        let album = album_existant(
            &state,
            &cle_d_identite_album("uuid:nas", "Album", Some("Artiste")),
        )
        .unwrap();
        let tracks = TrackRepo::with_backend(state.backend.clone());
        let avant = tracks.list_by_album(album).unwrap();
        let ids: Vec<_> = avant.iter().map(|t| t.id.unwrap()).collect();
        let playlists = PlaylistRepo::with_backend(state.backend.clone());
        let playlist = playlists.create("À garder", None, 1).unwrap();
        playlists
            .add_tracks(playlist, &[ids[0], ids[1], ids[0]], None)
            .unwrap();
        for (kind, id) in [("track", ids[0]), ("album", album)] {
            state
                .backend
                .execute(
                    "INSERT INTO favorites (profile_id,item_type,item_id) VALUES (1,?,?)",
                    &[&kind, &id.to_string()],
                )
                .unwrap();
        }
        state
            .backend
            .execute(
                "UPDATE tracks SET comments = 'Note personnelle' WHERE id = ?",
                &[&ids[0]],
            )
            .unwrap();
        // Correction de tous les tags et de la taille (tags dans le fichier).
        for p in &mut pistes {
            p.titre.push_str(" corrigé");
            p.artiste = Some("Artiste corrigé".into());
            p.album = Some("Album corrigé".into());
            p.taille = Some(123500);
        }
        let bilan = indexer(&state, "uuid:nas", &pistes);
        assert_eq!(
            bilan.mises_a_jour, 2,
            "les corrections de tags doivent garder les pistes existantes : {:?}",
            bilan.erreurs
        );
        assert_eq!(bilan.ajoutees, 0);
        assert_eq!(
            bilan.albums_ajoutes, 0,
            "le renommage complet conserve l'album favori"
        );
        assert_eq!(
            bilan.identites, initial.identites,
            "la clé durable ne suit pas les tags"
        );
        assert_eq!(
            AlbumRepo::with_backend(state.backend.clone())
                .get(album)
                .unwrap()
                .unwrap()
                .title,
            "Album corrigé"
        );
        assert_eq!(
            tracks.get(ids[0]).unwrap().unwrap().comments.as_deref(),
            Some("Note personnelle")
        );
        // Réouverture de la base, puis rescan distant qui change tous les
        // ObjectID et toutes les URL. Les tags courants suffisent désormais.
        drop(playlists);
        drop(tracks);
        drop(state);
        let state = AppState::new(path.to_str().unwrap(), 0, Default::default()).unwrap();
        for p in &mut pistes {
            p.object_id.push_str("-rescan");
            p.url_de_lecture = Some(format!("http://autre-adresse/{}", p.object_id));
        }
        let bilan = indexer(&state, "uuid:nas", &pistes);
        assert!(bilan.erreurs.is_empty(), "{:?}", bilan.erreurs);
        assert_eq!(bilan.mises_a_jour, 2);
        assert_eq!(bilan.identites, initial.identites);
        let liens = state
            .backend
            .query_many(
                "SELECT track_id FROM playlist_tracks WHERE playlist_id = ? ORDER BY position",
                &[&playlist],
            )
            .unwrap();
        assert_eq!(
            liens
                .iter()
                .map(|r| r[0].as_i64().unwrap())
                .collect::<Vec<_>>(),
            vec![ids[0], ids[1], ids[0]],
            "ordre et répétitions de playlist conservés"
        );
        assert_eq!(state.backend.query_one("SELECT COUNT(*) FROM favorites f JOIN tracks t ON CAST(f.item_id AS INTEGER) = t.id WHERE f.item_type = 'track' AND t.title LIKE '%corrigé'", &[]).unwrap().unwrap()[0].as_i64(), Some(1));
        assert_eq!(state.backend.query_one("SELECT COUNT(*) FROM favorites f JOIN albums a ON CAST(f.item_id AS INTEGER) = a.id WHERE f.item_type = 'album' AND a.title = 'Album corrigé'", &[]).unwrap().unwrap()[0].as_i64(), Some(1));
        for (i, p) in pistes.iter().enumerate() {
            let meta = TrackMetadataRepo::with_backend(state.backend.clone())
                .get_all(ids[i])
                .unwrap();
            assert_eq!(meta.get(CLE_URL_DE_LECTURE), p.url_de_lecture.as_ref());
        }
    }

    #[test]
    fn identites_upnp_indices_contradictoires_et_objet_reutilise_ne_volent_pas_les_liens() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let a = piste("A", "a");
        let b = piste("B", "b");
        indexer(&state, "u", &[a.clone(), b.clone()]);
        let mut conflit = a.clone();
        conflit.titre = "Inconnue".into();
        conflit.url_de_lecture = b.url_de_lecture.clone();
        let resultat = indexer(&state, "u", &[conflit]);
        assert!(
            !resultat.erreurs.is_empty(),
            "des indices contradictoires interdisent la réconciliation"
        );
        assert_eq!(resultat.mises_a_jour + resultat.ajoutees, 0);
        let mut recycle = a.clone();
        recycle.titre = "Autre fichier".into();
        recycle.duree_ms = Some(90000);
        let resultat = indexer(&state, "u", &[recycle]);
        assert!(
            !resultat.erreurs.is_empty(),
            "un ObjectID recyclé ne transfère pas un favori"
        );
        assert_eq!(resultat.mises_a_jour + resultat.ajoutees, 0);
        let mut copie = a.clone();
        copie.titre = "Autre A".into();
        let resultat = indexer(&state, "u", &[a.clone(), copie]);
        assert!(
            !resultat.erreurs.is_empty(),
            "deux candidats ne peuvent pas hériter d'une ligne"
        );
        assert_eq!(resultat.mises_a_jour + resultat.ajoutees, 0);
        assert_eq!(
            indexer(&state, "autre-serveur", &[a, b]).ajoutees,
            2,
            "aucun rapprochement entre serveurs"
        );
    }

    #[test]
    fn identites_upnp_ancienne_empreinte_reutilisee_ne_change_pas_la_cle_durable() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let original = piste("A", "a");
        let initial = indexer(&state, "u", std::slice::from_ref(&original));
        let mut renomme = original.clone();
        renomme.titre = "B".into();
        assert_eq!(
            indexer(&state, "u", std::slice::from_ref(&renomme)).identites,
            initial.identites
        );
        let nouveau = piste("A", "nouvel-objet");
        let resultat = indexer(&state, "u", &[renomme.clone(), nouveau.clone()]);
        assert!(resultat.erreurs.is_empty());
        assert_eq!(resultat.ajoutees, 1);
        assert_eq!(resultat.identites[0], initial.identites[0]);
        assert_ne!(
            resultat.identites[0], resultat.identites[1],
            "l'empreinte historique n'est pas réattribuée"
        );
        let encore = indexer(&state, "u", &[renomme, nouveau]);
        assert_eq!(encore.identites, resultat.identites);
        assert_eq!(encore.mises_a_jour, 2);
    }

    #[test]
    fn identites_upnp_un_deplacement_partiel_ne_renomme_pas_l_album_favori() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let mut a = piste("A", "a");
        let b = piste("B", "b");
        indexer(&state, "u", &[a.clone(), b]);
        let id =
            album_existant(&state, &cle_d_identite_album("u", "Album", Some("Artiste"))).unwrap();
        a.album = Some("Autre album".into());
        let resultat = indexer(&state, "u", &[a]);
        assert_eq!(resultat.mises_a_jour, 1);
        assert_eq!(resultat.albums_ajoutes, 1);
        let album = AlbumRepo::with_backend(state.backend.clone())
            .get(id)
            .unwrap()
            .unwrap();
        assert_eq!(album.title, "Album");
    }
    #[test]
    fn identites_upnp_un_indice_seul_exige_une_signature_et_reste_dans_son_serveur() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let original = piste("A", "a");
        let initial = indexer(&state, "uuid:Nas_%", std::slice::from_ref(&original));
        let mut change = original.clone();
        change.titre = "Titre corrigé".into();
        change.url_de_lecture = Some("http://nouvelle-adresse/a".into());
        let maj = indexer(&state, "uuid:Nas_%", std::slice::from_ref(&change));
        assert_eq!(
            maj.identites, initial.identites,
            "ObjectID, durée, format et taille concordent malgré une nouvelle URL"
        );
        assert_eq!(maj.mises_a_jour, 1);
        change.object_id = "autre-axe".into();
        change.titre = "Autre correction".into();
        let maj = indexer(&state, "uuid:Nas_%", std::slice::from_ref(&change));
        assert_eq!(
            maj.identites, initial.identites,
            "URL, durée, format et taille concordent malgré un autre ObjectID"
        );
        // LIKE SQLite est insensible à la casse et interprète %/_. Le filtre
        // doit néanmoins respecter exactement l'UDN et exclure le local.
        assert_eq!(
            indexer(&state, "uuid:nas_%", std::slice::from_ref(&change)).ajoutees,
            1
        );
        assert_eq!(
            indexer(&state, "uuid:Nas_X", std::slice::from_ref(&change)).ajoutees,
            1
        );
        let mut incertaine = change;
        incertaine.titre = "Non prouvée".into();
        incertaine.object_id = "encore-un-axe".into();
        incertaine.taille = None;
        let resultat = indexer(&state, "uuid:Nas_%", &[incertaine]);
        assert!(!resultat.erreurs.is_empty());
        assert_eq!(resultat.mises_a_jour, 0);
    }
    #[test]
    fn identites_upnp_un_album_local_ne_peut_pas_etre_renomme_par_une_piste_distante() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let albums = AlbumRepo::with_backend(state.backend.clone());
        let local = albums.create(&Album::new("Album local".into())).unwrap();
        let mut a = piste("A", "a");
        indexer(&state, "u", std::slice::from_ref(&a));
        state
            .backend
            .execute(
                "UPDATE tracks SET album_id = ? WHERE source = 'upnp'",
                &[&local],
            )
            .unwrap();
        a.album = Some("Nouveau nom".into());
        let resultat = indexer(&state, "u", &[a]);
        assert_eq!(resultat.mises_a_jour, 1);
        assert_eq!(resultat.albums_ajoutes, 1);
        assert_eq!(albums.get(local).unwrap().unwrap().title, "Album local");
    }
}
