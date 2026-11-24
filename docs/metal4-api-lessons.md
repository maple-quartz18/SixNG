# Metal 4 API Porting Lessons

## Scope

These lessons apply to macOS implementations using Metal 4, MetalFX, `CAMetalLayer`,
and the safe Rust facade in Metal-Rust. They are based on the SixNG Metal I/O,
temporal-upscaling, and frame-generation work, including failures found during
GPU validation.

## Dependency and source-of-truth rules

1. Pin Metal-Rust to an audited commit in the workspace root. A dependency
   update must be reviewed, built, and tested as part of the porting change.
2. When an API is missing, incorrectly bound, or thread unsafe in Metal-Rust,
   fix that API in the Metal-Rust fork first. Then pin SixNG to the reviewed
   revision.
3. Do not add `unsafe`, raw Objective-C pointers, or unsafe `Send`/`Sync`
   implementations to SixNG as a workaround. Apple objects should remain
   thread-bound unless their complete public API has been audited.
4. The metal-cpp/SDK header is the source of truth for names and signatures.
   Generated bindings must preserve availability checks, ownership, finite
   values, dimensions, ranges, and enum validation.
5. Keep separate declarations for Metal 3, Metal 4, and MetalFX types. A
   command-buffer protocol change is an API change, even when Objective-C
   selector names overlap.

## Metal 4 lifecycle

1. Create reusable `MTL4CommandBuffer` objects from the device, not the
   command queue. Create an allocator per in-flight frame and reset it only
   after GPU work using it has completed.
2. Recording always follows begin/end explicitly. Never commit an open command
   buffer, begin the same buffer twice, or reuse an allocator across open
   command buffers.
3. Use `ResidencySet` allocation for resources. Command buffers do not retain
   all resource lifetimes for the application.
4. Removing resources or releasing old target textures while they are
   in-flight can cause GPU faults or validation failures. Drain or otherwise
   prove completion before deferred removal.
5. Use the queue for batch submission and completion feedback. Do not port
   `MTLCommandBuffer.addCompletedHandler` as if it were equivalent to Metal 4
   queue feedback.

## Resource binding

1. Metal 4 removed per-encoder `setBytes` and most per-encoder resource
   setters. Use `MTL4ArgumentTable` for buffers, textures, samplers, and
   resource IDs.
2. Bind:
   - buffer GPU addresses for buffer pointers;
   - resource IDs for textures, samplers, tensors, and acceleration structures;
   - the same table rules to all relevant render stages.
3. Size argument tables for the broadest shader set they can serve. A missing
   binding is a runtime/validation error, not a safe fallback.
4. Keep the backing resident while referenced. Commit residency additions before
   encoding and remove allocations only after the GPU finishes.
5. Prefer private storage for GPU-only textures and shared storage only for
   CPU-written or readback buffers/textures. Do not restore managed storage.

## Synchronization

1. Metal 4 is explicit. Do not rely on driver hazard tracking.
2. Use the narrowest synchronization primitive:
   - barriers for queue/encoder stage dependencies;
   - fences for precise dependencies on the same queue;
   - events for cross-queue GPU work;
   - shared events only when CPU or cross-device participation is required.
3. Use `MTL4VisibilityOptionDevice` for write-to-read/write transitions.
4. Interleave blit and compute carefully: a copy/previous-frame texture update
   that follows interpolator encoding must be ordered with an appropriate
   Dispatch-to-Blit barrier or split submission.
5. Do not put Fragment work in encoder-stage after-stages on a TBDR render
   encoder. Split passes and use inter-pass producer/consumer barriers instead.
6. In a normal real-frame render path, retain submission waits only where
   readback or tests require completion. Do not make every scene frame block on
   completion in an interactive presentation loop.

## Drawable presentation

1. Configure `CAMetalLayer` once for the engine's needs:
   - backing pixel format;
   - framebuffer-only mode;
   - three drawables only when triple buffering is intentional;
   - display-sync mode;
   - drawable size updated on resize.
2. Metal 4 drawable flow is:
   - acquire with `nextDrawable`;
   - encode;
   - `waitForDrawable`;
   - queue submit;
   - `signalDrawable`;
   - `drawable.present`.
3. Never hold drawable references longer than necessary. Present immediately
   after queue signaling.
4. Keep the layer residency set on the queue or command buffer. Do not add or
   remove allocations from this read-only layer residency set.
5. For live resize, update the drawable size only after in-flight work has
   drained. Guard zero width and height on every pre-layout call.
6. Do not use `presentDrawable:` on Metal 4 recording command buffers.

