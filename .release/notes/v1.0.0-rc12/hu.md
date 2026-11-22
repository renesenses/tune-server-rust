Tune 1.0.0-rc12, a Tune 1.0 tizenkettedik kiadásjelöltje. Több képernyő új megjelenést kap, amelyet Levente, a Tune közösségének tervezője rajzolt.

## Újdonságok (New features)

- **Új felület**: újratervezett Tune Circle (Felfedezés, Üzenetek, Megosztott lejátszási listák, Saját kör); albumoldal leírással, közreműködőkkel, részletekkel és egy Verziók füllel a katalógusszámmal; előadói oldal nagy portréval, számokkal és fülekkel; a Beállítások egy jobban olvasható oszlopban; Szerverállapot és hallgatási irányítópult, időszakválasztással.
- **Kezdőlap**: új „Hallgatási áttekintés (új elrendezés)” widget, amely a Kezdőlap › Szerkesztés alatt adható hozzá, Zónák panellel; nagyobb csempék évvel és műfajjal.
- **Könyvtár**: rögzítse a kívánt szűrőket a Szűrők gombbal; klasszikus zene Karmester, Együttes / zenekar és Mű mezőkkel; csatornák száma és Sztereó szűrő; térkép és szűrő az előadó országa szerint; „Útvonal cseréje” egy zenemappához; hamis Hi-Res felismerése, alapértelmezés szerint kikapcsolva.
- **Szám műszaki adatlapja**: spektrogram és csatornánkénti hullámforma, a könyvtárban lévő Qobuz- és TIDAL-számokhoz is.
- **Rádió a sor végén**: amikor a lejátszási sor véget ér, a Tune hasonló számokkal folytatja, „Rádió” jelöléssel; zónánként állítható.
- **Elalváskapcsoló**: a zene a választott időpontban leáll; Tune OS-en a gép is kikapcsolhat.
- **Lejátszási listák**: lejátszások száma és utolsó lejátszás minden sorban; Eltávolítás kereszt és „Eltávolítás ebből a lejátszási listából”; szám eltávolítása a saját Qobuz-lejátszási listából.
- **Rádió**: Veszteségmentes, Kiváló minőség és Hi-Res szűrők egy sorban; a címtár egy állomása csak az Ön számára szerkeszthető; egy rádió hozzáadható lejátszási listához.
- **Lejátszás**: natív DSD a Linux helyi kimenetén (ALSA); rögzített PCM-mintavételi frekvencia zónánként; zónánként állítható kizárólagos mód; a törölt zónák visszaállíthatók; valódi AccurateRip-ellenőrzés a CD-khez.
- **DLNA**: folyamatos adásfolyam a számok között azokhoz a rendererekhez, amelyek rosszul illesztenek össze két számot, alapértelmezés szerint kikapcsolva; egy csoport fő hangereje bármelyik tagjáról állítható.
- **Streaming**: a TIDAL-katalógus országának kiválasztása; a Qobuz újdonságai megegyeznek a Qobuz-lejátszóéval; a Spotify csatlakozási segédet kap, a Spotify Connect pedig a választott zónában szól.
- **Jelút**: egy feldolgozási lépésre kattintva megnyílnak a beállításai; a csoportos lejátszás minden kimenetet megmutat.
- **Beállítások és felület**: keresés beállítás szerint, szövegméret, hullámforma alakú folyamatjelző sáv, egyetlen keresőmező minden képernyőn, hangbevitel a segédben.

## Fejlesztések (Improvements)

