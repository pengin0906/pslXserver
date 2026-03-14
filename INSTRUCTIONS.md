# Xserver 作業指示・課題管理

## 作業方針
- [x] 問題を一つ解決するたびに修正内容をメモし、git pushして手戻りを防ぐ
- [x] 結果を見て一歩ずつ前に進める。冷静に判断して大規模プロジェクトを遂行する
- [x] 常にプロファイルを意識し、遅いコードを最適化する
- [x] セキュリティに注意し、脆弱性を持ち込まない
- [x] ユーザーからの指示は必ずINSTRUCTIONS.mdに記録し、チェックボックスで課題管理する

---

## Phase 0-2: X11サーバーコア (commit 5679507)
- [x] X11接続ハンドシェイクとセットアップリプライ
- [x] 30+ X11リクエストハンドラ (CreateWindow, MapWindow, ChangeProperty等)
- [x] Atom internテーブル (定義済みatom含む)
- [x] XIDリソース管理 (windows, pixmaps, GCs, fonts, cursors)
- [x] Unixソケットリスナー (/tmp/.X11-unix/X<N>)
- [x] TCP リスナー (port 6000+N)
- [x] 座標系変換 (X11 top-left ↔ macOS bottom-left)
- [x] xdpyinfo, xclock, xev, xlsfonts接続確認

## Phase 3: IOSurface・入力イベント・パフォーマンス最適化 (commit ae18b1b)
- [x] CGImage → IOSurface zero-copyに切替 (GPU compositing)
- [x] マウスボタン/スクロール/モーションイベント転送 (NSEvent → X11)
- [x] root_x/root_y座標修正 (ButtonPress/Release/Scroll)
- [x] トラックパッドスクロール (pixel delta蓄積、80pxしきい値)
- [x] スクリーン高さキャッシュ ([NSScreen mainScreen]の毎回呼出を排除)
- [x] ウィンドウ位置追跡 + ConfigureNotify on move
- [x] CGDisplay APIでスクリーン寸法検出
- [x] キーボードイベント転送 + modifier state
- [x] パフォーマンス: Chrome+YouTube再生時 95%+アイドル達成

