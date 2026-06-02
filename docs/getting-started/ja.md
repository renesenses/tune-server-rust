# Tune を始める

**Tune** は、ローカルライブラリとストリーミングサービス（Tidal、Qobuz、Spotify、Deezer）を Web と iPad のインターフェースに統合するオープンソースのマルチルーム音楽サーバーです。クラウドに依存せず、DLNA、AirPlay、Chromecast、BluOS、Squeezebox デバイスにストリーミングします。

> 注：この翻訳は初版です。フォーラムでの改善提案を歓迎します。

---

## 1. インストール

### Docker（推奨）

```bash
docker run -d \
  --name tune \
  --network host \
  -v /音楽パス:/music \
  -v ~/.tune:/data \
  renesenses/tune:latest
```

> **重要**：ローカルネットワークでの DLNA/mDNS 検出には `--network host` が必要です。

### macOS（Homebrew）

```bash
brew tap renesenses/tap
brew install tune-server
brew services start tune-server
```

### Windows

[GitHub Releases](https://github.com/renesenses/tune-server-rust/releases) から `.exe` インストーラーをダウンロードして実行します。

### iPad（TestFlight）

[mozaiklabs フォーラム](https://mozaiklabs.fr/forum) で TestFlight 招待をリクエストしてください。

---

## 2. 初回起動

ブラウザで `http://localhost:8888`（またはサーバーのアドレス）を開きます。

オンボーディングウィザードが次の手順をガイドします：

1. 音楽が入っているフォルダを指定
2. 最初のスキャンを実行
3. 出力ゾーンを選択（DAC、DLNA スピーカーなど）

---

## 3. ライブラリを追加

**設定 → 音楽フォルダ → 追加**

Tune は一般的なオーディオ形式すべてをサポートしています：

- **ロスレス**：FLAC、WAV、AIFF、ALAC、APE、WavPack
- **DSD**：DSF、DFF、DST
- **ロッシー**：MP3、AAC、OGG、Opus、WMA

スキャンは段階的です：トラックはインデックス化されるとライブラリに表示されます。10万トラックのライブラリで約30分かかります。

---

## 4. 最初のゾーン

**ゾーン** はオーディオ出力を表します。Tune は自動的に検出します：

- **DLNA/UPnP**：Hi-Fi ストリーマー（Eversolo、Lindemann、Cocktail Audio、Hifi Rose、Sonos）
- **AirPlay**：Apple スピーカー、互換 AVR
- **Chromecast**：Google スピーカー、一部の TV
- **BluOS**：Bluesound、NAD
- **OAAT**：オープンソースのビットパーフェクトプロトコル（RPi + USB DAC）
- **ローカル出力**：サーバーに接続された USB DAC

**設定 → デバイス** に検出されたすべてが一覧表示されます。

ゾーンを作成するには：**設定 → ゾーン → 新規**、名前を選択してデバイスを関連付けます。

---

## 5. 最初の再生

上部バーで対象ゾーンを選択します。次にライブラリで：

- トラックをクリック → 即時再生
- **アルバムを再生** をクリック → アルバム全体をキューに追加
- プレイリストの矢印をクリック → プレイリスト再生

再生コントロール（再生/一時停止/次へ/音量）は Web クライアントの下部にあります。

---

## 6. ストリーミングサービス

**設定 → ストリーミングサービス** → 接続

| サービス | 認証 | 最大品質 |
|---------|------|---------|
| Tidal | OAuth（HiFi アカウント） | FLAC 24/192 |
| Qobuz | ログイン/パスワード（Studio） | FLAC 24/192 |
| Spotify | OAuth（Premium） | 320 kbps |
| Deezer | ARL トークン | FLAC 16/44 |
| YouTube Music | OAuth | ~256 kbps |

接続するとサービスは **ストリーミング** メニューに表示されます。

---

## 7. マルチルーム

**複数のゾーンで同じトラックを同時に再生** するには：

**設定 → ゾーングループ → グループを作成**

サーバーは NTP 経由で出力を同期します。レイテンシはゾーンごとに調整可能です（**設定 → ゾーン → 同期遅延**）。

---

## 8. さらに進む

- **テスト計画**：[docs/cahier-recette-v0.8.20.md](../cahier-recette-v0.8.20.md) — 58 のテスト
- **API ドキュメント**：`GET /api/v1/system/api-docs` またはブラウザでサーバーにアクセス
- **コミュニティフォーラム**：https://mozaiklabs.fr/forum
- **GitHub**：https://github.com/renesenses/tune-server-rust
- **CLI**：ターミナルから操作するには `cargo install tune-cli`

良い音楽体験を！
