Tune 1.0.0-rc13, trzynasta wersja kandydująca Tune 1.0. Dziękujemy Gros Bidon, r5yk5, Tades, cpadiou77, Pierre, Daniel LEVY, Lio, sashasha, ajm, Jean Valjean, Semakawa, Jon Rotaetxe, hifi25nl, aqs, Christopher J, FabienM, Begastor i Régis za zgłoszenia i testy, alec_eiffel za Smart Upsampler, cometdom za wkład w obsługę Qobuz, a Levente za projekt graficzny.

## Nowości (New features)

- **Nadpróbkowanie**: Smart Upsampler Rémiego (alec_eiffel) gra na tym samym poziomie co bez niego, a zmianę ustawień słychać od razu.
- **Odtwarzanie i wyjścia**: Sendspin i Devialet AIR otrzymują przetworzony dźwięk (korektor, wtyczki, nadpróbkowanie). Wyłącznik czasowy może wyłączyć komputer; systemowe sterowanie multimediami w Linuksie.
- **Crossfeed**: bezpłatny, razem ze swoimi ustawieniami i presetami.
- **Splot**: jeden plik odpowiedzi impulsowej na każdą częstotliwość próbkowania.
- **DSD**: DSD z serwera UPnP jest odtwarzane natywnie na wymagających odtwarzaczach sieciowych, a dwa utwory DSD następują po sobie bez przerwy.
- **Biblioteka**: Twoje osobiste tagi stają się tagami Tune; zaznaczanie kilku albumów dla zbiorczej akcji; filtr według instrumentu.
- **Zmienione źródło** zastępuje „Wątpliwe Hi-Res”, dla wszystkich formatów, ze zmierzoną częstotliwością odcięcia.
- **Przebieg** drobniejszy, także dla utworów CUE, Qobuz i TIDAL (opcjonalnie).
- **Kolejka**: albumy można zwijać i przenosić jednym gestem.
- **Start**: 5 playlist „Ponieważ lubisz…” oraz widżety Najczęściej odtwarzani wykonawcy, Często słuchane, Posłuchaj później i Wznów słuchanie.
- **Serwisy**: strona „Dla Ciebie” w TIDAL; 200 nowości Qobuz na gatunek.
- **Tune Circle**: członkowie kręgu widzą się nawzajem, jeśli się zgodzą, a widok Świat pokazuje wszystkie kręgi.
- **Interfejs WWW**: turecki, uwierzytelnianie włączane z poziomu interfejsu oraz historia według utworu lub odsłuchu.
- **Tune Client (aplikacja desktopowa)**: sama znajduje serwery w sieci i steruje lokalnym serwerem.
- **Windows**: uruchamianie Tune razem z Windows, proponowane przez instalator.
- **Aplikacja iOS**: pełniejszy CarPlay w sieci 5G, powrót paska bocznego na iPadzie.

## Ulepszenia (Improvements)

- **Przejścia**: ReplayGain „Auto” zachowuje wzmocnienie albumowe między dwoma utworami tego samego albumu; AAC, Opus i utwory CUE łączą się bez przerwy.
- **Odtwarzanie i wyjścia**: ta sama krzywa głośności w AirPlay 1 i 2; radio startuje szybciej na odtwarzaczu sieciowym.
- **Windows**: w trybie wyłącznym WASAPI Tune przechodzi na drugą rodzinę częstotliwości zamiast zatrzymywać utwór.
- **Devialet AIR**: głośność w krokach co 1 dB, do 192 kHz.
- **DSD**: wykrywanie natywnego DSD jest ostrożniejsze i ponownie oceniane.
- **Duże biblioteki**: ekran Foldery otwiera się bez czekania, a zadania w tle nie blokują już reszty.
- **Serwisy**: własny klient logowania Google w Tune dla YouTube; Spotify i TIDAL stabilniejsze w trakcie odtwarzania.
- **Interfejs WWW**: dokładniejszy korektor parametryczny i potwierdzenie przed usunięciem.
- **Aplikacja iOS**: ekran blokady obsługuje strefę nawet wtedy, gdy nie gra; bez DAC suwak reguluje głośność iOS.
- **Aktualizacja i diagnostyka**: aktualizacja zastępuje też programy dostarczane z Tune; raporty błędów ukrywają nazwy.

## Poprawki (Fixes)

