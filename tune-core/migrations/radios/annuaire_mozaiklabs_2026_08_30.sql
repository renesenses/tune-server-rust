-- Semis du catalogue de radios, releve sur NOTRE annuaire.
--
-- Source : GET https://mozaiklabs.fr/api/v1/radios, releve le 2026-08-30.
-- 51 entrees servies ce jour-la. 25 sont posees ici (issue #2119).
--
-- POURQUOI UN FICHIER FIGE ET PAS UN APPEL RESEAU. Une migration ne doit
-- jamais dependre du reseau : elle tourne au demarrage, avant que quoi que ce
-- soit ne reponde, et un serveur hors ligne ou un annuaire indisponible
-- donnerait une base a moitie semee — differente d'une machine a l'autre,
-- irreproductible, intestable. Le serveur telecharge deja cet annuaire a
-- chaque demarrage (`refresh_radio_logos`, tune-server/src/routes/radios.rs)
-- et n'en garde QUE les logos : c'est precisement ce qui a produit #2119, un
-- annuaire de 51 stations lu en entier et jete, pour un catalogue livre de 24
-- stations toutes francaises. Le contenu est donc fige ICI, dans le depot :
-- reproductible, relisible en revue, testable hors ligne.
--
-- UN SEUL FICHIER POUR LES DEUX BASES. Ce texte est `include_str!` DEUX fois
-- (migration SQLite 90 et migration PostgreSQL 042) : le SQL ci-dessous est
-- volontairement portable — `INSERT ... SELECT ... WHERE NOT EXISTS`, pas de
-- `INSERT OR IGNORE` (SQLite seul) ni de `ON CONFLICT` (qui exigerait une
-- contrainte d'unicite que `radio_stations` n'a pas dans le schema). Ecrire
-- deux fichiers, c'est se garantir qu'ils divergeront.
--
-- IDEMPOTENCE. Chaque insertion est gardee par
-- `WHERE NOT EXISTS (... url = ... OR name = ...)`. Rejouer le fichier
-- n'ajoute rien, et une station deja presente n'est PAS dupliquee — qu'elle
-- ait ete saisie a la main ou posee par le bouton « + Ajouter a Tune » de la
-- page Radios (`add_from_web`, qui ecrit exactement ces adresses).
--
-- La garde porte sur l'URL **ou** le nom, et c'est deliberement plus large que
-- les migrations 70/78/86, qui ciblent l'URL seule. Elles SUPPRIMENT : viser
-- large y detruirait du travail, donc elles visent l'identifiant du flux. Ce
-- fichier AJOUTE : viser large n'y coute qu'une station non posee, alors que
-- viser etroit poserait un deuxieme « TSF Jazz » a qui avait repointe le sien
-- vers son propre relais — et le catalogue livre ne doit jamais porter deux
-- fois le meme nom (test `le_catalogue_livre_a_une_forme_valide`).
--
-- CE QUE CE SEMIS N'ECRASE PAS. Il n'y a ici aucun UPDATE : une station que
-- l'utilisateur a renommee, re-genree, remise en favori ou repointee garde
-- TOUTES ses valeurs. Et une station qu'il SUPPRIME ne revient pas : la
-- migration est enregistree dans `_migrations` / `schema_version` au premier
-- passage et n'est plus jamais rejouee. C'est aussi pourquoi ce fichier ne
-- contient AUCUNE des 24 stations du semis d'origine (migration 33) : les
-- reposer ici ressusciterait chez tout le monde celles que leur proprietaire
-- avait retirees.
--
-- CE QUI A ETE RETIRE DE L'ANNUAIRE AVANT DE FIGER, ET POURQUOI.
-- Les 51 adresses ont ete sondees le 2026-08-30 (curl -L, 9 s, redirections
-- suivies, statut ET content-type releves — un 200 `text/html` est le pire
-- cas : rien n'echoue et l'auditeur n'a que du silence). Meme verdict que le
-- sondage du 2026-08-28 de la migration 86.
--
--   * 20 entrees portent une URL DEJA semee par la migration 33 : sautees
--     (elles seraient de toute facon refusees par la garde `NOT EXISTS`).
--   * BBC Radio 3 (`stream.live.vc.bbcmedia.co.uk/bbc_radio_three`) : 200
--     `text/html`, redirige vers www.bbc.co.uk. RETIREE — aucune adresse de
--     remplacement verifiee, cf. migration 86.
--   * France Musique Musiques du monde (`francemusiqueocoramondial`) et
--     Mouv Xtra (`mouvxtra`) : 404. RETIREES — ce sont exactement les deux
--     stations que la migration 78 arrache ; les semer serait poser ce qu'une
--     autre migration supprime.
--   * Caribbean variety mix (`.../listen/caribbean/live.flac`) : 404, station
--     disparue de chez l'operateur. RETIREE, cf. migration 86.
--   * WBGO Jazz, Reggae Classic Mix, Classic Oldies Mix : l'adresse de
--     l'annuaire est morte (DNS injoignable pour la premiere, 404 pour les
--     deux autres). SEMEES AVEC L'ADRESSE DE REMPLACEMENT deja verifiee par la
--     migration 86, re-sondee ici : 200 `audio/mpeg` (638 Ko en 9 s) pour
--     WBGO, 200 `audio/aac` pour les deux autres.
--   * Deux entrees nommees exactement « Radio Paradise » (annuaire id 49,
--     `rock-flac`, et id 63, `aac-128`) : RETIREES TOUTES LES DEUX. Elles
--     portent le meme nom, ce que le test `le_catalogue_livre_a_une_forme_valide`
--     interdit au catalogue livre — et chacune double un canal deja seme ici
--     dans une version superieure : `rock-flacm` (Radio Paradise Rock Mix) et
--     `flacm` (Radio Paradise - Main Mix), tous deux FLAC AVEC metadonnees.
--     Ce sont les trois adresses que Bilou citait au fil 1506.
--
-- FORME NORMALISEE, VALEURS VERBATIM. `country` et `genre` sont ramenes au
-- vocabulaire deja livre par la migration 33 — le catalogue est UNE liste et
-- doit se lire d'une seule facon ; melanger « France » et « FR » dans la meme
-- colonne serait un defaut visible, et `search` filtre sur `country` et
-- `genre` (radio_repo.rs), en francais donc.
--   pays  : FR→France, GB→Royaume-Uni, US→Etats-Unis, CH→Suisse, CA→Canada,
--           BE→Belgique, JP→Japon, NL→Pays-Bas
--   genre : jazz→Jazz, classical→Classique, rock→Rock, eclectic→Eclectique,
--           electronic→Electronique, blues→Blues, world→Monde, reggae→Reggae
--           (les entrees deja en francais sont reprises telles quelles)
-- La VALEUR, elle, est celle de l'annuaire, sans correction. Quelques-unes
-- sont fausses (Radio Paradise donne « FR », PureClassic Radio « jazz »…) :
-- cela se corrige SUR LE SITE, en une edition qui repare du meme coup le
-- bouton « + Ajouter a Tune », les logos et le prochain semis. Corriger ici
-- recreerait la divergence annuaire/produit qui EST le sujet de #2119.
--
-- `logo_url` est pose absolu (prefixe `https://mozaiklabs.fr`, comme le fait
-- `refresh_radio_logos`) : ces 25 stations ont donc leur vignette des
-- l'installation, sans attendre le rattrapage de fond, et meme hors ligne
-- (#2421). `refresh_radio_logos` n'ecrase jamais un logo deja pose.
--
-- `codec` et `bitrate` restent NULL, comme pour les 24 stations du semis
-- d'origine : l'annuaire ne sert qu'un champ `quality` (« flac », « 128k »,
-- « aac192 »…) dont la moitie des valeurs ne dit pas le codec — « 128k » y
-- designe aussi bien un MP3 (Radio Nostalgie) qu'un AAC (Radio Paradise).
--
-- POUR REGENERER ce fichier : relever `GET https://mozaiklabs.fr/api/v1/radios`,
-- sonder chaque `stream_url` (statut + content-type), ecarter ce qui ne rend
-- pas d'audio et ce qu'une migration supprime, appliquer les deux tables de
-- normalisation ci-dessus. Le test `le_semis_de_l_annuaire_ne_pose_rien_qu_une_migration_supprime`
-- garde la deuxieme regle, `le_catalogue_livre_a_une_forme_valide` la forme.

INSERT INTO radio_stations (name, url, homepage, logo_url, country, genre)
SELECT 'Blueswave Radio', 'http://blueswave.radio:8050/FlacBlues', 'https://blueswave.radio/fm223/radiochannel/blueswave-radio-flac/', 'https://mozaiklabs.fr/storage/radio-logos/01KV5ZRSNMGJGJKQYSHHDBTYJA.png', 'Suisse', 'Blues'
WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = 'http://blueswave.radio:8050/FlacBlues' OR name = 'Blueswave Radio');

