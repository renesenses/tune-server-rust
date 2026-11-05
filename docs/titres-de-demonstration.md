# Titres de démonstration (#7858)

Au premier démarrage d'un serveur sans bibliothèque, Tune peut proposer
d'ajouter un petit jeu de titres de démonstration (CD, Hi-Res, DSD). Aucun
fichier audio n'est dans le dépôt ni dans le binaire : le serveur lit une
liste publiée sur mozaiklabs.fr (le **manifeste**), télécharge chaque titre
et ne le garde que si sa somme SHA-256 est celle de la liste.

Ce document décrit ce qu'il faut publier pour que la fonction marche.

## Où publier le manifeste

```
https://mozaiklabs.fr/tune/demo-tracks/manifest.json
```

La racine `https://mozaiklabs.fr` est remplacée par le réglage
`mozaik_base_url` s'il est posé (le même réglage que le rapport de bogue).
Le manifeste doit répondre en HTTP 200 avec du JSON, et peser moins de
256 Kio.

## Format du manifeste

Exemple complet : [`titres-de-demonstration.exemple.json`](titres-de-demonstration.exemple.json)
(les sommes et adresses y sont fictives ; un test vérifie que cet exemple
reste lisible par le serveur).

```json
{
  "version": 1,
  "tracks": [
    {
      "title": "Exemple CD 16/44,1",
      "artist": "Artiste d'exemple",
      "format": "cd_16_44",
      "license": "CC BY 4.0",
      "license_url": "https://creativecommons.org/licenses/by/4.0/",
      "url": "https://mozaiklabs.fr/tune/demo-tracks/01-exemple-cd.flac",
      "sha256": "<64 caractères hexadécimaux>",
      "size": 31457280,
      "file_name": "01 - Exemple CD 16-44.flac"
    }
  ]
}
```

| Champ | Obligatoire | Règle |
|---|---|---|
| `version` | oui | `1`. Toute autre valeur fait refuser le manifeste. |
| `tracks` | oui | Liste de 1 à 40 titres. |
| `title` | oui | Titre affiché, non vide. |
| `artist` | non | Artiste affiché. |
| `format` | oui | Libellé libre, non vide. Valeurs conseillées : `cd_16_44`, `hires_24_96`, `hires_24_192`, `dsd64`. |
| `license` | oui | La licence, citée telle quelle (`CC BY 4.0`, « accord écrit du label X »…), non vide. |
| `license_url` | non | Lien vers le texte de la licence. |
| `url` | oui | Adresse du fichier, en `https`. |
| `sha256` | oui | Somme SHA-256 du fichier, 64 caractères hexadécimaux (majuscules ou minuscules). |
| `size` | non | Taille exacte en octets. Si elle est donnée, un fichier d'une autre taille est refusé. |
| `file_name` | oui | Nom du fichier sur le serveur : un nom simple (ni `/`, ni `\`, ni `:`, ni `..`, ne commence pas par un point), unique dans la liste, terminé par une extension audio : `flac`, `wav`, `aif`, `aiff`, `dsf`, `dff`, `wv`, `m4a`, `ogg`, `mp3`. |

Les champs inconnus sont ignorés : on peut en ajouter sans casser les
serveurs déjà installés.

Calculer une somme : `sha256sum fichier.flac` (Linux) ou
`shasum -a 256 fichier.flac` (macOS).

Plafond par fichier : 2 Gio.

## Ce que fait le serveur

Routes, sous `/api/v1/onboarding` :

- `GET /demo-tracks` : `available` (le serveur a un dossier de données),
  `offered` (bibliothèque vide et titres pas encore installés), `installed`,
  `dir`, `manifest_url`, et `tracks` (titre, artiste, format, licence, lien de
  licence) une fois installés.
- `GET /status` porte aussi `demo_tracks_offered` : vrai si l'assistant de
  premier démarrage n'est pas terminé, la bibliothèque est vide et les titres
  ne sont pas installés.
- `POST /demo-tracks/add` (administrateur) :
  1. lit le manifeste ; s'il est injoignable, rend 502
     `demo_manifest_unavailable` ; s'il est invalide, 502
     `demo_manifest_invalid` ; rien n'est écrit ;
  2. télécharge chaque titre dans `<données>/demo-tracks/<file_name>.part`,
     vérifie taille et somme, et ne renomme le fichier qu'après vérification ;
     un titre refusé est effacé et signalé (`checksum_mismatch`,
     `size_mismatch`, `http_status_404`…) sans bloquer les autres ;
  3. si aucun titre n'est bon, rend 502 `demo_downloads_failed` et n'ajoute
     rien ;
  4. sinon, pose une copie du manifeste (titres gardés seulement) dans le
     dossier, ajoute le dossier aux dossiers de musique en gardant les autres,
     et lance le scan.
  L'opération se relance sans doublon : un fichier déjà présent et intact
  n'est pas retéléchargé.
- `POST /demo-tracks/remove` (administrateur) : retire le dossier des
  dossiers de musique avec ses seules pistes (même chemin que « Retirer un
  dossier »), puis efface le dossier `<données>/demo-tracks`. Les autres
  dossiers et leurs pistes ne sont pas touchés.

Un seul ajout ou retrait à la fois (409 `demo_tracks_busy` sinon). Un
serveur sans dossier de données (base en mémoire) rend 503
`demo_tracks_unavailable`.

## Ce qui reste à faire

- Choisir les titres et obtenir leurs licences (Bertrand), puis publier les
  fichiers et le manifeste à l'adresse ci-dessus.
- Les boutons « Ajouter les titres de démo » et « Retirer » du client web.
