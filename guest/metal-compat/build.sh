#!/bin/bash
set -euo pipefail
if [[ $# != 3 ]]; then
    echo "usage: build.sh Lilu-source MacKernelSDK-source output-directory" >&2
    exit 2
fi
source_dir=$(cd "$(dirname "$0")" && pwd)
lilu=$(cd "$1" && pwd)
kernel_sdk=$(cd "$2" && pwd)
mkdir -p "$3"
output=$(cd "$3" && pwd)
test -f "$lilu/Lilu/Headers/kern_api.hpp"
test -f "$kernel_sdk/Headers/IOKit/IOUserClient.h"
bundle="$output/ReimsMetalCompat.kext"
if [[ -f "$bundle/Contents/Info.plist" ]]; then
    test "$(/usr/libexec/PlistBuddy -c 'Print CFBundleIdentifier' "$bundle/Contents/Info.plist")" = io.reims.MetalCompat
fi
mkdir -p "$bundle/Contents/MacOS" "$output/objects"
flags=(-arch x86_64 -mmacosx-version-min=10.13 -mkernel -fno-builtin
       -fno-common -fno-asynchronous-unwind-tables -DKERNEL -DKERNEL_PRIVATE
       -DDRIVER_PRIVATE -DLILU_KEXTPATCH_SUPPORT -DDEBUG=1 -DPRODUCT_NAME=ReimsMetalCompat -DMODULE_VERSION=0.1.0
       -Wall -Wextra -Werror -Wno-unused-parameter -Wno-deprecated-declarations
       -isystem "$kernel_sdk/Headers" -isystem "$lilu/Lilu")
cxxflags=(-std=c++17 -fno-rtti -fno-exceptions -fapple-kext)
xcrun clang++ "${flags[@]}" "${cxxflags[@]}" -c "$source_dir/ReimsMetalCompat.cpp" -o "$output/objects/compat.o"
xcrun clang++ "${flags[@]}" "${cxxflags[@]}" -c "$lilu/Lilu/Library/plugin_start.cpp" -o "$output/objects/start.o"
cat >"$output/objects/kmod.c" <<'EOF'
#include <mach/mach_types.h>
#include <mach/kmod.h>
extern kern_return_t _start(kmod_info_t *, void *);
extern kern_return_t _stop(kmod_info_t *, void *);
extern kern_return_t ReimsMetalCompat_kern_start(kmod_info_t *, void *);
extern kern_return_t ReimsMetalCompat_kern_stop(kmod_info_t *, void *);
KMOD_EXPLICIT_DECL(io.reims.MetalCompat, "0.1.0", _start, _stop)
__private_extern__ kmod_start_func_t *_realmain = ReimsMetalCompat_kern_start;
__private_extern__ kmod_stop_func_t *_antimain = ReimsMetalCompat_kern_stop;
__private_extern__ int _kext_apple_cc = __APPLE_CC__;
EOF
xcrun clang "${flags[@]}" -c "$output/objects/kmod.c" -o "$output/objects/kmod.o"
xcrun clang++ -arch x86_64 -nostdlib -Wl,-kext -Wl,-undefined,dynamic_lookup \
    "$output/objects/compat.o" "$output/objects/start.o" "$output/objects/kmod.o" \
    -L"$kernel_sdk/Library/universal" -lkmod -lkmodc++ \
    -o "$bundle/Contents/MacOS/ReimsMetalCompat"
cp "$source_dir/Info.plist" "$bundle/Contents/Info.plist"
plutil -lint "$bundle/Contents/Info.plist"
codesign --force --sign - "$bundle"
codesign --verify --strict "$bundle"
file "$bundle/Contents/MacOS/ReimsMetalCompat"
