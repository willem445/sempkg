; Scala tier-3 definition node types (issue #78 Phase 2c part 3).
; object_definition → class, trait_definition → trait; val/var and enum cases
; are handled by a Rust visit hook. Adapted from CodeGraph's MIT-licensed config.
(function_definition) @def.method
(function_declaration) @def.method
(class_definition) @def.class
(object_definition) @def.class
(trait_definition) @def.trait
(enum_definition) @def.enum
(enum_case_definitions) @def.enum_member
(type_definition) @def.type_alias
(val_definition) @def.constant
(var_definition) @def.variable
(import_declaration) @def.import
