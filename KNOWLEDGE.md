# pslXserver メモ

## プロジェクト概要
- Rust製macOSネイティブX11サーバー（XQuartzの代替、IME対応）
- 主要ファイル: `connection.rs`(~3700行, プロトコル処理), `macos.rs`(Cocoa), `renderer.rs`(描画), `mod.rs`(イベント)

## ビルド
- `~/.cargo/bin/cargo build --release`
- macOSフレームワーク: AppKit, CoreGraphics, CoreText, CoreFoundation, Carbon
- テスト用X11アプリ: `/opt/X11/bin/`（ico, xterm, xclock, xeyes等）

## 修正履歴 (2026-02-28)
- **xterm文字表示**: MapWindow中のExpose送信はevent_tx経由（キュー）にすること
- **キーボードフリーズ**: CopyArea(graphics_exposures=true)はNoExposureイベント(type 14)必須
- **スクロール**: CopyAreaのoffset_render_commandsはsrc/dst両方の座標をオフセット
- **ウィンドウリサイズ**: check_window_resizes()で60fpsでmacOSフレーム変更検知→IOSurface再生成→ConfigureNotify+Expose送信
- **高速出力**: フレーム合体を廃止（500超でDrawText破棄されていた）
- **biased select**: tokio::select!をbiasedにしてクライアント読み取り優先
- **シーケンス再スタンプ**: 書き込み時にイベントのシーケンス番号を現在値に更新（xcb単調増加要件）
- **ラウンドトリップ必須**: Opcodes 44,48,52,73,106,110はreply/error必須（Ok(())だけではダメ）

## 修正履歴 (2026-03-02)
- **IMEインラインプリエディット**: `setMarkedText`で`ImePreeditDraw`送信→xterm内にリアルタイム変換表示。確定時はBS消去→確定文挿入
- **IME確定タイミング**: `SUPPRESS_NEXT_KEYUP`フラグでEnter/SpaceのKeyReleaseがxtermに漏れるのを防止
- **記号入力(`:` → `;`バグ)**: `needs_shift()`追加 — `:!@#`等のASCII記号にShift状態を正しく付与きききm Xftモード対応
- **ImageText16/PolyText16**: オペコード75/77の16bit文字描画実装
- **CoreText描画**: `render_coretext_char()` — CJK/Unicode文字をHiraginoSans-W3で描画
- **全角正規化**: IME確定文の全角英数(U+FF01-FF5E, U+3000)をASCIIに変換
- **ConvertSelection**: X11コピペプロトコル実装（SelectionRequest→オーナーが応答→SelectionNotify）

## 基本原則
- XQuartzのソースは当てにするな。空ディレクトリしかない。独自に正しい設計を行う
- **常にプロファイルを取る**: チューニング・修正時は必ずプロファイリングを実施し、データに基づいて最適化する。遅さ起因の問題も多いため、パフォーマンス計測を習慣化すること

## アーキテクチャ
- レンダーメールボックス: `Arc<DashMap<u64, Vec<RenderCommand>>>` — プロトコルスレッドが追加、表示スレッドが60fpsで消費
- IOSurfaceをCALayerのcontentsに直接設定（フレーム毎のCGImage生成なし）
- ネイティブサーフェスなし子ウィンドウ: 祖先チェーンを辿ってレンダーコマンドをオフセット
- フォーカス: `XServer.focus_window` (AtomicU32: 0=None, 1=PointerRoot, >1=特定ウィンドウ)
- キーイベント: PointerRoot時はfind_deepest_child()、KEY_PRESSマスクを持つ祖先まで伝播

## IMEアーキテクチャ
- `IME_COMPOSING` (AtomicBool): プリエディット中true、KeyRelease抑制
- `SUPPRESS_NEXT_KEYUP` (AtomicBool): ワンショット、確定キーのKeyRelease抑制
- Unicode keysym方式: `0x01000000 | codepoint`を仮想キーコード200-254に割当、MappingNotify送信
- xterm起動時: `-u8`フラグ + `LC_ALL=en_US.UTF-8`が必須（Unicode keysym→UTF-8変換のため）
- `ascii_to_x11_keycode()` + `needs_shift()`でASCII文字のShift状態制御
- `hasMarkedText`はNO返却 — macOS側のIME候補ウィンドウを使用
- `firstRectForCharacterRange`でX11座標→NSView→スクリーン座標変換して候補位置決定

