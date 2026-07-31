; Go reference-site query (issue #78, Phase 2c tier-2).
;
; Captures *call sites* only. A bare `Name(...)` resolves to a function; a
; selector call `recv.M(...)` resolves to a method by its (unique) name (a
; package-qualified `pkg.F(...)` naturally drops when `F` is not a graph symbol).
; Type-reference sites (struct/interface types in a signature) are walked over
; each definition's signature in Rust. See src/parse.rs.
;
; Adapted from CodeGraph's MIT-licensed tree-sitter tag queries
; (https://github.com/colbymchenry/codegraph). See NOTICE.

(call_expression) @call
