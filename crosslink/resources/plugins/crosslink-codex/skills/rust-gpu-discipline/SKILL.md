---
name: rust-gpu-discipline
description: "Implement and review Rust GPU work with verified device execution, explicit fallback policy, and backend-specific tests."
---

# Rust GPU delivery

Use this with Rust work involving CUDA, ROCm, Metal, Vulkan, WGPU, CubeCL, cudarc, tensor operations, kernels, device movement, or autograd on accelerators.

## Probe the environment

Run available hardware and toolchain probes before planning. Check device visibility, drivers, toolkit paths, workspace feature flags, backend crate compilation, and test buildability. Record observed results rather than assuming hardware is absent.

## Map execution

For every operation, identify input device, allocation location, dispatch API, kernel or library primitive, synchronization, output device, error path, and gradient implementation. Name the files and crates involved.

## Implement the real backend

The accelerated path must launch backend work and keep data on the intended device. A host implementation behind a GPU-shaped API, a dispatch wrapper with no kernel, a disabled test, or a placeholder branch is incomplete.

Fallback behavior must match the project’s documented device contract. Unsupported device operations should return a clear error unless the library explicitly promises automatic fallback. Never copy to the host silently to make a test pass.

## Verify mechanically

Search the changed call graph for backend dispatch evidence, host transfers, synchronization, stubs, feature gates, and ignored tests. Build the relevant feature combinations. Run device assertions and numerical comparisons against an independent reference when hardware is present. When it is absent, compile the full path and report runtime verification as unavailable.

Review allocation lifetime, stream or queue ordering, bounds, dtype and shape behavior, launch dimensions, error propagation, and autograd device continuity.

## Report

State the backend exercised, hardware observed, commands run, device evidence, numerical tolerances, unsupported configurations, and anything not executed. Do not describe compile-only evidence as a device test.

Use `anti-patterns.md`, `ferrotorch-stack.md`, and `verification-script.md` for focused checklists.