INSERT INTO radio_stations (name, url, homepage, logo_url, country, genre)
SELECT 'Classic 21', 'http://radio.rtbf.be/c21/aac-128/fl', 'https://www.fluxradios.com/', 'https://mozaiklabs.fr/storage/radio-logos/01KV37KEFSM2N3E546813VZHBG.jpeg', 'Belgique', 'Rock'
WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = 'http://radio.rtbf.be/c21/aac-128/fl' OR name = 'Classic 21');

INSERT INTO radio_stations (name, url, homepage, logo_url, country, genre)
SELECT 'Classic Oldies Mix (Tubes des années 50, 60, 70)', 'https://radio.jamminvibezonline.ca/listen/oldies/stream.aac', NULL, 'https://mozaiklabs.fr/storage/radio-logos/01M0YKC0X9MQ4BB9Y3Y1BNW2FE.jpg', 'France', 'Éclectique'
WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = 'https://radio.jamminvibezonline.ca/listen/oldies/stream.aac' OR name = 'Classic Oldies Mix (Tubes des années 50, 60, 70)');

INSERT INTO radio_stations (name, url, homepage, logo_url, country, genre)
SELECT 'Fip Cultes', 'https://icecast.radiofrance.fr/fipcultes-hifi.aac', 'https://www.radiofrance.fr/fip/radio-cultes', 'https://mozaiklabs.fr/storage/radio-logos/01KV7KNMK6ZH6BKYMCN7Z1R5DY.jpg', 'France', 'Monde'
WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = 'https://icecast.radiofrance.fr/fipcultes-hifi.aac' OR name = 'Fip Cultes');

