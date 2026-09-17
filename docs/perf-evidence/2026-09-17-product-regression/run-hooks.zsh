#!/bin/zsh
set -euo pipefail
umask 077
export TMPDIR=/private/tmp/egfinal27-hooks
mkdir -p "$TMPDIR"
cd /private/tmp/engram-product-final-20260917
engram eval run hooks.yaml --output /private/tmp/engram-product-final-20260917/hooks --json > hooks-result.json
print complete > hooks.complete
