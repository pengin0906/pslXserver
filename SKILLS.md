# SKILLS.md — 開発規範・クライアント起動ガイド

## 開発姿勢
- 問題を1つ解決するたびに修正内容をメモしてgit pushし、手戻りを防ぐ
- 結果を確認しながら一歩ずつ進める。冷静に判断して大規模プロジェクトを遂行する
- 常にプロファイルを取得し、ボトルネックを特定して最適化する
- セキュリティに注意し、脆弱性やマルウェアを持ち込まない

## 課題管理
- ユーザーからの指示はINSTRUCTIONS.mdにチェックボックス付きで記録する
- 記録した課題は確実に追跡し、無視しない

## デバッグ方針
- クラッシュ時は必ず原因を調査する
- 自分で操作・確認できることは自力で試行錯誤する
- ユーザーの手を煩わせず、可能な限り自動で解決まで持っていく

## 基本原則
- アプリ(X11クライアント)は絶対にいじらない。問題はXサーバー側で対応する
- XQuartzの実装を安易に真似しない。独自に正しい設計を行う

---

## クライアント起動ガイド

### 共通ルール
- 全クライアントはUTF-8ロケールで起動する
- xterm: 必ず `-u8` オプションをつける
- XI2/XKBは無効化必須（有効だとChrome/VS Code/Firefoxがクラッシュする）
- faceName(.Xresources)は使わない（RENDER拡張未実装）
- 既存Chromeと分離するため `--user-data-dir` 必須

### xterm
```bash
# Docker/Linux
xterm -u8

# macOS (Homebrew) — フォント指定不要（サーバー側でiso10646をデフォルト広告）
LANG=en_US.UTF-8 XMODIFIERS=@im=none xterm -u8
```

### Chrome
```bash
# リモート(9955wx)
DISPLAY=<host>:0 /opt/google/chrome/chrome --no-sandbox --disable-gpu --disable-dev-shm-usage --no-first-run --user-data-dir=/tmp/chrome-pslx

# Docker
DISPLAY=192.168.5.2:0 chromium --no-sandbox --disable-gpu --disable-dev-shm-usage --no-first-run
```

### VS Code
```bash
# リモート(9955wx)
DISPLAY=<host>:0 code --no-sandbox --disable-gpu --user-data-dir=/tmp/vscode-pslx

# Docker
DISPLAY=192.168.5.2:0 code --no-sandbox --disable-gpu --user-data-dir=/tmp/vscode-data
```

---

## トラブルシューティング: IME日本語がインライン表示されずBSだけ動く

### 症状
- xtermでIME入力時、ひらがな・漢字がインライン表示されない
- BSだけが効いてカーソルが後退する
- xterm起動時に `Warning: locale not supported by Xlib, locale set to C` が出る

### 原因
Homebrew libX11のlocaleモジュール(.so)のファイル命名がmacOSとLinuxで異なる。
- libX11内部の`_XlcDynamicLoad`は `xlcUTF8Load.so.2` (Linux命名) でdlopenする
- Homebrew macOSビルドは `xlcUTF8Load.2.so` (macOS命名) で生成する
- ファイル名不一致でdlopen失敗 → `XSupportsLocale()=0` → C locale fallback
- C localeではUnicode keysym(0x0100XXXX)をUTF-8に変換できず、文字が表示されない

### 確認手順
```bash
# 1. XSupportsLocaleの確認（0なら壊れている）
cat > /tmp/test_locale.c << 'EOF'
#include <stdio.h>
#include <locale.h>
#include <X11/Xlib.h>
int main() {
    setlocale(LC_ALL, "en_US.UTF-8");
    printf("XSupportsLocale() = %d\n", XSupportsLocale());
}
EOF
cc -o /tmp/test_locale /tmp/test_locale.c -I/opt/homebrew/include -L/opt/homebrew/lib -lX11
LANG=en_US.UTF-8 /tmp/test_locale
# → 0 なら修正が必要

# 2. .so.2ファイルの存在確認
ls /opt/homebrew/lib/X11/locale/common/xlcUTF8Load.so.2
# → "No such file" なら修正が必要
```

