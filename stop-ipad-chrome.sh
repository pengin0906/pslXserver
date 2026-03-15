#!/bin/bash
# iPad Chrome 停止スクリプト
REMOTE="pengin0906@9955wx"

echo "Chrome + 音声ストリーム停止中..."
ssh "$REMOTE" "pkill -f 'chrome-ipad'; pkill -f 'parec.*monitor.*paplay'" 2>/dev/null
echo "停止完了"