## Temporal upscaling

1. Jitter belongs consistently in both shader clip space and MetalFX
   pixel-space properties. A sign mismatch produces screen shake or crawl.
2. Motion-vector scale must match the render resolution and the NDC convention.
   The common checked values are approximately half the render width/height for
   normalized motion data, but verify against the pipeline's actual convention.
3. Track current and previous view-projection matrices. The previous matrix must
   be updated after real-frame submission, not after a presentation-only slot.
4. Keep the previous frame in an explicitly resident private texture.
5. Reset temporal history on mode, resize, discontinuity, and scene reset.
6. At minimum, produce a static-camera diagnostic where motion vectors are
   uniform and no geometry moves.

## Frame interpolation

1. Frame interpolation is based on Temporal Upscaling outputs. Do not enable it
   if temporal scaling is unavailable or if its real/upscaled texture contract
   is incomplete.
2. Separate two concepts:
   - real-render slot: render scene, temporal upscale, and encode interpolation;
   - presentation phase: present real or interpolated output.
3. For 60 Hz real/60 Hz interpolated presentation:
   - render on only the real slot;
   - interpolate on the real slot;
   - present interpolated and real frames in the correct temporal order;
   - never count fallback real presentation as interpolation.
4. Preserve `colorTexture`, `prevColorTexture`, depth, motion, jitter, motion
   scale, camera planes/FOV/aspect, delta time, reset state and UI policy.
5. The first real frame and resize/start history reset must not read an
   uninitialized previous texture. Fallback to real presentation until the
   interpolation texture is valid.
6. The interpolated texture and previous-texture copy must not race. Add explicit
   synchronization or split submissions before copying the texture consumed by
   the frame interpolator.
7. Do not hardcode 120 Hz. Query the active monitor refresh rate and disable
   frame generation when the display cannot sustain at least 120 Hz.

## Metal-Rust rules

1. Add missing Metal 4/MetalFX APIs to Metal-Rust before modifying SixNG if the
   API does not already exist in the pinned safe facade.
2. Pin one exact fork revision in workspace dependencies and update both
   `metal-rust` and `metal-rust-ffi` together.
3. Never expose raw Objective-C objects or pointers in public Metal-Rust APIs.
4. Use checked errors and capability queries. Optional frameworks must weak-load
   safely and return `Unsupported`, not crash or send selectors to missing
   classes.
5. Keep wrappers thread-bound unless a complete Apple-thread-safety audit exists.
   Do not add unsafe `Send`/`Sync` implementations merely to cross a channel.
6. Generated and handwritten facades must agree. After changing inherited
   properties or object generation, run:
   - generated object-type check;
   - generated facade check;
   - API coverage check;
   - safety check;
   - cargo tests.

## Failure fallback

1. OS or GPU Metal 4 capability failure disables only the MTL4 path.
2. MetalFX descriptor, compiler, interpolator, temporal scaler or target
   allocation failure disables frame generation or temporal scaling
   independently; do not strand the native MTL4 renderer.
3. WGPU remains a complete fallback.
4. Low refresh/unknown refresh disables frame generation. Count only real
   textures that are ready and only interpolation slots that select an actual
   interpolated texture.
5. Zero-size resize returns before texture allocation or presentation.
6. Log fallback reasons once or on state change, not every frame.

## Validation checklist

### Build

- `cargo build --release --locked`
- `cargo test --release --locked`
- MTL4 feature target and example compile
- no unreviewed generated/stale files

### Metal validation

Run before the process creates a device:

```sh
MTL_DEBUG_LAYER=1 \
MTL_SHADER_VALIDATION=1 \
MTL_SHADER_VALIDATION_REPORT_TO_STDERR=1 \
cargo test --locked -p sixng-gui --features metal4-poc
```

Check for no API or GPU validation errors, no red/white load/store diagnostics,
and no shader memory violations.

### Runtime

- startup on macOS 26+ with an M6 or supported GPU;
- 60 Hz native MTL4 mode;
- 120 Hz display gate;
- first-frame interpolation history;
- resize and resize-zero;
- static camera;
- pure translation;
- pure rotation;
- UI compositing;
- mode switch and unsupported capability fallback;
- Metal HUD 8.33 ms frame interval in 120 Hz mode;
- real/interpolated/fallback counters remain coherent.

### Visual

When screenshot permission is available, capture the app window rather than the
whole desktop. Verify:

- scene is visible and not blank;
- no obvious tearing or temporal reversal;
- moving objects do not leave heavy ghosting;
- UI remains sharp and correctly composited;
- resize/camera changes do not leave stale textures.
