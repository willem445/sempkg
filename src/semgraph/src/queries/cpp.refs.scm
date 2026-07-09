; C++ reference-site query (issue #78, Phase 2c tier-2).
;
; Captures *call sites* only. A bare `name(...)` resolves to a function; a member
; call `recv.m(...)` / `recv->m(...)` resolves to a method by its (unique) name —
; CodeGraph 0.9.7 resolves C++ member calls name-based, without receiver typing.
; Callee shape is derived in Rust (see src/parse.rs). C++ emits no `references`.
;
; Adapted from CodeGraph's MIT-licensed tree-sitter tag queries
; (https://github.com/colbymchenry/codegraph). See NOTICE.

(call_expression) @call
