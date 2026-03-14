#!/bin/bash
# iPadでChrome + YouTube + 音声ストリーム起動スクリプト
# 使い方: ./start-ipad-chrome.sh [URL]
#   URL省略時: https://www.youtube.com

IPAD_IP="192.168.0.100"
REMOTE="pengin0906@9955wx"
URL="${1:-https://www.youtube.com}"

echo "=== iPad Chrome 起動スクリプト ==="

# 1. 既存プロセス停止
echo "[1/4] 既存プロセス停止..."
ssh "$REMOTE" "pkill -f 'chrome-ipad'" 2>/dev/null
sleep 1

# 2. iPad Xサーバー確認
echo "[2/4] iPad Xサーバー確認..."
if ! nc -z -w 3 "$IPAD_IP" 6000 2>/dev/null; then
    echo "  iPad Xサーバーが起動してません。iPadでXserverアプリを起動してください。"
    echo "  待機中..."
    while ! nc -z -w 3 "$IPAD_IP" 6000 2>/dev/null; do sleep 2; done
    echo "  接続OK!"
fi
echo "  X11ポート: OK"

# 3. Chrome起動（ローカルPulseAudioで音声）
echo "[3/4] Chrome起動: $URL"
ssh "$REMOTE" "DISPLAY=${IPAD_IP}:0 /opt/google/chrome/chrome \
    --no-sandbox --disable-gpu --disable-dev-shm-usage \
    --no-first-run --user-data-dir=/tmp/chrome-ipad \
    --ozone-platform=x11 '$URL' &disown" 2>/dev/null &
sleep 5

# 4. 音声ストリーム（9955wx → iPad PAサーバー）
echo "[4/4] 音声ストリーム開始..."
ssh "$REMOTE" "bash -c 'parec --format=float32le --rate=48000 --channels=2 \
    --device=alsa_output.pci-0000_21_00.7.iec958-stereo.monitor | \
    PULSE_SERVER=tcp:${IPAD_IP}:4713 paplay --raw --format=float32le \
    --rate=48000 --channels=2' &disown" 2>/dev/null &

echo ""
echo "=== 起動完了 ==="
echo "  映像: iPad (${IPAD_IP}:0)"
echo "  音声: iPad スピーカー (PA ${IPAD_IP}:4713)"
echo ""
echo "停止: ./stop-ipad-chrome.sh"
