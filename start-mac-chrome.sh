#!/bin/bash
# MacBookでChrome + YouTube + 音声ストリーム起動スクリプト
# 使い方: ./start-mac-chrome.sh [URL]

MAC_IP="192.168.0.101"
REMOTE="pengin0906@9955wx"
URL="${1:-https://www.youtube.com}"

echo "=== MacBook Chrome 起動スクリプト ==="

# 1. 既存プロセス停止
echo "[1/4] 既存プロセス停止..."
ssh "$REMOTE" "pkill -f 'chrome-pslx'" 2>/dev/null
pkill -f "ffplay.*f32le" 2>/dev/null
sleep 1

# 2. Xserver確認
echo "[2/4] Xserver確認..."
if ! lsof -i :6000 -P 2>/dev/null | grep -q LISTEN; then
    echo "  Xserver起動中..."
    nohup /Users/penguin/projects/Xserver/target/release/Xserver --tcp --log-level warn &>/dev/null &
    sleep 2
fi
echo "  X11ポート: OK"

# 3. Chrome起動
echo "[3/4] Chrome起動: $URL"
ssh "$REMOTE" "DISPLAY=${MAC_IP}:0 /opt/google/chrome/chrome \
    --no-sandbox --disable-gpu --disable-dev-shm-usage \
    --no-first-run --user-data-dir=/tmp/chrome-pslx \
    --ozone-platform=x11 '$URL' &disown" 2>/dev/null &
sleep 5

# 4. 音声ストリーム（9955wx → MacBookスピーカー）
echo "[4/4] 音声ストリーム開始..."
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
