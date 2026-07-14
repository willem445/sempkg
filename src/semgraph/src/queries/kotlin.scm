; Kotlin tier-3 definition node types (issue #78 Phase 2c part 3). A
; `package_header` becomes a `namespace` scoping the file; `class_declaration`
; is reclassified to class/interface/enum in Rust. Grammar: tree-sitter-kotlin-ng.
; Adapted from CodeGraph's MIT-licensed extractor config. See NOTICE.
(function_declaration) @def.callable
(class_declaration) @def.class
(object_declaration) @def.class
(enum_entry) @def.enum_member
(type_alias) @def.type_alias
(import) @def.import
(package_header) @def.namespace
