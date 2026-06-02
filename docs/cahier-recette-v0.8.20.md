# Cahier de recette — Tune v0.8.20

**Date** : 2 juin 2026
**Testeurs** : Babacar, Sergio, Dominique, Pascal, Benjithom, Laurent
**Plateformes** : macOS (DMG/Homebrew), Windows (NSIS), Linux (Docker), iPad (TestFlight)

---

## Instructions

Ouvrir le client web Tune dans un navigateur : `http://<adresse-serveur>:8888`

Pour chaque test :
- Suivre les etapes dans l'interface web
- Noter OK ou KO dans la colonne Resultat
- Si KO, decrire le comportement observe dans Commentaire

---

## 1. Dashboard & Statistiques

### 1.1 Page d'accueil — Compteurs

| Etape | Action | Resultat attendu | Resultat | Commentaire |
|-------|--------|-------------------|----------|-------------|
| 1 | Ouvrir la page d'accueil (icone maison) | Les compteurs Pistes, Albums, Artistes, Zones s'affichent | | |
| 2 | Verifier que Zones > 0 | Le nombre de zones correspond aux appareils detectes | | |
| 3 | Lancer un scan (Reglages > Scanner la bibliotheque) | Le compteur de progression s'affiche en temps reel | | |
| 4 | Attendre la fin du scan | Les compteurs se mettent a jour | | |

### 1.2 Dashboard ecoutes

| Etape | Action | Resultat attendu | Resultat | Commentaire |
|-------|--------|-------------------|----------|-------------|
| 1 | Jouer quelques pistes pendant 30 secondes chacune | La lecture fonctionne normalement | | |
| 2 | Ouvrir la page Dashboard (si disponible dans le menu) | Top artistes, top pistes, historique par jour s'affichent | | |

### 1.3 Wrapped — Annee en musique (API uniquement)

```bash
curl http://SERVER:8888/api/v1/dashboard/wrapped?year=2026
```
**Attendu** : total_listens, total_hours, top_artists (top 10), max_streak_days.

---

## 2. Bibliotheque

### 2.1 Navigation albums

| Etape | Action | Resultat attendu | Resultat | Commentaire |
|-------|--------|-------------------|----------|-------------|
| 1 | Aller dans Bibliotheque > Albums | La liste des albums s'affiche avec pochettes | | |
| 2 | Cliquer sur un album | Les pistes de l'album s'affichent avec numeros de piste | | |
| 3 | Verifier la pochette | L'image de couverture s'affiche correctement | | |

### 2.2 Navigation artistes + timeline

| Etape | Action | Resultat attendu | Resultat | Commentaire |
|-------|--------|-------------------|----------|-------------|
| 1 | Aller dans Bibliotheque > Artistes | La liste des artistes s'affiche | | |
| 2 | Cliquer sur un artiste avec plusieurs albums | Ses albums s'affichent tries par annee | | |

### 2.3 Recherche

| Etape | Action | Resultat attendu | Resultat | Commentaire |
|-------|--------|-------------------|----------|-------------|
| 1 | Cliquer sur la loupe de recherche | Le champ de recherche s'ouvre | | |
| 2 | Taper "miles davis" | Des resultats apparaissent (artistes, albums, pistes) | | |
| 3 | Taper un mot avec accent "musique experimentale" | Les resultats ignorent les accents | | |
| 4 | Taper un mot sans accent "cafe" pour trouver "Cafe" | La recherche trouve les correspondances accentuees | | |

### 2.4 Arbre de genres

| Etape | Action | Resultat attendu | Resultat | Commentaire |
|-------|--------|-------------------|----------|-------------|
| 1 | Aller dans Bibliotheque > Genres | L'arbre hierarchique de genres s'affiche | | |
| 2 | Cliquer sur un genre (ex: Jazz) | Les albums/pistes du genre s'affichent | | |

---

## 3. Lecture & Zones

### 3.1 Lecture basique

| Etape | Action | Resultat attendu | Resultat | Commentaire |
|-------|--------|-------------------|----------|-------------|
| 1 | Choisir une zone dans le selecteur de zone | La zone est selectionnee | | |
| 2 | Lancer un album en cliquant sur Play | La lecture demarre sur la zone choisie | | |
| 3 | Appuyer sur Pause | La lecture se met en pause | | |
| 4 | Appuyer sur Play (reprendre) | La lecture reprend a la meme position | | |
| 5 | Appuyer sur Suivant | La piste suivante demarre | | |
| 6 | Ajuster le volume | Le volume change sur l'appareil | | |

### 3.2 Transition entre pistes (gapless)

