; Java definition-extraction query (issue #78, Phase 2c tier-2).
;
; Captures the package declaration (a `namespace` node), classes, interfaces,
; enums + members, methods + constructors, and fields. Imports are captured too.
; Qualified names are `package::Class::member` (`::`-joined), derived in Rust
; along with visibility/static flags and Javadoc docstrings (see src/parse.rs).
;
; Adapted from CodeGraph's MIT-licensed tree-sitter tag queries
; (https://github.com/colbymchenry/codegraph). See NOTICE.

(package_declaration) @def.namespace
(class_declaration) @def.class
(interface_declaration) @def.interface
(enum_declaration) @def.enum
(enum_constant) @def.enum_member
(method_declaration) @def.method
(constructor_declaration) @def.method
(field_declaration) @def.field
(import_declaration) @def.import
