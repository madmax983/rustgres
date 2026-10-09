//! Executor: runs the parsed AST against MVCC storage (v0.5).
//!
//! Every statement runs under the engine lock with a snapshot and its
//! transaction's xid. Reads filter row/table versions through
//! `row_visible`/`table_visible`; writes append new row versions (INSERT),
//! mark `xmax` (DELETE), or do both (UPDATE), recording each change in
//! the statement context's write log for undo and WAL.
//!
//! Errors carry a Postgres SQLSTATE code so the server can emit a
//! proper ErrorResponse.
//!
//! v0.2 additions: `+` expression evaluation, `$N` parameter type
//! inference (`infer_param_types`), text-format parameter parsing
//! (`bind_params`), parameter substitution (`subst_params`), and
//! result-column typing for Describe (`describe_columns`).

//! v0.3: transaction control statements (Begin/Commit/...) are parsed into
//! the AST but never reach `execute` — the session layer in server.rs
//! intercepts them. Their match arms below are defensive only.
//!
//! v0.5: MVCC execution. UPDATE/DELETE are new. Write-write conflicts:
//! if a row version visible in our snapshot was deleted/updated by a
//! transaction that committed after our snapshot was taken,
//! REPEATABLE READ and SERIALIZABLE fail with 40001 ("could not serialize
//! access due to concurrent update"), like Postgres. READ COMMITTED
//! takes a fresh snapshot per statement, so the conflicting version is
//! simply not visible and the statement operates on the newest committed
//! data. Concurrent *uncommitted* writes are last-writer-wins (no row
//! locking in v0.5), documented in the README.
//!
//! v0.6: query engine. SELECT is a real pipeline now: FROM sources
//! (tables, derived tables, INNER/LEFT/CROSS joins with ON), general
//! WHERE/ON/HAVING predicates (AND/OR/NOT, comparisons, IS NULL,
//! IN/EXISTS subqueries — correlated subqueries supported), hash-based
//! GROUP BY with aggregates (COUNT/SUM/AVG/MIN/MAX), DISTINCT, ORDER BY
//! (output names, aliases, positions, and — for plain queries —
//! non-projected columns), OFFSET, and SELECT ... FOR UPDATE row locks.

pub(crate) use crate::index::{Index, IndexDef, IndexKey, index_key_cmp};
pub(crate) use crate::sql::{
    AggFunc, AlterAction, ArithOp, CheckDef, CmpOp, ConflictAction, ConflictArbiter, CteBody,
    CteDef, CteMaterialize, DefaultExpr, ExplainFormat, Expr, FkAction, FkDef, FrameBound,
    FrameExclusion, FromItem, IndexColSpec, InsertIndirection, InsertTarget, InsertValue,
    IsolationLevel, JoinKind, Literal, OnConflict, OrderTerm, OrderedSetAgg, QuantKind, QuantOp,
    RaiseLevel, SelectItem, SelectStmt, SequenceOpts, SerialKind, SetOpKind, SetOpRoot, SqlError,
    Stmt, TableDef, TriggerBodyStmt, TriggerDef, TriggerTiming, UniqueDef, WindowFrame, WindowFunc,
    collect_col_refs, collect_table_refs, parse_statement, parse_trigger_body, trig_event,
    validate_constraint_expr,
};
pub(crate) use crate::storage::index_visible;
pub(crate) use crate::storage::{
    ArrayElem, ArrayVal, BigDec, BitString, ColStats, ColType, Database, Engine, Numeric,
    NumericSpecial, OperDef, Row, RowVersion, Sequence, ShellType, Snapshot, Table, TableStats,
    Value, ViewDef, WriteOp, row_visible, toast_consts, toast_storage,
};
pub(crate) use std::cell::RefCell;
pub(crate) use std::cmp::Ordering;
pub(crate) use std::collections::{HashMap, HashSet};
pub(crate) use std::ops::Bound;
pub(crate) use std::rc::Rc;

