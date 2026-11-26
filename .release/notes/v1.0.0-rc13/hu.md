Tune 1.0.0-rc13, a Tune 1.0 tizenharmadik kiadásjelöltje. Köszönjük Gros Bidon, r5yk5, Tades, cpadiou77, Pierre, Daniel LEVY, Lio, sashasha, ajm, Jean Valjean, Semakawa, Jon Rotaetxe, hifi25nl, aqs, Christopher J, FabienM, Begastor és Régis bejelentéseit és tesztjeit, alec_eiffel-nek a Smart Upsamplert, cometdomnak a Qobuz-hozzájárulásait, Leventének pedig a dizájnt.

## Újdonságok (New features)

- **Felülmintavételező**: a Rémi (alec_eiffel) által tervezett Smart Upsampler ugyanazon a szinten szól, mint nélküle, és a beállítás módosítása azonnal hallható.
- **Lejátszás és kimenetek**: a Sendspin és a Devialet AIR a feldolgozott hangot kapja (hangszínszabályzó, bővítmények, felülmintavételező). Az elalváskapcsoló a számítógépet is kikapcsolhatja; Linuxon a Tune megjelenik az asztali környezet médiavezérlőiben.
- **Crossfeed**: ingyenes, a beállításaival és előbeállításaival együtt.
- **Konvolúció**: mintavételi frekvenciánként egy impulzusválasz-fájl.
- **DSD**: egy UPnP-szerverről érkező DSD natívan szól az igényes hálózati lejátszókon, és két DSD-szám szünet nélkül követi egymást.
- **Könyvtár**: a fájljaiba írt személyes címkék Tune-címkékké válnak; több album kijelölhető egy csoportos művelethez; hangszer szerinti szűrő.
- **Módosult forrás** váltja fel a „Kétes Hi-Res” jelölést, minden formátumra, a mért levágási frekvenciával.
- **Hullámforma**: finomabb, CUE-számoknál, valamint (opcionálisan) Qobuz- és TIDAL-számoknál is.
- **Lejátszási sor**: az albumok összecsukhatók és egyetlen mozdulattal áthelyezhetők.
- **Kezdőlap**: 5 „Mert szereted…” lejátszási lista, valamint a Legtöbbet hallgatott előadók, Gyakran hallgatott, Meghallgatás később és Hallgatás folytatása widgetek.
- **Szolgáltatások**: a TIDAL „Neked” oldala; a Qobuz műfajonként 200 újdonságot kínál.
- **Tune Circle**: egy kör tagjai látják egymást, ha beleegyeznek, és a Világ nézet az összes Tune-kört mutatja.
- **Webes felület**: török nyelv, a felületről bekapcsolható hitelesítés, valamint számonkénti vagy lejátszásonkénti előzmények.
- **Tune Client (asztali alkalmazás)**: magától megtalálja a hálózat Tune-szervereit, és vezérli a helyi szervert.
- **Windows**: a telepítő felajánlja, hogy a Tune a Windowsszal együtt induljon.
- **iOS-alkalmazás**: teljesebb CarPlay 5G-n, iPaden visszatér az oldalsáv.

## Fejlesztések (Improvements)

- **Átmenetek**: „Auto” módban a ReplayGain az albumerősítést alkalmazza ugyanazon album két száma között; az AAC, az Opus és a CUE-számok szünet nélkül kapcsolódnak.
- **Lejátszás és kimenetek**: az AirPlay 1 és 2 ugyanazt a hangerőgörbét használja; egy rádió gyorsabban indul hálózati lejátszón.
- **Windows**: WASAPI kizárólagos módban a Tune a szám leállítása helyett a másik frekvenciacsaládra vált.
- **Devialet AIR**: 1 dB-es hangerőlépések, 192 kHz-ig.
- **DSD**: a natív DSD felismerése óvatosabb, és újraértékelődik.
- **Nagy könyvtárak**: a Mappák képernyő várakozás nélkül nyílik meg, a háttérfeladatok már nem blokkolják a többit.
- **Szolgáltatások**: a YouTube-hoz a Tune saját Google-bejelentkezési kliense; a Spotify és a TIDAL lejátszás közben megbízhatóbb.
- **Webes felület**: pontosabb parametrikus hangszínszabályzó, és megerősítés törlés előtt.
- **iOS-alkalmazás**: a zárolási képernyő akkor is vezérli a zónát, ha az nem szól; DAC nélkül a csúszka az iOS hangerejét állítja.
- **Frissítés és diagnosztika**: a frissítés a Tune-nal szállított programokat is lecseréli; a hibajelentések elrejtik a neveket.