| Etape | Action | Resultat attendu | Resultat | Commentaire |
|-------|--------|-------------------|----------|-------------|
| 1 | Lancer un album complet sur une zone DLNA | La lecture demarre | | |
| 2 | Attendre la fin de la 1ere piste | La transition vers la 2e piste est fluide, sans coupure | | |
| 3 | Verifier que la piste ne "redemarre" pas | Pas de saut audible ni de restart a 1-2 secondes | | |

### 3.3 File d'attente

| Etape | Action | Resultat attendu | Resultat | Commentaire |
|-------|--------|-------------------|----------|-------------|
| 1 | Lancer un album | Les pistes apparaissent dans la file d'attente | | |
| 2 | Ouvrir la vue File d'attente | Toutes les pistes de l'album sont listees | | |
| 3 | La piste en cours est surlignee | La position actuelle est visible | | |

### 3.4 Sleep timer

| Etape | Action | Resultat attendu | Resultat | Commentaire |
|-------|--------|-------------------|----------|-------------|
| 1 | Lancer une piste en lecture | La lecture est en cours | | |
| 2 | Activer le sleep timer (si disponible dans l'UI zone, sinon curl ci-dessous) | Le timer demarre | | |
| 3 | Attendre la duree configuree | Le volume baisse progressivement puis la lecture s'arrete | | |

Si pas d'UI :
```bash
curl -X POST http://SERVER:8888/api/v1/zones/1/sleep -H "Content-Type: application/json" -d '{"minutes": 1}'
```

---

## 4. Services de streaming

### 4.1 Connexion Qobuz/Tidal

| Etape | Action | Resultat attendu | Resultat | Commentaire |
|-------|--------|-------------------|----------|-------------|
| 1 | Aller dans Reglages > Services de streaming | La liste des services s'affiche | | |
| 2 | Se connecter a Qobuz (ou Tidal) | L'indicateur passe au vert | | |
| 3 | Aller dans Streaming > Qobuz | Le contenu Qobuz s'affiche (nouveautes, playlists) | | |
| 4 | Rechercher un artiste dans Qobuz | Des resultats apparaissent | | |
| 5 | Lancer une piste Qobuz | La lecture demarre sur la zone selectionnee | | |

### 4.2 Navigation genres streaming

| Etape | Action | Resultat attendu | Resultat | Commentaire |
|-------|--------|-------------------|----------|-------------|
| 1 | Aller dans Streaming > Qobuz > Genres | Les genres s'affichent | | |
| 2 | Cliquer sur un genre | Les albums du genre s'affichent | | |

---

## 5. Playlists

### 5.1 Creation et gestion

| Etape | Action | Resultat attendu | Resultat | Commentaire |
|-------|--------|-------------------|----------|-------------|
| 1 | Aller dans Playlists | La liste des playlists s'affiche | | |
| 2 | Creer une nouvelle playlist | La playlist est creee et apparait dans la liste | | |
| 3 | Ajouter des pistes a la playlist | Les pistes sont ajoutees | | |
| 4 | Lancer la playlist | Les pistes sont jouees dans l'ordre | | |

### 5.2 Export (API)

```bash
# Export JSON
curl "http://SERVER:8888/api/v1/playlists/1/export?format=json"

# Export M3U (defaut)
curl "http://SERVER:8888/api/v1/playlists/1/export"

# Export CSV
curl "http://SERVER:8888/api/v1/playlists/1/export?format=csv"
```
**Attendu** : le fichier est telecharge dans le bon format.

---

## 6. Reglages & Administration

### 6.1 Dossiers musicaux

| Etape | Action | Resultat attendu | Resultat | Commentaire |
|-------|--------|-------------------|----------|-------------|
| 1 | Aller dans Reglages > Dossiers musicaux | Les dossiers configures s'affichent | | |
| 2 | Ajouter un nouveau dossier | Le dossier est ajoute | | |
| 3 | Lancer un scan | Les fichiers du nouveau dossier sont indexes | | |

### 6.2 Appareils detectes

| Etape | Action | Resultat attendu | Resultat | Commentaire |
|-------|--------|-------------------|----------|-------------|
| 1 | Aller dans Reglages > Appareils | Les appareils DLNA/AirPlay/Chromecast detectes s'affichent | | |
| 2 | Verifier que chaque appareil a un nom et un type | Noms corrects, types affiches | | |

### 6.3 Zones

| Etape | Action | Resultat attendu | Resultat | Commentaire |
|-------|--------|-------------------|----------|-------------|
| 1 | Aller dans Reglages > Zones | Les zones creees s'affichent | | |
| 2 | Creer une nouvelle zone associee a un appareil | La zone est creee et fonctionnelle | | |

### 6.4 Diagnostics

| Etape | Action | Resultat attendu | Resultat | Commentaire |
|-------|--------|-------------------|----------|-------------|
| 1 | Aller dans Reglages > Diagnostics | Les infos systeme s'affichent (version, moteur, FFmpeg, zones) | | |
| 2 | Verifier que la version affichee est 0.8.20 | Version correcte | | |

---

## 7. Client web — Stabilite

### 7.1 Connexion WebSocket

| Etape | Action | Resultat attendu | Resultat | Commentaire |
|-------|--------|-------------------|----------|-------------|
| 1 | Ouvrir le client web | Le voyant de connexion est vert (stable) | | |
| 2 | Attendre 5 minutes sans action | Le voyant reste vert (pas de deconnexion) | | |
| 3 | Lancer une piste | La position de lecture se met a jour en temps reel | | |

### 7.2 Responsive

| Etape | Action | Resultat attendu | Resultat | Commentaire |
|-------|--------|-------------------|----------|-------------|
| 1 | Redimensionner la fenetre en mode tablette (~768px) | L'interface s'adapte avec icones sidebar | | |
| 2 | Redimensionner en mode mobile (~375px) | La barre de navigation passe en bas | | |

---

## 8. Tests specifiques par plateforme

### 8.1 Windows

| Etape | Action | Resultat attendu | Resultat | Commentaire |
|-------|--------|-------------------|----------|-------------|
| 1 | Lancer tune-server.exe | Le terminal s'ouvre avec les logs, pas de crash | | |
| 2 | Ouvrir http://localhost:8888 | L'interface web s'affiche | | |
| 3 | Verifier la detection des sorties audio USB | Les DAC USB apparaissent dans les zones | | |

### 8.2 Docker

| Etape | Action | Resultat attendu | Resultat | Commentaire |
|-------|--------|-------------------|----------|-------------|
| 1 | `docker pull renesenses/tune:latest` | L'image se telecharge | | |
| 2 | Lancer le conteneur avec `network_mode: host` | Le serveur demarre | | |
| 3 | Verifier la detection DLNA/AirPlay | Les appareils sont detectes | | |

### 8.3 macOS (Homebrew)

| Etape | Action | Resultat attendu | Resultat | Commentaire |
|-------|--------|-------------------|----------|-------------|
| 1 | `brew install renesenses/tap/tune-server` | Installation reussie | | |
| 2 | `tune-server-launcher` | Le serveur demarre | | |
| 3 | Ouvrir http://localhost:8888 | L'interface web s'affiche | | |

### 8.4 iPad (TestFlight)

| Etape | Action | Resultat attendu | Resultat | Commentaire |
|-------|--------|-------------------|----------|-------------|
| 1 | Ouvrir Tune sur iPad | L'app se lance | | |
| 2 | Renseigner l'adresse du serveur | La connexion s'etablit | | |
| 3 | Verifier les zones | Les zones du serveur s'affichent | | |
| 4 | Lancer une piste | La lecture demarre | | |

---

## 9. Tests API uniquement (pas d'UI)

Ces endpoints n'ont pas encore d'interface web. Tester avec curl :

### Documentation API
```bash
curl http://SERVER:8888/api/v1/system/api-docs | python3 -m json.tool | head -20
```
**Attendu** : liste de 60+ endpoints.

### Monitoring proactif
```bash
curl http://SERVER:8888/api/v1/system/api-insights | python3 -m json.tool
```
**Attendu** : `status: "healthy"` ou issues detectees.

### Auto-DJ
```bash
curl "http://SERVER:8888/api/v1/radio/auto?seed_track=1&count=10" | python3 -m json.tool
```
**Attendu** : liste de pistes similaires a la seed.

### Sante bibliotheque
```bash
curl http://SERVER:8888/api/v1/library/stats/completeness | python3 -m json.tool
```
**Attendu** : `health_score` (0-100), `health_grade` (A-F).

---

## Synthese

| Categorie | Tests | OK | KO | Non teste |
|-----------|-------|----|----|-----------|
| Dashboard | 4 | | | |
| Bibliotheque | 8 | | | |
| Lecture & Zones | 10 | | | |
| Streaming | 5 | | | |
| Playlists | 5 | | | |
| Reglages | 8 | | | |
| Stabilite web | 4 | | | |
| Plateforme | 10 | | | |
| API only | 4 | | | |
| **Total** | **58** | | | |

**Testeur** : _______________
**Date** : _______________
**Version** : _______________
**Plateforme** : _______________

**Commentaire general** :


