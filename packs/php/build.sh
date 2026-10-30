#!/bin/sh
# Builds the PHP pack: php-src (embed SAPI, no extensions beyond the core) for wasm32-wasip1
# with wasi-sdk, plus pack.c (the pack ABI). Downloads both into .build/ on first run.
#
# Each PHP minor version is its own line (8.2 and 8.4 differ a lot), built from its own source
# with its own patches (patches/<line>/):
#
#   packs/php/build.sh                       # 8.4.27, the line the browser ships
#   PHP_VERSION=8.2.35 packs/php/build.sh    # another line
#
# Output: target/php-<line>/php_pack.wasm; the default line is also target/php_pack.wasm,
# which the browser loads.
set -eu
cd "$(dirname "$0")"
DEFAULT=8.4.27
PHP=${PHP_VERSION:-$DEFAULT}
LINE=$(echo "$PHP" | cut -d. -f1,2)
SDK_VER=34
here=$PWD
mkdir -p .build && cd .build

SDK=$PWD/wasi-sdk-$SDK_VER.0-arm64-macos
if [ ! -d "$SDK" ]; then
  curl -sSL "https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-$SDK_VER/wasi-sdk-$SDK_VER.0-arm64-macos.tar.gz" | tar xz
  xattr -dr com.apple.quarantine "$SDK" 2>/dev/null || true
fi
SRC=$PWD/php-$PHP
if [ ! -d "$SRC" ]; then
  curl -fsSL "https://www.php.net/distributions/php-$PHP.tar.gz" | tar xz
  [ -d "$here/patches/$LINE" ] || { echo "no patches for PHP $LINE: add packs/php/patches/$LINE/" >&2; exit 1; }
  for p in "$here"/patches/"$LINE"/*.patch; do patch -d "$SRC" -p1 < "$p"; done
fi

export CC="$SDK/bin/clang --target=wasm32-wasip1 --sysroot=$SDK/share/wasi-sysroot"
export AR=$SDK/bin/llvm-ar RANLIB=$SDK/bin/llvm-ranlib NM=$SDK/bin/llvm-nm
# Emulated signals, mman, getpid and process clocks come from wasi-libc; setjmp/longjmp
# (PHP's error bailout) compile to Wasm exceptions.
export CFLAGS="-O2 -I$here/shim -include $here/shim/posix_stubs.h -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_MMAN -D_WASI_EMULATED_GETPID -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false -mexception-handling"
export LDFLAGS="-lwasi-emulated-signal -lwasi-emulated-process-clocks -lwasi-emulated-mman -lwasi-emulated-getpid -lsetjmp"

cd "$SRC"
if [ ! -f Makefile ]; then
  ./configure --host=wasm32-unknown-wasi --build="$(sh ./build/config.guess)" \
    --enable-embed=static --disable-all --disable-cli --disable-cgi --disable-phpdbg --without-pear \
    --disable-fiber-asm --disable-opcache --without-pcre-jit --disable-zend-signals --without-valgrind \
    --without-iconv --without-libxml --without-sqlite3 --disable-zend-max-execution-timers > "../configure-$PHP.log"
  # WASI emulates only ru_utime and ru_stime.
  sed -i '' 's|^#define HAVE_GETRUSAGE 1|/* #undef HAVE_GETRUSAGE */|' main/php_config.h
fi
make -j"$(sysctl -n hw.ncpu)" libphp.la > "../make-$PHP.log" 2>&1 || { grep -m20 "error:" "../make-$PHP.log"; exit 1; }


# The pack: pack.c (the ABI) and the POSIX stubs, linked with libphp as a WASI reactor.
cd "$here"
OUT=target/php-$LINE
mkdir -p "$OUT"
(cd src && xxd -i prelude.php) > .build/prelude.h
INC="-I$SRC -I$SRC/main -I$SRC/Zend -I$SRC/TSRM -I$SRC/sapi/embed -I$here/.build"
# PHP recurses deeply (the compiler, nested calls): 8 MiB of Wasm stack.
$CC $CFLAGS -D_GNU_SOURCE -DHOST_API="$(cat ../HOST_API)" $INC -mexec-model=reactor -Wl,-z,stack-size=8388608 \
  src/pack.c shim/posix_stubs.c "$SRC/.libs/libphp.a" $LDFLAGS -lm -o "$OUT/php_pack.wasm"
echo "$PHP" > "$OUT/VERSION"
echo "built $OUT/php_pack.wasm, PHP $PHP ($(du -h "$OUT/php_pack.wasm" | cut -f1))"
if [ "$PHP" = "$DEFAULT" ]; then
  cp "$OUT/php_pack.wasm" target/php_pack.wasm
fi