mod agg;
mod alter;
mod analyze;
mod arith;
mod cast;
mod catalog;
mod constraint;
mod copy_stmt;
mod core;
mod create;
mod cte;
mod ddl_func;
mod ddl_types;
mod describe;
mod dispatch;
mod dml;
mod eval;
mod explain;
mod explain_nodes;
mod from;
mod funcs;
mod indexes;
mod info_schema;
mod join;
mod orderby;
mod params;
mod partition;
mod privilege;
mod project;
mod reorder;
mod roles;
mod seq;
mod sje;
mod views;
mod window;

#[cfg(test)]
#[path = "test_mods/hash_join_tests.rs"]
mod hash_join_tests;
#[cfg(test)]
#[path = "test_mods/tests.rs"]
mod tests;
#[cfg(test)]
#[path = "test_mods/v067_math_tests.rs"]
mod v067_math_tests;
#[cfg(test)]
#[path = "test_mods/v097_domain_tests.rs"]
mod v097_domain_tests;
#[cfg(test)]
#[path = "test_mods/v102_raise_notice_tests.rs"]
mod v102_raise_notice_tests;
#[cfg(test)]
#[path = "test_mods/v108_costs_off_tests.rs"]
mod v108_costs_off_tests;
#[cfg(test)]
#[path = "test_mods/v110_lateral_tests.rs"]
mod v110_lateral_tests;
#[cfg(test)]
#[path = "test_mods/v112_empty_select_tests.rs"]
mod v112_empty_select_tests;
#[cfg(test)]
#[path = "test_mods/v126_lateral_validation_tests.rs"]
mod v126_lateral_validation_tests;
#[cfg(test)]
#[path = "test_mods/v127_projectset_tests.rs"]
mod v127_projectset_tests;
#[cfg(test)]
#[path = "test_mods/v128_projectset_tests.rs"]
mod v128_projectset_tests;
#[cfg(test)]
#[path = "test_mods/v129_filter_tests.rs"]
mod v129_filter_tests;
#[cfg(test)]
#[path = "test_mods/v130_ordered_set_tests.rs"]
mod v130_ordered_set_tests;
#[cfg(test)]
#[path = "test_mods/v131_groups_exclusion_tests.rs"]
mod v131_groups_exclusion_tests;
#[cfg(test)]
#[path = "test_mods/v132_sql_function_tests.rs"]
mod v132_sql_function_tests;
#[cfg(test)]
#[path = "test_mods/v133_dml_function_bodies.rs"]
mod v133_dml_function_bodies;
#[cfg(test)]
#[path = "test_mods/v134_dml_returning_coercion.rs"]
mod v134_dml_returning_coercion;
#[cfg(test)]
#[path = "test_mods/v135_select_final_coercion.rs"]
mod v135_select_final_coercion;
#[cfg(test)]
#[path = "test_mods/v136_immutable_fold_tests.rs"]
mod v136_immutable_fold_tests;
#[cfg(test)]
#[path = "test_mods/v137_plan_fold_tests.rs"]
mod v137_plan_fold_tests;
#[cfg(test)]
#[path = "test_mods/v138_cast_tests.rs"]
mod v138_cast_tests;
#[cfg(test)]
#[path = "test_mods/v139_bit_cte_tests.rs"]
mod v139_bit_cte_tests;
#[cfg(test)]
#[path = "test_mods/v140_syscols_returning_tests.rs"]
mod v140_syscols_returning_tests;
#[cfg(test)]
#[path = "test_mods/v145_join_removal_tests.rs"]
mod v145_join_removal_tests;
#[cfg(test)]
#[path = "test_mods/v146_verbose_output_tests.rs"]
mod v146_verbose_output_tests;
#[cfg(test)]
#[path = "test_mods/v147_join_removal_tests.rs"]
mod v147_join_removal_tests;
#[cfg(test)]
#[path = "test_mods/v148_join_label_materialize_tests.rs"]
mod v148_join_label_materialize_tests;
#[cfg(test)]
#[path = "test_mods/v149_right_join_flip_tests.rs"]
mod v149_right_join_flip_tests;
#[cfg(test)]
#[path = "test_mods/v149b_const_false_tests.rs"]
mod v149b_const_false_tests;
#[cfg(test)]
#[path = "test_mods/v151_pullup_repair_tests.rs"]
mod v151_pullup_repair_tests;
#[cfg(test)]
#[path = "test_mods/v154_cte_inline_tests.rs"]
mod v154_cte_inline_tests;
#[cfg(test)]
#[path = "test_mods/v156_in_any_tests.rs"]
mod v156_in_any_tests;
#[cfg(test)]
#[path = "test_mods/v159_const_false_tests.rs"]
mod v159_const_false_tests;
#[cfg(test)]
#[path = "test_mods/v160_immutable_fold_tests.rs"]
mod v160_immutable_fold_tests;
#[cfg(test)]
#[path = "test_mods/v161_distinct_limit_tests.rs"]
mod v161_distinct_limit_tests;
#[cfg(test)]
#[path = "test_mods/v162_tiny_table_seqscan_tests.rs"]
mod v162_tiny_table_seqscan_tests;
#[cfg(test)]
#[path = "test_mods/v163_nestloop_side_selection_tests.rs"]
mod v163_nestloop_side_selection_tests;
#[cfg(test)]
#[path = "test_mods/v164_hashjoin_choice_tests.rs"]
mod v164_hashjoin_choice_tests;
#[cfg(test)]
#[path = "test_mods/v165_cross_join_reorder_tests.rs"]
mod v165_cross_join_reorder_tests;
#[cfg(test)]
#[path = "test_mods/v166_nway_cross_join_reorder_tests.rs"]
mod v166_nway_cross_join_reorder_tests;
#[cfg(test)]
#[path = "test_mods/v168_hash_anti_join_tests.rs"]
mod v168_hash_anti_join_tests;
#[cfg(test)]
#[path = "test_mods/v169_merge_anti_join_tests.rs"]
mod v169_merge_anti_join_tests;
#[cfg(test)]
#[path = "test_mods/v175_hashjoin_order_tests.rs"]
mod v175_hashjoin_order_tests;
#[cfg(test)]
#[path = "test_mods/v176_filter_order_tests.rs"]
mod v176_filter_order_tests;
#[cfg(test)]
#[path = "test_mods/v177_nested_sje_tests.rs"]
mod v177_nested_sje_tests;
#[cfg(test)]
#[path = "test_mods/v179_sje_star_tests.rs"]
mod v179_sje_star_tests;
#[cfg(test)]
#[path = "test_mods/variance_stress_tests.rs"]
mod variance_stress_tests;

pub(crate) use agg::*;
pub(crate) use alter::*;
pub(crate) use analyze::*;
pub(crate) use arith::*;
pub(crate) use cast::*;
pub(crate) use catalog::*;
pub(crate) use constraint::*;
pub(crate) use copy_stmt::*;
pub(crate) use core::*;
pub(crate) use create::*;
pub(crate) use cte::*;
pub(crate) use ddl_func::*;
pub(crate) use ddl_types::*;
pub(crate) use describe::*;
pub(crate) use dispatch::*;
pub(crate) use dml::*;
pub(crate) use eval::*;
pub(crate) use explain::*;
pub(crate) use explain_nodes::*;
pub(crate) use from::*;
pub(crate) use funcs::*;
pub(crate) use indexes::*;
pub(crate) use info_schema::*;
pub(crate) use join::*;
pub(crate) use orderby::*;
pub(crate) use params::*;
pub(crate) use partition::*;
pub(crate) use privilege::*;
pub(crate) use project::*;
pub(crate) use reorder::*;
pub(crate) use roles::*;
pub(crate) use seq::*;
pub(crate) use sje::*;
pub(crate) use views::*;
pub(crate) use window::*;
