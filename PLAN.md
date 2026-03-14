# Chrome IME インライン: 仮想keycode方式 → XIM方式

## 目標
Mac のかな・漢字変換の途中表示（preedit）と確定文字列（commit）を、
Chrome/Electron の入力欄の中で普通にインライン表示する。

## 現状の問題
`send_ime_text()` (mod.rs:1089) が日本語文字を仮想keycodeに載せて KeyPress として送っている。
- Unicode keysym `0x01000000 | codepoint` を keycode 86〜135 に割り当て
- MappingNotify → GetKeyboardMapping のラウンドトリップ待ちが必要
- preedit 更新時に BS で消して打ち直す（BS食い込みバグの温床）
- Chrome は evdev/XKB 前提の固定 keycode 変換表を持つので仮想 keycode が意味をなさない

## 方針: raw key と IM 文字列を分離する

### 原則
1. **raw key は raw key**: BS, Enter, 矢印, 修飾キー, ASCII は標準 evdev keycode で送る（今まで通り）
2. **日本語文字は IM の仕事**: preedit は `XIM_PREEDIT_DRAW`, 確定は `XIM_COMMIT` で送る
3. **keymap に日本語文字を押し込まない**: `virtual_keysyms`, `MappingNotify`, 仮想 keycode 86〜135 は Chrome には使わない

## 実装手順

### Phase 1: XIM を有効化して Chrome の挙動を確認

**ファイル: `src/server/mod.rs`**

1. 328行目付近の `if false` ガードを外して XIM_SERVERS を root に設定する
2. ログで Chrome が XIM 接続してくるか、どの input_style を選ぶかを確認する
3. `src/server/xim.rs` 582行目: `XIM_SET_EVENT_MASK` の `forward_event_mask` を
   `KeyPressMask | KeyReleaseMask` (0x03) に変更する
   - これにより Chrome は KeyPress を XIM サーバー（Xserver）に転送してくる
   - Xserver は非IME中のキーはそのまま `XIM_FORWARD_EVENT` で Chrome に返す

### Phase 2: XIM ForwardEvent フロー実装

**ファイル: `src/server/xim.rs`**

`handle_forward_event()` (669行目) を修正:
- 現状: イベントをそのまま event_tx に送り返すだけ
- 修正後:
  - IME composing 中（`IME_COMPOSING` フラグ）→ イベントを飲み込む（macOS IME が処理）
  - IME composing 中でない → `XIM_FORWARD_EVENT` メッセージとして返す
    - **重要**: 生イベントではなく XIM_FORWARD_EVENT(opcode=60) メッセージとして返す
    - flag の synchronous bit をクリアして返す

フロー:
```
クライアント → XIM_FORWARD_EVENT(KeyPress) → Xserver
  IME中でない場合:
    Xserver → XIM_FORWARD_EVENT(KeyPress, flag=0) → クライアント
  IME中の場合:
    (macOS IMEが処理 → ImeCommit/ImePreeditDraw として戻ってくる)
```

### Phase 3: ImeCommit / ImePreeditDraw の XIM 経由配信

**ファイル: `src/server/mod.rs`**

`DisplayEvent::ImeCommit` ハンドラ (654行目):
- XIM クライアントの場合: `xim.send_commit()` のみ（既存コード）
- 非 XIM クライアント（xterm）: 今まで通り `send_ime_text()` で仮想 keycode 方式

`DisplayEvent::ImePreeditDraw` ハンドラ (686行目):
- XIM クライアントの場合: `xim.send_preedit_start/draw/done()` のみ（既存コード）
- 非 XIM クライアント（xterm）: 今まで通り BS + 仮想 keycode 方式

**ポイント**: XIM クライアントとの判別は `server.xim.has_xim_client()` で既に実装済み。
Chrome が XIM 接続してくれば、自動的に XIM 経路が使われる。
mod.rs の ImeCommit/ImePreeditDraw 分岐は既にこの構造になっている。

### Phase 4: XFilterEvent 問題の対処

Chrome の `XFilterEvent` は XIM 接続があると全 KeyPress を「フィルター済み」として消費する。
これは正常動作 — XIM プロトコルのフロー:

1. KeyPress → `XFilterEvent()` → true → XIM サーバーに `XIM_FORWARD_EVENT` で転送
2. XIM サーバーが `XIM_FORWARD_EVENT` で返す → `XFilterEvent()` → false → アプリが `XmbLookupString()` で処理
3. XIM サーバーが `XIM_COMMIT` を送る → `XmbLookupString()` が確定文字列を返す

`forward_event_mask` を正しく設定すれば、Xserver が全キーイベントのゲートキーパーになる。
- IME 中でなければそのまま返す → Chrome が普通に処理
- IME 中なら飲み込んで preedit/commit で返す → Chrome がインライン表示

### Phase 5: xterm との共存

xterm は `XMODIFIERS=@im=none` で起動するため XIM 接続を作らない。
→ `has_xim_client()` が false → 既存の仮想 keycode 方式がそのまま使われる。
**xterm 側は一切変更不要。**

## 変更しないもの
- `send_ime_text()`, `send_backspaces()`, `virtual_keysyms` — xterm 用に残す
- `ascii_to_x11_keycode()`, `needs_shift()` — ASCII キー処理は変更なし
- macOS 側の IME ハンドリング (`setMarkedText:`, `insertText:`) — 変更なし
- `DisplayEvent::KeyPress/KeyRelease` — 物理キーの配信は変更なし

## テスト手順
1. Xserver を `--tcp` 付きで起動（XIM有効化ビルド）
2. Docker Chrome を起動（`XMODIFIERS` は不要、デフォルトで XIM_SERVERS を見る）
3. ログで XIM 接続確認: `XIM: _XIM_XCONNECT`, `XIM: CreateIC input_style=...`
4. Chrome のアドレスバーに ASCII を入力 → 通常通り表示されること
5. 日本語 IME で「にほんご」→ 変換 → 確定 → Chrome 入力欄にインライン表示されること
6. xterm で同様のテスト → 既存動作が壊れていないこと

## リスクと注意
- Chrome が `XIMPreeditCallbacks` を選ばず `XIMPreeditNothing` を選ぶ可能性
  → preedit のインライン表示は Chrome 側でやらない（候補ウィンドウ方式）
  → commit は `XIM_COMMIT` で正常に動く。これでも現状よりは大幅改善
- `forward_event_mask` の設定値を間違えると全入力が死ぬ
  → Phase 1 で確認しながら段階的に進める
- xterm が意図せず XIM 接続を作る場合
  → `XMODIFIERS=@im=none` で回避