### 修正方法
```bash
# Homebrew libX11のlocale共有ライブラリディレクトリでsymlinkを作成
cd /opt/homebrew/Cellar/libx11/$(brew list --versions libx11 | awk '{print $2}')/lib/X11/locale/common
for f in *.2.so; do
  base="${f%.2.so}"
  ln -sf "$f" "${base}.so.2"
done
```

### 修正後の確認
```bash
# XSupportsLocale() = 1 になること
LANG=en_US.UTF-8 /tmp/test_locale

# xtermで locale警告が出ないこと
DISPLAY=:0 LANG=en_US.UTF-8 XMODIFIERS=@im=none xterm -u8 2>&1 | head -3
```

### 注意事項
- `brew upgrade libx11` でsymlinkが消える。再発時は同じ手順を再実行する
- Docker/Linux環境では発生しない（Linux ELF命名が一致するため）
- この問題はXerver固有ではなくHomebrew libX11のバグ


---

## 自動テスト（XTest + python-xlib + Selenium/Appium）

### 方針
- **人間の手を一切煩わせません。全テストはClaudeが自動で実行・検証する。**
- XTEST拡張でX11側からキー入力・マウス操作を注入
- python-xlibでX11プロトコルレベルのテスト
- CGEvent(pyobjc-framework-Quartz)でmacOS IMEパイプライン経由のテスト
- **Selenium（Appium）を使ってiOSシミュレーターでテスト**
- Seleniumを使ってXウィンドウやXTermも、たまにはXTESTも使う

### インストール済みツール
```bash
pip3 install python-xlib pyobjc-framework-Quartz selenium
brew install xdotool  # ※ EWMH未対応のためSEGVする場合あり、python-xlibを優先
```

### テスト方法

#### 1. ASCII入力テスト（XTEST経由）
python-xlibのxtest拡張でKeyPress/KeyReleaseを注入し、xtermに文字が正しく入力されるか確認。
```python
from Xlib import X, display
from Xlib.ext import xtest
d = display.Display(":0")
# キーコード取得 → xtest.fake_input(d, X.KeyPress, keycode) → sync
```

#### 2. Unicode/CJK入力テスト（ChangeKeyboardMapping経由）
Unicode keysym `0x01000000 | codepoint` を仮想キーコードに割当て、KeyPressで送信。
```python
unicode_keysym = 0x01000000 | ord('漢')
d.change_keyboard_mapping(200, [(unicode_keysym,)])
d.sync()
xtest.fake_input(d, X.KeyPress, 200)
```

#### 3. インラインIMEシミュレーション（XTEST経由）
プリエディット→変換→確定の全フローをBS消去+再送信で再現。
- `test_inline_ime.py`: かんじ→漢字、にほんご→日本語、わたしはにほんじんです→私は日本人です

#### 4. 実macOS IMEテスト（CGEvent経由）
CGEventCreateKeyboardEventでmacOSレベルのキーストロークを送信。
かなキー(104)→ローマ字/かな入力→スペース変換→Enter確定の実IMEパイプラインをテスト。
```python
from Quartz import CGEventCreateKeyboardEvent, CGEventPost, kCGHIDEventTap
event = CGEventCreateKeyboardEvent(None, keycode, True)
CGEventPost(kCGHIDEventTap, event)
```
- `test_real_ime.py` / `test_real_ime2.py`: 実IMEテスト
- `test_simple_ime.py`: 簡易2文字かな入力テスト

#### 5. スクリーンショット検証
各テストステップでscreencaptureを実行し、目視確認可能な証拠を保存。
```python
subprocess.run(["screencapture", "-x", "/tmp/pslx_step1.png"])
```

### テストスクリプト一覧
| スクリプト | 内容 |
|---|---|
| `test_xtest.py` | XTEST拡張の基本動作確認 |
| `test_keyboard.py` | ASCII全キー入力テスト |
| `test_kanji.py` | CJK文字入力（Unicode keysym経由） |
| `test_inline_ime.py` | IMEインラインシミュレーション |
| `test_real_ime.py` | 実macOS IMEテスト（CGEvent） |
| `test_real_ime2.py` | 実IMEテスト（ステップ毎スクリーンショット） |
| `test_simple_ime.py` | 簡易IMEテスト（2文字） |
| `test_bs_cjk.py` | CJK文字のBS消去テスト |
| `test_ime.py` | ASCII入力+BSテスト |

