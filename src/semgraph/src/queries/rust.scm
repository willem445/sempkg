; Rust definition-extraction query (issue #78, Phase 2a).
;
; Captures top-level and nested *definitions* only; reference/call resolution is
; Phase 2b. Names, qualified names, visibility, async, and structural nesting are
; derived in Rust from the captured node (see src/parse.rs), not in the query, so
; the query stays small and portable across grammar revisions.
;
; Structure and capture conventions are adapted from CodeGraph's MIT-licensed
; tree-sitter tag queries (https://github.com/colbymchenry/codegraph). See NOTICE.

(function_item) @def.function
(trait_item) @def.trait
(struct_item) @def.struct
(enum_item) @def.enum
(enum_variant) @def.enum_member
(type_item) @def.type_alias
(const_item) @def.variable
(static_item) @def.variable
(use_declaration) @def.import
