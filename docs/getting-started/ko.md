# Tune 시작하기

**Tune**은 로컬 라이브러리와 스트리밍 서비스(Tidal, Qobuz, Spotify, Deezer)를 웹 및 iPad 인터페이스로 통합하는 오픈소스 멀티룸 음악 서버입니다. 클라우드 의존성 없이 DLNA, AirPlay, Chromecast, BluOS, Squeezebox 장치로 스트리밍합니다.

> 참고: 이 번역은 초안입니다. 포럼에서 개선 사항을 제안해 주세요.

---

## 1. 설치

### Docker (권장)

```bash
docker run -d \
  --name tune \
  --network host \
  -v /음악경로:/music \
  -v ~/.tune:/data \
  renesenses/tune:latest
```

> **중요**: 로컬 네트워크의 DLNA/mDNS 검색을 위해 `--network host`가 필요합니다.

### macOS (Homebrew)

```bash
brew tap renesenses/tap
brew install tune-server
brew services start tune-server
```

### Windows

[GitHub Releases](https://github.com/renesenses/tune-server-rust/releases)에서 `.exe` 설치 프로그램을 다운로드하여 실행합니다.

### iPad (TestFlight)

[mozaiklabs 포럼](https://mozaiklabs.fr/forum)에서 TestFlight 초대를 요청하세요.

---

## 2. 첫 실행

브라우저에서 `http://localhost:8888` (또는 서버 주소)을 엽니다.

온보딩 마법사가 다음 단계를 안내합니다:

1. 음악이 들어 있는 폴더 지정
2. 첫 스캔 실행
3. 출력 영역 선택 (DAC, DLNA 스피커 등)

---

## 3. 라이브러리 추가

**설정 → 음악 폴더 → 추가**

Tune은 모든 일반적인 오디오 형식을 지원합니다:

- **무손실**: FLAC, WAV, AIFF, ALAC, APE, WavPack
- **DSD**: DSF, DFF, DST
- **손실**: MP3, AAC, OGG, Opus, WMA

스캔은 점진적입니다: 트랙은 인덱싱되면서 라이브러리에 표시됩니다. 10만 트랙 라이브러리는 약 30분이 걸립니다.

---

## 4. 첫 영역

**영역**은 오디오 출력을 나타냅니다. Tune은 자동으로 감지합니다:

- **DLNA/UPnP**: Hi-Fi 스트리머 (Eversolo, Lindemann, Cocktail Audio, Hifi Rose, Sonos)
- **AirPlay**: Apple 스피커, 호환 AVR
- **Chromecast**: Google 스피커, 일부 TV
- **BluOS**: Bluesound, NAD
- **OAAT**: 오픈소스 비트퍼펙트 프로토콜 (RPi + USB DAC)
- **로컬 출력**: 서버에 연결된 USB DAC

**설정 → 장치**에 감지된 모든 항목이 나열됩니다.

영역 생성: **설정 → 영역 → 신규**, 이름을 정하고 장치를 연결합니다.

---

## 5. 첫 재생

상단 표시줄에서 대상 영역을 선택합니다. 그런 다음 라이브러리에서:

- 트랙 클릭 → 즉시 재생
- **앨범 재생** 클릭 → 전체 앨범을 큐에 추가
- 플레이리스트 화살표 클릭 → 플레이리스트 재생

재생 컨트롤 (재생/일시정지/다음/볼륨)은 웹 클라이언트 하단에 있습니다.

---

## 6. 스트리밍 서비스

**설정 → 스트리밍 서비스** → 연결

| 서비스 | 인증 | 최대 품질 |
|--------|------|----------|
| Tidal | OAuth (HiFi 계정) | FLAC 24/192 |
| Qobuz | 로그인/비밀번호 (Studio) | FLAC 24/192 |
| Spotify | OAuth (Premium) | 320 kbps |
| Deezer | ARL 토큰 | FLAC 16/44 |
| YouTube Music | OAuth | ~256 kbps |

연결되면 서비스가 **스트리밍** 메뉴에 표시됩니다.

---

## 7. 멀티룸

**여러 영역에서 동시에 같은 트랙을 재생**하려면:

**설정 → 영역 그룹 → 그룹 생성**

서버는 NTP를 통해 출력을 동기화합니다. 지연 시간은 영역별로 조정 가능합니다 (**설정 → 영역 → 동기화 지연**).

---

## 8. 더 알아보기

- **테스트 계획**: [docs/cahier-recette-v0.8.20.md](../cahier-recette-v0.8.20.md) — 58개 테스트
- **API 문서**: `GET /api/v1/system/api-docs` 또는 브라우저로 서버에서 액세스
- **커뮤니티 포럼**: https://mozaiklabs.fr/forum
- **GitHub**: https://github.com/renesenses/tune-server-rust
- **CLI**: 터미널에서 제어하려면 `cargo install tune-cli`

즐거운 감상 되세요!