### 6. iOSシミュレーターテスト（XTEST + xcrun simctl）

#### 動作確認済み手順 (2026-03-14)
AppiumではなくXTEST + `xcrun simctl io screenshot`を使う方法が安定して動作。

```bash
# シミュレーターUDID
SIMULATOR=FBB5C54D-DB2C-4E6C-BFCB-0147CEDB3BFB

# 1. Xerver-iOS をビルド・インストール・起動
~/.cargo/bin/cargo build --release --target aarch64-apple-ios-sim --lib
xcodebuild -project ios-app/Xerver-iOS.xcodeproj -scheme Xerver-iOS \
  -configuration Release \
  -destination "platform=iOS Simulator,id=$SIMULATOR" \
  -derivedDataPath /tmp/Xerver-sim-build \
  CODE_SIGN_IDENTITY=- CODE_SIGNING_REQUIRED=NO CODE_SIGNING_ALLOWED=NO build
xcrun simctl terminate $SIMULATOR com.pslx.Xerver-iOS
xcrun simctl install $SIMULATOR /tmp/Xerver-sim-build/Build/Products/Release-iphonesimulator/Xerver-iOS.app
xcrun simctl launch $SIMULATOR com.pslx.Xerver-iOS

# 2. xterm接続 (DISPLAY=127.0.0.1:1)
LANG=en_US.UTF-8 XMODIFIERS=@im=none DISPLAY=127.0.0.1:1 \
  /opt/homebrew/bin/xterm -u8 \
  -fn "-misc-fixed-medium-r-semicondensed--13-120-75-75-c-60-iso10646-1" &

# 3. テスト実行
python3 test_ios_sim.py

# 4. スクリーンショット取得
xcrun simctl io $SIMULATOR screenshot /tmp/ipad_test.png
```

#### test_ios_sim.py テスト内容
| テスト | 内容 |
|---|---|
| Basic ASCII input | XTEST経由でASCIIキー入力 → xterm表示確認 |
| Special keys | BackSpace等の特殊キー |
| Unicode/CJK input | ChangeKeyboardMapping+XTESTでCJK文字 |
| Window visibility | render_debug.txtでウィンドウ状態確認 |

#### シミュレーター描画アーキテクチャ
- X11座標系とUIKit点座標は1:1マッピング（contentsScale=1.0）
- IOSurface直接CALayerにセット（ゼロコピー、macOSと同じ）
- スクリーンショットはlandscapeアプリをportraitで撮影するため、top-left(0,0)が画像の左下に見える
- デバッグファイル: `/tmp/pslx_render_debug.txt`（アプリが書き込む）

#### Appiumによる代替テスト（参考）
```bash
~/.local/bin/appium --port 4723 &
pip3 install Appium-Python-Client --break-system-packages
```
XCUITestドライバ経由でUIKit操作可能だが、XTESTの方がX11レベルで直接テストできて確実。

### 注意事項
- xdotoolはEWMH(`_NET_ACTIVE_WINDOW`)未対応のためSEGVする。python-xlibを使うこと
- CGEventテストはXerverがフォアグラウンドでないと動作しない
- ユーザーのIMEは`Kotoeri.KanaTyping`（かな入力モード）。ローマ字入力前提のテストは不正確になる
- テスト実行前に`Xerver`を起動し、xtermを立ち上げておくこと
- iOSシミュレーター: XTESTはTCP経由(DISPLAY=127.0.0.1:1)で動作確認済み

---

## UIバグのセルフデバッグ・修正手順

### 方針
- **画面表示・入力のバグはClaudeが自力で発見・修正・検証する。ユーザーの手を借りない。**
- スクリーンショットで目視確認し、テストスクリプトで再現・修正確認を行う

### セルフデバッグの全体フロー

