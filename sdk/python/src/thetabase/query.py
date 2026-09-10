"""The typed query builder.

Fluent in Python, because that is what makes a query pleasant to write. What it
produces is an AST, and the AST is rendered to SQL-subset source and bound
parameters by the WebAssembly core — so the same query built in TypeScript
reaches the server as the same bytes, and the rule that a value never becomes
query text has one implementation rather than one per language.

Nothing here interpolates. ``eq("name", user_input)`` puts ``user_input`` in the
parameter map and ``$p0`` in the text, whatever it contains.
"""

from __future__ import annotations

import copy
from dataclasses import dataclass, field
from typing import Any, Literal, TYPE_CHECKING

if TYPE_CHECKING:
    from .scribe import ScribeCore

CompareOp = Literal["eq", "ne", "lt", "lte", "gt", "gte"]
Predicate = dict[str, Any]


def eq(column: str, value: Any) -> Predicate:
    return {"kind": "compare", "column": column, "op": "eq", "value": value}


def ne(column: str, value: Any) -> Predicate:
    return {"kind": "compare", "column": column, "op": "ne", "value": value}


def lt(column: str, value: Any) -> Predicate:
    return {"kind": "compare", "column": column, "op": "lt", "value": value}


def lte(column: str, value: Any) -> Predicate:
    return {"kind": "compare", "column": column, "op": "lte", "value": value}


def gt(column: str, value: Any) -> Predicate:
    return {"kind": "compare", "column": column, "op": "gt", "value": value}


def gte(column: str, value: Any) -> Predicate:
    return {"kind": "compare", "column": column, "op": "gte", "value": value}


def is_in(column: str, values: list[Any]) -> Predicate:
    return {"kind": "in", "column": column, "values": values}


def is_null(column: str) -> Predicate:
    return {"kind": "isNull", "column": column}


def not_null(column: str) -> Predicate:
    return {"kind": "notNull", "column": column}


def and_(*terms: Predicate) -> Predicate:
    return {"kind": "and", "terms": list(terms)}


def or_(*terms: Predicate) -> Predicate:
    return {"kind": "or", "terms": list(terms)}


def not_(term: Predicate) -> Predicate:
    return {"kind": "not", "term": term}


@dataclass(frozen=True)
class Query:
    """A query under construction.

    Frozen, and every method returns a new one: a builder that mutated would
    make a shared base query change under whoever else was holding it.
    """

    table_name: str
    columns: tuple[str, ...] = ()
    filter: Predicate | None = None
    order_by_terms: tuple[dict[str, Any], ...] = ()
    limit_count: int | None = None
    offset_count: int | None = None

    def select(self, *columns: str) -> Query:
        from dataclasses import replace

        return replace(self, columns=self.columns + columns)

    def where(self, predicate: Predicate) -> Query:
        """Keep rows matching ``predicate``. Repeated calls are ANDed."""
        from dataclasses import replace

        combined = and_(self.filter, predicate) if self.filter else predicate
        return replace(self, filter=combined)

    def order_by(self, column: str, descending: bool = False) -> Query:
        from dataclasses import replace

        term = {"column": column, "descending": descending}
        return replace(self, order_by_terms=self.order_by_terms + (term,))

    def limit(self, count: int) -> Query:
        from dataclasses import replace

        return replace(self, limit_count=count)

    def offset(self, count: int) -> Query:
        from dataclasses import replace

        return replace(self, offset_count=count)

    def to_ast(self) -> dict[str, Any]:
        """The AST, for a caller that wants to inspect or send it."""
        ast: dict[str, Any] = {
            "table": self.table_name,
            "columns": list(self.columns),
            "orderBy": [dict(t) for t in self.order_by_terms],
        }
        if self.filter is not None:
            ast["filter"] = copy.deepcopy(self.filter)
        if self.limit_count is not None:
            ast["limit"] = self.limit_count
        if self.offset_count is not None:
            ast["offset"] = self.offset_count
        return ast

    def render(self, core: ScribeCore) -> dict[str, Any]:
        """Render through the core. Raises if an identifier could carry syntax."""
        return core.render_query(self.to_ast())


def table(name: str) -> Query:
    """Start a query against ``name``."""
    return Query(table_name=name)
