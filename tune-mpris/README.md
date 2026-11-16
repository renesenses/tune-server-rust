# tune-mpris

Compagnon MPRIS de Tune pour Linux (#8824). Il publie une zone Tune sur le bus
D-Bus de la session (`org.mpris.MediaPlayer2.Tune`) : les touches multimédia du
clavier, les applets de GNOME et KDE et `playerctl` la pilotent (lecture/pause,
suivant, précédent, position, volume) et affichent le titre, l'artiste,
l'album et la pochette.

Il tourne sur le poste de l'utilisateur ; le serveur Tune peut être ailleurs
(NAS, Raspberry Pi, Docker). Aucun réglage côté serveur.

## Lancer

    cargo build --release -p tune-mpris
    tune-mpris --server http://nas:8888 --list-zones
    tune-mpris --server http://nas:8888 --zone Salon

| Option | Variable | Défaut |
|---|---|---|
| `--server` | `TUNE_SERVER` | `http://localhost:8888` |
| `--zone` (identifiant ou nom) | `TUNE_ZONE` | la zone qui joue, sinon une en pause, sinon la première |
| `--token` (JWT des clients, ou `ApiKey <clé>`) | `TUNE_TOKEN` | aucun (authentification coupée) |

Un second compagnon pour une autre zone prend le nom
`org.mpris.MediaPlayer2.Tune.zone<id>`.

Au démarrage de la session : `tune-mpris.service` (service utilisateur systemd).

## Essayer

    playerctl -l                       # tune doit apparaître
    playerctl -p Tune metadata
    playerctl -p Tune play-pause
    playerctl -p Tune volume 0.3
    playerctl -p Tune position 60

La pochette est téléchargée (avec le jeton) dans `$XDG_CACHE_HOME/tune-mpris/`
et publiée en `file://`.
