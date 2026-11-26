Tune 1.0.0-rc13.91: **druga wersja przedpremierowa (pre-release) rc14**, tylko dla testerów. To nie jest opublikowana wersja: automatyczna aktualizacja jej nie proponuje. Zawiera całą zawartość rc13.90. Dziękujemy wszystkim, którzy ją testują.

## Nowości (New features)

- **Korektory łączone szeregowo**: korektor graficzny, parametryczny, słuchawkowy i korekcja pomieszczenia następują po sobie w tym samym torze TESS; zastosowanie korekcji pomieszczenia nie nadpisuje już pozostałych korektorów. Zakres wzmocnienia korektora parametrycznego został poszerzony.
- **Pełne Bypass all**: jeden gest wyłącza z obwodu cały tor TESS i przywraca go w niezmienionym stanie.
- **Wtyczka Słuchawki** oparta na bazie OPRA: profil Twoich słuchawek jest rozpoznawany, wraz z przedwzmocnieniem i poziomem.
- **Wtyczka Speaker Setup** (ustawienie głośników): odległość, wzmocnienie i faza, kanał po kanale.
- **Upsampling do DSD na wyjściu lokalnym**: dedykowany modulator sigma-delta, a możliwości DSD wyjścia są publikowane.
- **Wybór celu upsamplingu**: w TESS ekran pozwala wybrać, do czego wykonywany jest upsampling, do PCM lub do DSD.
- **Test poziomu mikrofonu** przed pomiarem pomieszczenia.
- **Pomiar z natywną częstotliwością mikrofonu**, od 44,1 do 192 kHz.
- **Ustawienia TESS w locie**: wyłączenie stopnia działa od razu, a przedwzmocnienie pozostaje ograniczone.

## Ulepszenia (Improvements)

- **Korekcja pomieszczenia**: pomiar kontrolny słyszy korekcję strefy; nieskorygowane odchylenia są wyjaśniane, a górną granicę można ustawić do 1 kHz.
- **Korekcja pomieszczenia**: wprowadzone dane, ostatni pomiar i zastosowana korekcja są przywracane po powrocie na ekran; wskazówki dotyczące ustawienia mikrofonu i ramka „Przed pomiarem” prowadzą przez pomiar.
- **Pomiar w trybie PURE**: głośność jest wymuszana na 100% na czas przemiatania, a potem przywracana do poprzedniej wartości.
- **Ścieżka sygnału**: wyłączony stopień (upsampling, splot) podaje powód, w TESS i na stronie wtyczki; w systemie Windows w trybie współdzielonym upsampling celuje w częstotliwość miksera.
- **Informacje o twórcach**: instrument zamiast „Muzyk”, autor odrębny od kompozytora, jedno nazwisko na rolę.
- **Skanowanie**: kod kreskowy obecny w tagach uzupełnia album, jeśli go nie ma.
- **Stan serwera**: wyświetla się ostrzeżenie, jeśli nie można utworzyć pamięci podręcznej okładek.

## Poprawki (Fixes)

- **Kalibracja**: przemiatanie nie kończy się już przed zapowiedzianą długością i nie utyka już w kolejce; strefa bez wyjścia jest jasno odrzucana.
- **Kalibracja**: szczyt na maksimum nie blokuje już kolejnych pasm.
- **RT60**: „niemierzalne (szum)” zamiast błędnej wartości; komunikaty o błędach pozostają czytelne przez co najmniej 10 s.
- **Ostrzeżenie o częstotliwości**: pojawia się już tylko poza akceptowanymi częstotliwościami.
- **Radio**: zmiana stacji nie jest już nadpisywana przez poprzednią stację.
- **Okładki**: w nowej instalacji pamięć podręczna jest tworzona w folderze danych.
- **TESS**: jeden komunikat „następny utwór”, a podpowiedź przycisku „Zastosuj” została poprawiona.

## Zmiany w działaniu

- **Zmiana toru TESS podczas odtwarzania**: wyłączony stopień milknie natychmiast; dodany lub ponownie włączony stopień słychać od następnego odtwarzania.

## Do sprawdzenia u siebie

Te punkty nie zostały jeszcze przetestowane na prawdziwym sprzęcie:

- **Upsampling do DSD na prawdziwym przetworniku DSD**: stabilność, poziom, brak trzasków.
- **Pomiar pomieszczenia przy 44,1 kHz i 96 kHz** na prawdziwym interfejsie audio, z testem poziomu mikrofonu.
- **W systemie Windows, w trybie współdzielonym WASAPI**: odtwarzanie i upsampling.
- **Speaker Setup na wyjściu sieciowym** (UPnP, AirPlay lub OAAT): odległość, wzmocnienie i faza.

A także:

- **Kilka korektorów szeregowo** z Bypass all: czy ścieżka sygnału odpowiada temu, co słyszysz?
- **Wtyczka Słuchawki** z Twoimi własnymi słuchawkami.
- **Aktualizacja z rc13.90.**

## Znane problemy

- **SACD skompresowane w DST**: te obrazy ISO nie są jeszcze odtwarzane natywnie.
- **Wskaźniki VU** na przekazywanym utworze MP3, AAC lub Ogg: po przewinięciu wyłączają się do końca utworu. Dźwięk nie jest zmieniony.
- **Słuchanie zdalne w CarPlay**: poprawka przekaźnika przesunięta do rc15.
- **Serwer bez ekranu z wyłączonym uwierzytelnianiem**: aby zmienić połączone konto mozaiklabs.fr, włącz uwierzytelnianie lub użyj tunelu SSH do localhost:8888.
