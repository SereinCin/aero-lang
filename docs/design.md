# Aero Language Core Design Whitepaper (v1.0)

**Audience:** Aero compiler R&D team, standard library team, low-level architects

**Core mission:** Build a modern systems language that combines Python's development
efficiency with C++'s extreme performance, with native support for AI computing.

## Core Philosophy — the "Three Never" Principles (baseline for everyone)

Before writing a single line of code, every developer must keep Aero's "Three Never"
principles in mind:

1. **Never introduce a traditional garbage collector (GC).** Our performance floor is
   C++ level. Any memory-management mechanism that causes uncontrollable runtime pauses
   (stop-the-world) is rejected outright.
2. **Never do at runtime what can be done at compile time.** Type inference, memory
   layout, even parts of macro expansion must be flattened during front-end parsing.
3. **Never reinvent low-level wheels.** We do not write our own machine-code generator;
   we fully embrace LLVM / Cranelift infrastructure.

## Architecture Directives for the Compiler R&D Team

The R&D team's core task is building Aero's front-end parser and IR conversion layer.

- **Front-end (Parser) requirements:** Implement an extremely powerful type-inference
  engine. When a user writes `let x = 1 + 2`, the compiler must infer a 64-bit integer
  within 0.1 ms, and reject any implicit dangerous type conversion.
- **IR (Intermediate Representation) extension:** Standard LLVM IR is not enough. The
  R&D team must build a custom Aero-Tensor IR on top of LLVM. This IR must directly
  describe multi-dimensional matrix operations and GPU memory scheduling — the lifeline
  for Aero's future AI compute.
- **Memory model:** Implement scope-based arena allocation. When a code block ends, the
  compiler must emit instructions that instantly clear that block's memory, with zero
  runtime overhead.

## Ecosystem Directives for the Standard Library Team

The standard library team's core task is making Aero genuinely useful in the real world
and seamlessly integrating with the existing ecosystem.

- **FFI (Foreign Function Interface) is the top priority:** Aero must not be an island.
  The team must first open the interop channel between Aero and C/C++ headers
  (.h / .hpp). Aero must be able to call Linux POSIX APIs and Windows Win32 APIs
  directly, with no glue code.
- **AI-native runtime bridge:** Build a low-level communication bridge so Aero tensor
  data can be pushed directly into CUDA / ROCm environments. Users writing AI
  algorithms in Aero should experience near-native C++ compute power.
- **Minimalist concurrency standard library:** Combined with the compiler's ownership
  mechanism, provide zero-lock-overhead concurrency primitives (such as Channel and
  Actor models), making multithreaded programming as safe as single-threaded.

## Evolution Roadmap for the Design & Architecture Group

- **Agile iteration, no long treatises:** We do not write tens-of-thousands-of-word
  language specifications. All syntax evolution must go through the agile flow:
  core-whitepaper proposal → prototype code → community/internal testing → finalize
  syntax.
- **Killer test cases:** Before the Aero 1.0 release, we must write, in Aero itself, a
  minimalist high-performance web server and a native AI matrix-multiplication engine.
  If writing these two tools feels awkward, the language design has a problem and must
  be reworked from scratch.
