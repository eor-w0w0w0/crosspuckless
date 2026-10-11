#!/usr/bin/env bash
set -euo pipefail

# Preparation is the default. Uploads require an explicit --publish.
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec python3 "$script_dir/generate-appcast.py" "$@"
