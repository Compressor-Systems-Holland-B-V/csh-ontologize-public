#!/bin/sh
ZONTOLOGIZE_FAKE_STRICT=yes exec "$(dirname "$0")/fake-claude-eval.sh" "$@"
