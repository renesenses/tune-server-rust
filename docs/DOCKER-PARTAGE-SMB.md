# Docker : écouter un partage SMB (NAS, Synology)

Ce guide répond à un cas signalé sur un NAS Synology
([#8069](https://github.com/renesenses/tune-server-rust/issues/8069)) : un
partage Samba monté sur le NAS dans le dossier de musique n'apparaît pas dans
le conteneur Tune, et le bouton de montage d'un partage SMB, dans les
emplacements de la bibliothèque, échoue.

## Pourquoi Tune ne monte pas lui-même un partage dans un conteneur

Pour monter un partage SMB, il faut les privilèges système (root et la capacité
`CAP_SYS_ADMIN`) et l'outil `mount.cifs` (paquet `cifs-utils`). L'image Docker
de Tune n'a ni l'un ni l'autre : le serveur y tourne sous le compte `tune`
(UID 1000), sans `sudo` ni outil de montage. Sur Tune OS (image Raspberry Pi),
un assistant privilégié fait ce travail. Dans un conteneur, cet assistant
n'existe pas.

Le montage depuis Tune ne reste plus muet dans ce cas : il affiche un message
qui explique la marche à suivre. Dans un conteneur, la bonne méthode est de
**monter le partage sur l'hôte**, puis de le **lier au conteneur**.

## 1. Monter le partage sur l'hôte

Sur un Synology, dans DSM : **File Station › Outils › Monter un dossier
distant › Dossier CIFS**. Choisissez comme point de montage un dossier vide
d'un dossier partagé, par exemple `/volume1/music/nas-salon`.

Sur un autre Linux, utilisez une ligne `cifs` dans `/etc/fstab` ou une unité
systemd `.mount`.

## 2. Lier le dossier au conteneur, avec la propagation `rslave`

Par défaut, Docker copie l'arborescence des montages **au démarrage du
conteneur** (propagation `rprivate`). Un partage monté sur l'hôte **après** ce
démarrage n'apparaît donc pas dans le conteneur : le dossier y reste vide.
C'est le cas d'un dossier distant que DSM remonte au démarrage du NAS, parfois
après le lancement des conteneurs.

La propagation `rslave` fait suivre au conteneur les montages ajoutés ensuite
sur l'hôte.

Avec `docker run` :

```bash
docker run -d \
  --name tune-server \
  --network host \
  -v /volume1/music:/music:ro,rslave \
  -v tune-data:/data \
  renesenses/tune:latest
```

Avec Compose (ou un **Projet** de Container Manager sur Synology), il faut la
forme longue du volume :

```yaml
services:
  tune:
    image: renesenses/tune:latest
    volumes:
      - type: bind
        source: /volume1/music
        target: /music
        read_only: true
        bind:
          propagation: rslave
      - tune-data:/data
```

Remplacez `/volume1/music` par le dossier de votre NAS. L'écran **Volume** de
Container Manager ne propose pas toujours le choix de la propagation : dans ce
cas, créez le conteneur à partir d'un projet Compose comme ci-dessus.

Le dossier monté (ici `/music/nas-salon`) se trouve sous `/music`, qui est déjà
un dossier de musique de l'image : relancez une analyse de la bibliothèque. Si
vous liez le partage ailleurs, ajoutez ce dossier aux dossiers de musique dans
les réglages de Tune.

## Si le dossier reste vide

- **Redémarrez le conteneur** une fois le partage monté sur l'hôte. Docker
  reprend au démarrage les montages déjà présents sous le dossier lié, même
  sans `rslave`.
- La propagation ne passe que si le montage du dossier source est lui-même
  partagé sur l'hôte. `findmnt -o TARGET,PROPAGATION --target /volume1/music`
  doit afficher `shared`. Sinon, `mount --make-rshared /volume1` (en root sur
  l'hôte) le rend partagé jusqu'au prochain redémarrage. Sur DSM, ce
  comportement n'a pas été vérifié pour chaque version.
- Vérifiez depuis le conteneur : `docker exec tune-server ls /music`.

## Et `--cap-add SYS_ADMIN` ?

Donner au conteneur la capacité `SYS_ADMIN` (ou `--privileged`) ne suffit pas
avec l'image officielle : le serveur n'y tourne pas en root et l'outil
`mount.cifs` n'y est pas installé. Cela élargirait en outre beaucoup ce que le
conteneur peut faire sur l'hôte. Nous ne le recommandons pas : montez le
partage sur l'hôte.
