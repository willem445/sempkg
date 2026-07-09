; Go definition-extraction query (issue #78, Phase 2c tier-2).
;
; Captures functions, methods (receiver-qualified `Type::Method`), type
; declarations (reclassified in Rust to struct / interface / type_alias by their
; right-hand side), top-level constants and variables, and imports. Struct fields
; are NOT captured (0.9.7 emits none). is_exported, qualified names, docstrings,
; and struct/interface reclassification are derived in Rust (see src/parse.rs).
;
; Adapted from CodeGraph's MIT-licensed tree-sitter tag queries
; (https://github.com/colbymchenry/codegraph). See NOTICE.

; Only top-level (source_file-scoped) type/const/var declarations are emitted —
; locals inside function bodies are not definitions. CodeGraph 0.9.7 also emits a
; node for a `type X int` *definition* (captured here as a `type_spec`) but NOT
; for a `type X = Y` *alias* (`type_alias`), so the alias form is deliberately
; not captured.
(function_declaration) @def.function
(method_declaration) @def.method
(source_file (type_declaration (type_spec) @def.type_alias))
(source_file (const_declaration (const_spec) @def.constant))
(source_file (var_declaration (var_spec) @def.variable))
(import_declaration (import_spec) @def.import)
(import_declaration (import_spec_list (import_spec) @def.import))
