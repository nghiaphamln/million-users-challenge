#!/usr/bin/env bash
# Builds bin/server from the C3 sources. No dependencies: the Postgres driver and gzip are part of the program.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p bin
C3C=${C3C:-c3c}
# the benchmark box is x86-64; other machines (local testing) build for their own CPU
case "$(uname -m)" in
  x86_64) TARGET="--target linux-x64 --x86cpu=sse4";;
  aarch64) TARGET="--target linux-aarch64";;
  *) TARGET="";;
esac
"$C3C" compile -O3 --single-module=yes $TARGET --safe=no -o bin/server *.c3
