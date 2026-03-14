#!/bin/bash
# MacBookでChrome + YouTube + 音声ストリーム起動スクリプト
# 使い方: ./start-mac-chrome.sh [URL]
# NOTE: Xserverは必ず.appバンドルから起動する（解像度検出に必要）

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
MAC_IP="192.168.0.101"
REMOTE="pengin0906@9955wx"
URL="${1:-https://www.youtube.com}"

echo "=== MacBook Chrome 起動スクリプト ==="

# 1. 既存プロセス停止
echo "[1/5] 既存プロセス停止..."
ssh "$REMOTE" "pkill -f 'chrome-pslx'" 2>/dev/null
ssh "$REMOTE" "pkill -f 'xterm'" 2>/dev/null
pkill -f "ffplay.*f32le" 2>/dev/null
pkill -f Xserver.app || true
sleep 1

# 2. Xserver起動（.appバンドル経由 — 解像度が正しく取れる）
echo "[2/5] Xserver起動..."
cp "$SCRIPT_DIR/target/release/Xserver" "$SCRIPT_DIR/Xserver.app/Contents/MacOS/"
open "$SCRIPT_DIR/Xserver.app" --args --tcp --log-level warn
sleep 2
if lsof -i :6000 -P 2>/dev/null | grep -q LISTEN; then
    echo "  X11ポート: OK"
else
    echo "  ERROR: Xserver起動失敗"
    exit 1
fi

# 3. xterm起動
echo "[3/5] xterm起動..."
ssh "$REMOTE" "DISPLAY=${MAC_IP}:0 LANG=ja_JP.UTF-8 xterm -u8 &disown" 2>/dev/null &

# 4. Chrome起動
echo "[4/5] Chrome起動: $URL"
ssh "$REMOTE" "DISPLAY=${MAC_IP}:0 /opt/google/chrome/chrome \
    --no-sandbox --disable-gpu --disable-dev-shm-usage \
    --no-first-run --user-data-dir=/tmp/chrome-pslx \
    --ozone-platform=x11 '$URL' &disown" 2>/dev/null &
sleep 5

# 5. 音声ストリーム（9955wx → MacBookスピーカー）
echo "[5/5] 音声ストリーム開始..."
nohup bash -c "ssh $REMOTE 'DISPLAY= parec --format=float32le --rate=48000 --channels=2 \
    --device=alsa_output.pci-0000_21_00.7.iec958-stereo.monitor' 2>/dev/null | \
    /opt/homebrew/bin/ffplay -nodisp -f f32le -ar 48000 -ch_layout stereo -i - 2>/dev/null" &
AUDIO_PID=$!

echo ""
echo "=== 起動完了 ==="
echo "  映像: MacBook (${MAC_IP}:0)"
echo "  音声: MacBook スピーカー (PID $AUDIO_PID)"
echo ""
echo "停止: ./stop-mac-chrome.sh"
