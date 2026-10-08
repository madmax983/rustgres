//! SQL tokenizer + parser for the rustgres subset.
//!
//! v0.1 statements:
//!   CREATE TABLE name (col TYPE [, ...])
//!   INSERT INTO name [(col, ...)] VALUES (v, ...), (...), ...
//!   SELECT [* | expr [, ...]] FROM name [WHERE col = lit|$N [AND ...]] [LIMIT n]
//!   SELECT expr [, ...]                      (no FROM: single row)
//!   DROP TABLE [IF EXISTS] name
//!
//! v0.2: `$N` parameter placeholders (1-based) and a tiny expression
//! language for the SELECT list: literals, column refs, params, `+`, parens.
//!
//! v0.3: transaction control statements (BEGIN/COMMIT/.../SAVEPOINT).
//!
//! v0.5: UPDATE / DELETE, VACUUM.
//!
//! v0.6: query engine.
//!   SELECT [DISTINCT] items
//!     FROM source [, ...]                     -- comma = CROSS JOIN
//!          | t [AS] alias
//!          | (SELECT ...) [AS] alias           -- derived table (alias required)
//!          | source [INNER] JOIN source ON expr
//!          | source LEFT [OUTER] JOIN source ON expr
//!     [WHERE predicate] [GROUP BY expr, ...] [HAVING predicate]
//!     [ORDER BY ...] [LIMIT n] [OFFSET n] [FOR UPDATE]
//!   Predicates: AND / OR / NOT, comparisons (= <> < <= > >=),
//!   IS [NOT] NULL, [NOT] IN (subquery), [NOT] EXISTS (subquery).
//!   Expressions: qualified refs (t.col), `t.*`, aggregates
//!   (COUNT(*)/COUNT(e)/SUM(e)/AVG(e)/MIN(e)/MAX(e)), scalar subqueries,
//!   select-list aliases ([AS] name).
//!
//! Keywords are case-insensitive; unquoted identifiers fold to lowercase.
//! String literals use single quotes with `''` as the escape for a quote.

pub(crate) use std::sync::Arc;

pub(crate) use crate::storage::ColType;

mod ast;
mod parser;

#[cfg(test)]
#[path = "test_mods/for_update_of_tests.rs"]
mod for_update_of_tests;
#[cfg(test)]
#[path = "test_mods/partition_by_tests.rs"]
mod partition_by_tests;
#[cfg(test)]
#[path = "test_mods/v107_set_tests.rs"]
mod v107_set_tests;
#[cfg(test)]
#[path = "test_mods/v108_explain_costs_tests.rs"]
mod v108_explain_costs_tests;
#[cfg(test)]
#[path = "test_mods/v109_function_tests.rs"]
mod v109_function_tests;
#[cfg(test)]
#[path = "test_mods/v145_explain_options_tests.rs"]
mod v145_explain_options_tests;
#[cfg(test)]
#[path = "test_mods/v72_ddl_tests.rs"]
mod v72_ddl_tests;

pub(crate) use ast::*;
pub(crate) use parser::*;
