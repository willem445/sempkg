; Rust reference-site query (issue #78, Phase 2b).
;
; Captures *call sites* only; the callee shape (bare `foo(...)`, qualified
; `Type::assoc(...)`, or method `recv.m(...)`) and the receiver's inferred type
; are derived in Rust from the captured node (see src/parse.rs). Type-reference
; sites are walked directly over each definition's signature in Rust rather than
; via this query, and import sites reuse the Phase-2a `import` definition nodes.
;
; Adapted from CodeGraph's MIT-licensed tree-sitter tag queries
; (https://github.com/colbymchenry/codegraph). See NOTICE.

(call_expression) @call
