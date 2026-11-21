# Contributing

Short version: open an issue or a Discussion first if it's more than a typo.
We're four people, so a PR we can't review costs us more than the fix is worth.

## Building

```bash
cargo build --release
./target/release/aero run examples/hello.aero
cargo test
```

Rust 1.97+, LLVM 22 (`LLVM_SYS_221_PREFIX` must point at it), and on Windows a
MinGW UCRT64 `gcc` on PATH for AOT linking. If `cargo build` fails on
`llvm-sys`, that's the LLVM variable, not your toolchain.

## Sending a patch

1. Fork, branch off `main`, keep the branch to one thing.
2. `cargo test` must pass. If you touched codegen, run `aero build` on an
   example too — the JIT path and the AOT path resolve symbols differently and
   CI has caught things that only broke one of them.
3. Run `aero fmt` on any `.aero` file you add, and `cargo fmt` on Rust.
4. Open the PR with a line about **what you actually observed**, not what you
   assume. "Panics on `aero build` when the path has a space" is useful.
   "Improves robustness" is not.

Commit subject: lowercase, imperative, one line.

```
fix(parse): allow omitted trailing semicolon in closure body
hir: reject closures that capture before codegen
```

## AI-generated code

We use AI-assisted tooling ourselves, so this is not a ban.

What we require:

- You can explain every line you submit, including why it's placed where it is.
- You ran it. Not "it should work" — you compiled it and looked at the output.
- It doesn't invent APIs. `aero-std` is small; check the function exists before
  calling it. Hallucinated builtins are the single most common failure we see.
- New public API gets a test. New error messages get an error-path test, not
  just the happy path.

We reject patches that look generated and unverified. We don't reject patches
because they were generated.

## What we're not taking right now

- New language features. 1.2.x is about fixing what's broken; 1.2.6 is a
  fixes-only release. Proposals are welcome in Discussions, not as PRs.
- New target platforms. Four supported targets that work beats eight that
  mostly work.
- Refactors of `crates/aero-ir/src/codegen.rs`. It's the largest file in the tree and
  we'd rather land fixes than churn it.
- Formatting-only PRs.

## Reporting a bug

Open an issue and include `aero --version`, the full error output (paste it, no
summary), and a `.aero` file small enough to read. If we can't reproduce it in
five minutes it gets labelled `needs-info`, and closed after a week — not
because we're ignoring you, but because an unreproducible issue just sits there.
