; TypeScript/JavaScript reference-site query (issue #78, Phase 2b).
;
; Captures *call* and *construction* sites. `new Name(...)` becomes an
; `instantiates` edge; a plain `name(...)` resolves to a function (→ `calls`);
; a member call `recv.m(...)` resolves against the receiver's locally inferred
; type. Callee shape and receiver typing are derived in Rust (see src/parse.rs).
; Type-reference sites are walked over each definition's signature/field
; annotation in Rust rather than via this query.
;
; Adapted from CodeGraph's MIT-licensed tree-sitter tag queries
; (https://github.com/colbymchenry/codegraph). See NOTICE.

(call_expression) @call
(new_expression) @new
