# Experimental x86 Metal user-client bridge

This Lilu plugin translates the stock IOGPU-family Metal plugin's requests
to the x86 IOAccelerator-family kernel driver. It is under development:
existing Safari 27 and Chrome 155 have passed all 22 GPU WebGL acceptance stages in the disposable lab. Performance and broader qualification remain pending.
Do not install it in the production VM.

The current target is Tahoe 26.4 (25E246), AppleParavirtGPU 15.0.0 and
AppleParavirtGPUMetalIOGPUFamily 64.4.7, on Reims PCI `106b:eeee`.
The transport returns actual legacy resource IDs, compiler information and
GPU completions. It does not override GPU-family queries or emulate a
successful draw. Untranslated requests fail explicitly.

## Build and check

Build in the disposable macOS guest, using Lilu 1.7.2 source
(`e4748cc081bf060302c7d3c44a643ce1d11b7e1d`) and MacKernelSDK
`3f750085caa17ec3a7880f11c11bf4f48cd6a164`:

```sh
clang++ -std=c++17 -Wall -Wextra -Werror abi-test.cpp -o /path/to/lab/abi-test
/path/to/lab/abi-test
bash build.sh /path/to/Lilu /path/to/MacKernelSDK /path/to/lab/build
clang -fobjc-arc -framework Foundation -framework Metal resource-probe.m -o /path/to/lab/resource-probe
clang -fobjc-arc -framework Foundation -framework Metal compute-probe.m -o /path/to/lab/compute-probe
```

Keep the compiled kext and private driver captures outside Git. Install
the authored kext after Lilu in a separate OpenCore image, restricted to
kernel `25.4.0`. Preserve the original image and all other settings. SIP and
AMFI remain enabled. Without `-reimsmetal`, only the diagnostic connection
type `0x524d0005` is enabled and ordinary apps continue using the old driver.
`-reimsmetal` selects the newer plugin for the existing Safari WebKit GPU
and Chrome GPU helper processes. Registry serialization and connection
routing use the full process name (`proc_selfname`); `proc_name` truncates
these helper names. Other processes, including WindowServer, retain the
legacy plugin. Native authored probes use the diagnostic connection;
`-reimsmetaloff` disables this Lilu plugin. Neither setting is a production
recommendation until the cold boot and browser checks pass.

## Measured progress, 2026-10-15

- CPU-only record translation checks pass, including truncated metadata,
  invalid resource IDs and 64-bit dirty-buffer ranges.
- The kernel bridge allocated and released 3,200 buffers and 100 textures;
  native source compilation and pipeline creation passed.
- Seven buffers over 100 GPU frames passed every 64-value readback.
- Four concurrent queues over 100 frames passed ten consecutive runs after
  serializing completion delivery. The original driver passed the same test.
- Native ANGLE Metal clear, shader, triangle, texture, FBO and readback passed.
- Global activation exposed additional WindowServer calls: memoryless
  texture creation, fences and dirty-resource notifications. Creation of the
  first two now passes; global display activation remains under investigation.
- Managed buffer upload, memoryless MSAA resolve and managed texture upload
  now pass exact GPU readback. The translator handles multiple encoder
  resource lists and variable-length mipmapped texture records.
- A memoryless/managed texture reuse probe passes 100 rounds, sampling the
  managed texture into another render target and checking all 256 pixels.
- Resource-list sizes are validated as rounded-up KiB values; the legacy
  submission receives the exact allocation length. Synthetic boundary and
  overflow checks pass, as does GPU/CPU readback of a 16x17 IOSurface.
- Shared-event signal and infinite wait commands pass a native GPU probe.
  Converted commands use owned shared memory held until actual completion.
- Stock Safari passes WebGL 1 context, shaders, triangle, texture upload,
  FBO, blend and depth checks. Continuous drawing still loses the context;
  this is not browser acceptance.

The dirty-texture notification initially blocked resource release, causing
WindowServer to reuse IDs still held by the kernel. Forwarding its mip/slice
update to the legacy dirty ring removed that failure. The subsequent global
boot reached a blended framebuffer-fetch pipeline refused by the Windows
runtime. An experiment advertising framebuffer reads as unavailable instead
made guest rendering bind memoryless textures for shader reads, killing the
host. That capability change was withdrawn; the candidate uses the original
host library while browser-scoped driver translation is investigated.
Both stock browsers now pass context creation, shaders, triangle/VBO, textures, FBO/readback, blend/depth, 180 frames per API, resource lifetime, loss/recovery and context stability. This does not establish 60 FPS or validate other browsers.

Completion delivery must run outside the legacy callback's driver locks.
The thread call must also use `THREAD_CALL_OPTIONS_ONCE`: otherwise two
drainers can deliver scheduled/completed notifications out of order.
CPU aliases of existing device-pager memory use `kIOMemoryHostOnly`, without
attempting to DMA-wire the same pages again (`prepare()` returned unsupported).

Remaining qualification includes mip/slice preservation and
resource cleanup under failure, WindowServer, both stock
browsers, guest restart and a matched TestUFO performance comparison.
