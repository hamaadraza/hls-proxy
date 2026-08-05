# BoringSSL (via wreq -> boring-sys2) assembles its crypto with NASM on Windows.
# boring-sys2 only auto-disables that when cross-compiling -- its build script
# returns early when host == target -- so a native Windows build fails with
# "No CMAKE_ASM_NASM_COMPILER could be found" unless NASM is installed.
#
# Assembly is preferred and is used whenever it can be: if NASM is on PATH this
# file changes nothing. Only when NASM is missing do we fall back to BoringSSL's
# portable C backend, which costs raw AES/SHA throughput but does NOT change the
# TLS JA3/JA4 fingerprint -- that comes from cipher suites, extensions and
# HTTP/2 settings, not from how the crypto is compiled.
#
# Non-Windows hosts always have a working assembler, so this file is a no-op
# there and Linux/macOS builds keep the optimized backend.
#
# To get the assembly build on Windows: `choco install nasm` (needs an elevated
# shell), reopen the terminal, then `cargo clean -p boring-sys2` and rebuild.
# A stale CMakeCache.txt will otherwise keep the previous decision.
if(CMAKE_HOST_WIN32)
  find_program(HLS_PROXY_NASM NAMES nasm)

  if(HLS_PROXY_NASM)
    message(STATUS "hls-proxy: NASM found (${HLS_PROXY_NASM}); building BoringSSL with assembly")
  else()
    message(STATUS "hls-proxy: NASM not found; building BoringSSL without assembly "
                   "(slower crypto, identical TLS fingerprint)")
    set(OPENSSL_NO_ASM YES CACHE BOOL "Build BoringSSL without NASM assembly" FORCE)
  endif()
endif()
