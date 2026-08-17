# GPU implementation failure patterns

## Host work presented as device work

An API accepts a GPU tensor, copies it to host memory, computes on the CPU, and copies the result back. Device-shaped naming does not make this accelerated.

## Deferred kernel

Dispatch, traits, or feature wiring are added while the backend body remains missing. Infrastructure without executable compute does not complete the operation.

## Feature-hidden placeholder

The advertised accelerator feature compiles only because its branch returns a constant, panics, or is excluded from normal tests.

## Reference path used as production

A CPU reference implementation is useful for numerical comparison but cannot stand in for the requested backend.

## Empty dispatch

The code selects a device backend but ultimately calls the same host routine for every branch.

## Gradient detour

Forward execution stays on the device while backward moves tensors to the host or returns gradients on the wrong device.

## Unnecessary readback

Device results are synchronized and copied to host inside a hot operation when the next consumer could remain on device.

## Missing reachable tests

Tests exist but require an undocumented feature, are ignored, never assert the output device, or do not reach the new dispatch.

## Shape shortcut

Only the convenient shape, stride, dtype, or batch case is implemented while the API accepts broader input without validation.

## Invented environmental limitation

Hardware or tooling is declared unavailable without running the local probes. Verification constraints must be observed, not guessed.

The shared pattern is a mismatch between the claimed execution surface and the code path that actually runs. Follow allocations and dispatch through to the backend before accepting the claim.
