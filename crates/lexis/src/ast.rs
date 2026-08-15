//! Query-expression AST vocabulary (Decision 6, Decision 14).
//!
//! WHY scoped to expressions only: Decision 14 assigns lexis "query AST,
//! SQL vocabulary" — the node types an expression tree is built from.
//! Decision 6 locks the parser *strategy* (hand-rolled recursive descent)
//! but explicitly defers the full statement grammar: "SELECT alone has
//! precedence tables, correlated subqueries, lateral joins, window
//! functions" is named there as the reason a combinator parser does not
//! scale, and ROADMAP.md assigns that grammar to Phase 04. This module
//! defines the expression vocabulary those statements will be built from —
//! `Expr` and its operators — not the statement types (`SELECT` / `INSERT`
//! / `UPDATE` / `DELETE` / `CREATE TABLE`) themselves, and it implements no
//! evaluator: null-propagation, comparison magnitude, and arithmetic
//! overflow behavior are executor concerns for the pinax facade.

use crate::identifier::ColumnName;
use crate::types::SqlType;
use crate::value::Value;

/// A query expression node.
///
/// WHY `#[non_exhaustive]`: the statement-level grammar landing in Phase 04
/// will need to extend this vocabulary (function calls, subqueries,
/// `CASE`); existing exhaustive matches outside this crate must not become
/// a breaking change when it does.
///
/// This type carries no evaluation semantics. In particular:
/// - Null propagation ("any operator on NULL returns NULL", Decision 5) is
///   not implemented here — evaluating an `Expr` is executor behavior.
/// - `BinaryOperator` variants are not type-checked against their operands
///   by this type; [`SqlType::check_comparable`] is the type-level rule a
///   future executor consults before evaluating a comparison.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Expr {
    /// A literal value.
    Literal(Value),
    /// A reference to a column by name.
    Column(ColumnName),
    /// A unary operator applied to one operand.
    UnaryOp {
        /// The operator.
        op: UnaryOperator,
        /// The operand.
        operand: Box<Expr>,
    },
    /// A binary operator applied to two operands.
    BinaryOp {
        /// The operator.
        op: BinaryOperator,
        /// The left-hand operand.
        left: Box<Expr>,
        /// The right-hand operand.
        right: Box<Expr>,
    },
    /// An explicit `CAST(operand AS target)`.
    ///
    /// WHY explicit CAST is its own node rather than an implicit
    /// conversion inside `BinaryOp`: Decision 5 requires "cross-type
    /// operators require explicit `CAST(x AS TYPE)`" — the AST vocabulary
    /// must have a place to put that explicitness, or the parser would
    /// have nowhere to record it.
    Cast {
        /// The operand being cast.
        operand: Box<Expr>,
        /// The target type.
        target: SqlType,
    },
    /// `operand IS NULL`.
    IsNull(Box<Expr>),
    /// `operand IS NOT NULL`.
    IsNotNull(Box<Expr>),
}

/// A unary query operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum UnaryOperator {
    /// Logical negation.
    Not,
    /// Arithmetic negation.
    Neg,
}

/// A binary query operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BinaryOperator {
    /// `=`
    Eq,
    /// `<>` / `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `AND`
    And,
    /// `OR`
    Or,
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Mod,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_wraps_a_value() {
        let expr = Expr::Literal(Value::Integer(1));
        assert_eq!(expr, Expr::Literal(Value::Integer(1)));
    }

    #[test]
    fn binary_op_nests_boxed_operands() {
        let expr = Expr::BinaryOp {
            op: BinaryOperator::Eq,
            left: Box::new(Expr::Column(
                ColumnName::try_from("id").expect("valid identifier"),
            )),
            right: Box::new(Expr::Literal(Value::Integer(1))),
        };
        let Expr::BinaryOp { op, .. } = expr else {
            panic!("constructed a BinaryOp");
        };
        assert_eq!(op, BinaryOperator::Eq);
    }

    #[test]
    fn cast_carries_target_type() {
        let expr = Expr::Cast {
            operand: Box::new(Expr::Column(
                ColumnName::try_from("id").expect("valid identifier"),
            )),
            target: SqlType::Text,
        };
        let Expr::Cast { target, .. } = expr else {
            panic!("constructed a Cast");
        };
        assert_eq!(target, SqlType::Text);
    }

    #[test]
    fn is_null_and_is_not_null_wrap_one_operand() {
        let column = || {
            Box::new(Expr::Column(
                ColumnName::try_from("id").expect("valid identifier"),
            ))
        };
        assert_eq!(Expr::IsNull(column()), Expr::IsNull(column()));
        assert_ne!(Expr::IsNull(column()), Expr::IsNotNull(column()));
    }
}
