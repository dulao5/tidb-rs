#!/usr/bin/env bash
# Builds ps_smoke, a libmysqlclient load generator for the protocol layer (prepared `SELECT ?`
# echo, or text `SELECT 1`). Needs MySQL client headers/libs (Homebrew: mysql@8.4).
#   tools/protocol-smoke/build.sh && target/ps_smoke 4001 8 10 ps [tls]
set -euo pipefail
cd "$(dirname "$0")/../.."
MYSQL_PREFIX=${MYSQL_PREFIX:-$(brew --prefix mysql@8.4 2>/dev/null || echo /usr)}
mkdir -p target
/usr/bin/cc -O2 -o target/ps_smoke tools/protocol-smoke/ps_smoke.c \
    -I"$MYSQL_PREFIX/include/mysql" -L"$MYSQL_PREFIX/lib" -lmysqlclient -lpthread \
    -Wl,-rpath,"$MYSQL_PREFIX/lib"
echo "built target/ps_smoke"
