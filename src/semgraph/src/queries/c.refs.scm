; C reference-site query (issue #78, Phase 2c tier-2).
;
; Captures *call sites* only; a bare `name(...)` resolves to a function by name.
; C has no methods, type references, or module imports beyond `#include` (which
; reuse the Phase-2a `import` definition nodes). See src/parse.rs.
;
; Adapted from CodeGraph's MIT-licensed tree-sitter tag queries
; (https://github.com/colbymchenry/codegraph). See NOTICE.

(call_expression) @call
