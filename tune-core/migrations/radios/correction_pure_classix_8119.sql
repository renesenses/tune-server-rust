-- #8119 : la station semee le 30/08 sous le nom « PureClassic Radio » s'appelle
-- « Pure Classix Radio » (avec un X), et elle est neerlandaise, pas francaise.
--
-- Source : l'annuaire v2 de mozaiklabs.fr (fiche radio-metadata.fr S002163,
-- releve le 09/11/2026) : name = "Pure Classix Radio", country = "NL",
-- genre = "oldies", meme flux http://mscp4.live-streams.nl:8140/flac.ogg et
-- meme site https://www.pureclassix.com. Le semis du 30/08
-- (annuaire_mozaiklabs_2026_08_30.sql) avait recopie l'annuaire v1 de ce
-- jour-la : « PureClassic Radio », France, Jazz. Le signalement vient de
-- l'auteur de radio-metadata.fr lui-meme (fil 2696).
--
-- On ne corrige QUE la fiche semee restee telle quelle : meme flux ET nom
-- d'origine. Une station renommee a la main n'est pas touchee, et chaque
-- champ n'est remplace que s'il porte encore la valeur du semis. Le fichier
-- de semis n'est pas modifie : il est deja joue sur tout le parc.
--
-- Un seul texte pour les deux bases : `include_str!` par la migration SQLite
-- 143 et par la PostgreSQL 109. SQL portable, rejouable sans effet.

UPDATE radio_stations SET country = 'Pays-Bas'
WHERE url = 'http://mscp4.live-streams.nl:8140/flac.ogg'
  AND name = 'PureClassic Radio'
  AND country = 'France';

UPDATE radio_stations SET genre = 'Oldies'
WHERE url = 'http://mscp4.live-streams.nl:8140/flac.ogg'
  AND name = 'PureClassic Radio'
  AND genre = 'Jazz';

UPDATE radio_stations SET name = 'Pure Classix Radio'
WHERE url = 'http://mscp4.live-streams.nl:8140/flac.ogg'
  AND name = 'PureClassic Radio';
