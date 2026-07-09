; Ruby tier-3 definition node types (issue #78 Phase 2c part 3).
;
; The tier-3 packs use a config-driven recursive-descent extractor
; (src/tier3/) rather than the flat query path of tier-1, mirroring CodeGraph's
; own engine. This file is the reviewed manifest of the *definition* node types
; that become graph nodes; it is compiled against the grammar in a unit test so
; a grammar upgrade that renames a node type fails loudly. Kind selection
; (function vs method, module scoping) and qualified names are derived in Rust.
;
; Adapted from CodeGraph's MIT-licensed extractor config. See NOTICE.
(method) @def.callable
(singleton_method) @def.method
(class) @def.class
(module) @def.module
(assignment) @def.variable
