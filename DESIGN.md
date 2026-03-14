# Xerver 設計書

## ビジョン
macOSネイティブのフルセットX11サーバー。AppleがXQuartzのサポートを終了した穴を埋め、
LinuxサーバーとMacをシームレスに統合する開発環境の中核を担う。

## 設計原則

1. **完全なX11互換** — 標準Xアプリ(xterm, Chrome, Firefox, VS Code等)が無修正で動作
2. **高速・軽量** — 常時プロファイリング、IOSurface zero-copy描画、60fps維持でCPU 5%以下
3. **セキュリティ万全** — メモリ安全(Rust)、入力バリデーション、OWASP対策
4. **macOS完全統合** — ネイティブウィンドウ、コピペ双方向、IMEインライン変換
5. **コンパクト** — 最小限のコードで最大の機能。不要な抽象化をしない

## アーキテクチャ

```
┌─────────────────────────────────────────────────────┐
│  macOS (NSWindow / CALayer / IOSurface)             │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐          │
│  │ Chrome   │  │ xterm    │  │ VS Code  │  ← 各X11アプリが1つのNSWindowに対応
│  └──────────┘  └──────────┘  └──────────┘          │
│       ↑ DisplayCommand / RenderCommand               │
│  ┌───────────────────────────────────────────┐      │
│  │  display/macos.rs — Cocoa統合             │      │
│  │  ・NSWindow/NSView管理                     │      │
│  │  ・IOSurface double-buffered rendering    │      │
│  │  ・NSTextInputClient (IME)                │      │
│  │  ・マウス/キーボードイベント変換            │      │
│  │  ・60fps CFRunLoopTimer                    │      │
│  └───────────────────────────────────────────┘      │
│       ↑ DisplayEvent / RenderMailbox(DashMap)        │
│  ┌───────────────────────────────────────────┐      │
│  │  server/connection.rs — X11プロトコル処理   │      │
│  │  ・130+ X11リクエストハンドラ               │      │
│  │  ・Atom管理、リソース管理(XID)              │      │
│  │  ・フォント(QueryFont, ListFonts)          │      │
│  │  ・レンダーコマンド生成                     │      │
│  └───────────────────────────────────────────┘      │
│       ↑ TCP / Unix socket                            │
│  ┌───────────────────────────────────────────┐      │
│  │  server/mod.rs — イベントディスパッチ        │      │
│  │  ・tokio非同期ランタイム                    │      │
│  │  ・フォーカス管理 (SetInputFocus)          │      │
│  │  ・キー/マウスイベント配送                  │      │
│  │  ・biased select (クライアント読み取り優先)  │      │
│  └───────────────────────────────────────────┘      │
│       ↑                                              │
│  X11クライアント (xterm, Chrome, Firefox, etc.)      │
│  接続: Unix socket (/tmp/.X11-unix/X0)              │
│        TCP (port 6000) ← リモート/Docker/SSH        │
└─────────────────────────────────────────────────────┘
```

## 主要機能

### 1. ウィンドウ管理
- アプリ単位で1つのmacOSウィンドウ（トップレベルX11ウィンドウ = NSWindow）
- サイズ不変式: macOSウィンドウ = トップレベル = 直下の子ウィンドウ
- ライブリサイズ: setFrameSize:オーバーライドで同期IOSurface更新
- 縮小時はIOSurface再生成なし（masksToBoundsでクリップ）
- layerContentsPlacement=TopLeft（引き伸ばし防止）

### 2. レンダリング
- IOSurface double-buffered: フリッカーなし、CALayerに直接設定
- contentsGravity=topLeft, contentsScale=1.0（ピクセル1:1）
- RenderMailbox (DashMap): プロトコルスレッドが追加、表示スレッドが60fpsで消費
- PutImage合体: Electron全画面連打は最後の1枚のみ保持
- 全画面PutImage時はサーフェスコピースキップ（memcpy不要）
- CoreText描画: CJK/Unicode文字をシステムフォントで描画

