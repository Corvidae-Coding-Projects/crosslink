# Ferrotorch accelerator map

Verify these names against the current workspace before relying on them.

## Crates and responsibilities

Locate the tensor abstraction, backend traits, CUDA or portable backend crate, JIT representation, autograd engine, and allocator. Keep public tensor behavior independent from a specific low-level library.

## Device-aware tensors

Trace device identity and storage ownership from construction through views, operations, and gradients. New operations must preserve dtype, shape, strides, and device according to the tensor contract.

## NVIDIA backend

Use the workspace’s cudarc device and allocation types. Prefer cuBLAS or another established library for supported dense linear algebra. Use compiled PTX or a project-approved kernel source for custom elementwise and fused work. Launch on the correct stream and translate backend errors without losing their cause.

## Portable backend

For CubeCL or WGPU paths, define kernel inputs and outputs explicitly, compute safe launch dimensions, validate bounds inside kernels, and keep backend-specific details behind the project abstraction. Confirm feature flags compile on their supported targets.

## JIT path

Fusion belongs in the intermediate representation and code generator rather than ad hoc eager branches. Eager and generated execution must agree on shape, dtype, device, and numerical behavior.

## Memory

Integrate with the caching allocator and lifetime guard used by the workspace. Do not free storage while queued work still refers to it. Avoid hidden synchronizations introduced only to simplify ownership.

## Autograd

Save the minimum device-resident state needed by backward. Implement gradients using the same device contract and test both values and returned device.

## End-to-end addition

Add the public operation, backend trait entry, backend implementation, dispatch, shape and dtype validation, autograd rule, feature wiring, and reachable tests. Verify both successful device execution and an actionable unsupported-backend error.
