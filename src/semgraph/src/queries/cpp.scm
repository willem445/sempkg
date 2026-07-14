; C++ definition-extraction query (issue #78, Phase 2c tier-2).
;
; Captures function *definitions* (a definition whose declarator is a
; `Type::member` qualified id is reclassified to a `method` in Rust; in-class
; declarations and fields are NOT captured, matching CodeGraph 0.9.7),
; class/struct/enum definitions (incl. `enum class`), enum members, `using`/
; `typedef` type aliases, and `#include` directives. Namespaces are ignored (no
; node, not part of qualified names) — 0.9.7 does the same. See src/parse.rs.
;
; Adapted from CodeGraph's MIT-licensed tree-sitter tag queries
; (https://github.com/colbymchenry/codegraph). See NOTICE.

(function_definition) @def.function
(class_specifier name: (type_identifier) body: (field_declaration_list)) @def.class
(struct_specifier name: (type_identifier) body: (field_declaration_list)) @def.struct
(enum_specifier name: (type_identifier) body: (enumerator_list)) @def.enum
(enumerator) @def.enum_member
(type_definition) @def.type_alias
(alias_declaration) @def.type_alias
(preproc_include) @def.import
