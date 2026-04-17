# Darc

1. Follow idiomatic Rust and standard library-first design.
2. Prefer less code when behavior stays equally clear and correct.
3. Avoid new dependencies unless clearly justified. When adding one, enable only the required Cargo features and disable
   default features unless they are necessary.
4. DRY: Reuse existing abstractions before adding new ones.
5. Keep code DRY and cohesive. If the same logic or data shape appears in more than one place, refactor it into a shared
   function, helper type, or module unless duplication is clearly simpler. Avoid redundant struct fields and duplicated
   derived state; store canonical data once and compute the rest from it, unless duplication is clearly justified by
   readability, performance, or API boundaries.
6. Add a one-line doc comment to every major struct and function.
7. Run Cargo fmt and clippy after every patch or refactor that touches Rust code.
    - fmt: `cargo +nightly fmt`
    - clippy: `cargo clippy --workspace --all-targets --all-features -- -D warnings -W clippy::all`
8. Split crates by cohesive capability and clear dependency direction, not by file count or verb names alone. 
9. Prefer leaf capability crates under a thin orchestration/facade crate. Lower-level crates must not depend on higher-level workflow crates.
10. Only create or keep a crate boundary when it gives one dominant reason to change, a small API surface, and reduced change-coupling.
11. Extract shared models and helpers downward into lower-level crates instead of duplicating them or making parser/storage code depend on orchestration code.
12. Do not use real or local data for test fixtures or examples; use synthetic placeholders instead (for example, never copy UUIDs from local session history).
13. Use conventional commits.
- Format: `<type>(<scope>): <imperative summary>`
- Rules:
    - Use lowercase for `type` and `scope`.
    - `type` must be one of: `feat`, `fix`, `refactor`, `perf`, `docs`, `test`, `chore`, `build`, `ci`.
    - `scope` must name the primary area affected, such as a crate, module, package, or feature. e.g., `cli`, `parser`,
      `storage`, `api` (optional).
    - `summary` must be a short imperative phrase describing the change itself, not the intention behind it.
    - Do not end the summary with a period.
    - Keep the subject line concise, ideally under 72 characters.
    - One commit should represent one logical change. The message should describe that single change.
