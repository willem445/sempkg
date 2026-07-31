"""A deliberately-unresolvable reference case.

This file exists to probe the ``unresolved_refs`` table: it calls a name that
is defined nowhere in the fixture and imports from a module that does not
exist. CodeGraph 0.9.7 still records **zero** rows in ``unresolved_refs`` for
this input (it empties the table during batched resolution — see
``tests/fixtures/README.md``), so this case documents that behavior rather than
populating the table.
"""

from __future__ import annotations

from this_module_does_not_exist import missing_helper


def calls_the_void() -> int:
    # `totally_undefined_symbol` is defined nowhere in the fixture tree.
    return totally_undefined_symbol(missing_helper())
