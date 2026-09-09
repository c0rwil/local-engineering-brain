#!/bin/sh

set -eu

test -s answer.txt
grep -Eq '^mode=(off|brain)$' answer.txt
grep -Fq 'Create the deterministic fixture answer.' answer.txt
