#!/usr/bin/env bash
# Start the remoat daemon.
set -e
cd "$(dirname "$0")"
exec ./.venv/bin/python -m daemon "$@"