## Hibajavítások (Fixes)

- **Lejátszás és kimenetek**: a folytatás működik, ha egy szünet felszabadította a DAC-ot; az „Összes ismétlése” már nem téveszti össze a pillanatnyi megállást a szám végével; Windowson a Tune után bekapcsolt DAC is megtalálható.
- **Zónák**: egy már kötött eszköz megnevezi a zónáját; OAAT esetén a folytatás újranyitja a munkamenetet ahelyett, hogy néma maradna.
- **DLNA, UPnP és OpenHome**: nincs több fölösleges Stop a Musicbook Source-on, stabil némítás, a Linn Klimax DS/2 felismerése.
- **AirPlay**: az AirPlay 1 kimenet a Tune újraindítása után már nem maximális hangerővel indul.
- **Jelút**: már nem jelez olyan feldolgozást, amely nem történik meg.
- **Könyvtár**: ugyanazon album DSD- és CD-kiadása már nem olvad össze; a DR írása már nem fagyasztja le a szervert.
- **Szolgáltatások**: a Qobuz- és TIDAL-kedvencek a hosszukkal jelennek meg; a Spotify rovatai már nem duplázódnak.
- **Rádiók**: a FIP-állomások visszakapják a logójukat.
- **Bővítmények és crossfeed**: egy telepített bővítmény újraindítás nélkül működik; a crossfeed már nincs zárolva OAAT-n és a böngészőben.
- **Webes felület**: a rács a végéig görgethető; „a szerver nem válaszol” szalag eltűnik, amint a szerver válaszol.

## Biztonság

- Több biztonsági megerősítés.
- A naplók és a hibajelentések mostantól elrejtik az egyes adatfolyam-címekben található hitelesítő adatokat.
- Az Útvonal cseréje (Beállítások › zenemappák) mostantól ellenőrzi a mappákat, és hitelesítés nélkül már nem érhető el egy másik eszközről.

## Működésbeli változások

- **Tune Circle**: bekapcsolt hitelesítéssel csak rendszergazdáknak; távoli hallgatásból csak olvasható.
- **YouTube**: az e verzió előtt csatlakoztatott fiókokat egyszer újra kell csatlakoztatni.
- **Rádió a sor végén**: ismét alapértelmezés szerint ki van kapcsolva; ha korábban ezt választotta, jelölje be újra a zónában.
- **Hangszínszabályzó**: kikapcsoláskor az előerősítése a „Szint visszaállítása” gombig érvényben marad.
- **Multiroom**: egy csoport 3 zónáig ingyenes; felette Premium szükséges. A sztereó párok ingyenesek maradnak.
- **CUE INDEX 02-vel**: a számok kezdete a következő beolvasáskor javul, az előzmények megmaradnak.
- **Tune OS**: az ideiglenes jelszó a gép képernyőjén vagy a rendszerindító kártyán jelenik meg, már nem a bejelentkezési oldalon; a távoli hozzáférés zárva marad, amíg meg nem változtatják.
- **Profilok és előzmények**: bekapcsolt hitelesítéssel csak a rendszergazda kezeli a többi profilt.
- **Útvonal cseréje**: hitelesítés nélkül arról a gépről, amelyen a Tune fut.

## Ellenőrizze otthon

Ezek a változtatások átmentek az automatikus teszteken, de valódi hardveren még nem próbálták ki őket. Írja meg, mit tapasztal:

- **Sendspin és Devialet AIR** aktív feldolgozással.
- **Windows**: minilejátszó, WASAPI kizárólagos mód, indulás a Windowsszal.
- **AirPlay 1**: a szint induláskor és a számok között.
- **DSD hálózati lejátszóra** (Atoll ST200, Eversolo).
- **Linn Klimax DS/2** OpenHome-ként, **Tune Client macOS-en** és **CarPlay 5G-n**.

## Ismert problémák

- **DSD DoP-n keresztül a helyi kimeneten**: csillapított PCM után teljes kivezérléssel szólhat. Tartsa alacsonyan az erősítő hangerejét.
- **UPnP címváltozás után**: egy szám lehet, hogy nem szólal meg; indexelje újra azt a szervert.
- **Integritásellenőrzés**: néhány fájlt még tévesen jelezhet; ne töröljön semmit csak e döntés alapján.
- **Másik szerver által vezérelt Windows ASIO**: Szünet, majd Lejátszás után nincs hang; lépjen a következő számra.
- **A frissítés után**: ha a felület nem változik, kényszerítse az újratöltést (Cmd+Shift+R vagy Ctrl+Shift+R).
