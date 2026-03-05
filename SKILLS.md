# SKILLS.md — クライアント起動ガイド

## 共通ルール
- 全クライアントはUTF-8ロケールで起動する
- xterm: 必ず `-u8` オプションをつける

## 起動コマンド例

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

## 注意事項
- XI2/XKBは無効化必須（有効だとChrome/VS Code/Firefoxがクラッシュする）
- faceName(.Xresources)は使わない（RENDER拡張未実装）
- 既存Chromeと分離するため `--user-data-dir` 必須
