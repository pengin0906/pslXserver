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
