; Python reference-site query (issue #78, Phase 2b).
;
; Captures *call sites* only. A bare `name(...)` resolves to a function (→
; `calls`) or a class (→ `instantiates`, e.g. `Circle(r)`); an attribute call
; `recv.m(...)` resolves against the receiver's locally inferred type. The
; distinction and receiver typing are derived in Rust (see src/parse.rs). Python
; type annotations deliberately do NOT emit `references` edges — CodeGraph 0.9.7
; emits none for Python, and we match that.
;
; Adapted from CodeGraph's MIT-licensed tree-sitter tag queries
; (https://github.com/colbymchenry/codegraph). See NOTICE.

(call) @call
