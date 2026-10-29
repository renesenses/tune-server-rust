# Import de filtres APO stéréo

L'import ParametricEQ accepte une sélection de canaux avant les filtres :

```text
Preamp: -6 dB
Channel: L
Filter 1: ON PK Fc 100 Hz Gain -3 dB Q 1
Channel: R
Filter 1: ON PK Fc 200 Hz Gain -4 dB Q 1
Channel: all
Filter 1: ON PK Fc 1000 Hz Gain -2 dB Q 1
```

`L` ou `1` sélectionne la gauche ; `R` ou `2` la droite. Plusieurs positions
peuvent être séparées par des espaces (`Channel: L R`), sans doubler un canal
répété. La sélection vaut pour les filtres suivants, jusqu'à une nouvelle
directive. `all` rétablit tous les canaux, comme un fichier sans directive.
La casse n'importe pas ; `Channel: R # droite` accepte le commentaire.
Les indices enregistrés sont 0 pour la gauche et 1 pour la droite ; une bande
sans champ `channel` reste globale, y compris dans les anciens préréglages.

Ce n'est pas un interpréteur complet d'Equalizer APO. Les identifiants de
configurations multicanales (`C`, `LFE`, etc.), les positions autres que 1/2,
les listes séparées par des virgules et les préamplis après une sélection
ciblée sont refusés avec leur numéro de ligne. Aucune partie du fichier n'est
enregistrée après ce refus. Les commandes de copie, d'inclusion ou de choix
de périphérique ne sont pas prises en charge.

Comme pour les fichiers AutoEq précédents, le `Preamp` global est lu pour
information et n'est pas ajouté au pré-gain automatique de Tune : la marge
réservée par l'égaliseur est déjà appliquée par canal. Le compte rendu donne
`reserved_headroom_db_per_channel`, gauche puis droite. La valeur scalaire
`reserved_headroom_db` est l'atténuation minimale garantie sur les deux canaux
(la valeur la moins négative), identique à l'ancien résultat pour un profil
global. La couverture du préampli annoncé n'est affirmée que si elle vaut sur
les deux canaux. Importer crée un préréglage ; son activation reste distincte.

Référence de syntaxe : [documentation Equalizer APO, Channel](https://sourceforge.net/p/equalizerapo/wiki/Configuration%20reference/#channel-since-version-08).
Le format complet décrit davantage de positions ; ce sous-ensemble stéréo
évite d'inventer leur correspondance sans connaître la configuration de sortie.