```
1. バグ報告/発見
    ↓
2. スクリーンショットで現状確認
    screencapture -x /tmp/pslx_before.png
    ↓
3. テストスクリプトでバグを再現
    python3 test_keyboard.py / test_inline_ime.py / etc.
    ↓
4. ログ確認（RUST_LOG=debug で再起動して再現）
    ↓
5. 原因特定 → コード修正
    ↓
6. cargo build --release
    ↓
7. Xerver再起動 + xterm再起動
    ↓
8. 同じテストスクリプトで修正確認
    ↓
9. スクリーンショットで目視確認
    screencapture -x /tmp/pslx_after.png
    ↓
10. コミット＆プッシュ＆メモ更新
```

### 実績: 自力で修正したUIバグ (2026-03-13)

#### バグ1: 英数入力がひらがなになる
- **発見方法**: xtermでASCIIキー入力 → ひらがなが表示される
- **再現**: `test_keyboard.py`でXTEST経由ASCII入力 → 文字化け確認
- **原因調査**: `build_keyboard_map()`の`TISCopyCurrentKeyboardLayoutInputSource`がIMEアクティブ時に日本語レイアウトを返す
- **修正**: `TISCopyCurrentASCIICapableKeyboardLayoutInputSource`に変更（常にASCIIレイアウト取得）
- **検証**: `test_keyboard.py`再実行 → ASCII正常表示確認

#### バグ2: IMEプリエディットが蓄積する（文字がどんどん進む）
- **発見方法**: CGEventでかな入力 → プリエディット文字が消えずに蓄積
- **再現**: `test_simple_ime.py`でかな入力→変換→確定 → BS消去が効かず文字が増える
- **原因調査**: `send_backspaces()`が`focus`(トップレベルウィンドウ)からKEY_PRESSマスクを探索 → xtermのvt100子ウィンドウにマスクがあるため発見できずBSイベント未配信
- **修正**: `find_deepest_child()`で最深子ウィンドウから探索開始（`send_ime_text()`と同じロジック）
- **検証**: `test_simple_ime.py`再実行 → ちに→血に の変換でBS消去が正常動作

#### バグ3: XTESTイベントが正しいウィンドウに届かない
- **発見方法**: python-xlibの`xtest.fake_input`でKeyPress送信 → xtermに文字が表示されない
- **原因調査**: FakeInputが呼び出し元コネクション(テストスクリプト)にイベント送信していた
- **修正**: `send_key_event`/`send_button_event`/`send_motion_event`を使用し、フォーカスウィンドウのオーナーコネクションに配信
- **検証**: `test_keyboard.py`再実行 → 全ASCII文字がxtermに正常入力

### デバッグツール早見表

| ツール | 用途 | コマンド例 |
|---|---|---|
| screencapture | 画面キャプチャ | `screencapture -x /tmp/pslx_debug.png` |
| RUST_LOG=debug | 詳細ログ | Xerver起動時に環境変数設定 |
| test_keyboard.py | ASCII入力テスト | `DISPLAY=:0 python3 test_keyboard.py` |
| test_inline_ime.py | IMEシミュレーション | `DISPLAY=:0 python3 test_inline_ime.py` |
| test_simple_ime.py | 簡易IMEテスト | CGEvent経由（macOSフォアグラウンド必須） |
| test_real_ime2.py | 実IMEフルテスト | CGEvent経由（ステップ毎スクショ） |
| git diff | 変更箇所確認 | `git diff src/server/mod.rs` |

### 重要な教訓
- **`send_backspaces`と`send_ime_text`は同じターゲット計算を使え**: 両方とも`find_deepest_child()`。片方だけ変えると、テキスト挿入はできるがBS消去が壊れる（逆も然り）
- **IMEアクティブ時のキーボードレイアウト取得**: `TISCopyCurrentASCIICapableKeyboardLayoutInputSource`を使う。`TISCopyCurrentKeyboardLayoutInputSource`はIMEの状態に依存する
- **XTESTイベントの配信先**: テストクライアントのコネクションではなく、フォーカスウィンドウのオーナーコネクションに送る
- **バグの原因調査**: まず`git diff`で前回コミットからの変更点を確認。変更箇所が原因であることが多い
