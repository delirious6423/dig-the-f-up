#!/bin/bash
# Builds ~/DigUpTestbed/zh: Simplified and Traditional Chinese files (txt, md, rtf, docx, pdf) for evals/chinese.json.
# macOS only (textutil, cupsfilter). Index it together with the synthetic testbed: a 9-file index is too small for the z-gate.
set -euo pipefail
D="${1:-$HOME/DigUpTestbed/zh}"; mkdir -p "$D"; cd "$D"
printf '今天天气很好，我们计划在周末去海边散步，然后在附近的餐厅吃海鲜。记得带上防晒霜和雨伞。\n' > 天气计划_简体.txt
printf '今天天氣很好，我們計劃在週末去海邊散步，然後在附近的餐廳吃海鮮。記得帶上防曬霜和雨傘。\n' > 天氣計劃_繁體.txt
printf '# 会议纪要\n\n产品发布推迟到十二月，原因是电子支付系统出现问题。服务器团队将在月底前修复错误，并与一小部分用户一起测试新版本。\n' > 会议纪要_简体.md
printf '# 會議紀要\n\n產品發布推遲到十二月，原因是電子支付系統出現問題。伺服器團隊將在月底前修復錯誤，並與一小部分用戶一起測試新版本。\n' > 會議紀要_繁體.md
printf '我的咖啡店收据：一杯拿铁，两块蛋糕，总计八十五元。谢谢惠顾，欢迎再次光临。\n' > 咖啡店收据.txt
printf '量子計算機利用量子位元進行運算，與傳統電腦相比，在某些問題上具有指數級的速度優勢。\n' > 量子計算.txt
textutil -convert rtf 天气计划_简体.txt -output 天气_简体.rtf
textutil -convert docx 會議紀要_繁體.md -output 會議_繁體.docx
cupsfilter 咖啡店收据.txt > 收据.pdf 2>/dev/null
