; Java reference-site query (issue #78, Phase 2c tier-2).
;
; Captures method-invocation and object-creation (`new T(...)` →
; `instantiates`) sites. A qualified `Class.method(...)` resolves to the method
; by its package-qualified name; an instance call `recv.m(...)` resolves when the
; receiver's type is locally inferable (a typed parameter/local) and is otherwise
; dropped. Type-reference sites are walked over each definition's signature in
; Rust. See src/parse.rs.
;
; Adapted from CodeGraph's MIT-licensed tree-sitter tag queries
; (https://github.com/colbymchenry/codegraph). See NOTICE.

(method_invocation) @call
(object_creation_expression) @new
