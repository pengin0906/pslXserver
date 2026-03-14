# Xserver iOS ビルド手順

## 前提条件

1. **Xcode** (フルバージョン、App Storeからインストール)
   - Command Line Toolsだけではダメ（iOS SDKが必要）
   - `sudo xcode-select -s /Applications/Xcode.app/Contents/Developer`

2. **Rust iOS target**
   ```bash
   rustup target add aarch64-apple-ios
   ```

## ビルド

### 方法1: Xcodeから（推奨）

```bash
open ios-app/Xserver-iOS.xcodeproj
```

- Xcodeが開いたらTeamを設定（Signing & Capabilities）
- 実機を接続してRun（シミュレータは aarch64-apple-ios-sim target が別途必要）
- Xcodeのビルドフェーズ "Build Rust Library" が自動で `cargo build --release --target aarch64-apple-ios --lib` を実行

### 方法2: コマンドライン

```bash
# 1. Rust静的ライブラリをビルド
~/.cargo/bin/cargo build --release --target aarch64-apple-ios --lib

# 2. Xcodeプロジェクトをビルド
cd ios-app
xcodebuild -project Xserver-iOS.xcodeproj \
  -scheme Xserver-iOS \
  -configuration Release \
  -destination 'generic/platform=iOS' \
  CODE_SIGN_IDENTITY=- \
  build
```

## アーキテクチャ

```
Mac (Xserver macOS版)     iPhone/iPad (Xserver iOS版)
┌──────────────────────┐    ┌──────────────────────────────┐
│ server/              │    │ server/                      │
│  connection.rs       │    │  connection.rs               │
│  mod.rs              │    │  mod.rs                      │
│  (X11 protocol)      │    │  (X11 protocol) ← 同じコード │
├──────────────────────┤    ├──────────────────────────────┤
│ display/             │    │ display/                     │
│  macos.rs (AppKit)   │    │  ios.rs (UIKit)              │
│  NSWindow            │    │  UIWindow (1つ/フルスクリーン) │
│  NSView + IME        │    │  UIView + UIKeyInput         │
│  CGEventTap          │    │  Touch → Mouse変換           │
├──────────────────────┤    ├──────────────────────────────┤
│ renderer.rs          │    │ renderer.rs ← 同じコード      │
│ IOSurface + CALayer  │    │ IOSurface + CALayer          │
└──────────────────────┘    └──────────────────────────────┘
```

## 使い方

iPhone上でXserverが起動すると、TCP ポート6000でX11接続を待ち受ける。

同じWiFi上のLinuxマシンから：
```bash
DISPLAY=<iPhone_IP>:0 xterm -u8
```

または SSH X11フォワーディング経由。

## 制限事項

- Unix domain socketは使えない（iOS sandbox） → TCP接続のみ
- ソフトウェアキーボードからの入力（ハードウェアキーボード対応は今後）
- タッチ→マウスの変換（左クリックのみ、右クリックは長押しで追加予定）
- 単一フルスクリーン表示（X11ウィンドウの重ね表示は今後）
- iPad Split View/Slide Over未対応
