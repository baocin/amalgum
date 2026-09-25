#!/usr/bin/env bash
# PostToolUse (Edit|Write): rustfmt the one file just written. Never blocks.
payload="$(cat)"
f=""
command -v jq >/dev/null 2>&1 && f="$(printf '%s' "$payload" | jq -r '.tool_input.file_path // empty' 2>/dev/null)"
case "$f" in *.rs) [ -f "$f" ] && command -v rustfmt >/dev/null 2>&1 && rustfmt "$f" >/dev/null 2>&1 ;; esac
exit 0
