# SEGV

This branch exists only to investigate a segmentation fault.

With certain input corpus there is a SIGSEGV for `PROFILE=dev just run`.

```
qemu_cmin: QEMU internal SIGSEGV {code=MAPERR, addr=0x7fd96c725008}
```

Unfortunately ASAN doesn't provide any info about it:

```
=================================================================
==668407==ERROR: LeakSanitizer: detected memory leaks

Direct leak of 80 byte(s) in 1 object(s) allocated from:
    #0 0x7fd58ca8477b in __interceptor_strdup ../../../../src/libsanitizer/asan/asan_interceptors.cpp:439
    #1 0x561c75602e34 in parse_args ../linux-user/main.c:687
    #2 0x561c75602e34 in _libafl_qemu_user_init ../linux-user/main.c:762

Direct leak of 16 byte(s) in 1 object(s) allocated from:
    #0 0x7fd58cacc3b7 in __interceptor_calloc ../../../../src/libsanitizer/asan/asan_malloc_linux.cpp:77
    #1 0x561c754bedfc in libafl_qemu_set_breakpoint ../libafl/exit.c:25

SUMMARY: AddressSanitizer: 96 byte(s) leaked in 2 allocation(s).
```
