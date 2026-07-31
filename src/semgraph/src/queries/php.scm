; PHP tier-3 definition node types (issue #78 Phase 2c part 3). See ruby.scm for
; how tier-3 `.scm` manifests are used. Kinds/signatures derived in Rust.
; Adapted from CodeGraph's MIT-licensed extractor config. See NOTICE.
(function_definition) @def.function
(method_declaration) @def.method
(class_declaration) @def.class
(trait_declaration) @def.trait
(interface_declaration) @def.interface
(enum_declaration) @def.enum
(enum_case) @def.enum_member
(const_declaration) @def.constant
(property_declaration) @def.field
(namespace_use_declaration) @def.import
