# Agent guidelines

blooter is a small Rust Bluetooth HID emulator (see `README.md` and `design/ARCH.md`). Keep it small.

## Style

- **Succinctness first.** Prefer the shortest clear implementation. No speculative abstractions, config knobs, or "just in case" code paths.
- **Reuse before writing.** Before adding a function or type, look for existing code (here or in a current dependency) that already does the job, and factor out shared logic rather than duplicating it.
- **Efficiency matters.** This is a low-level input-forwarding daemon: avoid needless allocations, copies, and spawned tasks on the hot path (input event → HID report → L2CAP send).

## Dependencies

- Add a dependency only when it clearly beats writing the code in-tree.
- When one is needed, pick the **smallest** crate that does the job: fewest transitive deps, `default-features = false` plus only the features actually used (match the existing `Cargo.toml` style).
- The release profile is tuned for binary size (`opt-level = "z"`, LTO, strip); don't pull in anything that bloats it.

## Verifying

- `cargo build` must be warning-free; run `cargo clippy` and `cargo fmt` before finishing.
- Behavior is documented in `design/ARCH.md` — keep it in sync with changes.
