//! Témoins de la radio artiste (#5395), hors réseau : des sources factices
//! pour la composition, une base en mémoire et un service simulé pour les
//! sources réelles et la reprise.

use super::*;
use crate::db::sqlite::SqliteDb;
use crate::error::TuneError;
use crate::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamUrl,
};
use std::sync::atomic::{AtomicUsize, Ordering};

fn piste(id: &str, artiste: &str, titre: &str) -> StreamTrack {
    StreamTrack {
        id: id.into(),
        title: titre.into(),
        artist: artiste.into(),
        album: None,
        album_id: None,
        duration_ms: 200_000,
        cover_path: None,
        track_number: None,
        disc_number: None,
        explicit: false,
        disponible: None,
        quality: None,
        isrc: None,
        composer: None,
        artist_id: None,
    }
}

fn titre_de(source: &str, artiste: &str, n: usize) -> Candidat {
    Candidat::Service {
        source: source.into(),
        piste: piste(
            &format!("{source}-{artiste}-{n}"),
            artiste,
            &format!("{artiste} titre {n}"),
        ),
    }
}

/// Une source factice : des voisins pour la graine, `n` titres par artiste
/// qu'elle connaît. Compte les demandes de voisins.
struct Factice {
    nom: &'static str,
    voisins: Vec<String>,
    connus: Vec<String>,
    titres_par_artiste: usize,
    demandes_de_voisins: Arc<AtomicUsize>,
}

