; C definition-extraction query (issue #78, Phase 2c tier-2).
;
; Captures definitions only: function *definitions* (prototypes/declarations are
; not emitted, matching CodeGraph 0.9.7), struct/enum type definitions with a
; body, enum members, typedefs (type aliases), and `#include` directives.
; Struct fields and top-level variables are intentionally NOT captured — 0.9.7
; emits neither for C. Names, qualified names (`Enum::MEMBER`), and docstrings
; are derived in Rust (see src/parse.rs).
;
; Adapted from CodeGraph's MIT-licensed tree-sitter tag queries
; (https://github.com/colbymchenry/codegraph). See NOTICE.

(function_definition) @def.function
(struct_specifier name: (type_identifier) body: (field_declaration_list)) @def.struct
(enum_specifier name: (type_identifier) body: (enumerator_list)) @def.enum
(enumerator) @def.enum_member
(type_definition) @def.type_alias
(preproc_include) @def.import