INSERT INTO radio_stations (name, url, homepage, logo_url, country, genre)
SELECT 'JB Radio2', 'https://mediacp.jb-radio.net:8001/flac', 'https://jb-radio.net/Home', 'https://mozaiklabs.fr/storage/radio-logos/ClyXovcV79vPdxnUuERqslkZKaiDC1fUIzTqmHWZ.png', 'Canada', 'Éclectique'
WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = 'https://mediacp.jb-radio.net:8001/flac' OR name = 'JB Radio2');

INSERT INTO radio_stations (name, url, homepage, logo_url, country, genre)
SELECT 'KEXP', 'https://kexp-mp3-128.streamguys1.com/kexp128.mp3', 'https://www.kexp.org', 'https://mozaiklabs.fr/storage/radio-logos/01KVW6TP58RYZAV40QA8BHMKGH.png', 'États-Unis', 'Éclectique'
WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = 'https://kexp-mp3-128.streamguys1.com/kexp128.mp3' OR name = 'KEXP');

INSERT INTO radio_stations (name, url, homepage, logo_url, country, genre)
SELECT 'Le Bon Mix', 'https://stream10.xdevel.com/audio17s976748-2218/stream/icecast.audio', 'https://www.lebonmix.radio/', 'https://mozaiklabs.fr/storage/radio-logos/01KVW6SQP724B15XKYPVVQBSC9.jpeg', 'France', 'Éclectique'
WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = 'https://stream10.xdevel.com/audio17s976748-2218/stream/icecast.audio' OR name = 'Le Bon Mix');

INSERT INTO radio_stations (name, url, homepage, logo_url, country, genre)
SELECT 'Linn Classical', 'http://radio.linn.co.uk:8004/autodj', 'https://www.linn.co.uk/radio', 'https://mozaiklabs.fr/storage/radio-logos/01KV7KTF3XH3YPCREBT6944905.jpg', 'Royaume-Uni', 'Classique'
WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = 'http://radio.linn.co.uk:8004/autodj' OR name = 'Linn Classical');

