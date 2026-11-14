# Who works on Aero

Four people, plus a lot of AI. Three of us don't use public accounts, so this
file says what each person covers instead of who they are.

- **language and compiler core** — lexer, parser, HIR, type inference, borrow checker
- **code generation and toolchain** — LLVM IR, JIT, AOT, cross targets, CI on four platforms
- **interop and packages** — C FFI, Python extensions, C++ bindings, package manager
- **docs and community** — the book, the docs site, release notes, Discussions

Everything lands from the shared `SereinCin` account, which is why the commit
graph looks like one person. If you see `AeroStudio-CN` in the history, that's
the same team under a different git identity — a config mistake we haven't
rewritten.

Roughly 55% of the code is AI-assisted (scaffolding, boilerplate, first drafts),
45% is design, review, debugging and verification by the four of us. Details in
the README.
