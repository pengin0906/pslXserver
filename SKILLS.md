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

# macOS (Homebrew)
LANG=en_US.UTF-8 XMODIFIERS=@im=none xterm -u8 -fn "-misc-fixed-medium-r-semicondensed--13-120-75-75-c-60-iso10646-1"
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
- この問題はpslXserver固有ではなくHomebrew libX11のバグ
