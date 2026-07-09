; Java reference-site query (issue #78, Phase 2c tier-2).
;
; Captures *method-invocation* sites. A qualified `Class.method(...)` resolves to
; the method by its package-qualified name; an instance call `recv.m(...)` whose
; receiver is not a class name is dropped (CodeGraph 0.9.7 does not name-resolve
; Java instance calls). Type-reference sites are walked over each definition's
; signature in Rust. See src/parse.rs.
;
; Adapted from CodeGraph's MIT-licensed tree-sitter tag queries
; (https://github.com/colbymchenry/codegraph). See NOTICE.

(method_invocation) @call
