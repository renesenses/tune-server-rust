# Native audio ABI 1

Supported targets are the host's exact Rust target triple. ABI 1 uses `extern C`, `repr(C)`, fixed width integers, opaque handles and pointer/length buffers. Rust `String`, `Vec`, trait objects, references and allocator ownership never cross the boundary. The native libraries are **trusted code in the server process**, not a security sandbox. Signatures prove the configured publisher; they cannot make arbitrary native code safe. Only `dev` executes explicitly requested local builds without a signature.

The entry is `tune_audio_plugin_v1() -> *const Api`. Check its non-null pointer and initial `u32 size` before accessing the full table; check version, kind, manifest SDK and required capabilities before setup. Sizes must match exactly. Changing a layout, error meaning, encoding or ownership rule requires an ABI bump and a new entry point. A release package declares ABI and target separately from the implementation's source manifest.

## Ownership and threading

- `Api` is immutable static library data. A host `Arc<Library>` pins it until every processor/job is destroyed. Updates swap the on-disk activation pointer, apply at startup and retain the previous version. No unload while code runs.
- Every request and input buffer remains alive for the synchronous call, aligned for its actual sample type. Inputs are borrowed, never freed by the receiver. No pointer or callback may be retained after return.
- CREATE returns one opaque instance handle. PROCESS/UPDATE/RESET/DESTROY require exclusive access. INHERIT borrows a distinct, quiescent previous handle from the same library. Destroy once with the originating library. Independent instances can run concurrently.
- DSP buffers are complete interleaved frames, bounded by prepared capacity. S16/S32/F32/F64 buffers have native scalar representation; packed S24 is little endian. Tune's byte adapter handles little endian storage explicitly. Current EQ/crossfeed refuse F64; the ABI can represent it for future processors.
- `Buffer` replies are owned by their producer. Plugin replies use `Api.free`; host callback replies use `HostApi.free_reply`. Free exactly once, including parse failures. Do not free borrowed request/PCM buffers.
- RUN_BATCH owns a stack-lived HostApi and synchronized per-job context. Reader handles are scoped to that job. Worker threads must join and drop readers before RUN_BATCH returns. Host callbacks serialize mutable BatchHost access, limit open readers and validate frame counts. Cancellation is checked between blocks and before publication.
- Host and plugin catch Rust unwinding panics at their entry boundaries and return code 255. Aborts, segmentation faults and foreign UB cannot be recovered in-process. Never unwind through C.

## Lifecycle and diagnostics

Prepare allocates state/scratch on the producer/control thread. Process does no codec, filesystem, licence or network work. These compatibility stages run in Tune producers, not device callbacks. A native failure returns an error; it is never retried using another algorithm over partially processed audio. The compatibility facade reports failure and preserves the failing scratch block. Successfully completed earlier blocks remain processed.

Hot replacement inherits filter/ring/dither history only within the same provider and format. Track/seek/format discontinuities use reset or a new processor. Source observations are independent of DSP instances. Cumulative clipping details cross as scalars, including peak IEEE-754 bits; Tune folds their deltas into the host registry so diagnostics do not disappear into a library-local static.

Errors: 1 invalid format, 2 incomplete frame, 3 capacity exceeded, 4 unsupported format, 5 invalid settings, 6 non-finite, 7 cancelled, 8 missing capability, 9 invalid state, 10 invalid observation, 11 host failure, 12 silence-only, 255 panic. Unknown codes become host failure. A successful call is 0. Sizes and discriminants are checked before processing; valid pointers/handles remain obligations of the trusted caller.

## Qualification

`python sdk/scripts/verify_native.py` builds and actually loads all four cdylibs, processes floating and integer PCM, transfers history, drops the external library owner, calls real batch callbacks, copies metadata and cancels inside a file. `verify_dsp_parity.py` compares historical, extracted and native-facade bytes. Signed package tests cover install/update/rollback, target mismatch, altered signatures/files, wrong feature slot, unsafe paths and preserved activation on refusal. Run these on Linux, macOS and Windows; cross compilation alone does not validate loading or device runtime.