## Phase 4: IME入力・キーボードマッピング・Unicode描画 (commit 20db498)
- [x] NSTextInputClientプロトコル実装 (setMarkedText, insertText, firstRectForCharacterRange)
- [x] 日本語IME変換・候補ウィンドウ位置制御
- [x] SUPPRESS_NEXT_KEYUPフラグ (確定キーのKeyRelease漏洩防止)
- [x] PutImageグリフ位置追跡 → IME_SPOT更新 (xterm Xftパス)
- [x] needs_shift() — ASCII記号 (: ! @ # 等) のShift状態正しく付与
- [x] Unicode keysym方式 (0x01000000 | codepoint) + MappingNotify + 仮想キーコード
- [x] ImageText16 (opcode 77) / PolyText16 (opcode 75) — 16bit文字描画
- [x] CoreTextベースUnicode文字レンダリング (HiraginoSans-W3)
- [x] 全角→ASCII正規化 (U+FF01-FF5E, U+3000)

## Phase 5: IMEインラインプリエディット (commit 632b68e)
- [x] setMarkedTextでImePreeditDraw送信 → xterm内リアルタイム変換表示
- [x] unmarkTextで空ImePreeditDraw → プリエディット消去
- [x] 確定時: BS消去→確定文挿入

## 修正済みバグ (2026-02-28)
- [x] xterm文字表示: MapWindow中のExpose送信はevent_tx経由（キュー）
- [x] キーボードフリーズ: CopyArea(graphics_exposures=true)にNoExposure(type 14)必須
- [x] スクロール: CopyAreaのoffset_render_commandsはsrc/dst両方オフセット
- [x] ウィンドウリサイズ: check_window_resizes()で60fps検知→IOSurface再生成→ConfigureNotify+Expose
- [x] 高速出力: フレーム合体廃止（500超でDrawText破棄問題）
- [x] biased select: tokio::select!をbiasedにしてクライアント読み取り優先
- [x] シーケンス再スタンプ: 書き込み時にシーケンス番号を現在値に更新（xcb単調増加要件）
- [x] ラウンドトリップ必須: Opcodes 44,48,52,73,106,110はreply/error必須

## 修正済みバグ (2026-03-02)
- [x] 記号入力バグ (`:` → `;`): needs_shift()追加
- [x] IME確定タイミング: SUPPRESS_NEXT_KEYUPでKeyRelease抑制
- [x] IME候補位置: PutImageグリフ座標追跡
- [x] ConvertSelection: X11コピペプロトコル実装 (SelectionRequest→SelectionNotify)

## 現在作業中 (未コミット)
- [x] Cmd+V ペースト: macOSペーストボード→ImeCommitで挿入
- [x] Cmd+C コピー: X11 PRIMARY選択→macOSクリップボード (pbcopy)
- [x] ClipboardCopyRequestイベント + pending_clipboard_copy フラグ
- [x] GXxor対応: FillRectangleにgc_function追加、Xor時は半透明ハイライト描画
- [x] find_child_at_point(): マウスイベントを最深子ウィンドウに正しく配送
- [x] ButtonPress/Release/MotionNotifyで子ウィンドウ座標変換

---

## Phase 6: 品質改善・仕様準拠 (2026-03-02)

### 修正済み
- [x] QueryColors (opcode 91): AllocColorの正確な16bit色値返却修正 (0xFF→0xFFFF)
- [x] 欠落colormapオペコード追加: 86(AllocColorCells→BadAlloc), 87(AllocColorPlanes→BadAlloc), 88-90(FreeColors/StoreColors/StoreNamedColor→no-op)
- [x] 欠落一般オペコード追加: 39(GetMotionEvents→空リスト), 50(ListFontsWithInfo→終端マーカー), 6(ChangeSaveSet), 13(CirculateWindow), 83(InstallColormap)
- [x] X11カーソル→macOS NSCursor連動: CreateGlyphCursorのsource_char→MacOSCursorType変換、ChangeWindowAttributesでSetWindowCursor送信、addCursorRect適用
- [x] ウィンドウリサイズ引き伸ばし: NSViewLayerContentsPlacementTopLeft(=11)設定でmacOSがlive resize中にコンテンツを伸縮させるのを防止
- [x] 死コード除去: opcode 41のno-opリスト重複（handle_warp_pointerが先にマッチ）

### テスト整備 (12→41テスト)
- [x] cursor/mod.rs: カーソルグリフマッピングテスト、全標準カーソル網羅テスト
- [x] server/resources.rs: WindowState初期値、WindowClass変換、プロパティCRUD、イベント配信、GCデフォルト値、GcFunction変換、複数クライアントイベント選択
- [x] server/events.rs: EventBuilder 32バイト保証、シーケンス番号、BE/LE対応、KeyPress/Expose/ConfigureNotify/MapNotify/ClientMessage全フィールド検証
- [x] server/protocol.rs: イベントマスクbit一意性、仕様値一致、EXPOSURE/STRUCTURE_NOTIFYビット位置
- [x] server/mod.rs: ServerErrorコード、サーバー生成、ルートウィンドウ、リソースID割当、接続ID割当、TrueColorピクセル形式、色値ラウンドトリップ、フォーカスデフォルト

### X11仕様準拠検証

#### QueryColors (opcode 91) — X11R7.7仕様準拠
- リクエスト: COLORMAP + LISTofCARD32(pixel) ✓
- リプライ: LISTofRGB (各エントリ red/green/blue 16bit + 2byte pad) ✓
- TrueColor: ピクセル値→RGB直接分解 (red_mask=0xFF0000, green_mask=0xFF00, blue_mask=0xFF) ✓
- 16bit変換: 8bit値→`(val << 8 | val)`で正確な16bit表現 ✓

#### AllocColor (opcode 84) — X11R7.7仕様準拠
- TrueColor: RGB16→RGB8→pixel計算 ✓
- exact color: 実際にハードウェアが使う色値を16bitで返却 ✓
- 色値ラウンドトリップ: AllocColor→QueryColorsで一貫した値 ✓ (テスト検証済み)

#### ChangeWindowAttributes (opcode 2) — カーソル属性
- value_mask 0x4000: cursor属性ビット ✓
- cursor=0: デフォルトカーソル(arrow) ✓
- cursor>0: CursorStateのsource_char→MacOSCursorTypeマッピング ✓
- ネイティブウィンドウ検索: 子ウィンドウから祖先チェーンでtop-level検索 ✓

#### ConfigureWindow (opcode 12) — リサイズイベント
- ConfigureNotify送信: StructureNotifyMask持つクライアントに配信 ✓
- Expose送信: ExposureMask持つクライアントに配信 ✓
- 子ウィンドウ連動: 親リサイズ時に直接の子も同サイズに更新 ✓

#### ウィンドウリサイズ — macOS/X11連携
- setFrameSize:オーバーライド: macOS live resize中にsynchronous IOSurface更新 ✓
- 縮小時: IOSurface再生成なし、masksToBoundsでクリップ ✓
- 拡大時: 新IOSurface生成、旧コンテンツcopy_nonoverlapping ✓
- layerContentsPlacement=TopLeft: ドラッグ中のコンテンツ引き伸ばし防止 ✓
- contentsGravity=topLeft, contentsScale=1.0: ピクセル1:1マッピング ✓
- check_window_resizes(): バックアップ検知（位置変更等） ✓

### X11コマンド動作対応状況
| コマンド | 必要な機能 | 対応状況 |
|----------|-----------|----------|
| xterm | KeyPress/Release, Expose, ConfigureNotify, IME | ✓ 完全動作 |
| ico | CreateWindow, MapWindow, PolyLine, ClearArea | ✓ 動作確認済み |
| xlsfonts | ListFonts, OpenFont, QueryFont | ✓ 動作確認済み |
| xev | KeyPress/Release, ButtonPress/Release, MotionNotify | ✓ 動作確認済み |
| xdpyinfo | QueryExtension, ListExtensions, GetProperty | ✓ 動作確認済み |
| xeyes | FillArc, PolyFillArc, CreateGlyphCursor | △ 基本動作（SHAPE拡張なし） |
| xclock | FillArc, DrawArc, AllocColor, QueryColors | △ 基本動作（Xt/Xaw依存） |
| xcalc | Xt/Xaw widgets, 多数のウィンドウ | △ 基本動作 |
| xlogo | FillPolygon, SHAPE拡張 | △ 矩形表示（SHAPE未実装） |

### 修正済みバグ (2026-03-03)
- [x] mac_y座標修正: MoveResizeWindowの`screen_h - y - pt_h - title_bar_h`を`screen_h - y - pt_h`に修正（forward/reverse変換不整合）
- [x] リサイズ時全面背景クリア: setFrameSizeで拡大・縮小ともIOSurface全面をbackground_pixelでクリアしてからExpose送信（xclock等が白背景前提で描画）
- [x] 特殊マウスハンドラ撤去: PSLXInputViewのno-op mouseDown/mouseDragged、mouseDownCanMoveWindow、setMovableByWindowBackground全削除
- [x] sendEventフィルタ撤去: マウスイベントの選択的ブロック廃止、全イベントをAppKitに渡す
- [x] ドラッグ引きずり: ポーリング/NSEvent二重ButtonPress防止(LAST_BUTTONS)、MotionNotify watchdog
- [x] IME変換中キーイベント漏れ: IME_COMPOSINGチェック追加

### 今後の課題 (実装✅ / 検証✅)
- [ ][ ] Chrome/VS CodeのURL窓・検索窓でキーボード入力が効かない（FocusIn/Out送信追加済み、要検証）
- [ ][ ] Chrome/VS Codeで日本語IMEインライン編集が動かない
- [x][x] xtermでIME日本語インライン表示+BSバグ修正 — (1) Homebrew libX11のXSupportsLocale()=0問題: locale module(.2.so)にLinux命名(.so.2)のsymlink作成で解決 (2) プリエディットフリッカー: 毎回全消去→全再描画をインクリメンタル更新に変更(starts_withで接頭辞一致なら差分のみ送信)
- [x][ ] ブラウザ音声出力: PulseAudio TCP (port 4713, auth-anonymous) + start-with-audio.shスクリプト
- [x][ ] SHAPE拡張実装 (xeyes/xlogo非矩形ウィンドウ対応) — shape.rs作成、全サブオペコード対応
- [x][ ] RENDER拡張実装 (スタブ) — QueryVersion 0.11, QueryPictFormats(ARGB32/RGB24/A8), 描画op全no-op
- [x][x] BIG-REQUESTS拡張実装 — 既存実装、opcode 133で16MB対応
- [x][ ] XTest拡張実装 (xdotool対応) — xtest.rs作成、FakeInput/GetVersion対応
- [x][ ] GetImage: IOSurfaceからの実ピクセルデータ読み取り — ReadPixelsコマンド追加
- [x][x] bit_gravity対応（リサイズ時のコンテンツ移動制御）— 既にフィールド+setFrameSizeで動作

## iPad (iOS) 課題 (2026-03-13)
- [x] 漢字入力: UITextInput(setMarkedText/insertText/unmarkText)実装、IME変換・確定動作確認済み
- [x] ソフトウェアキーボード: becomeFirstResponder実装、タップ時にバーチャルキーボード表示
- [ ] ウィンドウサイズ: Stage Managerでぴったりサイズ出ない問題
- [x] コピペ: UIPasteboard経由のSetClipboard/GetClipboard実装済み
- [x] マウス対応: UIHoverGestureRecognizer + buttonMask右クリック検出実装
- [x] スクロール: UIPanGestureRecognizer(allowedScrollTypesMask=1)実装、30pxしきい値
- [x] Ctrl+キー: pressesBegan修飾キー検出、Ctrl+C/D/Z等の端末制御対応
- [x] IOSurface zero-copy: CGImage経由コピー廃止、macOS同様のsetContents:IOSurface

### パフォーマンス最適化 (2026-03-13)
- [x] DashMap is_empty()廃止: 全16シャード読みロック→個別get_mutに変更。プロトコルスレッドとの競合19K→9サンプルに激減
- [x] レンダーメールボックスMAX_COMMANDS=4096: メモリリーク防止 (ico 55GB→121MB安定)
- [x] iOS flush_window_to_layer: CFData→CGImage→setContentsを廃止、IOSurface直接設定（5サンプルに激減）