INSERT INTO radio_stations (name, url, homepage, logo_url, country, genre)
SELECT 'Linn Jazz', 'http://radio.linn.co.uk:8003/autodj', 'https://www.linn.co.uk/linn-radio', 'https://mozaiklabs.fr/storage/radio-logos/01KV7KS3AZB37WJ8N1X1B4YDPV.jpg', 'Royaume-Uni', 'Jazz'
WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = 'http://radio.linn.co.uk:8003/autodj' OR name = 'Linn Jazz');

INSERT INTO radio_stations (name, url, homepage, logo_url, country, genre)
SELECT 'Morow', 'http://stream.fr.morow.com:8080/morow_hi.aacp', 'https://www.morow.com/', 'https://mozaiklabs.fr/storage/radio-logos/01KZ1FYWGVG0WN84SDQCE68G6E.png', 'Canada', 'Rock'
WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = 'http://stream.fr.morow.com:8080/morow_hi.aacp' OR name = 'Morow');

INSERT INTO radio_stations (name, url, homepage, logo_url, country, genre)
SELECT 'Naim Classical', 'https://mscp3.live-streams.nl:8252/class-flac.flac', 'https://www.naimaudio.com/fr/actualites/naim-radio', 'https://mozaiklabs.fr/storage/radio-logos/01KZ0HRVDAHG02G3HRXADY8H1Q.jpg', 'Royaume-Uni', 'Classique'
WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = 'https://mscp3.live-streams.nl:8252/class-flac.flac' OR name = 'Naim Classical');

INSERT INTO radio_stations (name, url, homepage, logo_url, country, genre)
SELECT 'naim JAZZ', 'http://mscp3.live-streams.nl:8340/jazz-high.aac', 'https://www.naimaudio.com/news/naim-radio', 'https://mozaiklabs.fr/storage/radio-logos/l7yK9D9tTCebccJcG9EO2PzviJH5cfiXwnbtp7Bs.jpg', 'Royaume-Uni', 'Jazz'
WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = 'http://mscp3.live-streams.nl:8340/jazz-high.aac' OR name = 'naim JAZZ');

INSERT INTO radio_stations (name, url, homepage, logo_url, country, genre)
SELECT 'NTS Radio', 'https://stream-relay-geo.ntslive.net/stream', 'https://www.nts.live', 'https://mozaiklabs.fr/storage/radio-logos/01KV2QXB8EXNAMBV8VAT8VV91Y.png', 'Royaume-Uni', 'Électronique'
WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = 'https://stream-relay-geo.ntslive.net/stream' OR name = 'NTS Radio');

INSERT INTO radio_stations (name, url, homepage, logo_url, country, genre)
SELECT 'Progressieve Rock', 'https://progressieverock.nl/flac', 'https://progressieverock.nl/website.php', 'https://mozaiklabs.fr/storage/radio-logos/o7cCcDNZI0YWEHD85wiPbKZtwJXgKLlXlcLo8lSz.png', 'Pays-Bas', 'Rock'
WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = 'https://progressieverock.nl/flac' OR name = 'Progressieve Rock');

INSERT INTO radio_stations (name, url, homepage, logo_url, country, genre)
SELECT 'PureClassic Radio', 'http://mscp4.live-streams.nl:8140/flac.ogg', 'https://www.pureclassix.com', 'https://mozaiklabs.fr/storage/radio-logos/01KV8RHYEC2ZJBCYXJ8E6EHYKB.png', 'France', 'Jazz'
WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = 'http://mscp4.live-streams.nl:8140/flac.ogg' OR name = 'PureClassic Radio');

INSERT INTO radio_stations (name, url, homepage, logo_url, country, genre)
SELECT 'Radio Calico', 'https://stream.radio-calico.com/calico', 'https://www.radio-calico.com/#nowplaying', 'https://mozaiklabs.fr/storage/radio-logos/01KV8RFRX7SPE2PNPYFJXTA35V.png', 'États-Unis', 'Éclectique'
WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = 'https://stream.radio-calico.com/calico' OR name = 'Radio Calico');

INSERT INTO radio_stations (name, url, homepage, logo_url, country, genre)
SELECT 'Radio Nostalgie', 'https://streaming.nrjaudio.fm/oug7girb92oc', NULL, 'https://mozaiklabs.fr/storage/radio-logos/01M0TD7HK9P3BMWNS2HH6MW106.jpg', 'France', 'Éclectique'
WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = 'https://streaming.nrjaudio.fm/oug7girb92oc' OR name = 'Radio Nostalgie');