## 修正履歴 (2026-03-03)
- **XKB GetMapヘッダ修正**: 32→40バイト（firstKeyExplicit等の欠落フィールド追加、totalSyms/nKeySymsの順序修正）。リモートLinux xtermで必須
- **XI2無効化**: XInputExtensionを公開するとChrome/Electronがクラッシュ(SIGTRAP)。実装が不完全なため無効化
- **BIG-REQUESTS追加**: opcode 133、max request length 16MB
- **リモートX11接続**: 9955wx(Linux)からTCP経由でxterm/Chrome/VS Code動作確認

## リモートX11接続 (9955wx)
- SSH: `ssh pengin0906@9955wx`
- xterm: `ssh pengin0906@9955wx "DISPLAY=penguinnoMacBook-Air.local:0 xterm"`
- Chrome: `ssh pengin0906@9955wx "DISPLAY=penguinnoMacBook-Air.local:0 google-chrome-stable --no-sandbox --disable-gpu --disable-dev-shm-usage --no-first-run --ozone-platform=x11"`
- VS Code: `ssh pengin0906@9955wx "DISPLAY=penguinnoMacBook-Air.local:0 code --disable-gpu --no-sandbox"`
- pslXserverは `--tcp` 付きで起動必須

## Homebrew libX11 locale修正（重要・永続）
- **問題**: Homebrew libX11 1.8.13のXSupportsLocale()が常に0を返す → xterm C locale fallback → Unicode keysym(0x0100XXXX)が文字に変換不可 → 日本語インライン表示不可
- **根本原因**: libX11の`_XlcDynamicLoad`がLinux命名`xlcUTF8Load.so.2`でdlopenするが、Homebrew macOSビルドは`xlcUTF8Load.2.so`で生成。ファイル名不一致でdlopen失敗
- **修正**: `/opt/homebrew/Cellar/libx11/1.8.13/lib/X11/locale/common/` に `.so.2` → `.2.so` シンボリックリンクを5つ作成
- **注意**: brew upgradeで消える可能性あり。再発時は同じsymlinkを再作成

## IMEプリエディット インクリメンタル更新
- 旧: 毎回全BS消去→全再描画（フリッカー大）
- 新: `text.starts_with(&preedit_text)` なら差分のみ追加送信（ひらがな入力時BSなし）
- 変換時（テキスト変化）のみ全消去→再描画

## 修正履歴 (2026-03-07)
- **ListFonts高速化**: `/opt/X11`から4604フォントを読み込んでいたのをやめ、6個のハードコードリストに変更。ListFontsレスポンスが1000分の1になりxterm起動が大幅高速化。pslXserverはどのフォントもファイルから読まず全てCoreTextで描画するので、ファイルシステムフォントを広告する意味がない
- **MappingNotify同期**: 固定50ms sleep → AtomicU32世代カウンター(mapping_gen)によるラウンドトリップ同期に変更。GetKeyboardMapping応答時にgenをインクリメント、send_ime_textはgen変化を1msポーリングで待機（通常5〜15ms）。初回登録のみ遅延、同じ漢字は再登録不要で即座に入力
- **virtual_keysymsリセット廃止**: ImeCommit/ImePreeditDoneでのtruncate(86)を削除。漢字スロットを積み上げたまま保持し、同じ漢字は既存スロット再利用→MappingNotify不送信→遅延ゼロ。スロット溢れ(119個超)時のみsend_ime_text内でtruncate(86)してリセット
- **Chrome/xtermプリエディット分離**: window_is_xterm()でxterm判定。xtermのみ仮想キーコードによるインラインプリエディット挿入、Chrome等は挿入しない（ChromeのURL補完がBackSpaceを横取りするため）

## 既知の問題
- XI2拡張が不完全（Chrome/Electronクラッシュ原因、現在無効化中）
- SHAPE拡張未実装（xeyes/xlogoが矩形ウィンドウのまま）
- RENDER拡張未実装（アンチエイリアス描画なし）
- Chrome ARM Linux(Docker)でYouTube再生可能
