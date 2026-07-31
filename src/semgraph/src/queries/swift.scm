; Swift tier-3 definition node types (issue #78 Phase 2c part 3).
; `class_declaration` is reclassified to class/struct/enum in Rust; protocol
; method requirements (protocol_function_declaration) are intentionally not
; captured, matching CodeGraph. Adapted from CodeGraph's MIT-licensed config.
(function_declaration) @def.callable
(class_declaration) @def.class
(protocol_declaration) @def.interface
(enum_entry) @def.enum_member
(typealias_declaration) @def.type_alias
(import_declaration) @def.import
