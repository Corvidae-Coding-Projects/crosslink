# Accelerator verification commands

Adapt package and feature names to the workspace.

## Environment

```bash
nvidia-smi
rustc --version
cargo metadata --no-deps --format-version 1
cargo build -p <gpu-crate> --features <backend>
cargo test -p <gpu-crate> --features <backend> --no-run
```

## Backend evidence

```bash
rg -n "launch|dispatch|cublas|cudarc|cubecl|wgpu|ptx|wgsl" <changed-paths>
rg -n "to_cpu|to_vec|as_slice|copy.*host|synchronize" <changed-paths>
```

## Incomplete paths

```bash
rg -n "todo!|unimplemented!|TODO|FIXME|panic!" <changed-paths>
rg -n "ignore|cfg_attr.*ignore|should_panic" <test-paths>
```

## Device and numerical tests

```bash
cargo test -p <gpu-crate> --features <backend> <operation> -- --nocapture
cargo test -p <autograd-crate> --features <backend> <operation> -- --nocapture
```

Tests should assert output device, shape, dtype, values within justified tolerances, boundary shapes, and gradients when applicable.

## Final review

```bash
git diff --check
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Record which commands executed on real hardware and which only compiled the backend.
