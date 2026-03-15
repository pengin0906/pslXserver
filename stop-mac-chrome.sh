#!/bin/bash
# MacBook Chrome 停止スクリプト
REMOTE="pengin0906@9955wx"

echo "Chrome + 音声ストリーム停止中..."
ssh "$REMOTE" "pkill -f 'chrome-pslx'" 2>/dev/null
pkill -f "ffplay.*f32le" 2>/dev/null
echo "停止完了"