impl Factice {
    fn new(nom: &'static str, voisins: &[&str], connus: &[&str], n: usize) -> Self {
        Factice {
            nom,
            voisins: voisins.iter().map(|s| s.to_string()).collect(),
            connus: connus.iter().map(|s| s.to_string()).collect(),
            titres_par_artiste: n,
            demandes_de_voisins: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait::async_trait]
impl SourceRadio for Factice {
    fn nom(&self) -> String {
        self.nom.into()
    }
    async fn artistes_similaires(&self, _artiste: &str, max: usize) -> Vec<String> {
        self.demandes_de_voisins.fetch_add(1, Ordering::SeqCst);
        self.voisins.iter().take(max).cloned().collect()
    }
    async fn titres_de(&self, artiste: &str, max: usize) -> Vec<Candidat> {
        if !self.connus.iter().any(|c| c.eq_ignore_ascii_case(artiste)) {
            return Vec::new();
        }
        (0..self.titres_par_artiste.min(max))
            .map(|n| titre_de(self.nom, artiste, n))
            .collect()
    }
}

fn voisins(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("Voisin {i}")).collect()
}

fn refs(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

fn source_de(c: &Candidat) -> &str {
    match c {
        Candidat::Local { .. } => "local",
        Candidat::Service { source, .. } => source,
    }
}

async fn lot(sources: Vec<Box<dyn SourceRadio>>, graine: u64) -> Lot {
    composer_lot(
        "Graine",
        &sources,
        TAILLE_LOT,
        &HashSet::new(),
        None,
        true,
        &mut Alea::fixe(graine),
    )
    .await
}

// ── Proportion ───────────────────────────────────────────────────────────

#[tokio::test]
async fn un_lot_porte_environ_vingt_pour_cent_de_l_artiste_de_depart() {
    let v = voisins(12);
    let mut connus = refs(&v);
    connus.push("Graine");
    for graine in 1..=20u64 {
        let l = lot(
            vec![Box::new(Factice::new("qobuz", &refs(&v), &connus, 10))],
            graine,
        )
        .await;
        assert_eq!(l.candidats.len(), TAILLE_LOT, "graine {graine}");
        let de_la_graine = l
            .candidats
            .iter()
            .filter(|c| c.artiste() == "Graine")
            .count();
        assert_eq!(
            de_la_graine, 5,
            "graine {graine} : 5 titres sur 25 (20 %) doivent venir de l'artiste de départ"
        );
        assert_eq!(l.titres_graine, 5);
        assert_eq!(
            l.candidats[0].artiste(),
            "Graine",
            "le premier lot s'ouvre sur l'artiste demandé"
        );
    }
}

// ── Ordre des sources ────────────────────────────────────────────────────

#[tokio::test]
async fn le_service_de_la_fiche_passe_d_abord_et_les_autres_ne_font_que_completer() {
    // La fiche a assez de voisins : le second service n'est même pas
    // interrogé sur les siens, et les titres viennent de la fiche.
    let v = voisins(30);
    let mut connus = refs(&v);
    connus.push("Graine");
    let fiche = Factice::new("qobuz", &refs(&v), &connus, 6);
    let autre = Factice::new("tidal", &["Autre voisin"], &connus, 6);
    let demandes_autre = autre.demandes_de_voisins.clone();
    let l = lot(vec![Box::new(fiche), Box::new(autre)], 7).await;
    assert_eq!(demandes_autre.load(Ordering::SeqCst), 0);
    assert!(l.candidats.iter().all(|c| source_de(c) == "qobuz"));
    assert_eq!(
        l.voisins_par_source,
        vec![("qobuz".to_string(), 30)],
        "une seule source consultée pour les voisins"
    );
}

#[tokio::test]
async fn un_voisin_absent_de_la_fiche_est_cherche_dans_le_service_suivant() {
    // La fiche ne connaît que 2 voisins sur 3 ; le troisième n'existe que
    // chez le second service.
    let fiche = Factice::new("qobuz", &["A", "B", "C"], &["Graine", "A", "B"], 4);
    let autre = Factice::new("tidal", &[], &["Graine", "A", "B", "C"], 4);
    let l = lot(vec![Box::new(fiche), Box::new(autre)], 3).await;
    // (Les titres de la graine peuvent venir des deux : il en faut plus que
    // la fiche n'en a pour combler les voisins trop courts.)
    for c in l.candidats.iter().filter(|c| c.artiste() != "Graine") {
        let attendu = if c.artiste() == "C" { "tidal" } else { "qobuz" };
        assert_eq!(
            source_de(c),
            attendu,
            "{} vient de {}",
            c.titre(),
            source_de(c)
        );
    }
    assert!(l.candidats.iter().any(|c| c.artiste() == "C"));
}

// ── Service sans similaires ──────────────────────────────────────────────

#[tokio::test]
async fn un_service_sans_similaires_est_complete_par_les_sources_suivantes() {
    // Le service de la fiche ne rend aucun voisin (Spotify, YouTube…) : les
    // voisins viennent de la source suivante, les titres restent ceux de la
    // fiche quand elle les a.
    let fiche = Factice::new("spotify", &[], &["Graine", "X", "Y"], 5);
    let suivante = Factice::new("local", &["X", "Y"], &["X", "Y"], 5);
    let l = lot(vec![Box::new(fiche), Box::new(suivante)], 11).await;
    assert_eq!(
        l.voisins_par_source,
        vec![("spotify".to_string(), 0), ("local".to_string(), 2)]
    );
    assert!(l.candidats.iter().any(|c| c.artiste() == "X"));
    assert!(l.candidats.iter().any(|c| c.artiste() == "Y"));
    assert!(l.candidats.iter().all(|c| source_de(c) == "spotify"));
}

#[tokio::test]
async fn sans_aucun_voisin_la_radio_joue_l_artiste_seul_plutot_que_rien() {
    let fiche = Factice::new("deezer", &[], &["Graine"], 8);
    let l = lot(vec![Box::new(fiche)], 5).await;
    assert_eq!(l.candidats.len(), 8);
    assert!(l.candidats.iter().all(|c| c.artiste() == "Graine"));
}

// ── Doublons et enchaînement ─────────────────────────────────────────────

#[tokio::test]
async fn le_meme_morceau_sur_deux_services_n_entre_qu_une_fois() {
    // Deux services rendent les MÊMES morceaux (même artiste, même titre,
    // identifiants différents) ; un titre « (Remastered) » est le même.
    struct Doublons;
    #[async_trait::async_trait]
    impl SourceRadio for Doublons {
        fn nom(&self) -> String {
            "tidal".into()
        }
        async fn artistes_similaires(&self, _a: &str, _m: usize) -> Vec<String> {
            vec!["A".into(), "B".into()]
        }
        async fn titres_de(&self, artiste: &str, _m: usize) -> Vec<Candidat> {
            (0..3)
                .map(|n| Candidat::Service {
                    source: "tidal".into(),
                    piste: piste(
                        &format!("t{artiste}{n}"),
                        artiste,
                        &format!("{artiste} titre {n} (Remastered 2011)"),
                    ),
                })
                .collect()
        }
    }
    let fiche = Factice::new("qobuz", &["A", "B"], &["Graine", "A", "B"], 3);
    let sources: Vec<Box<dyn SourceRadio>> = vec![Box::new(fiche), Box::new(Doublons)];
    let l = composer_lot(
        "Graine",
        &sources,
        TAILLE_LOT,
        &HashSet::new(),
        None,
        true,
        &mut Alea::fixe(9),
    )
    .await;
    let cles: Vec<String> = l.candidats.iter().map(Candidat::cle).collect();
    let uniques: HashSet<&String> = cles.iter().collect();
    assert_eq!(cles.len(), uniques.len(), "doublon dans {cles:?}");
    assert!(!l.candidats.is_empty());
}

#[tokio::test]
async fn deux_titres_du_meme_artiste_ne_se_suivent_pas() {
    let v = voisins(6);
    let mut connus = refs(&v);
    connus.push("Graine");
    for graine in 1..=30u64 {
        let l = composer_lot(
            "Graine",
            &[Box::new(Factice::new("qobuz", &refs(&v), &connus, 6)) as Box<dyn SourceRadio>],
            TAILLE_LOT,
            &HashSet::new(),
            Some("Voisin 0"),
            false,
            &mut Alea::fixe(graine),
        )
        .await;
        assert_ne!(
            l.candidats[0].artiste(),
            "Voisin 0",
            "graine {graine} : le lot ne répète pas le dernier artiste du lot précédent"
        );
        for paire in l.candidats.windows(2) {
            assert_ne!(
                paire[0].artiste(),
                paire[1].artiste(),
                "graine {graine} : deux titres de {} d'affilée",
                paire[0].artiste()
            );
        }
    }
}

#[tokio::test]
async fn les_titres_exclus_ne_reviennent_pas() {
    let fiche = Factice::new("qobuz", &["A"], &["Graine", "A"], 4);
    let exclus: HashSet<String> = [titre_de("qobuz", "A", 0), titre_de("qobuz", "Graine", 0)]
        .iter()
        .map(Candidat::cle)
        .collect();
    let l = composer_lot(
        "Graine",
        &[Box::new(fiche) as Box<dyn SourceRadio>],
        TAILLE_LOT,
        &exclus,
        None,
        true,
        &mut Alea::fixe(2),
    )
    .await;
    assert!(!l.candidats.is_empty());
    assert!(l.candidats.iter().all(|c| !exclus.contains(&c.cle())));
}

#[test]
fn normaliser_rapproche_les_variantes_d_un_meme_titre() {
    assert_eq!(
        normaliser("Wish You Were Here (Remastered 2011)"),
        "wish you were here"
    );
    assert_eq!(
        normaliser("  WISH you were   here [Live] "),
        "wish you were here"
    );
    assert_eq!(normaliser("AC/DC"), "ac dc");
}

// ── Contexte, reprise et redémarrage ─────────────────────────────────────

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    // L'API d'enrichissement sur un port fermé : aucun appel ne sort.
    crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .set("artist_enrichment_api", "http://127.0.0.1:9")
        .unwrap();
    db
}

/// Une bibliothèque : la graine et quatre artistes du même genre, huit
/// titres chacun, plus un artiste d'un autre genre.
fn bibliotheque(db: &Arc<dyn DbBackend>) {
    let artistes = [
        (1i64, "Graine", "Jazz"),
        (2, "Voisin A", "Jazz"),
        (3, "Voisin B", "Jazz"),
        (4, "Voisin C", "Jazz"),
        (5, "Voisin D", "Jazz"),
        (6, "Hors genre", "Metal"),
    ];
    let mut id = 1i64;
    for (aid, nom, genre) in artistes {
        db.execute(
            "INSERT INTO artists (id, name) VALUES (?, ?)",
            &[&aid, &nom],
        )
        .unwrap();
        for n in 0..8 {
            let titre = format!("{nom} {n}");
            db.execute(
                "INSERT INTO tracks (id, title, artist_id, genre, duration_ms) VALUES (?, ?, ?, ?, 200000)",
                &[&id, &titre.as_str(), &aid, &genre],
            )
            .unwrap();
            id += 1;
        }
    }
}

fn registre_vide() -> Registre {
    Arc::new(tokio::sync::Mutex::new(
        crate::streaming::registry::ServiceRegistry::new(),
    ))
}

#[tokio::test]
async fn la_bibliotheque_fournit_des_voisins_du_meme_genre() {
    let db = base();
    bibliotheque(&db);
    let src = SourceBibliotheque::new(db.clone());
    let mut v = src.artistes_similaires("Graine", 10).await;
    v.sort();
    assert_eq!(v, vec!["Voisin A", "Voisin B", "Voisin C", "Voisin D"]);
    assert_eq!(src.titres_de("voisin a", 3).await.len(), 3);
}

#[tokio::test]
async fn la_radio_se_recharge_sans_redire_ce_qu_elle_a_propose() {
    let db = base();
    bibliotheque(&db);
    let services = registre_vide();
    let zone = 1;
    let premier = demarrer(&db, &services, zone, "Graine", None, None).await;
    assert!(!premier.candidats.is_empty());
    // Le contexte est dans les réglages : un redémarrage le relit tel quel.
    let ctx = lire_contexte(&db, zone).expect("contexte écrit par le départ");
    assert_eq!(ctx.artiste, "Graine");
    assert_eq!(ctx.lots, 1);
    let fin = premier.candidats.last().unwrap().identite_file();
    assert!(ctx.continue_sur(&fin));

    let second = continuer(&db, &services, zone, std::slice::from_ref(&fin))
        .await
        .expect("la file s'achève sur la radio : elle continue");
    let deja: HashSet<String> = premier.candidats.iter().map(Candidat::cle).collect();
    assert!(!second.candidats.is_empty());
    for c in &second.candidats {
        assert!(
            !deja.contains(&c.cle()),
            "{} repris d'un lot à l'autre",
            c.titre()
        );
    }
    let ctx = lire_contexte(&db, zone).unwrap();
    assert_eq!(ctx.lots, 2);
    assert_eq!(
        ctx.deja_proposes.len(),
        premier.candidats.len() + second.candidats.len()
    );
}

#[tokio::test]
async fn une_file_qui_ne_finit_pas_sur_la_radio_rend_la_main_a_l_autoplay() {
    let db = base();
    bibliotheque(&db);
    let services = registre_vide();
    demarrer(&db, &services, 1, "Graine", None, None).await;
    assert!(
        continuer(&db, &services, 1, &["local:99999".to_string()])
            .await
            .is_none(),
        "un titre étranger à la radio ne la continue pas"
    );
    assert!(
        lire_contexte(&db, 1).is_none(),
        "le contexte d'une radio quittée est effacé"
    );
    // Et une zone sans radio ne change rien à l'auto-lecture.
    assert!(
        continuer(&db, &services, 2, &["local:1".to_string()])
            .await
            .is_none()
    );
}

#[test]
fn la_fenetre_anti_doublon_est_bornee() {
    let mut ctx = ContexteRadioArtiste::nouveau("Graine", Some("qobuz"), Some("42"));
    for n in 0..(FENETRE_ANTI_DOUBLON + 40) {
        ctx.consigner(&[titre_de("qobuz", "A", n)]);
    }
    assert_eq!(ctx.deja_proposes.len(), FENETRE_ANTI_DOUBLON);
    assert_eq!(
        ctx.deja_proposes.last(),
        Some(&titre_de("qobuz", "A", FENETRE_ANTI_DOUBLON + 39).cle())
    );
    assert_eq!(ctx.dernier_lot, vec!["qobuz:qobuz-A-339".to_string()]);
}

// ── Un service réel simulé : résolution, bannis, ordre des sources ───────

struct ServiceSimule {
    nom: &'static str,
    connecte: bool,
    voisins: Vec<StreamArtist>,
    recherches: Arc<AtomicUsize>,
}

fn artiste(id: &str, nom: &str) -> StreamArtist {
    StreamArtist {
        id: id.into(),
        name: nom.into(),
        image_path: None,
        bio: None,
    }
}

#[async_trait::async_trait]
impl StreamingService for ServiceSimule {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        self.nom
    }
    fn enabled(&self) -> bool {
        true
    }
    fn set_enabled(&mut self, _e: bool) {}
    async fn authenticate(&mut self, _c: &serde_json::Value) -> Result<AuthStatus, TuneError> {
        Ok(AuthStatus::default())
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: self.connecte,
            ..Default::default()
        }
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    /// « Graine » rend aussi un groupe hommage : seul le nom exact compte.
    async fn search(&self, q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        self.recherches.fetch_add(1, Ordering::SeqCst);
        let artists = if q == "Graine" {
            vec![artiste("hommage", "Graine Tribute"), artiste("g", "Graine")]
        } else {
            Vec::new()
        };
        Ok(SearchResults {
            tracks: Vec::new(),
            albums: Vec::new(),
            artists,
            playlists: Vec::new(),
        })
    }
    async fn get_track(&self, _id: &str) -> Result<StreamTrack, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_track_url(&self, _t: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album(&self, _a: &str) -> Result<StreamAlbum, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album_tracks(&self, _a: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_artist(&self, _a: &str) -> Result<StreamArtist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_artist_top_tracks(&self, id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        let nom = match id {
            "g" => "Graine",
            "hommage" => "Graine Tribute",
            "v1" => "Voisin Un",
            "v2" => "Voisin Deux",
            _ => return Ok(Vec::new()),
        };
        let mut pistes: Vec<StreamTrack> = (0..6)
            .map(|n| piste(&format!("{id}-{n}"), nom, &format!("{nom} {n}")))
            .collect();
        pistes[5].disponible = Some(false);
        Ok(pistes)
    }
    async fn get_similar_artists(
        &self,
        id: &str,
        _limit: usize,
    ) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(if id == "g" {
            self.voisins.clone()
        } else {
            Vec::new()
        })
    }
    async fn get_playlist(&self, _p: &str) -> Result<StreamPlaylist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist_tracks(&self, _p: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn les_sources_suivent_la_fiche_puis_les_services_connectes_puis_la_bibliotheque() {
    let db = base();
    let services = registre_vide();
    {
        let mut reg = services.lock().await;
        for (nom, connecte) in [
            ("amazon", true),
            ("qobuz", true),
            ("tidal", false),
            ("deezer", true),
        ] {
            reg.register(Box::new(ServiceSimule {
                nom,
                connecte,
                voisins: Vec::new(),
                recherches: Arc::new(AtomicUsize::new(0)),
            }));
        }
    }
    let ctx = ContexteRadioArtiste::nouveau("Graine", Some("qobuz"), None);
    let noms: Vec<String> = sources_de_la_radio(&db, &services, &ctx)
        .await
        .iter()
        .map(|s| s.nom())
        .collect();
    assert_eq!(
        noms,
        vec!["qobuz", "amazon", "deezer", "enrichissement", "local"],
        "la fiche d'abord, les services connectés ensuite (tidal est déconnecté)"
    );
}

#[tokio::test]
async fn un_service_resout_l_artiste_au_nom_exact_et_ecarte_bannis_et_indisponibles() {
    let recherches = Arc::new(AtomicUsize::new(0));
    let svc: Box<dyn StreamingService> = Box::new(ServiceSimule {
        nom: "qobuz",
        connecte: true,
        voisins: vec![artiste("v1", "Voisin Un"), artiste("v2", "Voisin Deux")],
        recherches: recherches.clone(),
    });
    let src = SourceService::new(
        "qobuz",
        Arc::new(tokio::sync::RwLock::new(svc)),
        HashSet::from(["g-0".to_string()]),
        None,
    );
    let titres = src.titres_de("Graine", 20).await;
    let ids: Vec<String> = titres
        .iter()
        .map(|c| match c {
            Candidat::Service { piste, .. } => piste.id.clone(),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(
        ids,
        vec!["g-1", "g-2", "g-3", "g-4"],
        "pas le groupe hommage, pas le titre banni g-0, pas l'indisponible g-5"
    );
    assert_eq!(
        src.artistes_similaires("Graine", 10).await,
        vec!["Voisin Un", "Voisin Deux"]
    );
    // Les voisins sont connus par leur identifiant : leurs titres ne coûtent
    // aucune recherche de plus.
    let avant = recherches.load(Ordering::SeqCst);
    assert_eq!(src.titres_de("Voisin Un", 3).await.len(), 3);
    assert_eq!(recherches.load(Ordering::SeqCst), avant);
}
