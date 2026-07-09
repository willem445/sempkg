; TypeScript/JavaScript definition-extraction query (issue #78, Phase 2a).
;
; Captures definitions only. Class fields (`public_field_definition`) are treated
; as members alongside methods — this mirrors CodeGraph, which records class
; property declarations as `method` nodes. Export/async/static/abstract flags,
; qualified names, and structural nesting are derived in Rust from the captured
; node (see src/parse.rs). Reference/call resolution is Phase 2b.
;
; Adapted from CodeGraph's MIT-licensed tree-sitter tag queries
; (https://github.com/colbymchenry/codegraph). See NOTICE.

(function_declaration) @def.function
(generator_function_declaration) @def.function
(class_declaration) @def.class
(abstract_class_declaration) @def.class
(interface_declaration) @def.interface
(method_definition) @def.method
(public_field_definition) @def.method
(enum_declaration) @def.enum
(enum_body (property_identifier) @def.enum_member)
(enum_body (enum_assignment name: (property_identifier)) @def.enum_member)
(type_alias_declaration) @def.type_alias
(import_statement) @def.import
