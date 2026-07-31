; C++ reference-site query (issue #78, Phase 2c tier-2).
;
; Captures call and construction sites. A bare `name(...)` resolves to a
; function; a member call `recv.m(...)` / `recv->m(...)` resolves to a method by
; its (unique) name (CodeGraph 0.9.7 resolves C++ member calls name-based, without
; receiver typing); `new T(...)` → `instantiates`. A namespace-qualified
; `ns::f(...)` call resolves against its qualified name (and drops when 0.9.7's
; namespace-stripped names have no match). Callee shape is derived in Rust (see
; src/parse.rs). C++ emits no `references`.
;
; Adapted from CodeGraph's MIT-licensed tree-sitter tag queries
; (https://github.com/colbymchenry/codegraph). See NOTICE.

(call_expression) @call
(new_expression) @new