### 3. 入力・IME
- NSTextInputClient完全実装
- macOS IME(日本語IM)のインライン変換をX11アプリ内に表示
- Unicode keysym方式 (0x01000000 | codepoint)
- キーボード: needs_shift()でASCII記号のShift状態を正確制御
- マウス: ポーリング + NSEventハンドラのハイブリッド
- カーソル: X11グリフ番号 → macOS NSCursor変換

### 4. コピー&ペースト
- Cmd+V: macOSペーストボード → X11 ImeCommit
- Cmd+C: X11 PRIMARY選択 → macOSクリップボード(pbcopy)
- ConvertSelection: X11コピペプロトコル(SelectionRequest/SelectionNotify)

### 5. セキュリティ
- Rust: メモリ安全、バッファオーバーフローなし
- 入力バリデーション: 全X11リクエストの長さ・範囲チェック
- リソースID検証: 不正XIDのBadWindow/BadDrawable
- Unix socket: ファイルシステムパーミッションによるアクセス制御
- TCP: --tcpフラグで明示的に有効化（デフォルトoff）

### 6. パフォーマンス最適化
- 常時プロファイリング対応（`sample` コマンドで即座に計測可能）
- DashMap targeted get_mut (iter_mutの16シャードスキャン回避)
- mouseLocation/pressedMouseButtons を毎tick 1回にキャッシュ
- PutImage 32-bit 1パス(copy+alpha fix統合)
- biased tokio::select! でクライアント読み取り優先
- Chrome接続時 CPU 5%以下、メモリ 44MB

## 拡張計画

### Phase 7: フルセットX11 (未実装)
- [ ] SHAPE拡張 — 非矩形ウィンドウ(xeyes, xlogo)
- [ ] RENDER拡張 — アンチエイリアス描画、Xft
- [ ] BIG-REQUESTS拡張
- [ ] XTest拡張 — xdotool対応
- [ ] GetImage — IOSurfaceからの実ピクセルデータ読み取り
- [ ] bit_gravity — リサイズ時のコンテンツ移動制御

### Phase 8: クロスプラットフォーム
- [ ] Windows対応 — Win32/DirectX版ディスプレイバックエンド
- [ ] 共通のX11プロトコル層を維持、OS固有部分のみ差し替え

## ネットワーク構成

```
┌──────────┐  TCP/6000   ┌──────────────┐
│  9955wx  │ ──────────→ │  Mac         │
│  (Linux) │  SSH -Y     │  Xerver  │
│  Chrome  │ ──────────→ │  :0          │
│  xterm   │             │              │
└──────────┘             └──────────────┘

┌──────────┐  TCP/6000   ┌──────────────┐
│  Docker  │ ──────────→ │  Mac         │
│  (Colima)│  via        │  Xerver  │
│  Chrome  │  192.168.   │  :0          │
│  VS Code │  5.2        │              │
└──────────┘             └──────────────┘
```

## 開発プロセス

### 原則
- 問題を1つ乗り越えるたびに修正内容をメモしてgit push（手戻り防止）
- 結果を見て一歩ずつ前に進める。冷静に判断して大規模プロジェクトを遂行する
- 常にプロファイルを取得して最適化に備える。遅いコードを撲滅する
- セキュリティに注意し、脆弱性を持ち込まない
- ユーザーからの指示は必ずINSTRUCTIONS.mdに記録し、チェックボックスで課題管理する
- クラッシュしたら必ず原因を調べる
- 画面確認等、自分でできることは自分で操作して試行錯誤する（ユーザーの手を煩わせない）

### 未解決の課題
- [ ] Chrome/VS CodeのURL窓・検索窓で日本語IMEインライン編集が動かない
- [ ] Chrome/VS Codeでキーボード入力がURL窓に届かない（SetInputFocusでFocusIn/Out送信を追加済み、要検証）
- [ ] xtermでひらがな編集後、バックスペースで削除しても1文字残るバグ

## リサイズ時のxtermの動作要件
- リサイズ中にリアルタイムでConfigureNotify + Expose送信
- xterm上部がリサイズに追従して伸縮する（スクロールバック表示域の拡大）
- setFrameSize:で同期的にIOSurface更新 → ちらつきなし
- 縮小時は再生成なし、拡大時のみ新IOSurface + 旧コンテンツコピー