- **Sebesség**: az intelligens gyűjtemények és az ábécés ugrások sokkal gyorsabbak nagy könyvtárban; a nagy listák már nem lassítják a szervert.
- **Memória**: a Tune hosszú hallgatás közben visszaadja a felszabadult memóriát; az átkódolás memóriája korlátozott.
- **Gyorsabb hangindulás**: a YouTube hamarabb indul; a TIDAL helyi vagy OAAT-kimeneten már nem várja meg a teljes szám letöltését.
- **Statisztikák**: egyetlen szám- és albumösszeg minden képernyőn, kodekenként egy bejegyzés, keresés a teljes rangsorban.
- **Tune Circle**: emojik az Üzenetekben; ha a hallgatás egy ismerősnél le van zárva, a Tune ugyanazt a számot keresi az Ön forrásaiban.
- **Mobilalkalmazások**: a CarPlay közvetlenül a telefonon játszik, Gyűjteményekkel és Rádió füllel; az Android megkapja a TIDAL-katalógust, a rádiók és podcastok előzményeit, valamint egy Kedvenc rádiók widgetet.
- **Windows**: egy sáv figyelmeztet, ha a tűzfal szűri a Tune-t; WASAPI- vagy ASIO-jelvény a helyi zónákon.
- **Docker**: az SMB-megosztási segéd működik a lemezképben; a CD-meghajtó használható a konténerben.
- **Fordítások**: helyes többes számok mind a 15 nyelven.
- **Függőségek** frissítve, köztük a beépített SQLite 3.54.

## Hibajavítások (Fixes)

- **Webes felület**: nincs többé fekete képernyő a felület frissítése után.
- **Némítás**: a némított zóna néma marad, amikor új szám indul; egy ébresztő vagy egy hangerőkorlát már nem oldja fel a némítást.
- **Lejátszás**: egy sérült fájl már nem fagyasztja le a zónát, a Tune továbblép; egy befejezett podcastepizód már nem nyílik meg újra a végén.
- **AirPlay 2**: szünet után a lejátszás ott folytatódik, ahol abbamaradt, és a zóna hangerején indul, sosem teljes erővel.
- **DLNA és csoportok**: a lejátszás 701-es hiba után folytatódik; a következő szám előre elő van készítve; egy nem válaszoló eszköz már nem blokkolja a többi zónát; a Sonos a modellje nevét kapja.
- **HQPlayer, Devialet és Diretta**: a pozíció és az állapot követése, automatikus újracsatlakozás; a Devialet erősítő újra válaszol; nincs többé látszólagos hangerőcsúszka a Direttán.
- **OAAT**: a natív DSD megjelenik és bit-perfect; a DSD1024 felismerve, PCM-re váltással korlátozott vevőn.
- **Rádiók**: nincsenek többé ismétlődő állomások; az Ön országában elutasított rádió a következő adásfolyamára lép.
- **Spotify, Qobuz és TIDAL**: a Spotify lejátszási listái és kedvencei az új API-t követik; egy Qobuz-elutasítás már nem jelenik meg hibás hitelesítő adatokként; a TIDAL előtöltése már nem késlelteti a lejátszást.
- **Könyvtár**: a hallgatások számonként számolódnak; egy kötőjel már nem hoz létre hamis előadót; egy díszdoboz lemezei jobban felismerhetők; az év nélküli szám az albuma évét kapja.
- **Windows**: egy hiányzó DAC már nem a PC hangszóróira küldi a hangot; a mappafigyelés a hálózati megosztásokra is kiterjed.
- **PostgreSQL 18**: az UPnP-kedvencek újra működnek.

## Biztonság

- A Tune felügyelete a hálózat egy másik gépéről mostantól bejelentkezést igényel (Tune OS: az új lemezképig).
- A webhookok már nem célozhatják magát a Tune-gépet.
- A Home Assistant és a Hue konfigurálása rendszergazdai jogokat vagy magát a Tune-gépet igényli; a Tune-nal azonos gépre telepített Home Assistant már nem támogatott.

## Ismert problémák

- **DSD DoP-n keresztül a helyi kimeneten**: csillapított PCM után teljes kivezérléssel szólhat, a hangerőt figyelmen kívül hagyva. Tartsa alacsonyan az erősítő hangerejét, amikor PCM-számról DoP-n keresztüli DSD-számra vált (következő verzió).
- **Integritásellenőrzés**: sok FLAC-fájlt tévesen sérültként jelezhet; ne töröljön fájlt csak e döntés alapján.
- **Másik szerver által vezérelt Windows ASIO**: nincs hang Szünet, majd Lejátszás után; lépjen a következő számra.
- **A frissítés után**: ha a felület nem változik, kényszerítse az újratöltést (Cmd+Shift+R vagy Ctrl+Shift+R). Az első indítás tovább tart, amíg két index létrejön.
