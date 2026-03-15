要するに、Mac 側のかな・漢字変換を、そのまま Chrome の入力欄の中で普通にインライン表示したい、ということですね。

結論
それが正しい目標です。
そして今の不具合は、日本語入力を「キーコードの問題」として運んでいるのがズレです。Ubuntu で普通に動く系統は、生キーは XKB/evdev の標準キーとして渡し、日本語の確定前文字列は IM の preedit、確定文字列は commit として渡す、この二本立てです。Xlib/XIM でも、IM が preedit を扱い、クライアントは event filter と lookup 経路で文字列を受け取る前提です。インライン表示をしたいなら、仕様上は XIMPreeditPosition か XIMPreeditCallbacks 側が本筋で、XIMPreeditNothing / XIMPreeditNone では入力欄内の preedit は出ません。

だから、今の
「Mac の marked text を、仮想 keycode + Unicode keysym + BackSpace で擬似タイピングする」
方式だと、かな入力の「た」→濁点で「だ」のようなpreedit 差し替えを正しく表現できません。
このとき本来やるべきことは「preedit 内容の更新」なのに、今は「一回打った文字を BS で消して別文字を打ち直す」に化けています。すると、新しい文字がクライアント側でまだ反映されていないのに BS だけ効いて、既存テキストが戻っていく、という今の症状になります。

Chrome 側も「Unicode を知らない」わけではありません。Chromium の X11 側コードは、X イベントから keycode と modifier を取り、keysym に変換し、その keysym から Unicode を得る流れです。しかも Linux/X11 では XKB keycode = evdev + 8 を前提にしており、既定の hardware keycode 変換表も標準 evdev 系の固定表です。Unicode-style keysym もコード上は扱いますが、これは正しい XKB/evdev の土台の上で来ることが前提です。なので、日本語文字そのものを高い仮想 keycode に載せて運ぶ設計は、Chrome では土台として弱い、という理解で合っています。

なので直す場所はこれです。

日本語 preedit/commit を fake KeyPress で送るのをやめる。

raw key は raw key、文字列は IM 文字列に分けます。

Mac 側の「確定前文字列」は preedit 更新、確定時だけ commit にします。

keymap は標準 evdev/XKB だけを担当させる。

BS、Enter、矢印、修飾キーはそのまま標準 keycode。

日本語文字まで keymap に押し込まないのが肝です。Chromium も evdev/XKB 前提で keycode を見ています。

インライン変換を本当にやるなら、XIM の inline 系を正しく通す。

仕様上は XIMPreeditPosition なら XNSpotLocation でキャレット位置を渡せますし、XIMPreeditCallbacks ならクライアント内描画もできます。逆に XIMPreeditNothing / None なら inline にはなりません。

つまり一言で言うと、

「正しい keymap に直す」のは raw key まで。
「漢字を普通にインラインで入れる」は keymap ではなく IM の仕事。

ここを分ければ、目標の
「Mac で普通に変換したものが、Chrome の入力欄にそのままインラインで見える」
に近づきます。
今の実装は、その二つを一つの仕組みに押し込んでいるので壊れています。

どうすればいい
claude codeにやってもらうので明確な指示書書いて

いまの材料だけで、Claude Code にそのまま渡せる形にまとめます。
狙いは「回り道の仮想 keycode いじりをやめて、Mac の変換中文字列を Chrome の入力欄に普通にインライン表示させる」方針で固定することです。

⋮