INSERT INTO radio_stations (name, url, homepage, logo_url, country, genre)
SELECT 'Radio Paradise - Main Mix', 'http://stream.radioparadise.com/flacm', 'https://radioparadise.com/home', 'https://mozaiklabs.fr/storage/radio-logos/jE3DXAtjaWtncvz7ClDfejMYzD8TRnxO6dQhlvjW.jpg', 'France', 'Éclectique'
WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = 'http://stream.radioparadise.com/flacm' OR name = 'Radio Paradise - Main Mix');

INSERT INTO radio_stations (name, url, homepage, logo_url, country, genre)
SELECT 'Radio Paradise Rock Mix', 'http://stream.radioparadise.com/rock-flacm', NULL, 'https://mozaiklabs.fr/storage/radio-logos/xvC74ErFH24E9tPn1szqvXa5jJ7u7zEfJeZ5Gh7t.jpg', 'États-Unis', 'Rock'
WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = 'http://stream.radioparadise.com/rock-flacm' OR name = 'Radio Paradise Rock Mix');

INSERT INTO radio_stations (name, url, homepage, logo_url, country, genre)
SELECT 'Radio Swiss Classic', 'https://stream.srg-ssr.ch/m/rsc_fr/mp3_128', 'https://www.radioswissclassic.ch', 'https://mozaiklabs.fr/storage/radio-logos/radio-swiss-classic.jpg', 'Suisse', 'Classique'
WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = 'https://stream.srg-ssr.ch/m/rsc_fr/mp3_128' OR name = 'Radio Swiss Classic');

INSERT INTO radio_stations (name, url, homepage, logo_url, country, genre)
SELECT 'Radio Swiss Jazz', 'https://stream.srg-ssr.ch/m/rsj/mp3_128', 'https://www.radioswissjazz.ch', 'https://mozaiklabs.fr/storage/radio-logos/radio-swiss-jazz.jpg', 'Suisse', 'Jazz'
WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = 'https://stream.srg-ssr.ch/m/rsj/mp3_128' OR name = 'Radio Swiss Jazz');

INSERT INTO radio_stations (name, url, homepage, logo_url, country, genre)
SELECT 'Reggae Classic Mix', 'https://radio.jamminvibezonline.ca/listen/reggae/stream.aac', NULL, 'https://mozaiklabs.fr/storage/radio-logos/01M0YKDJT00T2HYCDMX6C94P1B.jpg', 'France', 'Reggae'
WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = 'https://radio.jamminvibezonline.ca/listen/reggae/stream.aac' OR name = 'Reggae Classic Mix');

INSERT INTO radio_stations (name, url, homepage, logo_url, country, genre)
SELECT 'Shonan Beach', 'https://shonanbeachfm.out.airtime.pro/shonanbeachfm_c', 'https://www.beachfm.co.jp', 'https://mozaiklabs.fr/storage/radio-logos/EQYjqnQwcnLq9MKkaWjG6m3p0AKrcOGSKltzYqdy.png', 'Japon', 'Jazz'
WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = 'https://shonanbeachfm.out.airtime.pro/shonanbeachfm_c' OR name = 'Shonan Beach');

INSERT INTO radio_stations (name, url, homepage, logo_url, country, genre)
SELECT 'TSF Jazz', 'https://tsfjazz.ice.infomaniak.ch/tsfjazz-high.mp3', 'https://www.tsfjazz.com', 'https://mozaiklabs.fr/storage/radio-logos/01KV2KH5TTBZF7NNMN370NPWSP.png', 'France', 'Jazz'
WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = 'https://tsfjazz.ice.infomaniak.ch/tsfjazz-high.mp3' OR name = 'TSF Jazz');

INSERT INTO radio_stations (name, url, homepage, logo_url, country, genre)
SELECT 'WBGO Jazz', 'https://ais-sa8.cdnstream1.com/3630_128.mp3', 'https://www.wbgo.org', 'https://mozaiklabs.fr/storage/radio-logos/wbgo-jazz.png', 'États-Unis', 'Jazz'
WHERE NOT EXISTS (SELECT 1 FROM radio_stations WHERE url = 'https://ais-sa8.cdnstream1.com/3630_128.mp3' OR name = 'WBGO Jazz');