- **Odtwarzanie i wyjścia**: wznawianie działa, gdy pauza zwolniła DAC; „Powtarzaj wszystko” nie myli już chwilowego zatrzymania z końcem utworu; w Windows DAC włączony po Tune jest wykrywany.
- **Strefy**: urządzenie już powiązane podaje nazwę swojej strefy; w OAAT wznowienie ponownie otwiera sesję zamiast pozostać w ciszy.
- **DLNA, UPnP i OpenHome**: koniec zbędnego Stop na Musicbook Source, stabilne wyciszenie, Linn Klimax DS/2 rozpoznawany.
- **AirPlay**: wyjście AirPlay 1 nie startuje już od maksimum po ponownym uruchomieniu.
- **Ścieżka sygnału**: nie pokazuje już przetwarzania, które nie ma miejsca.
- **Biblioteka**: wydania DSD i CD tego samego albumu nie są już łączone; zapis DR nie zawiesza już serwera.
- **Serwisy**: ulubione z Qobuz i TIDAL z właściwym czasem; sekcje Spotify nie są już zdublowane.
- **Radio**: stacje FIP odzyskują swoje logo.
- **Wtyczki i crossfeed**: zainstalowana wtyczka działa bez ponownego uruchamiania; crossfeed nie jest już zablokowany w OAAT ani w przeglądarce.
- **Interfejs WWW**: siatka przewija się do końca; baner „serwer nie odpowiada” znika, gdy tylko serwer odpowie.

## Bezpieczeństwo

- Kilka wzmocnień bezpieczeństwa.
- Dzienniki i raporty błędów ukrywają teraz dane uwierzytelniające zawarte w niektórych adresach strumieni.
- Zamiana ścieżki (Ustawienia › foldery z muzyką) sprawdza teraz foldery i nie jest już dostępna z innego urządzenia bez uwierzytelnienia.

## Zmiany w działaniu

- **Tune Circle**: przy włączonym uwierzytelnianiu tylko dla administratorów; tylko do odczytu podczas słuchania zdalnego.
- **YouTube**: konta połączone przed tą wersją trzeba raz połączyć ponownie.
- **Radio na końcu kolejki**: znowu domyślnie wyłączone; zaznacz je ponownie w strefie, jeśli było przez Ciebie wybrane.
- **Korektor**: po wyłączeniu korektora jego przedwzmacniacz pozostaje aktywny aż do „Przywróć poziom”.
- **Multiroom**: grupa bezpłatna do 3 stref; powyżej potrzebne jest Premium. Pary stereo pozostają bezpłatne.
- **CUE z INDEX 02**: początki utworów korygowane przy następnym skanowaniu, historia zachowana.
- **Tune OS**: hasło tymczasowe wyświetla się na ekranie urządzenia lub na karcie startowej, a nie na stronie logowania; dostęp zdalny pozostaje zamknięty, dopóki nie zostanie zmienione.
- **Profile i historia**: przy włączonym uwierzytelnianiu tylko administrator zarządza innymi profilami.
- **Zamiana ścieżki**: bez uwierzytelniania z urządzenia, na którym działa Tune.

## Do sprawdzenia u siebie

Te zmiany przeszły testy automatyczne bez prób na prawdziwym sprzęcie. Napisz nam, co zauważasz:

- **Sendspin i Devialet AIR** z aktywnym przetwarzaniem.
- **Windows**: mini odtwarzacz, tryb wyłączny WASAPI, uruchamianie razem z Windows.
- **AirPlay 1**: poziom przy starcie i między utworami.
- **DSD do odtwarzacza sieciowego** (Atoll ST200, Eversolo).
- **Linn Klimax DS/2** jako OpenHome, **Tune Client w macOS** i **CarPlay w sieci 5G**.

## Znane problemy

- **DSD przez DoP na wyjściu lokalnym**: po wytłumionym PCM może grać z pełną skalą. Trzymaj niską głośność wzmacniacza.
- **UPnP po zmianie adresu**: utwór może się nie odtworzyć; ponownie zindeksuj ten serwer.
- **Audyt integralności**: kilka plików może zostać błędnie zgłoszonych; nie usuwaj niczego wyłącznie na podstawie tego werdyktu.
- **Windows ASIO sterowane przez inny serwer**: po Pauza, a następnie Odtwórz nie ma dźwięku; przejdź do następnego utworu.
- **Po aktualizacji**: jeśli interfejs się nie zmienia, wymuś ponowne załadowanie (Cmd+Shift+R lub Ctrl+Shift+R).
