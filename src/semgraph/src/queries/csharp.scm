; C# tier-3 definition node types (issue #78 Phase 2c part 3). namespace_declaration
; does not scope names (CodeGraph descends without a scope node). Adapted from
; CodeGraph's MIT-licensed extractor config. See NOTICE.
(class_declaration) @def.class
(interface_declaration) @def.interface
(struct_declaration) @def.struct
(enum_declaration) @def.enum
(enum_member_declaration) @def.enum_member
(method_declaration) @def.method
(constructor_declaration) @def.method
(property_declaration) @def.property
(field_declaration) @def.field
(using_directive) @def.import
