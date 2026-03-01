# pslXserver 作業指示・課題管理

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

## 未解決の課題
- [ ] QueryColors (opcode 91) 未完全 — xeyes/xclockがクラッシュする場合あり
- [ ] ウィンドウリサイズ: ドラッグ中は引き伸ばし、離すと正しいサイズに（60fps検知の限界）
- [ ] カーソルがX11ウィンドウと連動しない（macOSカーソルのまま）
- [ ] いろんなXで動くコマンドまず公式リリースの付属コマンド全部動作確認してダメなとこ一つずつ直して
- [ ] 一つ一つの単体テストが適当すぎるんだよ。ちゃんとテスト計画立ててテストしてください。
- [ ] ウインドウ広げたり縮めたりして挙動が標準Xサーバーと同じかどうか確認する
- [ ] 仕様どうり動いているか仕様書を見て挙動を確認する
