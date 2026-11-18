#!/bin/sh
input=$(cat)
dir=$(dirname "$0")
case "$input" in
"Documents to read for the world list."*"=== file: Alpha/mail one.txt"*) cat "$dir/eval-world-mail.jsonl" ;;
"Documents to read for the world list."*"=== file: Alpha/order two.txt"*) cat "$dir/eval-world-order.jsonl" ;;
"Entities to join"*) cat "$dir/eval-join-empty.jsonl" ;;
"Documents to write questions about."*) cat "$dir/eval-questions.jsonl" ;;
"Documents to write search queries for."*) cat "$dir/eval-finds.jsonl" ;;
"Search query: levering"*) cat "$dir/eval-answer-no.jsonl" ;;
"Search query:"*) cat "$dir/eval-answer-yes.jsonl" ;;
"Question to answer: Waar"*) cat "$dir/eval-answer-no.jsonl" ;;
"Question to answer:"*) cat "$dir/eval-answer-yes.jsonl" ;;
"Chunks to label (pass "*)
    printf '%s\n' "$input" | awk '
NR == 1 { pass = ($0 ~ /pass 1/) ? 1 : 2 }
/^=== chunk c[0-9]+ / { id = $3 }
/rated power 55 kW/ {
    out = out sep "{\"chunk\":\"" id "\",\"equipment\":\"M-1\",\"property\":\"rated power\",\"value\":\"55\",\"unit\":\"kW\",\"quote\":\"rated power 55 kW\"}"; sep = ","
    if (pass == 1) out = out ",{\"chunk\":\"" id "\",\"equipment\":\"M-1\",\"property\":\"speed\",\"value\":\"1480\",\"unit\":\"rpm\",\"quote\":\"1480 rpm\"}"
}
END { printf "{\"type\":\"result\",\"is_error\":false,\"total_cost_usd\":0,\"structured_output\":{\"values\":[%s]}}\n", out }' ;;
"Claims to judge:"*)
    printf '%s\n' "$input" | awk -v strict="$ZONTOLOGIZE_FAKE_STRICT" '
/^- j[0-9]+: / {
    id = $2; sub(":", "", id)
    right = (strict == "" && index($0, "Ghost") == 0) ? "true" : "false"
    out = out sep "{\"id\":\"" id "\",\"right\":" right "}"; sep = ","
}
END { printf "{\"type\":\"result\",\"is_error\":false,\"total_cost_usd\":0,\"structured_output\":{\"verdicts\":[%s]}}\n", out }' ;;
*) echo "fake-claude: no recorded answer for this prompt" >&2; exit 1 ;;
esac
