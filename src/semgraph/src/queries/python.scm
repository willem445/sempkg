; Python definition-extraction query (issue #78, Phase 2a).
;
; Captures definitions only (functions, classes, methods, imports, module-level
; variables, PEP 695 type aliases). Method-vs-function classification, qualified
; names, decorators, docstrings and async flags are derived in Rust from the
; captured node (see src/parse.rs). Reference/call resolution is Phase 2b.
;
; Adapted from CodeGraph's MIT-licensed tree-sitter tag queries
; (https://github.com/colbymchenry/codegraph). See NOTICE.

(function_definition) @def.function
(class_definition) @def.class
(import_statement) @def.import
(import_from_statement) @def.import
(type_alias_statement) @def.type_alias
; Module-level assignments only (nested class/enum members are not definitions).
(module (expression_statement (assignment left: (identifier)) @def.variable))
